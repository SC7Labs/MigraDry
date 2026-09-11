//! Foreign-key semantics during a preview.
//!
//! Upstream SQLite documents `PRAGMA foreign_keys` as **off**; the bundled
//! amalgamation this crate links is compiled with `SQLITE_DEFAULT_FOREIGN_KEYS`
//! and is therefore **on**. Relying on either would make MigraDry's semantics a
//! property of somebody else's build flags.
//!
//! So MigraDry sets it, reports that it set it, and lets a migration override
//! it the ordinary SQLite way. These tests pin all three.

mod common;

use common::*;
use migradry_lib::{MigrationService, SchemaChangeKind};

const PARENT_AND_CHILD: &str = "
    CREATE TABLE authors (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL
    );

    CREATE TABLE books (
        id INTEGER PRIMARY KEY,
        author_id INTEGER NOT NULL REFERENCES authors(id),
        title TEXT NOT NULL
    );

    INSERT INTO authors (id, name) VALUES (1, 'ada');
    INSERT INTO books (id, author_id, title) VALUES (1, 1, 'notes');
";

#[test]
fn the_result_states_the_foreign_key_policy() {
    let fixture = fixture(
        PARENT_AND_CHILD,
        "CREATE TABLE tags (id INTEGER PRIMARY KEY);",
    );
    let result = preview(&fixture);
    assert!(result.success, "{:?}", result.error);
    assert!(
        result.foreign_keys_enforced,
        "the policy must be visible in the result, not left to be guessed"
    );
}

#[test]
fn a_migration_that_respects_foreign_keys_succeeds() {
    let fixture = fixture(
        PARENT_AND_CHILD,
        "INSERT INTO authors (id, name) VALUES (2, 'grace');
         INSERT INTO books (id, author_id, title) VALUES (2, 2, 'compilers');

         CREATE TABLE reviews (
             id INTEGER PRIMARY KEY,
             book_id INTEGER NOT NULL REFERENCES books(id)
         );

         INSERT INTO reviews (id, book_id) VALUES (1, 1);",
    );

    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);
    let hash_after = sha256_of(&fixture.database);

    assert!(result.success, "{:?}", result.error);
    assert!(has_change(&result, SchemaChangeKind::TableAdded, "reviews"));
    assert_eq!(hash_before, hash_after);
}

/// The case that makes the policy worth having: with SQLite's own default this
/// migration would look fine.
#[test]
fn a_migration_that_orphans_a_row_is_caught() {
    let fixture = fixture(
        PARENT_AND_CHILD,
        "INSERT INTO books (id, author_id, title) VALUES (2, 999, 'ghostwritten');",
    );

    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);
    let hash_after = sha256_of(&fixture.database);

    assert!(
        !result.success,
        "the foreign-key violation must be reported"
    );
    let error = result.error.as_ref().unwrap();
    assert!(
        error.message.contains("FOREIGN KEY constraint failed"),
        "unexpected message: {}",
        error.message
    );
    assert_eq!(error.sqlite_code.as_deref(), Some("SQLITE_CONSTRAINT"));
    assert_eq!(hash_before, hash_after);
}

/// Proof that the previous test is not a tautology: the very same migration
/// passes once foreign keys are off, which is exactly what MigraDry would have
/// reported had it inherited SQLite's default.
#[test]
fn the_same_migration_would_have_passed_with_foreign_keys_off() {
    let fixture = fixture(
        PARENT_AND_CHILD,
        "PRAGMA foreign_keys = OFF;
         INSERT INTO books (id, author_id, title) VALUES (2, 999, 'ghostwritten');",
    );
    let result = preview(&fixture);
    assert!(
        result.success,
        "a migration may turn enforcement off itself: {:?}",
        result.error
    );
}

/// Dropping a table other tables depend on behaves differently under
/// enforcement, and that difference is the whole point of the policy.
#[test]
fn dropping_a_referenced_table_is_caught() {
    let fixture = fixture(PARENT_AND_CHILD, "DROP TABLE authors;");

    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);
    let hash_after = sha256_of(&fixture.database);

    assert!(
        !result.success,
        "dropping a referenced table must be reported"
    );
    assert!(
        result
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("FOREIGN KEY constraint failed"),
        "unexpected message: {}",
        result.error.as_ref().unwrap().message
    );

    // The original keeps both tables and all its rows.
    assert_eq!(hash_before, hash_after);
    assert_eq!(table_names(&fixture.database), vec!["authors", "books"]);
    assert_eq!(row_count(&fixture.database, "authors"), 1);
}

/// The standard SQLite table-rebuild recipe opens by switching enforcement off.
/// It must still work exactly as it does everywhere else.
#[test]
fn the_standard_table_rebuild_recipe_still_works() {
    let fixture = fixture(
        PARENT_AND_CHILD,
        "PRAGMA foreign_keys = OFF;

         BEGIN;
         CREATE TABLE books_new (
             id INTEGER PRIMARY KEY,
             author_id INTEGER NOT NULL REFERENCES authors(id),
             title TEXT NOT NULL,
             subtitle TEXT
         );
         INSERT INTO books_new (id, author_id, title)
             SELECT id, author_id, title FROM books;
         DROP TABLE books;
         ALTER TABLE books_new RENAME TO books;
         COMMIT;

         PRAGMA foreign_keys = ON;",
    );

    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);
    let hash_after = sha256_of(&fixture.database);

    assert!(result.success, "{:?}", result.error);
    assert!(has_child_change(
        &result,
        SchemaChangeKind::ColumnAdded,
        "books",
        "subtitle"
    ));
    assert_eq!(hash_before, hash_after);
    assert_eq!(
        column_names(&fixture.database, "books"),
        vec!["id", "author_id", "title"]
    );
}

