//! Write-ahead-log regression tests.
//!
//! A database in WAL mode is two files. Committed data can live entirely in
//! `app.db-wal` and not yet be present in `app.db` at all. A clone that copied
//! only the main file would silently lose that data and every conclusion drawn
//! from it would be wrong — so these tests build databases whose content exists
//! *only* in the write-ahead log and check that the preview still sees it.

mod common;

use common::*;
use migradry_lib::{CloneStrategy, MigraDryError, MigrationService, SchemaChangeKind};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

fn sidecar(database: &Path, suffix: &str) -> PathBuf {
    let mut name = database.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn wal_of(database: &Path) -> PathBuf {
    sidecar(database, "-wal")
}

fn shm_of(database: &Path) -> PathBuf {
    sidecar(database, "-shm")
}

/// Builds a WAL database whose `users` table exists only in the `-wal`.
///
/// Auto-checkpointing is switched off and the writing connection is returned
/// still open, which is what keeps SQLite from folding the log back into the
/// main file.
fn wal_database(database: &Path) -> Connection {
    let connection = Connection::open(database).unwrap();
    let mode: String = connection
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    connection
        .execute_batch("PRAGMA wal_autocheckpoint = 0;")
        .unwrap();
    connection
        .execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
             INSERT INTO users (name) VALUES ('ada'), ('grace');",
        )
        .unwrap();

    // The point of the fixture: the table is in the log, not in the main file.
    assert!(
        wal_of(database).metadata().unwrap().len() > 0,
        "-wal is empty"
    );
    let main_bytes = std::fs::read(database).unwrap();
    assert!(
        !main_bytes.windows(5).any(|window| window == b"users"),
        "the main database file already contains the table, so this test would prove nothing"
    );
    connection
}

/// A live WAL database with uncheckpointed commits: the backup API must read
/// straight through the log.
#[test]
fn a_migration_sees_data_that_exists_only_in_the_write_ahead_log() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    let writer = wal_database(&database);

    let migration = dir.path().join("001_email.sql");
    std::fs::write(&migration, "ALTER TABLE users ADD COLUMN email TEXT;").unwrap();

    let main_before = sha256_of(&database);
    let wal_before = sha256_of(&wal_of(&database));

    let result = MigrationService::preview(&database, &migration).expect("preview should run");

    let main_after = sha256_of(&database);
    let wal_after = sha256_of(&wal_of(&database));

    // A clone that dropped the log would have failed with "no such table: users".
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.clone_strategy, CloneStrategy::SqliteBackupApi);
    assert!(has_child_change(
        &result,
        SchemaChangeKind::ColumnAdded,
        "users",
        "email"
    ));

    // Neither file moved, and the log took part in the comparison.
    assert!(result.original_content_unchanged);
    assert!(result.original_integrity.wal_checked);
    assert_eq!(main_before, main_after, "main database file changed");
    assert_eq!(wal_before, wal_after, "write-ahead log changed");

    // Asked through the writer that owns the database, the column is still absent.
    let columns: Vec<String> = {
        let mut statement = writer
            .prepare("SELECT name FROM pragma_table_info('users')")
            .unwrap();
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap();
        rows.map(|row| row.unwrap()).collect()
    };
    assert_eq!(columns, vec!["id", "name"]);
    assert_eq!(
        writer
            .query_row("SELECT count(*) FROM users", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        2
    );
}

