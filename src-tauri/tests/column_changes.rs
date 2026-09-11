//! Changes to columns that survive the migration.
//!
//! SQLite has no `ALTER COLUMN`, so a column's type, nullability or default is
//! almost always changed by rebuilding the table: create a replacement, copy
//! the rows across, drop the original and rename the replacement into its
//! place. The final table keeps the old name, so MigraDry compares the baseline
//! `users` with the final `users` and the metadata differences fall out. No
//! rename inference is involved, and none is needed.

mod common;

use common::*;
use migradry_lib::{ColumnPropertyChange, SchemaChangeKind};

/// The rebuild recipe, parameterised by the new column definition.
fn rebuild_users(new_columns: &str, copied: &str) -> String {
    format!(
        "CREATE TABLE users_new ({new_columns});
         INSERT INTO users_new ({copied}) SELECT {copied} FROM users;
         DROP TABLE users;
         ALTER TABLE users_new RENAME TO users;"
    )
}

#[test]
fn a_changed_declared_type_is_reported_with_both_declarations() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, value TEXT);
         INSERT INTO users (value) VALUES ('1'), ('2');",
        &rebuild_users("id INTEGER PRIMARY KEY, value INTEGER", "id, value"),
    );

    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);
    let hash_after = sha256_of(&fixture.database);

    assert!(result.success, "{:?}", result.error);
    let change = modification(&result, "users", "value");
    assert_eq!(
        change.property_changes,
        vec![ColumnPropertyChange::DeclaredType {
            before: "TEXT".to_string(),
            after: "INTEGER".to_string(),
        }]
    );
    // A metadata change is not data loss.
    assert_eq!(result.destructive_change_count, 0);
    assert!(change.data_impact.is_none());
    assert_eq!(hash_before, hash_after);
}

#[test]
fn a_column_becoming_not_null_is_reported() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users (name) VALUES ('ada');",
        &rebuild_users("id INTEGER PRIMARY KEY, name TEXT NOT NULL", "id, name"),
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        modification(&result, "users", "name").property_changes,
        vec![ColumnPropertyChange::NotNull {
            before: false,
            after: true,
        }]
    );
}

#[test]
fn a_column_becoming_nullable_is_reported() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO users (name) VALUES ('ada');",
        &rebuild_users("id INTEGER PRIMARY KEY, name TEXT", "id, name"),
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        modification(&result, "users", "name").property_changes,
        vec![ColumnPropertyChange::NotNull {
            before: true,
            after: false,
        }]
    );
}

#[test]
fn a_new_default_is_reported() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, status TEXT);",
        &rebuild_users(
            "id INTEGER PRIMARY KEY, status TEXT DEFAULT 'active'",
            "id, status",
        ),
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        modification(&result, "users", "status").property_changes,
        vec![ColumnPropertyChange::DefaultValue {
            before: None,
            after: Some("'active'".to_string()),
        }]
    );
}

#[test]
fn a_changed_default_is_reported() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, status TEXT DEFAULT 'active');",
        &rebuild_users(
            "id INTEGER PRIMARY KEY, status TEXT DEFAULT 'pending'",
            "id, status",
        ),
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        modification(&result, "users", "status").property_changes,
        vec![ColumnPropertyChange::DefaultValue {
            before: Some("'active'".to_string()),
            after: Some("'pending'".to_string()),
        }]
    );
}

#[test]
fn a_changed_primary_key_position_is_reported() {
    let fixture = fixture(
        "CREATE TABLE memberships (
             user_id INTEGER NOT NULL,
             group_id INTEGER NOT NULL,
             PRIMARY KEY (user_id, group_id)
         );",
        "CREATE TABLE memberships_new (
             user_id INTEGER NOT NULL,
             group_id INTEGER NOT NULL,
             PRIMARY KEY (group_id, user_id)
         );
         INSERT INTO memberships_new (user_id, group_id)
             SELECT user_id, group_id FROM memberships;
         DROP TABLE memberships;
         ALTER TABLE memberships_new RENAME TO memberships;",
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        modification(&result, "memberships", "user_id").property_changes,
        vec![ColumnPropertyChange::PrimaryKeyPosition {
            before: 1,
            after: 2,
        }]
    );
    assert_eq!(
        modification(&result, "memberships", "group_id").property_changes,
        vec![ColumnPropertyChange::PrimaryKeyPosition {
            before: 2,
            after: 1,
        }]
    );
}

/// One column changing three ways is one finding, not three.
#[test]
fn several_property_changes_arrive_as_one_modification() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, status TEXT);",
        &rebuild_users(
            "id INTEGER PRIMARY KEY, status VARCHAR(32) NOT NULL DEFAULT 'active'",
            "id, status",
        ),
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    let change = modification(&result, "users", "status");
    assert_eq!(
        change.property_changes,
        vec![
            ColumnPropertyChange::DeclaredType {
                before: "TEXT".to_string(),
                after: "VARCHAR(32)".to_string(),
            },
            ColumnPropertyChange::NotNull {
                before: false,
                after: true,
            },
            ColumnPropertyChange::DefaultValue {
                before: None,
                after: Some("'active'".to_string()),
            },
        ],
        "properties must be one change carrying three deltas, in a fixed order"
    );

    // And exactly one top-level change for that column.
    assert_eq!(
        result
            .schema_changes
            .iter()
            .filter(|c| c.object_name == "status")
            .count(),
        1
    );
}

