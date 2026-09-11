//! The contract the frontend actually depends on.
//!
//! These call the Tauri command function directly, which exercises the same
//! code path an `invoke("preview_migration", ...)` reaches, including the
//! conversion of engine errors into the serializable shape the UI renders.

mod common;

use common::*;
use migradry_lib::commands::preview_migration;
use migradry_lib::PreviewErrorKind;

fn as_string(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

#[test]
fn the_command_returns_a_result_for_a_working_migration() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
        "ALTER TABLE users ADD COLUMN email TEXT;",
    );

    let hash_before = sha256_of(&fixture.database);
    let result = preview_migration(as_string(&fixture.database), as_string(&fixture.migration))
        .expect("the command should return a result");
    let hash_after = sha256_of(&fixture.database);

    assert!(result.success);
    assert!(result.original_content_unchanged);
    assert_eq!(hash_before, hash_after);
}

#[test]
fn the_command_trims_surrounding_whitespace_from_paths() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
        "CREATE TABLE orders (id INTEGER PRIMARY KEY);",
    );

    let result = preview_migration(
        format!("  {}\n", as_string(&fixture.database)),
        format!("\t{} ", as_string(&fixture.migration)),
    )
    .expect("padded paths should still resolve");
    assert!(result.success);
}

#[test]
fn a_failing_migration_still_comes_back_as_a_result_not_an_error() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);",
        "ALTER TABLE users ADD COLUMN name TEXT;",
    );

    let result = preview_migration(as_string(&fixture.database), as_string(&fixture.migration))
        .expect("a failed migration is still a successful preview");
    assert!(!result.success);
    assert!(result.error.is_some());
    assert!(result.original_content_unchanged);
}

#[test]
fn bad_input_comes_back_as_a_structured_error() {
    let dir = tempfile::tempdir().unwrap();
    let migration = dir.path().join("001.sql");
    std::fs::write(&migration, "SELECT 1;").unwrap();

    let error = preview_migration(
        as_string(&dir.path().join("absent.db")),
        as_string(&migration),
    )
    .expect_err("a missing database cannot be previewed");

    assert_eq!(error.kind, PreviewErrorKind::InvalidDatabase);
    assert_eq!(error.message, "Database file does not exist: absent.db");

    // The error is what crosses the IPC boundary, so it has to serialize.
    let json = serde_json::to_string(&error).unwrap();
    assert!(json.contains("\"kind\":\"invalid_database\""));
}

#[test]
fn a_file_that_is_not_a_database_is_named_as_such() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    std::fs::write(&database, "not a database").unwrap();
    let migration = dir.path().join("001.sql");
    std::fs::write(&migration, "SELECT 1;").unwrap();

    let error = preview_migration(as_string(&database), as_string(&migration))
        .expect_err("a text file is not a database");
    assert_eq!(error.kind, PreviewErrorKind::NotSqliteDatabase);
}
