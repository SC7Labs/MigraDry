//! The tests that matter most: proof that a preview never changes the original.
//!
//! Every test in this file hashes the original database itself, before and
//! after the preview, and compares the two. `result.original_content_unchanged` is
//! checked as well, but it is never the only evidence — the engine reporting
//! that it behaved is not the same as the engine having behaved.

mod common;

use common::*;
use migradry_lib::{MigraDryError, MigrationService, SchemaChangeKind};

/// Required test 1: a migration that succeeds.
#[test]
fn successful_migration_is_previewed_without_touching_the_original() {
    let fixture = fixture(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             name TEXT NOT NULL
         );",
        "ALTER TABLE users ADD COLUMN email TEXT;

         CREATE TABLE orders (
             id INTEGER PRIMARY KEY,
             user_id INTEGER NOT NULL
         );",
    );

    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);
    let hash_after = sha256_of(&fixture.database);

    // The migration itself.
    assert!(
        result.success,
        "migration should succeed: {:?}",
        result.error
    );
    assert!(result.error.is_none());
    assert!(has_child_change(
        &result,
        SchemaChangeKind::ColumnAdded,
        "users",
        "email"
    ));
    assert!(has_change(&result, SchemaChangeKind::TableAdded, "orders"));
    assert_eq!(result.schema_changes.len(), 2);
    assert_eq!(result.destructive_change_count, 0);

    // The engine's claim.
    assert!(result.original_content_unchanged);
    assert!(result.original_integrity.content_unchanged);

    // The independent proof.
    assert_eq!(hash_before, hash_after, "original database file changed");
    assert_eq!(result.original_integrity.before.main.sha256, hash_before);
    assert_eq!(result.original_integrity.after.main.sha256, hash_after);

    // And the original schema, read straight from the file.
    assert_eq!(column_names(&fixture.database, "users"), vec!["id", "name"]);
    assert!(!column_names(&fixture.database, "users").contains(&"email".to_string()));
    assert_eq!(table_names(&fixture.database), vec!["users"]);
    assert!(!table_names(&fixture.database).contains(&"orders".to_string()));
}

/// Required test 2: a migration that fails.
#[test]
fn failed_migration_is_reported_without_touching_the_original() {
    let fixture = fixture(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             name TEXT
         );",
        "ALTER TABLE users ADD COLUMN name TEXT;",
    );

    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);
    let hash_after = sha256_of(&fixture.database);

    assert!(!result.success);
    let error = result.error.as_ref().expect("a migration error");
    assert!(
        error.message.contains("duplicate column name"),
        "unexpected SQLite message: {}",
        error.message
    );
    assert_eq!(error.sqlite_code.as_deref(), Some("SQLITE_ERROR"));
    assert!(error.sqlite_extended_code.is_some());

    // A failed migration is still a successful preview.
    assert!(result.original_content_unchanged);
    assert_eq!(hash_before, hash_after);
    assert_eq!(column_names(&fixture.database, "users"), vec!["id", "name"]);
}

/// Required test 3: a destructive migration.
#[test]
fn destructive_migration_is_flagged_and_the_original_keeps_its_data() {
    let fixture = fixture(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             name TEXT
         );

         CREATE TABLE old_data (
             id INTEGER PRIMARY KEY
         );

         INSERT INTO old_data (id) VALUES (1), (2), (3);",
        "DROP TABLE old_data;",
    );

    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);
    let hash_after = sha256_of(&fixture.database);

    assert!(result.success);
    assert!(has_change(
        &result,
        SchemaChangeKind::TableRemoved,
        "old_data"
    ));
    assert_eq!(result.destructive_change_count, 1);

    assert!(result.original_content_unchanged);
    assert_eq!(hash_before, hash_after);

    // The original still holds the table and every row in it.
    assert!(table_names(&fixture.database).contains(&"old_data".to_string()));
    assert_eq!(row_count(&fixture.database, "old_data"), 3);
}

