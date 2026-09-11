//! Creation and lifetime of the disposable database clone.
//!
//! # Two databases, and why
//!
//! A preview needs the schema *and the data* as they were before the migration,
//! and it needs them after a destructive migration has already thrown them
//! away. Reading the original again afterwards would answer a different
//! question — it describes a later moment, and the source may have moved on.
//!
//! So a [`PreviewWorkspace`] holds two disposable databases:
//!
//! * the **baseline**, taken from the original and then never written again. It
//!   is the exact state the preview is *about*, and destructive-impact counts
//!   are taken from it.
//! * the **working clone**, copied from the baseline. The migration runs here
//!   and nowhere else.
//!
//! # Why these types exist
//!
//! [`BaselineSnapshot`] and [`WorkingClone`] have private fields and no public
//! constructor: only [`PreviewWorkspace::create`] can produce them. Migration
//! SQL is executed by a function that accepts a `&WorkingClone` rather than a
//! `&Path`, so there is no way — accidental or deliberate — to point it at the
//! original or at the baseline.
//!
//! The asymmetry is the whole design: `WorkingClone` exposes a read-write
//! opener, `BaselineSnapshot` does not. Immutability is not a rule anyone has
//! to remember; there is simply no method that could break it.
//!
//! # WAL correctness, and why there is only one snapshot mechanism
//!
//! A SQLite database in WAL mode is `app.db` *plus* `app.db-wal`. Committed
//! data can live entirely in the log, so copying only the main file can
//! silently discard transactions and make the whole preview a lie.
//!
//! The clone is therefore always taken with the **SQLite Online Backup API**,
//! run from a read-only source connection in a single `step(-1)` so the entire
//! copy happens inside one read transaction. SQLite reads through the `-wal`,
//! so uncheckpointed commits are included and the result is a point-in-time
//! consistent snapshot.
//!
//! MigraDry deliberately has no filesystem-copy fallback. Copying `app.db` and
//! `app.db-wal` with `fs::copy` is two non-atomic reads: a writer committing
//! between them yields a clone that is a blend of two moments, and neither
//! SQLite nor MigraDry can tell afterwards that it happened. `PRAGMA
//! integrity_check` would not catch it either — the result can be structurally
//! valid and transactionally incoherent at the same time. There is no condition
//! MigraDry can *detect* that rules this out, so rather than shipping a
//! probably-fine copy, it refuses:
//! [`MigraDryError::SnapshotNotPossible`].
//!
//! A refused preview is a small inconvenience. A confident, wrong preview is
//! the failure this whole program exists to prevent.
//!
//! Nothing here mutates the source to make the backup work: no checkpoint, no
//! `VACUUM`, no journal-mode change, no write of any kind.

use crate::database;
use crate::error::{MigraDryError, Result};
use crate::models::CloneStrategy;
use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, ErrorCode, OpenFlags};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Names of the two databases inside a workspace directory.
const BASELINE_FILE_NAME: &str = "baseline.db";
const WORKING_FILE_NAME: &str = "working.db";

/// The pre-migration state, captured once and then left alone.
///
/// There is deliberately no read-write opener. Everything that reads the
/// baseline — the "before" schema snapshot, and the exact row counts behind a
/// destructive change — goes through [`Self::open_readonly`].
#[derive(Debug)]
pub struct BaselineSnapshot {
    path: PathBuf,
}

impl BaselineSnapshot {
    pub fn open_readonly(&self) -> Result<Connection> {
        database::open_readonly(&self.path).map_err(|error| MigraDryError::SchemaReadFailed {
            reason: error.to_string(),
        })
    }

    /// Only for integrity verification and tests. Nothing may open this path
    /// for writing.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The one database migration SQL is allowed to touch.
#[derive(Debug)]
pub struct WorkingClone {
    path: PathBuf,
}

impl WorkingClone {
    pub fn open_readonly(&self) -> Result<Connection> {
        database::open_readonly(&self.path).map_err(|error| MigraDryError::SchemaReadFailed {
            reason: error.to_string(),
        })
    }

