//! Exact counts for what a destructive migration would discard.
//!
//! The numbers come from the **baseline** — the disposable snapshot the preview
//! was computed from. That is the point of having one: by the time the diff
//! knows a table is being dropped, the working clone no longer holds it, and
//! reading the original again would describe a later moment. Only the baseline
//! still has the exact state the preview is about.
//!
//! Every count here is a real `COUNT`, not an estimate.

mod common;

use common::*;
use migradry_lib::{DataImpact, MigrationService, SchemaChange, SchemaChangeKind};
use rusqlite::Connection;

/// Inserts `count` rows into a single-column table using a recursive CTE.
fn fill(table: &str, count: u32) -> String {
    format!(
        "WITH RECURSIVE seq(n) AS (
             SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < {count}
         )
         INSERT INTO {table} (id) SELECT n FROM seq;"
    )
}

// ---------------------------------------------------------------------------
// Dropped tables
// ---------------------------------------------------------------------------

#[test]
fn dropping_a_populated_table_reports_every_row() {
    let fixture = fixture(
        &format!(
            "CREATE TABLE events (id INTEGER PRIMARY KEY);
             {}",
            fill("events", 123)
        ),
        "DROP TABLE events;",
    );

    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);
    let hash_after = sha256_of(&fixture.database);

    assert!(result.success, "{:?}", result.error);
    assert!(has_change(
        &result,
        SchemaChangeKind::TableRemoved,
        "events"
    ));
    assert_eq!(result.destructive_change_count, 1);
    assert_eq!(measured_impact(&result, "events", None), (123, 123));

    // The rows are still there, because nothing ran against the original.
    assert_eq!(hash_before, hash_after);
    assert_eq!(row_count(&fixture.database, "events"), 123);
}

#[test]
fn dropping_an_empty_table_is_still_destructive() {
    let fixture = fixture(
        "CREATE TABLE events (id INTEGER PRIMARY KEY);",
        "DROP TABLE events;",
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert_eq!(measured_impact(&result, "events", None), (0, 0));
    assert_eq!(
        result.destructive_change_count, 1,
        "an empty table is still a table being removed"
    );
}

/// The count could not have come from the working clone: by then the table is
/// gone from it.
#[test]
fn the_count_comes_from_the_baseline_not_the_migrated_clone() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    seed_database(
        &database,
        &format!(
            "CREATE TABLE events (id INTEGER PRIMARY KEY);
             {}",
            fill("events", 77)
        ),
    );

    let workspace = workspace(&database);
    let run = migradry_lib::migration::execute_on_clone(workspace.working(), "DROP TABLE events;")
        .unwrap();
    assert!(run.failure.is_none(), "{:?}", run.failure);

    // Gone from the working clone...
    let working = workspace.working().open_readonly().unwrap();
    assert!(working
        .query_row("SELECT count(*) FROM events", [], |row| row
            .get::<_, i64>(0))
        .is_err());

    // ...still exactly 77 in the baseline, which is where the count is taken.
    let mut changes = vec![SchemaChange::new(SchemaChangeKind::TableRemoved, "events")];
    let warnings = migradry_lib::impact::measure(workspace.baseline(), &mut changes);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        changes[0].data_impact,
        Some(DataImpact::Measured {
            table: "events".to_string(),
            column: None,
            total_rows: 77,
            affected_rows: 77,
        })
    );
}

// ---------------------------------------------------------------------------
// Dropped columns
// ---------------------------------------------------------------------------

/// 100 rows, 73 of which carry a value.
const USERS_WITH_NULLS: &str = "
    CREATE TABLE users (id INTEGER PRIMARY KEY, legacy_code TEXT);
    WITH RECURSIVE seq(n) AS (
        SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 100
    )
    INSERT INTO users (id, legacy_code)
        SELECT n, CASE WHEN n <= 73 THEN 'code-' || n ELSE NULL END FROM seq;
";

#[test]
fn dropping_a_populated_column_counts_only_the_rows_that_lose_something() {
    let fixture = fixture(
        USERS_WITH_NULLS,
        "ALTER TABLE users DROP COLUMN legacy_code;",
    );

    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);
    let hash_after = sha256_of(&fixture.database);

    assert!(result.success, "{:?}", result.error);
    assert!(has_child_change(
        &result,
        SchemaChangeKind::ColumnRemoved,
        "users",
        "legacy_code"
    ));
    assert_eq!(
        measured_impact(&result, "users", Some("legacy_code")),
        (100, 73)
    );
    assert_eq!(hash_before, hash_after);
    assert!(column_names(&fixture.database, "users").contains(&"legacy_code".to_string()));
}

