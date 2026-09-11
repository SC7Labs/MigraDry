//! Re-audit of the one thing SQL could use to reach a second database file.
//!
//! `ATTACH` is the only mechanism, and `VACUUM INTO` uses it internally. Both
//! are refused for any *named* target, whether that name is absolute, relative,
//! URI-shaped, or `:memory:`. SQLite's own anonymous attachment — an empty
//! filename, which is how a plain `VACUUM` works — is allowed, because it
//! cannot name anything on disk.
//!
//! Every test here checks the filesystem afterwards, not just the error.

mod common;

use common::*;
use migradry_lib::MigrationService;
use std::path::{Path, PathBuf};

/// Runs a migration and returns the result, failing the test if no preview ran.
fn preview_sql(database: &Path, dir: &Path, sql: &str) -> migradry_lib::MigrationPreviewResult {
    let migration = dir.join("attempt.sql");
    std::fs::write(&migration, sql).unwrap();
    MigrationService::preview(database, &migration).expect("the preview itself should run")
}

fn seeded(dir: &Path) -> PathBuf {
    let database = dir.join("app.db");
    seed_database(
        &database,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users (name) VALUES ('ada');",
    );
    database
}

fn assert_refused(result: &migradry_lib::MigrationPreviewResult) {
    assert!(!result.success, "the statement must not be allowed");
    let error = result.error.as_ref().expect("an error");
    assert_eq!(
        error.sqlite_code.as_deref(),
        Some("SQLITE_AUTH"),
        "expected the authorizer to refuse: {}",
        error.message
    );
    assert!(
        error
            .message
            .contains("Naming another database file is not permitted"),
        "unexpected message: {}",
        error.message
    );
}

#[test]
fn attaching_the_original_by_absolute_path_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let database = seeded(dir.path());

    let hash_before = sha256_of(&database);
    let result = preview_sql(
        &database,
        dir.path(),
        &format!(
            "ATTACH DATABASE '{}' AS victim;\nDROP TABLE victim.users;\n",
            database.display()
        ),
    );
    assert_refused(&result);
    assert_eq!(hash_before, sha256_of(&database));
    assert_eq!(row_count(&database, "users"), 1);
}

#[test]
fn attaching_a_relative_path_is_refused_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let database = seeded(dir.path());

    // A relative name resolves against the process working directory, so if the
    // authorizer ever failed open, the file would land in the crate root.
    let escape = Path::new("migradry_containment_escape.db");
    assert!(!escape.exists(), "stale fixture from a previous run");

    let result = preview_sql(
        &database,
        dir.path(),
        "ATTACH DATABASE 'migradry_containment_escape.db' AS x;\
         \nCREATE TABLE x.loot (id INTEGER);\n",
    );
    assert_refused(&result);
    assert!(
        !escape.exists(),
        "migration SQL created a database outside the clone"
    );
}

#[test]
fn attaching_a_uri_shaped_name_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let database = seeded(dir.path());
    let target = dir.path().join("uri_escape.db");

    for name in [
        format!("file:{}?mode=rwc", target.display()),
        format!("file:{}", target.display()),
        "file:uri_escape.db?mode=memory&cache=shared".to_string(),
    ] {
        let result = preview_sql(
            &database,
            dir.path(),
            &format!("ATTACH DATABASE '{name}' AS x;\nCREATE TABLE x.loot (id INTEGER);\n"),
        );
        assert_refused(&result);
        assert!(!target.exists(), "a URI-shaped name reached the filesystem");
    }
}

#[test]
fn attaching_memory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let database = seeded(dir.path());

    // Harmless in itself, but it is a *named* target and the rule is simple on
    // purpose: only SQLite's own anonymous attachment is allowed.
    let result = preview_sql(
        &database,
        dir.path(),
        "ATTACH DATABASE ':memory:' AS scratch;\nCREATE TABLE scratch.t (id INTEGER);\n",
    );
    assert_refused(&result);
}

#[test]
fn vacuum_into_an_absolute_path_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let database = seeded(dir.path());
    let target = dir.path().join("vacuumed.db");

    let result = preview_sql(
        &database,
        dir.path(),
        &format!("VACUUM INTO '{}';\n", target.display()),
    );
    assert_refused(&result);
    assert!(!target.exists());
}

#[test]
fn vacuum_into_a_relative_path_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let database = seeded(dir.path());

    let escape = Path::new("migradry_containment_vacuum.db");
    assert!(!escape.exists(), "stale fixture from a previous run");

    let result = preview_sql(
        &database,
        dir.path(),
        "VACUUM INTO 'migradry_containment_vacuum.db';\n",
    );
    assert_refused(&result);
    assert!(!escape.exists());
}