#[test]
fn an_untouched_column_produces_no_finding() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL DEFAULT 'anon');
         INSERT INTO users (name) VALUES ('ada');",
        &rebuild_users(
            "id INTEGER PRIMARY KEY, name TEXT NOT NULL DEFAULT 'anon'",
            "id, name",
        ),
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert!(
        result.schema_changes.is_empty(),
        "a rebuild that changes nothing must report nothing: {:#?}",
        result.schema_changes
    );
}

/// Case and whitespace in a declared type are how it was typed, not what it
/// means, so they are normalised away before comparing.
#[test]
fn declared_type_spelling_is_not_a_change() {
    for (before, after) in [
        ("text", "TEXT"),
        ("TEXT", "text"),
        ("VarChar(255)", "VARCHAR(255)"),
        ("VARCHAR (255)", "VARCHAR(255)"),
        ("UNSIGNED BIG INT", "unsigned big int"),
    ] {
        let fixture = fixture(
            &format!("CREATE TABLE users (id INTEGER PRIMARY KEY, value {before});"),
            &rebuild_users(
                &format!("id INTEGER PRIMARY KEY, value {after}"),
                "id, value",
            ),
        );
        let result = preview(&fixture);
        assert!(result.success, "{:?}", result.error);
        assert!(
            result.schema_changes.is_empty(),
            "{before:?} -> {after:?} was reported as a change: {:#?}",
            result.schema_changes
        );
    }
}

/// Two declarations with the same SQLite affinity are still two declarations.
/// MigraDry reports what the migration did; it does not decide that a change
/// was unimportant.
#[test]
fn an_affinity_preserving_type_change_is_still_reported() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, value TEXT);",
        &rebuild_users("id INTEGER PRIMARY KEY, value VARCHAR(255)", "id, value"),
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        modification(&result, "users", "value").property_changes,
        vec![ColumnPropertyChange::DeclaredType {
            before: "TEXT".to_string(),
            after: "VARCHAR(255)".to_string(),
        }]
    );
}

/// A modification is advisory, not destructive: the rows are still there.
#[test]
fn modifications_are_advisory_rather_than_destructive() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, value TEXT);
         INSERT INTO users (value) VALUES ('a'), ('b');",
        &rebuild_users("id INTEGER PRIMARY KEY, value INTEGER", "id, value"),
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.destructive_change_count, 0);
    assert_eq!(result.advisory_change_count, 1);
    assert_eq!(
        modification(&result, "users", "value").impact,
        migradry_lib::ChangeImpact::Advisory
    );
}

/// A rebuild that adds, drops and alters columns reports all three, each once.
#[test]
fn additions_removals_and_modifications_coexist() {
    let fixture = fixture(
        "CREATE TABLE users (
             id INTEGER PRIMARY KEY,
             legacy TEXT,
             status TEXT
         );
         INSERT INTO users (legacy, status) VALUES ('x', 'active');",
        &rebuild_users(
            "id INTEGER PRIMARY KEY, status TEXT NOT NULL DEFAULT 'active', email TEXT",
            "id, status",
        ),
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert!(has_child_change(
        &result,
        SchemaChangeKind::ColumnAdded,
        "users",
        "email"
    ));
    assert!(has_child_change(
        &result,
        SchemaChangeKind::ColumnRemoved,
        "users",
        "legacy"
    ));
    assert_eq!(
        modification(&result, "users", "status")
            .property_changes
            .len(),
        2
    );
    assert_eq!(result.schema_changes.len(), 3);
}

/// The modern one-statement form must be seen just as clearly as a rebuild.
#[test]
fn a_direct_alter_table_add_column_is_not_a_modification() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);",
        "ALTER TABLE users ADD COLUMN email TEXT;",
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.schema_changes.len(), 1);
    assert_eq!(result.schema_changes[0].kind, SchemaChangeKind::ColumnAdded);
}

/// Deterministic ordering survives the new change kind.
#[test]
fn modifications_sort_deterministically() {
    let fixture = fixture(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, zeta TEXT, alpha TEXT, gone TEXT);
         INSERT INTO users (zeta, alpha, gone) VALUES ('z', 'a', 'g');",
        &rebuild_users(
            "id INTEGER PRIMARY KEY, zeta INTEGER, alpha INTEGER, added TEXT",
            "id, zeta, alpha",
        ),
    );

    let first = preview(&fixture);
    let second = preview(&fixture);
    assert_eq!(first.schema_changes, second.schema_changes);

    let order: Vec<(SchemaChangeKind, &str)> = first
        .schema_changes
        .iter()
        .map(|change| (change.kind, change.object_name.as_str()))
        .collect();
    assert_eq!(
        order,
        vec![
            (SchemaChangeKind::ColumnAdded, "added"),
            (SchemaChangeKind::ColumnRemoved, "gone"),
            (SchemaChangeKind::ColumnModified, "alpha"),
            (SchemaChangeKind::ColumnModified, "zeta"),
        ]
    );
}