/// The same answer when the column goes via a table rebuild rather than
/// `ALTER TABLE ... DROP COLUMN`.
#[test]
fn a_rebuild_that_drops_a_column_counts_the_same() {
    let fixture = fixture(
        USERS_WITH_NULLS,
        "CREATE TABLE users_new (id INTEGER PRIMARY KEY);
         INSERT INTO users_new (id) SELECT id FROM users;
         DROP TABLE users;
         ALTER TABLE users_new RENAME TO users;",
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        measured_impact(&result, "users", Some("legacy_code")),
        (100, 73)
    );
}

#[test]
fn dropping_an_all_null_column_reports_zero_affected_but_stays_destructive() {
    let fixture = fixture(
        &format!(
            "CREATE TABLE users (id INTEGER PRIMARY KEY);
             {}
             ALTER TABLE users ADD COLUMN unused TEXT;",
            fill("users", 40)
        ),
        "ALTER TABLE users DROP COLUMN unused;",
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    let (total, affected) = measured_impact(&result, "users", Some("unused"));
    assert_eq!(total, 40);
    assert_eq!(affected, 0);
    assert_eq!(
        result.destructive_change_count, 1,
        "an all-null column is still a column being removed"
    );
}

#[test]
fn dropping_a_column_from_an_empty_table_reports_zero_of_zero() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, legacy TEXT);",
        "ALTER TABLE users DROP COLUMN legacy;",
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert_eq!(measured_impact(&result, "users", Some("legacy")), (0, 0));
}

#[test]
fn several_destructive_changes_are_each_counted() {
    let fixture = fixture(
        &format!(
            "CREATE TABLE events (id INTEGER PRIMARY KEY);
             CREATE TABLE audit (id INTEGER PRIMARY KEY);
             {}
             {}
             {}",
            fill("events", 12),
            fill("audit", 5),
            USERS_WITH_NULLS
        ),
        "DROP TABLE events;
         DROP TABLE audit;
         ALTER TABLE users DROP COLUMN legacy_code;",
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.destructive_change_count, 3);
    assert_eq!(measured_impact(&result, "events", None), (12, 12));
    assert_eq!(measured_impact(&result, "audit", None), (5, 5));
    assert_eq!(
        measured_impact(&result, "users", Some("legacy_code")),
        (100, 73)
    );
}

// ---------------------------------------------------------------------------
// When impact is not measured
// ---------------------------------------------------------------------------

/// A migration that removes nothing must not read a single row.
#[test]
fn a_harmless_migration_carries_no_impact_at_all() {
    let fixture = fixture(
        &format!(
            "CREATE TABLE users (id INTEGER PRIMARY KEY);
             {}",
            fill("users", 500)
        ),
        "CREATE TABLE orders (id INTEGER PRIMARY KEY);
         CREATE INDEX idx_users_id ON users(id);",
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.destructive_change_count, 0);
    assert!(
        result
            .schema_changes
            .iter()
            .all(|change| change.data_impact.is_none()),
        "a non-destructive change must not carry an impact"
    );
    assert!(result
        .warnings
        .iter()
        .all(|warning| !warning.contains("Impact")));
}

/// Advisory changes never carry counts either.
#[test]
fn removing_a_support_object_carries_no_impact() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT);
         CREATE INDEX idx_users_email ON users(email);
         CREATE VIEW active AS SELECT id FROM users;",
        "DROP INDEX idx_users_email;
         DROP VIEW active;",
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.advisory_change_count, 2);
    assert!(result
        .schema_changes
        .iter()
        .all(|change| change.data_impact.is_none()));
}

/// A migration that stopped part-way has no settled answer to "how much would
/// this remove?", so MigraDry says nothing rather than guessing.
#[test]
fn a_failed_migration_omits_impact_and_explains_why() {
    let fixture = fixture(
        &format!(
            "CREATE TABLE events (id INTEGER PRIMARY KEY);
             CREATE TABLE users (id INTEGER PRIMARY KEY);
             {}",
            fill("events", 31)
        ),
        "DROP TABLE events;
         DROP TABLE definitely_not_here;",
    );

    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);
    let hash_after = sha256_of(&fixture.database);

    assert!(!result.success);
    assert!(has_change(
        &result,
        SchemaChangeKind::TableRemoved,
        "events"
    ));
    assert!(
        result
            .schema_changes
            .iter()
            .all(|change| change.data_impact.is_none()),
        "a migration that did not complete must not claim what it would remove"
    );
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("did not run to completion")),
        "the omission must be explained: {:?}",
        result.warnings
    );
    assert_eq!(hash_before, hash_after);
    assert_eq!(row_count(&fixture.database, "events"), 31);
}

