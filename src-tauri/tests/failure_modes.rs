//! Every way the snapshot can go wrong, and the requirement that all of them
//! end in a structured error rather than a panic or a half-built clone.

mod common;

use common::*;
use migradry_lib::{MigraDryError, MigrationService};
use rusqlite::Connection;
use std::path::Path;

fn migration_in(dir: &Path, sql: &str) -> std::path::PathBuf {
    let path = dir.join("001.sql");
    std::fs::write(&path, sql).unwrap();
    path
}

/// A database large enough that the copy spans many pages.
#[test]
fn a_multi_megabyte_database_is_copied_whole() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("big.db");
    {
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE blobs (id INTEGER PRIMARY KEY, payload BLOB NOT NULL);
                 WITH RECURSIVE seq(n) AS (
                     SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 8000
                 )
                 INSERT INTO blobs (payload) SELECT randomblob(1000) FROM seq;",
            )
            .unwrap();
        connection.close().unwrap();
    }
    let size = std::fs::metadata(&database).unwrap().len();
    assert!(size > 4 * 1024 * 1024, "fixture is only {size} bytes");

    let migration = migration_in(dir.path(), "CREATE INDEX idx_blobs_id ON blobs(id);");
    let hash_before = sha256_of(&database);
    let result = MigrationService::preview(&database, &migration).expect("preview should run");
    let hash_after = sha256_of(&database);

    assert!(result.success, "{:?}", result.error);
    assert!(has_change(
        &result,
        migradry_lib::SchemaChangeKind::IndexAdded,
        "idx_blobs_id"
    ));
    assert_eq!(hash_before, hash_after);

    // Every page really did come across.
    let workspace = workspace(&database);
    let connection = workspace.baseline().open_readonly().unwrap();
    let rows: i64 = connection
        .query_row("SELECT count(*) FROM blobs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 8000);
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
}

/// A file with a convincing header and nothing behind it.
#[test]
fn a_malformed_database_is_refused_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");

    // A real header, a plausible page size, and garbage from there on.
    let mut bytes = b"SQLite format 3\0".to_vec();
    bytes.extend_from_slice(&[0x10, 0x00]); // page size 4096
    bytes.extend(std::iter::repeat_n(0xab, 8192));
    std::fs::write(&database, &bytes).unwrap();

    let migration = migration_in(dir.path(), "CREATE TABLE t (id INTEGER);");
    let before = std::fs::read(&database).unwrap();

    let error = MigrationService::preview(&database, &migration)
        .expect_err("a malformed database cannot be previewed");
    assert!(
        matches!(
            error,
            MigraDryError::NotSqliteDatabase { .. }
                | MigraDryError::SnapshotNotPossible { .. }
                | MigraDryError::SchemaReadFailed { .. }
        ),
        "unexpected error: {error}"
    );

    // Refusing must not have rewritten the thing it refused.
    assert_eq!(std::fs::read(&database).unwrap(), before);
}

/// Truncated mid-page, so SQLite has a header it believes and a body it cannot use.
#[test]
fn a_truncated_database_is_refused_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("full.db");
    seed_database(
        &source,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users (name) VALUES ('ada'), ('grace');",
    );

    let database = dir.path().join("cut.db");
    let bytes = std::fs::read(&source).unwrap();
    std::fs::write(&database, &bytes[..bytes.len() / 2 + 1]).unwrap();

    let migration = migration_in(dir.path(), "CREATE TABLE t (id INTEGER);");
    match MigrationService::preview(&database, &migration) {
        // Refusing is the expected outcome.
        Err(error) => assert!(
            matches!(
                error,
                MigraDryError::NotSqliteDatabase { .. }
                    | MigraDryError::SnapshotNotPossible { .. }
                    | MigraDryError::SchemaReadFailed { .. }
            ),
            "unexpected error: {error}"
        ),
        // If SQLite considers the remainder readable, the preview must still be
        // internally consistent rather than nonsense.
        Ok(result) => assert!(result.original_content_unchanged),
    }
}

#[test]
fn a_database_that_cannot_be_read_is_refused() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("app.db");
        seed_database(&database, "CREATE TABLE users (id INTEGER PRIMARY KEY);");
        let migration = migration_in(dir.path(), "CREATE TABLE t (id INTEGER);");

        std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root ignores permission bits, so there would be nothing to test.
        let enforced = std::fs::File::open(&database).is_err();

        let outcome = MigrationService::preview(&database, &migration);
        std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o644)).unwrap();

        if enforced {
            let error = outcome.expect_err("an unreadable database cannot be previewed");
            assert!(
                matches!(
                    error,
                    MigraDryError::OriginalUnreadable { .. }
                        | MigraDryError::SnapshotNotPossible { .. }
                ),
                "unexpected error: {error}"
            );
        }
    }
}

