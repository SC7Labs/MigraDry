//! One realistic migration, end to end, checked from both sides.
//!
//! Everything else in the suite isolates one behaviour. This test does what a
//! developer actually does — a table rebuild that alters a column, drops a
//! populated one, drops a populated table and adds new objects — and then walks
//! back to the original database and confirms, independently of anything the
//! engine reported, that none of it happened there.

mod common;

use common::*;
use migradry_lib::{ColumnPropertyChange, MigrationService, SchemaChangeKind};

/// 50 users, 31 of them carrying a legacy code; 137 events.
const SCHEMA: &str = "
    CREATE TABLE users (
        id INTEGER PRIMARY KEY,
        email TEXT,
        legacy_code TEXT,
        name TEXT NOT NULL
    );

    CREATE TABLE old_events (
        id INTEGER PRIMARY KEY,
        payload TEXT NOT NULL
    );

    CREATE INDEX idx_users_email ON users(email);

    CREATE TRIGGER users_audit AFTER INSERT ON users
    BEGIN
        SELECT 1;
    END;

    WITH RECURSIVE seq(n) AS (
        SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 50
    )
    INSERT INTO users (id, email, legacy_code, name)
        SELECT n,
               'user' || n || '@example.test',
               CASE WHEN n <= 31 THEN 'legacy-' || n ELSE NULL END,
               'User ' || n
        FROM seq;

    WITH RECURSIVE seq(n) AS (
        SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 137
    )
    INSERT INTO old_events (id, payload) SELECT n, 'event-' || n FROM seq;
";

/// The table-rebuild pattern, plus a dropped table and a new index.
const MIGRATION: &str = "
    CREATE TABLE users_new (
        id INTEGER PRIMARY KEY,
        email TEXT NOT NULL DEFAULT '',
        name TEXT NOT NULL,
        last_login TEXT
    );

    INSERT INTO users_new (id, email, name)
        SELECT id, coalesce(email, ''), name FROM users;

    DROP TABLE users;
    ALTER TABLE users_new RENAME TO users;

    CREATE INDEX idx_users_last_login ON users(last_login);

    DROP TABLE old_events;
";

#[test]
fn a_representative_migration_is_previewed_correctly_and_changes_nothing() {
    let fixture = fixture(SCHEMA, MIGRATION);

    // --- independent evidence, taken before ------------------------------
    let hash_before = sha256_of(&fixture.database);
    assert_eq!(row_count(&fixture.database, "users"), 50);
    assert_eq!(row_count(&fixture.database, "old_events"), 137);

    let result = MigrationService::preview(&fixture.database, &fixture.migration)
        .expect("the preview should run");
    let hash_after = sha256_of(&fixture.database);

    // --- what the migration would do -------------------------------------
    assert!(result.success, "{:?}", result.error);
    assert!(result.foreign_keys_enforced);

    // A surviving column, altered in place. One finding, two deltas.
    let email = modification(&result, "users", "email");
    assert_eq!(
        email.property_changes,
        vec![
            ColumnPropertyChange::NotNull {
                before: false,
                after: true,
            },
            ColumnPropertyChange::DefaultValue {
                before: None,
                after: Some("''".to_string()),
            },
        ]
    );

    // A populated column, removed, with an exact non-null count.
    assert!(has_child_change(
        &result,
        SchemaChangeKind::ColumnRemoved,
        "users",
        "legacy_code"
    ));
    assert_eq!(
        measured_impact(&result, "users", Some("legacy_code")),
        (50, 31)
    );

    // A populated table, removed, with an exact row count.
    assert!(has_change(
        &result,
        SchemaChangeKind::TableRemoved,
        "old_events"
    ));
    assert_eq!(measured_impact(&result, "old_events", None), (137, 137));

    // Things the migration adds.
    assert!(has_child_change(
        &result,
        SchemaChangeKind::ColumnAdded,
        "users",
        "last_login"
    ));
    assert!(has_change(
        &result,
        SchemaChangeKind::IndexAdded,
        "idx_users_last_login"
    ));

    // A rebuild takes the old table's dependants with it, and MigraDry says so
    // rather than quietly leaving them out.
    assert!(has_change(
        &result,
        SchemaChangeKind::IndexRemoved,
        "idx_users_email"
    ));
    assert!(has_change(
        &result,
        SchemaChangeKind::TriggerRemoved,
        "users_audit"
    ));

    assert_eq!(result.destructive_change_count, 2, "one table, one column");
    assert_eq!(
        result.advisory_change_count, 3,
        "one modification, one index and one trigger removed"
    );
    assert_eq!(result.schema_changes.len(), 7);

    // --- the integrity proof the engine offers ---------------------------
    assert!(result.original_content_unchanged);
    assert!(result.original_integrity.content_unchanged);
    assert_eq!(result.original_integrity.before.main.sha256, hash_before);
    assert_eq!(result.original_integrity.after.main.sha256, hash_after);
    assert!(
        !result.original_integrity.shm_created_by_preview,
        "a rollback-journal database needs no shared-memory index"
    );

    // --- independent evidence, taken after -------------------------------
    // None of the above is trusted: the original is reopened and inspected.
    assert_eq!(hash_before, hash_after, "the original database changed");

    let columns = column_names(&fixture.database, "users");
    assert_eq!(columns, vec!["id", "email", "legacy_code", "name"]);
    assert!(!columns.contains(&"last_login".to_string()));

    assert!(table_names(&fixture.database).contains(&"old_events".to_string()));
    assert_eq!(row_count(&fixture.database, "old_events"), 137);
    assert_eq!(row_count(&fixture.database, "users"), 50);

    assert_eq!(index_names(&fixture.database), vec!["idx_users_email"]);
    assert_eq!(trigger_names(&fixture.database), vec!["users_audit"]);

    // The altered column is still exactly as it was declared.
    let connection = open_readonly(&fixture.database);
    let (not_null, default): (i64, Option<String>) = connection
        .query_row(
            "SELECT \"notnull\", dflt_value FROM pragma_table_info('users') WHERE name = 'email'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(not_null, 0, "email is still nullable in the original");
    assert_eq!(default, None, "email still has no default in the original");

    // And the rows the migration would have discarded are all still there.
    let legacy: i64 = connection
        .query_row("SELECT count(legacy_code) FROM users", [], |row| row.get(0))
        .unwrap();
    assert_eq!(legacy, 31);

    // Nothing was left beside the original either.
    let mut names: Vec<String> = std::fs::read_dir(fixture.database.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["001_change.sql", "app.db"]);
}
