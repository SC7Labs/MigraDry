//! Nothing temporary survives, on any path.
//!
//! A workspace is now two databases — a baseline and a working clone — in one
//! directory, so "cleaned up" means both files and the directory holding them,
//! on every route out of the engine including a panic.
//!
//! This file holds a single test on purpose. It inspects the *shared* system
//! temporary directory, so it must not run beside other MigraDry tests that are
//! creating and destroying clones of their own. Cargo runs each integration
//! test binary in turn, so being alone in this one is enough.

mod common;

use common::*;
use migradry_lib::MigrationService;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Every `migradry-*` directory currently in the system temporary directory.
fn migradry_temp_dirs() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("migradry-"))
        })
        .collect();
    found.sort();
    found
}

fn write(path: &Path, contents: &str) -> PathBuf {
    std::fs::write(path, contents).unwrap();
    path.to_path_buf()
}

#[test]
fn no_temporary_clone_survives_any_path_through_the_engine() {
    let baseline = migradry_temp_dirs();
    let dir = tempfile::tempdir().unwrap();

    // --- 1. a preview that succeeds ------------------------------------
    let database = dir.path().join("app.db");
    seed_database(
        &database,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);",
    );
    let ok = write(
        &dir.path().join("ok.sql"),
        "ALTER TABLE users ADD COLUMN email TEXT;",
    );
    assert!(MigrationService::preview(&database, &ok).unwrap().success);

    // --- 2. a migration that fails --------------------------------------
    let bad_sql = write(
        &dir.path().join("bad.sql"),
        "ALTER TABLE users ADD COLUMN name TEXT;",
    );
    assert!(
        !MigrationService::preview(&database, &bad_sql)
            .unwrap()
            .success
    );

    // --- 3. a migration that fails inside a transaction -----------------
    let in_txn = write(
        &dir.path().join("txn.sql"),
        "BEGIN;\nCREATE TABLE staging (id INTEGER);\nDROP TABLE missing;\nCOMMIT;",
    );
    assert!(
        !MigrationService::preview(&database, &in_txn)
            .unwrap()
            .success
    );

    // --- 4. a migration refused by the authorizer -----------------------
    let attack = write(
        &dir.path().join("attach.sql"),
        &format!("ATTACH DATABASE '{}' AS x;", database.display()),
    );
    assert!(
        !MigrationService::preview(&database, &attack)
            .unwrap()
            .success
    );

    // --- 4b. a destructive migration, so impact counting runs -----------
    let destructive = write(&dir.path().join("destructive.sql"), "DROP TABLE users;");
    assert!(
        MigrationService::preview(&database, &destructive)
            .unwrap()
            .success
    );

    // --- 4c. an impact count that cannot be taken -----------------------
    // The workspace must still be torn down when counting reports a problem.
    {
        let workspace = migradry_lib::temp::PreviewWorkspace::create(&database).unwrap();
        let workspace_dir = workspace.directory().to_path_buf();
        let mut changes = vec![migradry_lib::SchemaChange::new(
            migradry_lib::SchemaChangeKind::TableRemoved,
            "never_existed",
        )];
        let warnings = migradry_lib::impact::measure(workspace.baseline(), &mut changes);
        assert_eq!(warnings.len(), 1);
        drop(workspace);
        assert!(!workspace_dir.exists());
    }

    // --- 5. a source that cannot be snapshotted -------------------------
    let malformed = dir.path().join("malformed.db");
    let mut bytes = b"SQLite format 3\0".to_vec();
    bytes.extend(std::iter::repeat_n(0xab, 8192));
    std::fs::write(&malformed, &bytes).unwrap();
    assert!(MigrationService::preview(&malformed, &ok).is_err());

    // --- 6. inputs rejected before any clone exists ---------------------
    assert!(MigrationService::preview(&dir.path().join("absent.db"), &ok).is_err());
    assert!(MigrationService::preview(&database, &dir.path().join("absent.sql")).is_err());

    // --- 7. a panic while a clone is alive ------------------------------
    // Drop runs during unwinding, so even a crash mid-preview cleans up.
    let captured: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&captured);
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let unwound = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let workspace = migradry_lib::temp::PreviewWorkspace::create(&database).unwrap();
        *sink.lock().unwrap() = Some(workspace.directory().to_path_buf());
        panic!("simulated failure while the workspace is alive");
    }));
    std::panic::set_hook(previous_hook);

    assert!(unwound.is_err(), "the closure was supposed to panic");
    let panicked_dir = captured
        .lock()
        .unwrap()
        .clone()
        .expect("the workspace was created before the panic");
    assert!(
        !panicked_dir.exists(),
        "unwinding left {panicked_dir:?} behind"
    );
    // Both databases, not just the directory entry.
    assert!(!panicked_dir.join("baseline.db").exists());
    assert!(!panicked_dir.join("working.db").exists());

    // --- 8. an explicitly dropped workspace -----------------------------
    let workspace = migradry_lib::temp::PreviewWorkspace::create(&database).unwrap();
    let workspace_dir = workspace.directory().to_path_buf();
    let baseline_db = workspace.baseline().path().to_path_buf();
    let working_db = workspace.working().path().to_path_buf();
    assert!(baseline_db.exists() && working_db.exists());
    drop(workspace);
    assert!(!baseline_db.exists());
    assert!(!working_db.exists());
    assert!(!workspace_dir.exists());

    // --- the verdict -----------------------------------------------------
    let leaked: Vec<PathBuf> = migradry_temp_dirs()
        .into_iter()
        .filter(|path| !baseline.contains(path))
        .collect();
    assert!(
        leaked.is_empty(),
        "temporary clone directories survived: {leaked:?}"
    );
}
