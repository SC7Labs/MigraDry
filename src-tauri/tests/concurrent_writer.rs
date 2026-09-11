//! What happens when somebody else is writing to the database at the same time.
//!
//! This is the scenario the filesystem-copy fallback could not survive, and the
//! reason it was removed. Two outcomes are acceptable:
//!
//! **A.** MigraDry obtains a genuinely consistent SQLite snapshot.
//! **B.** MigraDry refuses the preview with a structured error.
//!
//! A third outcome — a torn clone handed back as if it were valid — is the
//! failure these tests exist to rule out.
//!
//! Consistency is checked with SQLite, not with hashes. The fixture maintains a
//! transactional invariant across two tables, so a clone assembled from two
//! different moments would show a mismatch that no amount of structural
//! checking (`PRAGMA integrity_check`) would catch on its own.

mod common;

use common::*;
use migradry_lib::{MigraDryError, MigrationService};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::thread;

fn wal_of(database: &Path) -> PathBuf {
    let mut name = database.as_os_str().to_os_string();
    name.push("-wal");
    PathBuf::from(name)
}

/// Creates a WAL database carrying a cross-table invariant:
/// `sum(ledger.amount)` must always equal `totals.total`.
///
/// Every writer transaction updates both tables, so the invariant holds at
/// every commit boundary and is violated at every point *inside* a transaction.
/// A snapshot that mixes moments will almost certainly break it.
fn ledger_database(path: &Path) {
    let connection = Connection::open(path).unwrap();
    let mode: String = connection
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    connection
        .execute_batch(
            "CREATE TABLE ledger (id INTEGER PRIMARY KEY, amount INTEGER NOT NULL);
             CREATE TABLE totals (id INTEGER PRIMARY KEY CHECK (id = 1), total INTEGER NOT NULL);
             INSERT INTO totals (id, total) VALUES (1, 0);",
        )
        .unwrap();
    connection.close().unwrap();
}

/// Runs transactions against `path` until `stop` is set, counting commits.
fn spawn_writer(
    path: PathBuf,
    stop: Arc<AtomicBool>,
    commits: Arc<AtomicI64>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let connection = Connection::open(&path).expect("writer connection");
        connection
            .busy_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let mut amount = 1i64;
        while !stop.load(Ordering::Relaxed) {
            let result = connection.execute_batch(&format!(
                "BEGIN IMMEDIATE;
                 INSERT INTO ledger (amount) VALUES ({amount});
                 UPDATE totals SET total = total + {amount} WHERE id = 1;
                 COMMIT;"
            ));
            if result.is_ok() {
                commits.fetch_add(1, Ordering::Relaxed);
            } else {
                let _ = connection.execute_batch("ROLLBACK;");
            }
            amount += 1;
        }
    })
}