#[test]
fn a_plain_vacuum_is_allowed_and_only_touches_the_clone() {
    let dir = tempfile::tempdir().unwrap();
    let database = seeded(dir.path());

    let migration = dir.path().join("attempt.sql");
    std::fs::write(
        &migration,
        "DELETE FROM users;\nVACUUM;\nCREATE TABLE audit (id INTEGER PRIMARY KEY);\n",
    )
    .unwrap();
    // Listed after the migration file exists, so only new files show up.
    let before = directory_listing(dir.path());
    let hash_before = sha256_of(&database);
    let result = MigrationService::preview(&database, &migration).expect("preview should run");

    assert!(result.success, "{:?}", result.error);
    assert!(has_change(
        &result,
        migradry_lib::SchemaChangeKind::TableAdded,
        "audit"
    ));
    assert_eq!(hash_before, sha256_of(&database));
    assert_eq!(row_count(&database, "users"), 1);
    assert_eq!(before, directory_listing(dir.path()));
}

/// SQLite's anonymous attachment cannot name a file, so it is allowed — and the
/// post-execution `PRAGMA database_list` audit accepts it rather than tripping.
#[test]
fn an_anonymous_attachment_is_allowed_and_reaches_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let database = seeded(dir.path());

    let migration = dir.path().join("attempt.sql");
    std::fs::write(
        &migration,
        "ATTACH DATABASE '' AS scratch;
         CREATE TABLE scratch.working (id INTEGER);
         CREATE TABLE kept (id INTEGER PRIMARY KEY);",
    )
    .unwrap();
    let before = directory_listing(dir.path());
    let result = MigrationService::preview(&database, &migration).expect("preview should run");

    assert!(result.success, "{:?}", result.error);
    assert!(has_change(
        &result,
        migradry_lib::SchemaChangeKind::TableAdded,
        "kept"
    ));
    // The scratch table lives in an anonymous database, so it is not part of
    // the clone's schema and never appears in the diff.
    assert!(!has_change(
        &result,
        migradry_lib::SchemaChangeKind::TableAdded,
        "working"
    ));
    assert_eq!(before, directory_listing(dir.path()));
}

/// The post-execution audit itself: after any migration, the connection must
/// still see nothing but the clone.
#[test]
fn after_execution_only_the_clone_is_open() {
    let dir = tempfile::tempdir().unwrap();
    let database = seeded(dir.path());

    let workspace = workspace(&database);
    let run = migradry_lib::migration::execute_on_clone(
        workspace.working(),
        "CREATE TABLE audit (id INTEGER PRIMARY KEY);
         CREATE TEMP TABLE scratch (id INTEGER);",
    )
    .unwrap();
    assert!(run.failure.is_none(), "{:?}", run.failure);

    // Re-open and confirm from the outside what the engine asserts internally.
    let connection = workspace.working().open_readonly().unwrap();
    let mut statement = connection.prepare("PRAGMA database_list").unwrap();
    let entries: Vec<(String, String)> = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            ))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect();

    for (name, file) in &entries {
        assert!(
            name == "main" || name == "temp" || file.is_empty(),
            "an unexpected database is attached: {name} -> {file}"
        );
        if name == "main" {
            assert_eq!(
                Path::new(file).canonicalize().unwrap(),
                workspace.working().path().canonicalize().unwrap()
            );
        }
    }
}

/// A database whose *filename* looks like a SQLite URI must be treated as a
/// path, not as a URI.
///
/// `SQLITE_OPEN_URI` is deliberately absent when the source is opened. With it
/// enabled, this filename would be parsed as a URI carrying `mode=rwc`, and
/// SQLite would open a *different*, writable file called `app.db`.
#[test]
fn a_filename_that_looks_like_a_uri_is_treated_as_a_path() {
    let dir = tempfile::tempdir().unwrap();
    let literal = dir.path().join("file:app.db?mode=rwc");
    seed_database(&literal, "CREATE TABLE users (id INTEGER PRIMARY KEY);");

    let migration = dir.path().join("001.sql");
    std::fs::write(&migration, "CREATE TABLE orders (id INTEGER PRIMARY KEY);").unwrap();

    let hash_before = sha256_of(&literal);
    let result = MigrationService::preview(&literal, &migration).expect("preview should run");
    let hash_after = sha256_of(&literal);

    assert!(result.success, "{:?}", result.error);
    assert!(has_change(
        &result,
        migradry_lib::SchemaChangeKind::TableAdded,
        "orders"
    ));
    assert_eq!(hash_before, hash_after);
    assert!(
        !dir.path().join("app.db").exists(),
        "the filename was reinterpreted as a URI"
    );
}

fn directory_listing(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}