/// A zero-length file is a valid empty database as far as SQLite is concerned.
#[test]
fn an_empty_database_snapshots_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("fresh.db");
    std::fs::write(&database, b"").unwrap();

    let workspace = workspace(&database);
    for connection in [
        workspace.baseline().open_readonly().unwrap(),
        workspace.working().open_readonly().unwrap(),
    ] {
        let objects: i64 = connection
            .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
            .unwrap();
        assert_eq!(objects, 0);
    }
    assert_eq!(std::fs::metadata(&database).unwrap().len(), 0);
}

/// The building block behind the "source disappeared" error.
///
/// The end-to-end version of this depends on a file being deleted inside a
/// window measured in milliseconds, which is not something to assert on a
/// schedule. The mapping it relies on is deterministic and is pinned here; the
/// source-changed path itself is covered end to end in `concurrent_writer.rs`.
#[test]
fn a_vanished_source_is_distinguishable_from_a_bad_path() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    seed_database(&database, "CREATE TABLE users (id INTEGER PRIMARY KEY);");

    assert!(migradry_lib::database::fingerprint_if_present(&database)
        .unwrap()
        .is_some());

    // Snapshot first, exactly as a preview would, then let the source vanish.
    let workspace = workspace(&database);
    std::fs::remove_file(&database).unwrap();

    assert!(migradry_lib::database::fingerprint_if_present(&database)
        .unwrap()
        .is_none());
    // The workspace is unaffected: separate files in a separate directory.
    let connection = workspace.baseline().open_readonly().unwrap();
    let objects: i64 = connection
        .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
        .unwrap();
    assert_eq!(objects, 1);

    // A path that never existed is a different complaint entirely.
    let migration = migration_in(dir.path(), "SELECT 1;");
    assert!(matches!(
        MigrationService::preview(&database, &migration),
        Err(MigraDryError::DatabaseNotFound { .. })
    ));
}

/// Locks held by another connection are waited on, not panicked over.
#[test]
fn a_locked_rollback_mode_database_is_handled() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    seed_database(
        &database,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users (name) VALUES ('ada');",
    );

    // Rollback journal mode, so an exclusive writer genuinely blocks readers.
    let holder = Connection::open(&database).unwrap();
    holder.execute_batch("BEGIN EXCLUSIVE;").unwrap();

    let migration = migration_in(dir.path(), "CREATE TABLE orders (id INTEGER PRIMARY KEY);");
    let hash_before = sha256_of(&database);
    let outcome = MigrationService::preview(&database, &migration);
    let hash_after = sha256_of(&database);

    holder.execute_batch("ROLLBACK;").unwrap();
    drop(holder);

    match outcome {
        Err(error) => assert!(
            matches!(error, MigraDryError::SnapshotNotPossible { .. }),
            "a held lock must produce a snapshot refusal, not {error}"
        ),
        Ok(result) => assert!(result.original_content_unchanged),
    }
    assert_eq!(hash_before, hash_after);
    assert_eq!(table_names(&database), vec!["users"]);
}

/// A runaway migration is interrupted within a bounded execution window and
/// reports a structured timeout failure without affecting the source database.
#[test]
fn a_runaway_migration_is_interrupted_boundedly() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    seed_database(
        &database,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users (name) VALUES ('ada');",
    );

    // Runaway recursive CTE query that would loop infinitely if unbounded.
    let migration = migration_in(
        dir.path(),
        "WITH RECURSIVE runaway(x) AS (
             VALUES(1)
             UNION ALL
             SELECT x + 1 FROM runaway
         )
         SELECT count(*) FROM runaway;",
    );

    let hash_before = sha256_of(&database);
    let timeout = std::time::Duration::from_millis(150);
    let start = std::time::Instant::now();
    let result = MigrationService::preview_with_timeout(&database, &migration, timeout)
        .expect("preview should execute and catch interruption");
    let elapsed = start.elapsed();
    let hash_after = sha256_of(&database);

    assert_eq!(hash_before, hash_after);
    assert!(
        !result.success,
        "runaway migration should not report success"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "migration should have been interrupted quickly, took {elapsed:?}"
    );

    let failure = result.error.expect("expected migration failure");
    assert_eq!(failure.sqlite_code.as_deref(), Some("SQLITE_INTERRUPT"));
    assert!(
        failure.message.contains("interrupted")
            || failure.message.contains("maximum execution time limit"),
        "expected timeout explanation, got: {}",
        failure.message
    );
}
