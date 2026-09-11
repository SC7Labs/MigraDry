//! Behaviour of the preview engine: input validation, diffing and cleanup.
//!
//! The original-database invariant is covered in `original_safety.rs`; this
//! file is about the engine giving correct and useful answers.

mod common;

use common::*;
use migradry_lib::{
    CloneStrategy, MigraDryError, MigrationService, SchemaChangeKind, SchemaObjectKind,
};
use std::path::Path;

// ---------------------------------------------------------------------------
// Input validation
// ---------------------------------------------------------------------------

#[test]
fn a_missing_database_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let migration = dir.path().join("001.sql");
    std::fs::write(&migration, "SELECT 1;").unwrap();

    let error = MigrationService::preview(&dir.path().join("absent.db"), &migration).unwrap_err();
    assert!(matches!(error, MigraDryError::DatabaseNotFound { .. }));
    assert_eq!(error.to_string(), "Database file does not exist: absent.db");
}

#[test]
fn a_directory_is_not_a_database() {
    let dir = tempfile::tempdir().unwrap();
    let migration = dir.path().join("001.sql");
    std::fs::write(&migration, "SELECT 1;").unwrap();

    let error = MigrationService::preview(dir.path(), &migration).unwrap_err();
    assert!(matches!(error, MigraDryError::DatabaseNotAFile { .. }));
}

#[test]
fn a_file_that_is_not_sqlite_is_rejected_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("notes.db");
    std::fs::write(&database, "Dear diary, today I renamed a file to .db\n").unwrap();
    let migration = dir.path().join("001.sql");
    std::fs::write(&migration, "SELECT 1;").unwrap();

    let error = MigrationService::preview(&database, &migration).unwrap_err();
    assert!(matches!(error, MigraDryError::NotSqliteDatabase { .. }));
    assert!(error.to_string().contains("not a valid SQLite database"));

    // Validation must not have written to the file it rejected.
    assert_eq!(
        std::fs::read_to_string(&database).unwrap(),
        "Dear diary, today I renamed a file to .db\n"
    );
}

#[test]
fn a_truncated_database_is_rejected_rather_than_previewed() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    // A plausible header on a file that is not actually a database.
    std::fs::write(&database, b"SQLite format 3\0truncated nonsense").unwrap();
    let migration = dir.path().join("001.sql");
    std::fs::write(&migration, "CREATE TABLE t (id INTEGER);").unwrap();

    let error = MigrationService::preview(&database, &migration).unwrap_err();
    assert!(
        matches!(
            error,
            MigraDryError::NotSqliteDatabase { .. } | MigraDryError::CloneFailed { .. }
        ),
        "unexpected error: {error}"
    );
}

#[test]
fn a_missing_migration_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    seed_database(&database, "CREATE TABLE users (id INTEGER);");

    let error = MigrationService::preview(&database, &dir.path().join("absent.sql")).unwrap_err();
    assert!(matches!(error, MigraDryError::MigrationNotFound { .. }));
}

#[test]
fn a_directory_is_not_a_migration() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    seed_database(&database, "CREATE TABLE users (id INTEGER);");

    let error = MigrationService::preview(&database, dir.path()).unwrap_err();
    assert!(matches!(error, MigraDryError::MigrationNotAFile { .. }));
}

#[test]
fn an_empty_database_can_be_migrated() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("fresh.db");
    std::fs::write(&database, b"").unwrap();
    let migration = dir.path().join("001_init.sql");
    std::fs::write(&migration, "CREATE TABLE users (id INTEGER PRIMARY KEY);").unwrap();

    let hash_before = sha256_of(&database);
    let result = MigrationService::preview(&database, &migration).expect("preview should run");
    let hash_after = sha256_of(&database);

    assert!(result.success, "{:?}", result.error);
    assert!(has_change(&result, SchemaChangeKind::TableAdded, "users"));
    assert_eq!(hash_before, hash_after);
    assert_eq!(std::fs::metadata(&database).unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// Migration content
// ---------------------------------------------------------------------------

#[test]
fn an_empty_migration_changes_nothing_and_says_so() {
    let fixture = fixture("CREATE TABLE users (id INTEGER PRIMARY KEY);", "");
    let result = preview(&fixture);

    assert!(result.success);
    assert!(result.schema_changes.is_empty());
    assert_eq!(result.destructive_change_count, 0);
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("no SQL statements")),
        "{:?}",
        result.warnings
    );
}