/// Dropping rows is not a schema change, but it must not reach the original either.
#[test]
fn data_deleting_migration_leaves_the_original_rows_alone() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users (name) VALUES ('ada'), ('grace'), ('katherine');",
        "DELETE FROM users;",
    );

    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);
    let hash_after = sha256_of(&fixture.database);

    assert!(result.success);
    // No schema object changed, so the diff is legitimately empty.
    assert!(result.schema_changes.is_empty());
    assert_eq!(hash_before, hash_after);
    assert_eq!(row_count(&fixture.database, "users"), 3);
}

/// A migration that tries to escape the clone by attaching the original.
///
/// This is the one SQL feature that could otherwise reach a second database
/// file, so it is refused outright.
#[test]
fn migration_cannot_attach_and_damage_the_original() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    seed_database(
        &database,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users (name) VALUES ('ada');",
    );

    let migration = dir.path().join("attack.sql");
    std::fs::write(
        &migration,
        format!(
            "ATTACH DATABASE '{}' AS victim;\nDROP TABLE victim.users;\n",
            database.display()
        ),
    )
    .unwrap();

    let hash_before = sha256_of(&database);
    let result = MigrationService::preview(&database, &migration).expect("preview should run");
    let hash_after = sha256_of(&database);

    assert!(!result.success, "ATTACH must not be allowed to succeed");
    let error = result.error.as_ref().expect("an error");
    assert!(
        error
            .message
            .contains("Naming another database file is not permitted"),
        "unexpected message: {}",
        error.message
    );
    assert_eq!(error.sqlite_code.as_deref(), Some("SQLITE_AUTH"));

    assert!(result.original_content_unchanged);
    assert_eq!(hash_before, hash_after);
    assert!(table_names(&database).contains(&"users".to_string()));
    assert_eq!(row_count(&database, "users"), 1);
}

/// `VACUUM INTO` is SQLite's other way of naming a file from SQL. It cannot
/// overwrite an existing database, but it is refused all the same so that
/// migration SQL cannot write anywhere on disk.
#[test]
fn migration_cannot_vacuum_into_a_named_file() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    seed_database(&database, "CREATE TABLE users (id INTEGER PRIMARY KEY);");

    let escape_target = dir.path().join("escaped.db");
    let migration = dir.path().join("escape.sql");
    std::fs::write(
        &migration,
        format!("VACUUM INTO '{}';\n", escape_target.display()),
    )
    .unwrap();

    let hash_before = sha256_of(&database);
    let result = MigrationService::preview(&database, &migration).expect("preview should run");
    let hash_after = sha256_of(&database);

    assert!(!result.success);
    assert_eq!(
        result.error.as_ref().unwrap().sqlite_code.as_deref(),
        Some("SQLITE_AUTH")
    );
    assert!(
        !escape_target.exists(),
        "migration wrote a file outside the clone"
    );
    assert_eq!(hash_before, hash_after);
}

/// A migration full of destructive statements, run against a database that has
/// something of everything.
#[test]
fn wholesale_schema_destruction_stays_inside_the_clone() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER);
         CREATE INDEX idx_users_email ON users(email);
         CREATE VIEW user_orders AS SELECT * FROM orders;
         CREATE TRIGGER users_guard AFTER INSERT ON users BEGIN SELECT 1; END;
         INSERT INTO users (email) VALUES ('ada@example.com');",
        "DROP TRIGGER users_guard;
         DROP VIEW user_orders;
         DROP INDEX idx_users_email;
         DROP TABLE orders;
         DROP TABLE users;",
    );

    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);
    let hash_after = sha256_of(&fixture.database);

    assert!(result.success);
    assert_eq!(result.destructive_change_count, 2, "two tables dropped");
    assert_eq!(
        result.advisory_change_count, 3,
        "index, view and trigger dropped"
    );

    assert_eq!(hash_before, hash_after);
    assert_eq!(table_names(&fixture.database), vec!["orders", "users"]);
    assert_eq!(index_names(&fixture.database), vec!["idx_users_email"]);
    assert_eq!(view_names(&fixture.database), vec!["user_orders"]);
    assert_eq!(trigger_names(&fixture.database), vec!["users_guard"]);
    assert_eq!(row_count(&fixture.database, "users"), 1);
}