/// A WAL database left behind by a process that died: `app.db` and
/// `app.db-wal` exist and the `-shm` does not.
///
/// SQLite can still open this read-only, so the backup API is used and the
/// log is read through correctly. Doing so makes SQLite create the `-shm`
/// beside the original — the one trace a preview can leave — and the engine
/// reports that rather than hiding it. The database file and its log are still
/// byte-identical afterwards.
#[test]
fn an_orphaned_write_ahead_log_is_read_through_and_the_shm_is_reported() {
    let source_dir = tempfile::tempdir().unwrap();
    let source = source_dir.path().join("app.db");
    let writer = wal_database(&source);

    // Copy the pair out from under the live writer, deliberately leaving the
    // -shm behind, then let the writer close and checkpoint the original.
    let orphan_dir = tempfile::tempdir().unwrap();
    let orphan = orphan_dir.path().join("app.db");
    std::fs::copy(&source, &orphan).unwrap();
    std::fs::copy(wal_of(&source), wal_of(&orphan)).unwrap();
    drop(writer);

    let shm = shm_of(&orphan);
    assert!(wal_of(&orphan).exists());
    assert!(!shm.exists(), "the fixture must not carry a -shm");

    let migration = orphan_dir.path().join("001_email.sql");
    std::fs::write(&migration, "ALTER TABLE users ADD COLUMN email TEXT;").unwrap();

    let main_before = sha256_of(&orphan);
    let wal_before = sha256_of(&wal_of(&orphan));

    let result = MigrationService::preview(&orphan, &migration).expect("preview should run");

    let main_after = sha256_of(&orphan);
    let wal_after = sha256_of(&wal_of(&orphan));

    // The table lives only in the log, so seeing the column added proves the
    // log was read.
    assert!(result.success, "{:?}", result.error);
    assert!(has_child_change(
        &result,
        SchemaChangeKind::ColumnAdded,
        "users",
        "email"
    ));

    // Durable content is untouched...
    assert!(result.original_content_unchanged);
    assert_eq!(main_before, main_after, "main database file changed");
    assert_eq!(wal_before, wal_after, "write-ahead log changed");

    // ...and the shared-memory index SQLite needed is declared.
    assert!(shm.exists(), "SQLite is expected to create a -shm here");
    assert!(result.original_integrity.shm_created_by_preview);
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("-shm shared-memory index")),
        "the -shm must be reported: {:?}",
        result.warnings
    );
}

/// When SQLite cannot open the original read-only at all — here because the
/// directory itself is not writable, so no `-shm` can be created — MigraDry
/// refuses the preview.
///
/// An earlier version copied `app.db` and `app.db-wal` by hand at this point.
/// That copy is two non-atomic reads and cannot be proven consistent against a
/// concurrent writer, so it is gone. Refusing is the whole point: a preview
/// MigraDry cannot vouch for is worse than no preview.
#[cfg(unix)]
#[test]
fn an_unopenable_wal_database_is_refused_rather_than_copied_by_hand() {
    use std::os::unix::fs::PermissionsExt;

    let source_dir = tempfile::tempdir().unwrap();
    let source = source_dir.path().join("app.db");
    let writer = wal_database(&source);

    let locked_dir = tempfile::tempdir().unwrap();
    let locked = locked_dir.path().join("app.db");
    std::fs::copy(&source, &locked).unwrap();
    std::fs::copy(wal_of(&source), wal_of(&locked)).unwrap();
    drop(writer);

    // The migration lives elsewhere; the database's directory becomes read-only.
    let work_dir = tempfile::tempdir().unwrap();
    let migration = work_dir.path().join("001_email.sql");
    std::fs::write(&migration, "ALTER TABLE users ADD COLUMN email TEXT;").unwrap();
    std::fs::set_permissions(locked_dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

    let main_before = sha256_of(&locked);
    let wal_before = sha256_of(&wal_of(&locked));

    let outcome = MigrationService::preview(&locked, &migration);

    let main_after = sha256_of(&locked);
    let wal_after = sha256_of(&wal_of(&locked));
    let shm_exists = shm_of(&locked).exists();

    // Restore permissions before asserting, so a failure still cleans up.
    std::fs::set_permissions(locked_dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

    let error = outcome.expect_err("MigraDry must refuse rather than improvise a snapshot");
    assert!(
        matches!(error, MigraDryError::SnapshotNotPossible { .. }),
        "unexpected error: {error}"
    );
    assert!(
        error
            .to_string()
            .contains("consistent SQLite snapshot could not be created"),
        "the message must say what went wrong: {error}"
    );

    // Refusing still leaves the database exactly as it was.
    assert_eq!(main_before, main_after, "main database file changed");
    assert_eq!(wal_before, wal_after, "write-ahead log changed");
    assert!(!shm_exists, "a refused preview must not create a -shm");
}

/// A migration that switches the clone into WAL mode must not leave sidecar
/// files next to the original.
#[test]
fn a_migration_enabling_wal_leaves_no_sidecars_by_the_original() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
        "PRAGMA journal_mode = WAL;
         CREATE TABLE audit (id INTEGER PRIMARY KEY);",
    );

    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);
    let hash_after = sha256_of(&fixture.database);

    assert!(result.success, "{:?}", result.error);
    assert_eq!(hash_before, hash_after);
    assert!(!wal_of(&fixture.database).exists());
    assert!(!shm_of(&fixture.database).exists());
    assert!(!result.original_integrity.shm_created_by_preview);
}