    /// Opens the working clone for writing.
    ///
    /// This is the only writable database handle MigraDry ever creates, and it
    /// is a method on `WorkingClone` precisely so that it cannot be called with
    /// any other path. There is no free function that opens an arbitrary path
    /// for writing, and `BaselineSnapshot` has no equivalent.
    ///
    /// `SQLITE_OPEN_CREATE` is absent on purpose: the clone already exists, and
    /// if it somehow does not, failing beats silently migrating an empty
    /// database and reporting the result as a preview.
    pub fn open_read_write(&self) -> Result<Connection> {
        Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| MigraDryError::CloneFailed {
            reason: error.to_string(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// A baseline and a working clone in one private temporary directory.
///
/// Dropping the workspace removes the directory and both databases with every
/// SQLite sidecar in it, on normal return and on unwinding alike.
#[derive(Debug)]
pub struct PreviewWorkspace {
    /// Held purely for its `Drop`: it deletes the directory tree.
    directory: TempDir,
    baseline: BaselineSnapshot,
    working: WorkingClone,
    strategy: CloneStrategy,
}

impl PreviewWorkspace {
    /// Captures `original` and prepares a working copy of it.
    ///
    /// The original is read exactly once, read-only, through the SQLite Online
    /// Backup API. The working clone is then made from the baseline by the same
    /// mechanism — the baseline is private, quiescent and sidecar-free by this
    /// point, so a plain file copy would also be correct, but using one code
    /// path for both means there is only one thing to get right.
    ///
    /// If SQLite cannot hand over a snapshot it guarantees, this fails. It does
    /// not improvise.
    pub fn create(original: &Path) -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("migradry-")
            .tempdir()
            .map_err(|error| MigraDryError::CloneFailed {
                reason: error.to_string(),
            })?;
        let baseline_path = directory.path().join(BASELINE_FILE_NAME);
        let working_path = directory.path().join(WORKING_FILE_NAME);

        // Belt and braces: both live in a temporary directory this process just
        // created, so these can only fire if the platform handed us something
        // absurd.
        assert_distinct_paths(original, &baseline_path)?;
        assert_distinct_paths(original, &working_path)?;

        // On any error the `TempDir` is dropped as this function unwinds,
        // taking a half-written database and every sidecar with it. Nothing
        // incomplete survives to be mistaken for a snapshot.
        backup_clone(original, &baseline_path)?;
        seal_baseline(&baseline_path);
        backup_clone(&baseline_path, &working_path)?;

        Ok(Self {
            directory,
            baseline: BaselineSnapshot {
                path: baseline_path,
            },
            working: WorkingClone { path: working_path },
            strategy: CloneStrategy::SqliteBackupApi,
        })
    }

    pub fn baseline(&self) -> &BaselineSnapshot {
        &self.baseline
    }

    pub fn working(&self) -> &WorkingClone {
        &self.working
    }

    pub fn strategy(&self) -> CloneStrategy {
        self.strategy
    }

    /// Directory holding both databases; useful for asserting cleanup in tests.
    pub fn directory(&self) -> &Path {
        self.directory.path()
    }
}

/// Marks the baseline read-only at the filesystem level, where the platform
/// allows it.
///
/// The type system already makes a writable baseline handle unexpressible; this
/// adds an independent barrier underneath it. Removing the file at cleanup
/// still works, because deleting needs write permission on the *directory*, not
/// on the file.
///
/// Best effort: a platform that refuses is not a reason to abandon a preview,
/// since it is the second lock on a door the first lock already closed.
fn seal_baseline(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o400));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Copies the source into a brand-new database using the Online Backup API.
///
/// A single `step(-1)` copies every page while holding one read transaction, so
/// the destination is a point-in-time consistent snapshot rather than a smear
/// of pages read at different moments. SQLite reads through the write-ahead log
/// as it goes, so uncheckpointed commits are included.
///
/// Note that opening the source succeeds lazily: SQLite does not touch the file
/// until the first read, so a database it cannot actually read only fails here,
/// at the copy, rather than at the open.
fn backup_clone(source_path: &Path, destination_path: &Path) -> Result<()> {
    let source =
        database::open_readonly(source_path).map_err(|error| classify(source_path, error))?;
    let mut destination = Connection::open(destination_path).map_err(cannot_clone)?;

    let step = {
        let backup =
            Backup::new(&source, &mut destination).map_err(|error| classify(source_path, error))?;
        // `step(-1)` copies every remaining page in one call, so the read
        // transaction covers the whole file rather than each chunk separately.
        backup
            .step(-1)
            .map_err(|error| classify(source_path, error))?
    };

    match step {
        StepResult::Done => {}
        // The busy timeout on the source connection has already elapsed by the
        // time either of these surfaces, so the lock is not going away.
        StepResult::Busy | StepResult::Locked => {
            return Err(MigraDryError::SnapshotNotPossible {
                reason: "another process held a lock on the database for longer than MigraDry \
                         was willing to wait"
                    .to_string(),
            })
        }
        StepResult::More => {
            return Err(MigraDryError::SnapshotNotPossible {
                reason: "SQLite stopped before the whole database had been copied".to_string(),
            })
        }
        other => {
            return Err(MigraDryError::SnapshotNotPossible {
                reason: format!("SQLite ended the snapshot in an unexpected state: {other:?}"),
            })
        }
    }

    // Normalise the clone to a rollback journal so no sidecars survive, then
    // close both handles. The clone is disposable, so changing its journal mode
    // costs nothing and keeps cleanup simple.
    normalise_journal_mode(&destination)?;
    drop(destination);
    drop(source);
    Ok(())
}

/// Turns a SQLite error from the snapshot attempt into MigraDry's vocabulary.
///
/// The interesting case is `SQLITE_CANTOPEN` / `SQLITE_READONLY` on a database
/// that demonstrably exists. That is SQLite saying it cannot build the
/// shared-memory index a write-ahead log needs — typically a `-wal` left behind
/// by a crashed writer in a directory MigraDry may not write to. An earlier
/// version copied the files by hand at this point. It no longer does, because
/// that copy could not be proven consistent; the preview stops here instead.
fn classify(source_path: &Path, error: rusqlite::Error) -> MigraDryError {
    if let rusqlite::Error::SqliteFailure(failure, _) = &error {
        match failure.code {
            ErrorCode::CannotOpen | ErrorCode::ReadOnly => {
                return MigraDryError::SnapshotNotPossible {
                    reason: "SQLite could not open the database read-only. A database using \
                             write-ahead logging needs a -shm shared-memory index, and one \
                             could not be created — often because the directory holding the \
                             database is not writable. MigraDry will not copy the database \
                             files itself, because such a copy cannot be proven consistent"
                        .to_string(),
                };
            }
            ErrorCode::NotADatabase => {
                return MigraDryError::NotSqliteDatabase {
                    name: crate::error::display_name(source_path),
                };
            }
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => {
                return MigraDryError::SnapshotNotPossible {
                    reason: "another process held a lock on the database for longer than \
                             MigraDry was willing to wait"
                        .to_string(),
                };
            }
            ErrorCode::DatabaseCorrupt => {
                return MigraDryError::SnapshotNotPossible {
                    reason: "SQLite reported the database as malformed while reading it"
                        .to_string(),
                };
            }
            _ => {}
        }
    }
    MigraDryError::SnapshotNotPossible {
        reason: error.to_string(),
    }
}

/// Failure to create the *destination* is a local problem, not a verdict on the
/// user's database, so it keeps its own error.
fn cannot_clone(error: rusqlite::Error) -> MigraDryError {
    MigraDryError::CloneFailed {
        reason: error.to_string(),
    }
}

/// Puts a clone into rollback-journal mode so it leaves no `-wal`/`-shm` behind.
fn normalise_journal_mode(connection: &Connection) -> Result<()> {
    // `PRAGMA journal_mode` returns a row, so it has to be queried rather than
    // executed.
    connection
        .query_row("PRAGMA journal_mode = DELETE", [], |row| {
            row.get::<_, String>(0)
        })
        .map(|_| ())
        .map_err(|error| MigraDryError::CloneFailed {
            reason: error.to_string(),
        })
}

/// Refuses to continue if a workspace path could possibly be the original.
fn assert_distinct_paths(original: &Path, clone_path: &Path) -> Result<()> {
    let original_real = original.canonicalize().unwrap_or_else(|_| original.into());
    // The file does not exist yet, so its parent directory is compared instead.
    let clone_parent = clone_path
        .parent()
        .map(|parent| parent.canonicalize().unwrap_or_else(|_| parent.into()));
    let file_name = clone_path.file_name().unwrap_or_default();
    let clone_real = match clone_parent {
        Some(parent) => parent.join(file_name),
        None => clone_path.into(),
    };
    if original_real == clone_real {
        return Err(MigraDryError::SafetyCheck {
            reason: "a temporary workspace database resolved to the original database path"
                .to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(path: &Path, sql: &str) {
        let connection = Connection::open(path).unwrap();
        connection.execute_batch(sql).unwrap();
    }

    fn workspace_for(sql: &str) -> (tempfile::TempDir, PreviewWorkspace) {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("app.db");
        seed(&original, sql);
        let workspace = PreviewWorkspace::create(&original).unwrap();
        (dir, workspace)
    }

    #[test]
    fn both_databases_land_outside_the_original_directory() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("app.db");
        seed(&original, "CREATE TABLE t (id INTEGER PRIMARY KEY);");

        let workspace = PreviewWorkspace::create(&original).unwrap();
        for path in [workspace.baseline().path(), workspace.working().path()] {
            assert_ne!(path, original);
            assert_ne!(path.parent(), original.parent());
        }
        assert_ne!(workspace.baseline().path(), workspace.working().path());
        assert_eq!(workspace.strategy(), CloneStrategy::SqliteBackupApi);
    }

    #[test]
    fn dropping_the_workspace_removes_both_databases() {
        let (_dir, workspace) = workspace_for("CREATE TABLE t (id INTEGER PRIMARY KEY);");
        let directory = workspace.directory().to_path_buf();
        let baseline = workspace.baseline().path().to_path_buf();
        let working = workspace.working().path().to_path_buf();
        assert!(baseline.exists() && working.exists());

        drop(workspace);
        assert!(!baseline.exists());
        assert!(!working.exists());
        assert!(!directory.exists());
    }

    #[test]
    fn both_databases_carry_the_data_of_the_original() {
        let (_dir, workspace) = workspace_for(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
             INSERT INTO users (name) VALUES ('ada'), ('grace');",
        );

        for connection in [
            workspace.baseline().open_readonly().unwrap(),
            workspace.working().open_readonly().unwrap(),
        ] {
            let count: i64 = connection
                .query_row("SELECT count(*) FROM users", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 2);
        }
    }

    #[test]
    fn the_baseline_refuses_to_be_written() {
        let (_dir, workspace) = workspace_for("CREATE TABLE t (id INTEGER PRIMARY KEY);");
        let connection = workspace.baseline().open_readonly().unwrap();
        assert!(connection.is_readonly("main").unwrap());
        assert!(connection
            .execute_batch("CREATE TABLE nope (id INTEGER);")
            .is_err());

        // And the file itself is read-only where the platform supports it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(workspace.baseline().path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o222, 0, "the baseline is writable on disk");
        }
    }

    #[test]
    fn writing_to_the_working_clone_leaves_the_baseline_alone() {
        let (_dir, workspace) = workspace_for(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
             INSERT INTO users (name) VALUES ('ada');",
        );

        let connection = workspace.working().open_read_write().unwrap();
        connection.execute_batch("DROP TABLE users;").unwrap();
        drop(connection);

        let baseline = workspace.baseline().open_readonly().unwrap();
        let count: i64 = baseline
            .query_row("SELECT count(*) FROM users", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "the baseline followed the working clone");
    }

    #[test]
    fn a_file_that_is_not_a_database_cannot_be_snapshotted() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("app.db");
        std::fs::write(&original, b"SQLite format 3\0but not really").unwrap();

        assert!(matches!(
            PreviewWorkspace::create(&original),
            Err(MigraDryError::NotSqliteDatabase { .. })
                | Err(MigraDryError::SnapshotNotPossible { .. })
        ));
    }
}