/// Pragmas that change durable file properties are a classic way to modify a
/// database as a side effect. They must only ever affect the clone.
#[test]
fn pragmas_in_a_migration_do_not_reach_the_original() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
        "PRAGMA journal_mode = WAL;
         PRAGMA user_version = 42;
         PRAGMA page_size = 8192;
         VACUUM;
         CREATE TABLE audit (id INTEGER PRIMARY KEY);",
    );

    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);
    let hash_after = sha256_of(&fixture.database);

    assert!(result.success, "{:?}", result.error);
    assert!(has_change(&result, SchemaChangeKind::TableAdded, "audit"));
    assert_eq!(hash_before, hash_after);

    // `user_version` is stored in the database header, so a leak would show here.
    let connection = open_readonly(&fixture.database);
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(user_version, 0);
    assert!(!fixture.database.with_extension("db-wal").exists());
}

/// A migration whose transaction is never committed.
#[test]
fn an_unclosed_transaction_is_resolved_on_the_clone_only() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
        "BEGIN;
         CREATE TABLE staging (id INTEGER PRIMARY KEY);",
    );

    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);
    let hash_after = sha256_of(&fixture.database);

    assert!(result.success, "{:?}", result.error);
    assert!(has_change(&result, SchemaChangeKind::TableAdded, "staging"));
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("left a transaction open")),
        "expected a warning about the open transaction: {:?}",
        result.warnings
    );
    assert_eq!(hash_before, hash_after);
    assert_eq!(table_names(&fixture.database), vec!["users"]);
}

/// A migration that rolls itself back should report nothing, and still not
/// touch the original.
#[test]
fn an_explicit_rollback_produces_no_changes() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY);",
        "BEGIN;
         CREATE TABLE staging (id INTEGER PRIMARY KEY);
         ROLLBACK;",
    );

    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);
    let hash_after = sha256_of(&fixture.database);

    assert!(result.success, "{:?}", result.error);
    assert!(result.schema_changes.is_empty());
    assert_eq!(hash_before, hash_after);
}

/// The original is never opened in a way that could write to it, even when the
/// file itself is read-only on disk.
#[test]
fn a_read_only_database_file_can_still_be_previewed() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let fixture = fixture(
            "CREATE TABLE users (id INTEGER PRIMARY KEY);",
            "CREATE TABLE orders (id INTEGER PRIMARY KEY);",
        );
        std::fs::set_permissions(&fixture.database, std::fs::Permissions::from_mode(0o444))
            .unwrap();

        let hash_before = sha256_of(&fixture.database);
        let result = preview(&fixture);
        let hash_after = sha256_of(&fixture.database);

        assert!(result.success, "{:?}", result.error);
        assert!(has_change(&result, SchemaChangeKind::TableAdded, "orders"));
        assert_eq!(hash_before, hash_after);

        std::fs::set_permissions(&fixture.database, std::fs::Permissions::from_mode(0o644))
            .unwrap();
    }
}

/// Repeated previews are pure: nothing accumulates in the original.
#[test]
fn repeated_previews_leave_the_original_byte_identical() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users (name) VALUES ('ada');",
        "ALTER TABLE users ADD COLUMN email TEXT;
         DROP TABLE IF EXISTS nothing_here;",
    );

    let hash_before = sha256_of(&fixture.database);
    for _ in 0..5 {
        let result = preview(&fixture);
        assert!(result.success, "{:?}", result.error);
        assert!(result.original_content_unchanged);
        assert_eq!(sha256_of(&fixture.database), hash_before);
    }
    assert_eq!(column_names(&fixture.database, "users"), vec!["id", "name"]);
}

/// The engine refuses to treat the same file as both inputs.
#[test]
fn the_same_file_cannot_be_both_database_and_migration() {
    let fixture = fixture("CREATE TABLE users (id INTEGER);", "SELECT 1;");
    let error = MigrationService::preview(&fixture.database, &fixture.database)
        .expect_err("the same path twice must be rejected");
    // The database is rejected as a migration before the paths are even compared.
    assert!(matches!(
        error,
        MigraDryError::MigrationNotSql { .. } | MigraDryError::SamePath
    ));
}