/// A count that cannot be taken is reported as unavailable, never as zero.
#[test]
fn an_uncountable_object_is_reported_unavailable_rather_than_empty() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    seed_database(&database, "CREATE TABLE users (id INTEGER PRIMARY KEY);");
    let workspace = workspace(&database);

    // A change naming something the baseline does not contain.
    let mut changes = vec![SchemaChange::new(
        SchemaChangeKind::TableRemoved,
        "never_existed",
    )];
    let warnings = migradry_lib::impact::measure(workspace.baseline(), &mut changes);

    match changes[0].data_impact.as_ref().expect("an impact verdict") {
        DataImpact::Unavailable { table, reason, .. } => {
            assert_eq!(table, "never_existed");
            assert!(reason.contains("no such table"), "reason: {reason}");
        }
        other => panic!("a failed count must not become a measured zero: {other:?}"),
    }
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].starts_with("Impact could not be calculated for never_existed:"));
}

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Names come from SQLite's own catalogue and may contain anything SQLite
/// allows, which is a great deal more than `[A-Za-z0-9_]`.
#[test]
fn awkward_identifiers_are_counted_safely() {
    let weird_tables = [
        "table with spaces",
        "quote\"name",
        "select",
        "naïve_ünïcode_日本語",
        "a;DROP TABLE sentinel--",
        "with'apostrophe",
    ];

    let mut schema = String::from("CREATE TABLE sentinel (id INTEGER PRIMARY KEY);\n");
    let mut migration = String::new();
    for (index, name) in weird_tables.iter().enumerate() {
        let quoted = migradry_lib::impact::quote_identifier(name);
        schema.push_str(&format!(
            "CREATE TABLE {quoted} (id INTEGER PRIMARY KEY);\n"
        ));
        for row in 0..=index {
            schema.push_str(&format!("INSERT INTO {quoted} (id) VALUES ({row});\n"));
        }
        migration.push_str(&format!("DROP TABLE {quoted};\n"));
    }

    let fixture = fixture(&schema, &migration);
    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);
    let hash_after = sha256_of(&fixture.database);

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.destructive_change_count, weird_tables.len());
    for (index, name) in weird_tables.iter().enumerate() {
        let expected = index as u64 + 1;
        assert_eq!(
            measured_impact(&result, name, None),
            (expected, expected),
            "counting {name:?}"
        );
    }

    // Nothing injected: the sentinel survives and the original is untouched.
    assert_eq!(hash_before, hash_after);
    assert!(table_names(&fixture.database).contains(&"sentinel".to_string()));
}

#[test]
fn awkward_column_names_are_counted_safely() {
    for column in [
        "column with spaces",
        "quote\"col",
        "select",
        "ünïcode_列",
        "x\"; DROP TABLE users; --",
    ] {
        let quoted = migradry_lib::impact::quote_identifier(column);
        let fixture = fixture(
            &format!(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, {quoted} TEXT);
                 INSERT INTO users (id, {quoted}) VALUES (1, 'a'), (2, NULL), (3, 'c');"
            ),
            &format!("ALTER TABLE users DROP COLUMN {quoted};"),
        );

        let result = preview(&fixture);
        assert!(result.success, "{column:?}: {:?}", result.error);
        assert_eq!(
            measured_impact(&result, "users", Some(column)),
            (3, 2),
            "counting users.{column:?}"
        );
        assert_eq!(row_count(&fixture.database, "users"), 3);
    }
}

// ---------------------------------------------------------------------------
// Baseline immutability
// ---------------------------------------------------------------------------