#[test]
fn a_comment_only_migration_changes_nothing() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
        "-- nothing to do yet\n\n/* still nothing */\n",
    );
    let result = preview(&fixture);
    assert!(result.success, "{:?}", result.error);
    assert!(result.schema_changes.is_empty());
}

#[test]
fn many_statements_are_applied_in_order() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);",
        "ALTER TABLE users ADD COLUMN email TEXT;

         CREATE TABLE orders (
             id INTEGER PRIMARY KEY,
             user_id INTEGER NOT NULL
         );

         CREATE INDEX idx_orders_user_id ON orders(user_id);

         CREATE VIEW user_orders AS
             SELECT users.name, orders.id FROM users JOIN orders ON orders.user_id = users.id;

         CREATE TRIGGER orders_guard AFTER INSERT ON orders
         BEGIN
             SELECT 1;
         END;",
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert!(has_change(&result, SchemaChangeKind::TableAdded, "orders"));
    assert!(has_child_change(
        &result,
        SchemaChangeKind::ColumnAdded,
        "users",
        "email"
    ));
    assert!(has_change(
        &result,
        SchemaChangeKind::IndexAdded,
        "idx_orders_user_id"
    ));
    assert!(has_change(
        &result,
        SchemaChangeKind::ViewAdded,
        "user_orders"
    ));
    assert!(has_change(
        &result,
        SchemaChangeKind::TriggerAdded,
        "orders_guard"
    ));
    assert_eq!(result.schema_changes.len(), 5);
    assert_eq!(result.destructive_change_count, 0);
    assert_eq!(result.advisory_change_count, 0);
}

#[test]
fn removing_support_objects_is_advisory_rather_than_destructive() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT);
         CREATE INDEX idx_old_email ON users(email);
         CREATE VIEW old_view AS SELECT id FROM users;
         CREATE TRIGGER old_guard AFTER INSERT ON users BEGIN SELECT 1; END;",
        "DROP INDEX idx_old_email;
         DROP VIEW old_view;
         DROP TRIGGER old_guard;",
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.destructive_change_count, 0);
    assert_eq!(result.advisory_change_count, 3);
    assert!(has_change(
        &result,
        SchemaChangeKind::IndexRemoved,
        "idx_old_email"
    ));
    assert!(has_change(
        &result,
        SchemaChangeKind::ViewRemoved,
        "old_view"
    ));
    assert!(has_change(
        &result,
        SchemaChangeKind::TriggerRemoved,
        "old_guard"
    ));
}

#[test]
fn a_failure_names_the_line_sqlite_objected_to() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
        "CREATE TABLE a (id INTEGER);\n\nCREATE TABLE b (id INTEGER);\n\nCREAT TABL oops;\n",
    );
    let result = preview(&fixture);

    assert!(!result.success);
    let error = result.error.as_ref().unwrap();
    assert_eq!(error.line, Some(5), "message was: {}", error.message);
}

/// The reported line comes from a byte offset SQLite hands back, and the SQL it
/// indexes is UTF-8. A migration full of multi-byte text must produce a line
/// number or no line number — never a panic.
#[test]
fn a_failure_in_multibyte_sql_never_panics() {
    let cases = [
        ("-- café ☕ naïve comment\nCREAT TABL oops;\n", Some(2)),
        (
            "CREATE TABLE \"café\" (id INTEGER);\nCREAT TABL oops;\n",
            Some(2),
        ),
        ("SELECT '日本語のテキスト';\nCREAT TABL oops;\n", Some(2)),
        ("-- 😀😀😀\nSELECT * FROM ;\n", Some(2)),
        ("\u{1F600}\u{1F600}\n((((", Some(1)),
    ];

    for (sql, expected_line) in cases {
        let fixture = fixture("CREATE TABLE users (id INTEGER PRIMARY KEY);", sql);
        let hash_before = sha256_of(&fixture.database);
        let result = preview(&fixture);
        assert!(!result.success, "expected a failure for {sql:?}");
        let error = result.error.as_ref().unwrap();
        assert_eq!(error.line, expected_line, "line for {sql:?}");
        assert_eq!(hash_before, sha256_of(&fixture.database));
    }
}