/// Asserts a clone is a coherent database, using SQLite rather than hashes.
fn assert_clone_is_consistent(connection: &Connection, label: &str) {
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap_or_else(|error| panic!("{label}: integrity_check could not run: {error}"));
    assert_eq!(integrity, "ok", "{label}: clone is structurally corrupt");

    // The invariant only holds if every page in the clone came from the same
    // committed moment.
    let ledger_sum: i64 = connection
        .query_row("SELECT coalesce(sum(amount), 0) FROM ledger", [], |row| {
            row.get(0)
        })
        .unwrap_or_else(|error| panic!("{label}: could not sum the ledger: {error}"));
    let recorded_total: i64 = connection
        .query_row("SELECT total FROM totals WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap_or_else(|error| panic!("{label}: could not read the total: {error}"));

    assert_eq!(
        ledger_sum, recorded_total,
        "{label}: the clone is a torn snapshot — it mixes states from different transactions"
    );
}

/// The snapshot mechanism itself, under sustained concurrent writes.
#[test]
fn a_snapshot_taken_while_another_process_writes_is_never_torn() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    ledger_database(&database);

    let stop = Arc::new(AtomicBool::new(false));
    let commits = Arc::new(AtomicI64::new(0));
    let writer = spawn_writer(database.clone(), Arc::clone(&stop), Arc::clone(&commits));

    let mut snapshots = 0;
    let mut refusals = 0;
    for _ in 0..25 {
        match migradry_lib::temp::PreviewWorkspace::create(&database) {
            Ok(workspace) => {
                // Both halves of the workspace must be coherent: the baseline
                // came from the live database, and the working clone came from
                // the baseline.
                assert_clone_is_consistent(
                    &workspace.baseline().open_readonly().unwrap(),
                    "baseline taken during concurrent writes",
                );
                assert_clone_is_consistent(
                    &workspace.working().open_readonly().unwrap(),
                    "working clone taken during concurrent writes",
                );
                snapshots += 1;
            }
            // Outcome B: refusing is always allowed, as long as it is a
            // structured refusal and not a panic or a bad clone.
            Err(error) => {
                assert!(
                    matches!(
                        error,
                        MigraDryError::SnapshotNotPossible { .. }
                            | MigraDryError::CloneFailed { .. }
                    ),
                    "unexpected error shape: {error}"
                );
                refusals += 1;
            }
        }
    }

    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();

    assert_eq!(snapshots + refusals, 25);
    println!(
        "snapshots taken under load: {snapshots} consistent, {refusals} refused, \
         {} writer commits",
        commits.load(Ordering::Relaxed)
    );
    assert!(
        commits.load(Ordering::Relaxed) > 0,
        "the writer never committed, so this test proved nothing"
    );
    // The original must still be a working database afterwards.
    let connection = open_readonly(&database);
    assert_clone_is_consistent(&connection, "original after concurrent snapshots");
}

/// A full preview while another process writes.
///
/// Either the source held still and the preview stands, or it did not and
/// MigraDry says so. It must never report a preview *and* claim the source was
/// stable when it was not.
#[test]
fn a_preview_during_concurrent_writes_either_holds_or_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    ledger_database(&database);

    let migration = dir.path().join("001_add_note.sql");
    std::fs::write(&migration, "ALTER TABLE ledger ADD COLUMN note TEXT;").unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let commits = Arc::new(AtomicI64::new(0));
    let writer = spawn_writer(database.clone(), Arc::clone(&stop), Arc::clone(&commits));

    let mut succeeded = 0;
    let mut refused = 0;
    for _ in 0..15 {
        match MigrationService::preview(&database, &migration) {
            Ok(result) => {
                // If a result came back at all, the claim it carries must be true.
                assert!(result.original_content_unchanged);
                assert_eq!(
                    result.original_integrity.before.main.sha256,
                    result.original_integrity.after.main.sha256
                );
                assert!(result.success, "{:?}", result.error);
                assert!(has_child_change(
                    &result,
                    migradry_lib::SchemaChangeKind::ColumnAdded,
                    "ledger",
                    "note"
                ));
                succeeded += 1;
            }
            Err(error) => {
                assert!(
                    matches!(
                        error,
                        MigraDryError::SourceChangedDuringSnapshot
                            | MigraDryError::SourceChangedAfterSnapshot
                            | MigraDryError::SnapshotNotPossible { .. }
                            | MigraDryError::CloneFailed { .. }
                    ),
                    "unexpected error shape: {error}"
                );
                // The refusal must not blame anyone for the change.
                let message = error.to_string();
                assert!(
                    !message.contains("MigraDry modified") && !message.contains("MigraDry wrote"),
                    "the error attributes the change to MigraDry: {message}"
                );
                refused += 1;
            }
        }
    }

    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();

    assert_eq!(succeeded + refused, 15);
    println!(
        "previews under load: {succeeded} stood, {refused} refused, {} writer commits",
        commits.load(Ordering::Relaxed)
    );
    assert!(commits.load(Ordering::Relaxed) > 0);
    assert!(
        refused > 0,
        "a writer committing throughout should have been detected at least once"
    );

    // Whatever happened, the writer's own database is intact and the migration
    // never reached it.
    let connection = open_readonly(&database);
    assert_clone_is_consistent(&connection, "original after concurrent previews");
    assert!(!column_names(&database, "ledger").contains(&"note".to_string()));
    assert!(
        wal_of(&database).exists(),
        "the fixture should still be in WAL mode"
    );
}