/// The baseline is byte-identical from creation to teardown, across a
/// destructive migration and every count taken from it.
#[test]
fn the_baseline_is_never_written_during_a_preview() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    seed_database(
        &database,
        &format!(
            "CREATE TABLE events (id INTEGER PRIMARY KEY);
             {}
             {}",
            fill("events", 64),
            USERS_WITH_NULLS
        ),
    );

    let original_before = sha256_of(&database);
    let workspace = workspace(&database);
    let baseline_before = migradry_lib::database::fingerprint(workspace.baseline().path()).unwrap();

    let schema_before =
        migradry_lib::schema::snapshot(&workspace.baseline().open_readonly().unwrap()).unwrap();
    let run = migradry_lib::migration::execute_on_clone(
        workspace.working(),
        "DROP TABLE events;
         ALTER TABLE users DROP COLUMN legacy_code;",
    )
    .unwrap();
    assert!(run.failure.is_none(), "{:?}", run.failure);
    let schema_after =
        migradry_lib::schema::snapshot(&workspace.working().open_readonly().unwrap()).unwrap();

    let mut changes = migradry_lib::diff::diff(&schema_before, &schema_after);
    let warnings = migradry_lib::impact::measure(workspace.baseline(), &mut changes);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(changes.len(), 2);

    let baseline_after = migradry_lib::database::fingerprint(workspace.baseline().path()).unwrap();
    assert!(
        baseline_before.content_matches(&baseline_after),
        "the baseline changed during the preview"
    );

    let baseline_path = workspace.baseline().path().to_path_buf();
    drop(workspace);
    assert!(!baseline_path.exists());
    assert_eq!(original_before, sha256_of(&database));
    assert_eq!(row_count(&database, "events"), 64);
}

/// The same guarantee through the public command, where the engine performs the
/// check itself on every run.
#[test]
fn a_full_preview_leaves_both_the_baseline_and_the_original_alone() {
    let fixture = fixture(
        &format!(
            "CREATE TABLE events (id INTEGER PRIMARY KEY);
             {}",
            fill("events", 9)
        ),
        "DROP TABLE events;",
    );

    let hash_before = sha256_of(&fixture.database);
    let result = MigrationService::preview(&fixture.database, &fixture.migration)
        .expect("the baseline check must not trip on a normal preview");
    let hash_after = sha256_of(&fixture.database);

    assert!(result.success, "{:?}", result.error);
    assert!(result.original_content_unchanged);
    assert_eq!(measured_impact(&result, "events", None), (9, 9));
    assert_eq!(hash_before, hash_after);
}

/// Counting reads the baseline; it must not leave a sidecar or a journal there.
#[test]
fn counting_leaves_the_workspace_directory_as_it_found_it() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    seed_database(
        &database,
        &format!(
            "CREATE TABLE events (id INTEGER PRIMARY KEY);
             {}",
            fill("events", 20)
        ),
    );

    let workspace = workspace(&database);
    let listing = |path: &std::path::Path| {
        let mut names: Vec<String> = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    let before = listing(workspace.directory());

    let mut changes = vec![SchemaChange::new(SchemaChangeKind::TableRemoved, "events")];
    migradry_lib::impact::measure(workspace.baseline(), &mut changes);

    assert_eq!(before, listing(workspace.directory()));
}

/// A count taken twice gives the same answer, because the baseline does not move.
#[test]
fn counts_are_stable_across_repeated_previews() {
    let fixture = fixture(
        &format!(
            "CREATE TABLE events (id INTEGER PRIMARY KEY);
             {}",
            fill("events", 250)
        ),
        "DROP TABLE events;",
    );

    for _ in 0..3 {
        let result = preview(&fixture);
        assert_eq!(measured_impact(&result, "events", None), (250, 250));
    }
    assert_eq!(row_count(&fixture.database, "events"), 250);
}

/// A virtual table has no ordinary storage; counting it must still behave.
#[test]
fn a_dropped_virtual_table_is_handled_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    let connection = Connection::open(&database).unwrap();
    // fts5 may or may not be compiled in; either outcome is acceptable here.
    let created = connection
        .execute_batch("CREATE VIRTUAL TABLE docs USING fts5(body);")
        .is_ok();
    connection.close().unwrap();
    if !created {
        return;
    }

    let migration = dir.path().join("001.sql");
    std::fs::write(&migration, "DROP TABLE docs;").unwrap();

    let hash_before = sha256_of(&database);
    let result = MigrationService::preview(&database, &migration).expect("preview should run");
    let hash_after = sha256_of(&database);

    assert_eq!(hash_before, hash_after);
    // Whatever the counts say, they must be a verdict rather than a panic.
    for change in &result.schema_changes {
        if change.kind == SchemaChangeKind::TableRemoved {
            assert!(
                change.data_impact.is_some(),
                "a destructive change must carry a verdict"
            );
        }
    }
}