/// A constraint violation is raised while running, not while preparing, so
/// SQLite reports no offset and MigraDry invents none.
#[test]
fn a_runtime_failure_reports_no_line_rather_than_a_guess() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);
         INSERT INTO users (id) VALUES (1);",
        "-- ünïcode preamble\nINSERT INTO users (id) VALUES (1);",
    );
    let result = preview(&fixture);
    assert!(!result.success);
    assert_eq!(result.error.as_ref().unwrap().line, None);
}

#[test]
fn a_failed_migration_reports_the_statements_that_had_already_applied() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);",
        "CREATE TABLE orders (id INTEGER PRIMARY KEY);
         ALTER TABLE users ADD COLUMN name TEXT;",
    );
    let result = preview(&fixture);

    assert!(!result.success);
    assert!(has_change(&result, SchemaChangeKind::TableAdded, "orders"));
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("failed part-way through")),
        "{:?}",
        result.warnings
    );
}

#[test]
fn syntax_errors_are_reported_with_the_sqlite_message() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
        "CREAT TABL oops (id INTEGER);",
    );
    let result = preview(&fixture);

    assert!(!result.success);
    let error = result.error.as_ref().unwrap();
    assert!(
        error.message.contains("syntax error"),
        "unexpected message: {}",
        error.message
    );
    assert_eq!(error.sqlite_code.as_deref(), Some("SQLITE_ERROR"));
    assert!(result.schema_changes.is_empty());
}

// ---------------------------------------------------------------------------
// Result shape
// ---------------------------------------------------------------------------

#[test]
fn the_result_names_the_files_without_leaking_anything_else() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
        "CREATE TABLE orders (id INTEGER PRIMARY KEY);",
    );
    let result = preview(&fixture);

    assert_eq!(result.database_name, "app.db");
    assert_eq!(result.migration_name, "001_change.sql");
    assert_eq!(result.clone_strategy, CloneStrategy::SqliteBackupApi);

    // Nothing in the serialized result should mention a temporary clone.
    let json = serde_json::to_string(&result).unwrap();
    assert!(!json.contains("clone.db"), "the clone path leaked: {json}");
    assert!(!json.contains("migradry-"), "the temp dir leaked: {json}");
}

#[test]
fn the_result_round_trips_through_json() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, legacy TEXT);
         INSERT INTO users (legacy) VALUES ('a'), (NULL);",
        "ALTER TABLE users DROP COLUMN legacy;",
    );
    let result = preview(&fixture);
    assert!(result.success, "{:?}", result.error);

    let json = serde_json::to_string(&result).unwrap();
    let parsed: migradry_lib::MigrationPreviewResult = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, result);

    // The frontend contract is camelCase with snake_case enum values.
    assert!(json.contains("\"originalContentUnchanged\":true"));
    assert!(json.contains("\"foreignKeysEnforced\":true"));
    assert!(json.contains("\"impact\":\"destructive\""));
    // Data impact is a tagged union, so the frontend can tell a measured zero
    // from a count that could not be taken.
    assert!(json.contains("\"status\":\"measured\""));
    assert!(json.contains("\"totalRows\":"));
    assert!(json.contains("\"affectedRows\":"));
    assert!(json.contains("\"kind\":\"column_removed\""));
    assert!(json.contains("\"impact\":\"destructive\""));
}

/// Column modifications carry their property deltas across the bridge.
#[test]
fn column_modifications_round_trip_through_json() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, value TEXT);",
        "CREATE TABLE users_new (id INTEGER PRIMARY KEY, value INTEGER NOT NULL DEFAULT 0);
         INSERT INTO users_new (id, value) SELECT id, 0 FROM users;
         DROP TABLE users;
         ALTER TABLE users_new RENAME TO users;",
    );
    let result = preview(&fixture);
    assert!(result.success, "{:?}", result.error);

    let json = serde_json::to_string(&result).unwrap();
    let parsed: migradry_lib::MigrationPreviewResult = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, result);

    assert!(json.contains("\"kind\":\"column_modified\""));
    assert!(json.contains("\"property\":\"declared_type\""));
    assert!(json.contains("\"property\":\"not_null\""));
    assert!(json.contains("\"property\":\"default_value\""));
    assert!(json.contains("\"propertyChanges\""));
}

