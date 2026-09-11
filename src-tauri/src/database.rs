//! Everything that touches the *original* database.
//!
//! Every read of the original goes through this module, and every path through
//! it is read-only. ([`open_readonly`] is also used for the clone, where
//! read-only access is simply the right tool; nothing here can write to
//! either.)
//!
//! * connections are opened with `SQLITE_OPEN_READ_ONLY` and no `CREATE` flag,
//!   so SQLite opens the underlying file descriptor read-only;
//! * `PRAGMA query_only` is set as a second, independent barrier;
//! * nothing here executes user-supplied SQL.
//!
//! Migration SQL is executed elsewhere, and only against a [`crate::temp::TempClone`].

use crate::error::{display_name, MigraDryError, Result};
use crate::models::{DatabaseFingerprint, FileFingerprint};
use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

/// Every SQLite database file starts with these 16 bytes.
const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

const HASH_BUFFER_BYTES: usize = 64 * 1024;

/// How long SQLite may wait for a lock before giving up.
///
/// A short wait rather than none: another process holding a write lock for a
/// moment is ordinary, and failing instantly would turn a routine hiccup into a
/// refused preview. Waiting cannot make an inconsistent snapshot — SQLite's
/// locking is what guarantees consistency — so this trades a little latency for
/// availability without touching safety.
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// Opens a database strictly for reading.
///
/// This is the only function in MigraDry that opens the original database, and
/// it has no way to express anything but read-only: the flags are constants,
/// not parameters. Writable access exists solely on
/// [`crate::temp::TempClone`], which can only ever name a clone.
///
/// `SQLITE_OPEN_READ_ONLY` is passed *without* `SQLITE_OPEN_CREATE` and without
/// `SQLITE_OPEN_URI`. Leaving URI handling off matters: with it enabled, a file
/// literally named `file:...` would be reinterpreted as a URI carrying
/// arbitrary connection parameters such as `mode=rwc`.
///
/// Three independent barriers, then, and the third is a check rather than a
/// promise: SQLite is asked whether the handle it just returned really is
/// read-only, and the connection is thrown away if it says otherwise.
pub fn open_readonly(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    // Defence in depth. A read-only handle already rejects writes at the VFS
    // layer; `query_only` rejects them at the SQL layer as well.
    connection.pragma_update(None, "query_only", true)?;
    connection.busy_timeout(LOCK_WAIT)?;

    // Ask SQLite to confirm what we believe we asked for.
    if !connection.is_readonly("main")? {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_MISUSE),
            Some("MigraDry opened a database that SQLite does not report as read-only".to_string()),
        ));
    }
    Ok(connection)
}

/// Path of a sidecar file such as `app.db-wal`.
///
/// SQLite derives sidecar names by appending to the *full* database path, so
/// the suffix is appended to the whole `OsString` rather than to a file stem.
pub fn sidecar_path(database: &Path, suffix: &str) -> PathBuf {
    let mut name = database.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Cheap structural checks performed before anything opens the file.
///
/// This deliberately stops at the 16-byte header. Proving that the file is a
/// *usable* database is left to the clone, so that a malformed original is
/// never opened more than strictly necessary.
pub fn validate_database_path(path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(MigraDryError::DatabaseNotFound {
            name: display_name(path),
        });
    }
    if !path.is_file() {
        return Err(MigraDryError::DatabaseNotAFile {
            name: display_name(path),
        });
    }

    let mut file = File::open(path).map_err(|error| MigraDryError::OriginalUnreadable {
        reason: error.to_string(),
    })?;
    let mut header = [0u8; SQLITE_HEADER.len()];
    let read =
        read_up_to(&mut file, &mut header).map_err(|error| MigraDryError::OriginalUnreadable {
            reason: error.to_string(),
        })?;

    // A zero-length file is a valid, empty SQLite database as far as SQLite is
    // concerned, so it is accepted rather than rejected.
    if read == 0 {
        return Ok(());
    }
    if read < SQLITE_HEADER.len() || &header != SQLITE_HEADER {
        return Err(MigraDryError::NotSqliteDatabase {
            name: display_name(path),
        });
    }
    Ok(())
}

/// Reads the durable content fingerprint of a database: the main file plus its
/// `-wal` sidecar when one exists.
pub fn fingerprint(path: &Path) -> Result<DatabaseFingerprint> {
    fingerprint_if_present(path)?.ok_or_else(|| MigraDryError::DatabaseNotFound {
        name: display_name(path),
    })
}

/// As [`fingerprint`], but reports a vanished database as `None` rather than as
/// an error.
///
/// Re-fingerprinting during verification uses this: a file that disappeared
/// mid-preview is a *source changed* problem, not a *you gave me a bad path*
/// problem, and the two deserve different words.
pub fn fingerprint_if_present(path: &Path) -> Result<Option<DatabaseFingerprint>> {
    let Some(main) = fingerprint_file(path)? else {
        return Ok(None);
    };
    let wal = fingerprint_file(&sidecar_path(path, "-wal"))?;
    Ok(Some(DatabaseFingerprint { main, wal }))
}

/// Fingerprints one file, returning `None` when it does not exist.
fn fingerprint_file(path: &Path) -> Result<Option<FileFingerprint>> {
    if !path.exists() {
        return Ok(None);
    }
    let metadata = std::fs::metadata(path).map_err(|error| MigraDryError::OriginalUnreadable {
        reason: error.to_string(),
    })?;
    let modified_unix_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|delta| u64::try_from(delta.as_millis()).ok());

    let sha256 = hash_file(path)?;
    Ok(Some(FileFingerprint {
        size_bytes: metadata.len(),
        sha256,
        modified_unix_ms,
    }))
}

/// Streams a file through SHA-256 so that database size does not drive memory use.
fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(|error| MigraDryError::OriginalUnreadable {
        reason: error.to_string(),
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; HASH_BUFFER_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| MigraDryError::OriginalUnreadable {
                reason: error.to_string(),
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(to_hex(&hasher.finalize()))
}

/// Fills `buffer` as far as the file allows, returning how many bytes were read.
fn read_up_to(file: &mut File, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_match_known_sha256_of_empty_input() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.bin");
        std::fs::write(&path, b"").unwrap();
        assert_eq!(
            hash_file(&path).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn hashes_match_known_sha256_of_abc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.bin");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            hash_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sidecar_path_appends_to_the_full_database_path() {
        let path = sidecar_path(Path::new("/data/app.db"), "-wal");
        assert_eq!(path, PathBuf::from("/data/app.db-wal"));
    }

    #[test]
    fn rejects_a_file_that_is_not_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, b"this is definitely not a database").unwrap();
        assert!(matches!(
            validate_database_path(&path),
            Err(MigraDryError::NotSqliteDatabase { .. })
        ));
    }

    #[test]
    fn accepts_a_zero_length_file_as_an_empty_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh.db");
        std::fs::write(&path, b"").unwrap();
        assert!(validate_database_path(&path).is_ok());
    }

    #[test]
    fn rejects_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            validate_database_path(dir.path()),
            Err(MigraDryError::DatabaseNotAFile { .. })
        ));
    }
}