/// Deferred constraints are checked at COMMIT, so a migration can legitimately
/// break and repair referential integrity inside one transaction.
#[test]
fn deferred_constraints_are_checked_at_commit() {
    let schema = "
        CREATE TABLE authors (id INTEGER PRIMARY KEY);
        CREATE TABLE books (
            id INTEGER PRIMARY KEY,
            author_id INTEGER NOT NULL
                REFERENCES authors(id) DEFERRABLE INITIALLY DEFERRED
        );
        INSERT INTO authors (id) VALUES (1);
    ";

    // Repaired before COMMIT: allowed.
    let repaired = fixture(
        schema,
        "BEGIN;
         INSERT INTO books (id, author_id) VALUES (1, 2);
         INSERT INTO authors (id) VALUES (2);
         COMMIT;",
    );
    let result = preview(&repaired);
    assert!(result.success, "{:?}", result.error);

    // Left broken at COMMIT: rejected.
    let broken = fixture(
        schema,
        "BEGIN;
         INSERT INTO books (id, author_id) VALUES (1, 2);
         COMMIT;",
    );
    let result = preview(&broken);
    assert!(!result.success);
    assert!(result
        .error
        .as_ref()
        .unwrap()
        .message
        .contains("FOREIGN KEY constraint failed"));
}

/// Enforcement is a property of the clone's connection and cannot reach the
/// original.
#[test]
fn enforcing_foreign_keys_does_not_alter_the_original() {
    let fixture = fixture(
        PARENT_AND_CHILD,
        "CREATE TABLE tags (id INTEGER PRIMARY KEY);",
    );

    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);
    let hash_after = sha256_of(&fixture.database);

    assert!(result.success, "{:?}", result.error);
    assert_eq!(hash_before, hash_after);
    assert_eq!(table_names(&fixture.database), vec!["authors", "books"]);
}

/// Asks the clone itself what it is enforcing, rather than trusting a default.
///
/// This is the test that would catch a silent change in `libsqlite3-sys`'s
/// compile flags, or a build against a system SQLite: whatever the amalgamation
/// thinks the default is, the connection a migration actually runs on reports
/// enforcement on.
#[test]
fn the_clone_connection_really_starts_with_enforcement_on() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    seed_database(&database, PARENT_AND_CHILD);

    let workspace = workspace(&database);
    let run = migradry_lib::migration::execute_on_clone(
        workspace.working(),
        "CREATE TABLE observed (foreign_keys INTEGER);
         INSERT INTO observed SELECT * FROM pragma_foreign_keys();",
    )
    .unwrap();
    assert!(run.failure.is_none(), "{:?}", run.failure);

    let connection = workspace.working().open_readonly().unwrap();
    let observed: i64 = connection
        .query_row("SELECT foreign_keys FROM observed", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        observed, 1,
        "migration SQL must run with foreign keys enforced"
    );
}

#[test]
fn a_migration_can_run_its_own_foreign_key_check() {
    let fixture = fixture(
        PARENT_AND_CHILD,
        "PRAGMA foreign_key_check;
         CREATE TABLE tags (id INTEGER PRIMARY KEY);",
    );
    let result = preview(&fixture);
    assert!(result.success, "{:?}", result.error);
    assert!(has_change(&result, SchemaChangeKind::TableAdded, "tags"));
}

/// Foreign keys are enforced from the first statement, not after the first
/// commit — the pragma is set before any migration SQL runs.
#[test]
fn enforcement_is_active_for_the_very_first_statement() {
    let fixture = fixture(
        PARENT_AND_CHILD,
        "INSERT INTO books (id, author_id, title) VALUES (7, 42, 'first');",
    );
    let result = preview(&fixture);
    assert!(!result.success);
    let error = result.error.as_ref().unwrap();
    assert_eq!(error.sqlite_code.as_deref(), Some("SQLITE_CONSTRAINT"));
    // No line number: SQLite raises a constraint violation while *running* a
    // statement, not while preparing one, so it reports no offset. MigraDry
    // reports what SQLite gives it and invents nothing.
    assert_eq!(error.line, None);
}

/// The policy the engine applies and the policy the result advertises are the
/// same value, so the two cannot drift apart.
#[test]
fn the_engine_constant_matches_what_the_result_reports() {
    let fixture = fixture(PARENT_AND_CHILD, "SELECT 1;");
    assert_eq!(
        preview(&fixture).foreign_keys_enforced,
        migradry_lib::migration::FOREIGN_KEYS_ENFORCED
    );
}

/// A preview must not leave enforcement behind on a later, unrelated preview.
#[test]
fn each_preview_starts_from_the_same_state() {
    let fixture = fixture(
        PARENT_AND_CHILD,
        "PRAGMA foreign_keys = OFF;
         INSERT INTO books (id, author_id, title) VALUES (2, 999, 'ghostwritten');",
    );
    assert!(preview(&fixture).success);

    // A second, different migration on the same database must still be judged
    // with enforcement on.
    let strict = fixture_with(
        &fixture,
        "INSERT INTO books (id, author_id, title) VALUES (3, 999, 'also ghostwritten');",
    );
    assert!(
        !MigrationService::preview(&fixture.database, &strict)
            .unwrap()
            .success
    );
}

/// Writes a second migration beside an existing fixture.
fn fixture_with(fixture: &Fixture, sql: &str) -> std::path::PathBuf {
    let path = fixture.database.parent().unwrap().join("002_second.sql");
    std::fs::write(&path, sql).unwrap();
    path
}