#[test]
fn timings_are_reported() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
        "CREATE TABLE orders (id INTEGER PRIMARY KEY);",
    );
    let result = preview(&fixture);
    assert!(result.total_duration_ms >= result.duration_ms);
}

#[test]
fn the_same_inputs_always_produce_the_same_result() {
    let fixture = fixture(
        "CREATE TABLE zeta (id INTEGER, legacy TEXT);
         CREATE TABLE alpha (id INTEGER);
         CREATE INDEX idx_zeta ON zeta(id);",
        "CREATE TABLE mid (id INTEGER);
         ALTER TABLE zeta ADD COLUMN added_b TEXT;
         ALTER TABLE zeta ADD COLUMN added_a TEXT;
         DROP INDEX idx_zeta;
         DROP TABLE alpha;",
    );

    let first = preview(&fixture);
    let second = preview(&fixture);
    assert_eq!(first.schema_changes, second.schema_changes);

    let ordering: Vec<_> = first
        .schema_changes
        .iter()
        .map(|change| (change.kind, change.object_name.as_str()))
        .collect();
    assert_eq!(
        ordering,
        vec![
            (SchemaChangeKind::TableAdded, "mid"),
            (SchemaChangeKind::TableRemoved, "alpha"),
            (SchemaChangeKind::ColumnAdded, "added_a"),
            (SchemaChangeKind::ColumnAdded, "added_b"),
            (SchemaChangeKind::IndexRemoved, "idx_zeta"),
        ]
    );
}

// ---------------------------------------------------------------------------
// Clone lifetime
// ---------------------------------------------------------------------------

#[test]
fn the_workspace_is_deleted_once_it_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    seed_database(&database, "CREATE TABLE users (id INTEGER PRIMARY KEY);");

    let workspace = workspace(&database);
    let baseline = workspace.baseline().path().to_path_buf();
    let working = workspace.working().path().to_path_buf();
    let workspace_dir = workspace.directory().to_path_buf();
    for path in [&baseline, &working] {
        assert!(path.exists());
        assert!(path.starts_with(std::env::temp_dir()));
        assert!(!path.starts_with(dir.path()));
    }

    drop(workspace);
    assert!(!baseline.exists());
    assert!(!working.exists());
    assert!(!workspace_dir.exists());
}

#[test]
fn the_workspace_is_deleted_even_when_the_migration_fails() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    seed_database(&database, "CREATE TABLE users (id INTEGER PRIMARY KEY);");

    let workspace = workspace(&database);
    let workspace_dir = workspace.directory().to_path_buf();
    let run =
        migradry_lib::migration::execute_on_clone(workspace.working(), "DROP TABLE nope;").unwrap();
    assert!(run.failure.is_some());

    drop(workspace);
    assert!(!workspace_dir.exists());
}

#[test]
fn a_preview_leaves_the_originals_directory_exactly_as_it_found_it() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
        "CREATE TABLE orders (id INTEGER PRIMARY KEY);",
    );
    let directory = fixture.database.parent().unwrap();
    let before = directory_listing(directory);

    let result = preview(&fixture);
    assert!(result.success, "{:?}", result.error);

    assert_eq!(before, directory_listing(directory));
}

fn directory_listing(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// Schema snapshot exposed through the engine
// ---------------------------------------------------------------------------

#[test]
fn column_metadata_survives_the_clone() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    seed_database(
        &database,
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             email TEXT NOT NULL,
             nickname TEXT DEFAULT 'anon'
         );",
    );

    let workspace = workspace(&database);
    let connection = workspace.baseline().open_readonly().unwrap();
    let snapshot = migradry_lib::schema::snapshot(&connection).unwrap();

    let users = snapshot.table("users").expect("users table");
    assert_eq!(users.object_type, SchemaObjectKind::Table);
    assert_eq!(users.columns.len(), 3);
    assert!(users.columns[1].not_null);
    assert_eq!(users.columns[2].default_value.as_deref(), Some("'anon'"));
    assert!(users.sql.as_deref().unwrap().contains("CREATE TABLE users"));
}
