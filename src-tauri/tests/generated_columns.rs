//! Generated columns must be visible.
//!
//! SQLite's `pragma_table_info` omits `GENERATED ALWAYS AS` columns entirely.
//! An engine built on it reports an empty diff for a migration that adds or
//! drops one — a silent omission, and the exact failure mode this program is
//! supposed to prevent. `pragma_table_xinfo` reports them, along with a flag
//! saying whether they are `VIRTUAL` or `STORED`.

mod common;

use common::*;
use migradry_lib::{ColumnKind, ColumnPropertyChange, SchemaChangeKind};

const ORDERS: &str = "
    CREATE TABLE orders (
        id INTEGER PRIMARY KEY,
        price REAL NOT NULL,
        qty INTEGER NOT NULL
    );
    INSERT INTO orders (id, price, qty) VALUES (1, 2.5, 4), (2, 1.0, 3);
";

/// The bug, stated directly: `pragma_table_info` cannot see these columns.
#[test]
fn generated_columns_appear_in_the_schema_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    seed_database(
        &database,
        "CREATE TABLE orders (
             id INTEGER PRIMARY KEY,
             price REAL NOT NULL,
             qty INTEGER NOT NULL,
             total_v REAL GENERATED ALWAYS AS (price * qty) VIRTUAL,
             total_s REAL GENERATED ALWAYS AS (price * qty) STORED
         );",
    );

    let workspace = workspace(&database);
    let snapshot =
        migradry_lib::schema::snapshot(&workspace.baseline().open_readonly().unwrap()).unwrap();
    let orders = snapshot.table("orders").expect("orders table");

    let names: Vec<&str> = orders.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["id", "price", "qty", "total_v", "total_s"],
        "generated columns were dropped from the snapshot"
    );

    let kinds: Vec<ColumnKind> = orders.columns.iter().map(|c| c.kind).collect();
    assert_eq!(
        kinds,
        vec![
            ColumnKind::Ordinary,
            ColumnKind::Ordinary,
            ColumnKind::Ordinary,
            ColumnKind::VirtualGenerated,
            ColumnKind::StoredGenerated,
        ]
    );
}

#[test]
fn adding_a_virtual_generated_column_is_reported() {
    let fixture = fixture(
        ORDERS,
        "ALTER TABLE orders ADD COLUMN total REAL GENERATED ALWAYS AS (price * qty) VIRTUAL;",
    );
    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert!(
        has_child_change(&result, SchemaChangeKind::ColumnAdded, "orders", "total"),
        "a migration that adds a generated column must not report an empty diff: {:#?}",
        result.schema_changes
    );
    assert_eq!(hash_before, sha256_of(&fixture.database));
}

/// SQLite refuses `ALTER TABLE ... ADD COLUMN ... STORED` outright, so a STORED
/// generated column always arrives through a table rebuild.
#[test]
fn adding_a_stored_generated_column_is_reported() {
    let fixture = fixture(
        ORDERS,
        "CREATE TABLE orders_new (
             id INTEGER PRIMARY KEY,
             price REAL NOT NULL,
             qty INTEGER NOT NULL,
             total REAL GENERATED ALWAYS AS (price * qty) STORED
         );
         INSERT INTO orders_new (id, price, qty) SELECT id, price, qty FROM orders;
         DROP TABLE orders;
         ALTER TABLE orders_new RENAME TO orders;",
    );
    let result = preview(&fixture);
    assert!(result.success, "{:?}", result.error);
    assert!(has_child_change(
        &result,
        SchemaChangeKind::ColumnAdded,
        "orders",
        "total"
    ));
}

/// And MigraDry reports SQLite's refusal faithfully rather than hiding it.
#[test]
fn sqlites_refusal_to_add_a_stored_column_is_surfaced() {
    let fixture = fixture(
        ORDERS,
        "ALTER TABLE orders ADD COLUMN total REAL GENERATED ALWAYS AS (price * qty) STORED;",
    );
    let result = preview(&fixture);
    assert!(!result.success);
    assert!(result
        .error
        .as_ref()
        .unwrap()
        .message
        .contains("cannot add a STORED column"));
}

#[test]
fn dropping_a_stored_generated_column_is_destructive_and_counted() {
    let fixture = fixture(
        "CREATE TABLE orders (
             id INTEGER PRIMARY KEY,
             price REAL NOT NULL,
             qty INTEGER NOT NULL,
             total REAL GENERATED ALWAYS AS (price * qty) STORED
         );
         INSERT INTO orders (id, price, qty) VALUES (1, 2.5, 4), (2, 1.0, 3);",
        "ALTER TABLE orders DROP COLUMN total;",
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert!(has_child_change(
        &result,
        SchemaChangeKind::ColumnRemoved,
        "orders",
        "total"
    ));
    // A STORED column occupies storage, so removing it really does discard data.
    assert_eq!(result.destructive_change_count, 1);
    assert_eq!(measured_impact(&result, "orders", Some("total")), (2, 2));
}

/// A `VIRTUAL` column stores nothing. Reporting "2 non-null values would be
/// removed" would be a false alarm about data that was never on disk.
#[test]
fn dropping_a_virtual_generated_column_is_advisory_and_uncounted() {
    let fixture = fixture(
        "CREATE TABLE orders (
             id INTEGER PRIMARY KEY,
             price REAL NOT NULL,
             qty INTEGER NOT NULL,
             total REAL GENERATED ALWAYS AS (price * qty) VIRTUAL
         );
         INSERT INTO orders (id, price, qty) VALUES (1, 2.5, 4), (2, 1.0, 3);",
        "ALTER TABLE orders DROP COLUMN total;",
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    let change = result
        .schema_changes
        .iter()
        .find(|c| c.kind == SchemaChangeKind::ColumnRemoved && c.object_name == "total")
        .expect("the removal must still be reported");
    assert_eq!(change.impact, migradry_lib::ChangeImpact::Advisory);
    assert!(
        change.data_impact.is_none(),
        "a virtual generated column stores nothing to count"
    );
    assert_eq!(result.destructive_change_count, 0);
    // Two advisory findings, both true: the column is gone, and the table's
    // definition text changed with it. The definition check is not suppressed
    // when column-level findings exist — a migration can change a generated
    // expression *and* add a column, and only reporting the column would put
    // the expression change back in the blind spot.
    assert_eq!(result.advisory_change_count, 2);
    assert!(has_change(
        &result,
        SchemaChangeKind::TableDefinitionChanged,
        "orders"
    ));
}

/// A rebuild that turns an ordinary column into a generated one.
#[test]
fn a_column_becoming_generated_is_reported_as_a_modification() {
    let fixture = fixture(
        "CREATE TABLE orders (
             id INTEGER PRIMARY KEY,
             price REAL NOT NULL,
             qty INTEGER NOT NULL,
             total REAL
         );
         INSERT INTO orders (id, price, qty, total) VALUES (1, 2.5, 4, 10.0);",
        "CREATE TABLE orders_new (
             id INTEGER PRIMARY KEY,
             price REAL NOT NULL,
             qty INTEGER NOT NULL,
             total REAL GENERATED ALWAYS AS (price * qty) STORED
         );
         INSERT INTO orders_new (id, price, qty) SELECT id, price, qty FROM orders;
         DROP TABLE orders;
         ALTER TABLE orders_new RENAME TO orders;",
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    let change = modification(&result, "orders", "total");
    assert!(
        change
            .property_changes
            .contains(&ColumnPropertyChange::Generated {
                before: ColumnKind::Ordinary,
                after: ColumnKind::StoredGenerated,
            }),
        "expected a Generated delta, got {:#?}",
        change.property_changes
    );
}

/// VIRTUAL to STORED changes where the data lives, and must not pass silently.
#[test]
fn moving_between_virtual_and_stored_is_reported() {
    let fixture = fixture(
        "CREATE TABLE orders (
             id INTEGER PRIMARY KEY,
             price REAL NOT NULL,
             qty INTEGER NOT NULL,
             total REAL GENERATED ALWAYS AS (price * qty) VIRTUAL
         );",
        "CREATE TABLE orders_new (
             id INTEGER PRIMARY KEY,
             price REAL NOT NULL,
             qty INTEGER NOT NULL,
             total REAL GENERATED ALWAYS AS (price * qty) STORED
         );
         INSERT INTO orders_new (id, price, qty) SELECT id, price, qty FROM orders;
         DROP TABLE orders;
         ALTER TABLE orders_new RENAME TO orders;",
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        modification(&result, "orders", "total").property_changes,
        vec![ColumnPropertyChange::Generated {
            before: ColumnKind::VirtualGenerated,
            after: ColumnKind::StoredGenerated,
        }]
    );
}

/// An unchanged generated column must not become a phantom finding now that it
/// is visible at all.
#[test]
fn an_unchanged_generated_column_produces_no_finding() {
    let schema = "CREATE TABLE orders (
             id INTEGER PRIMARY KEY,
             price REAL NOT NULL,
             qty INTEGER NOT NULL,
             total REAL GENERATED ALWAYS AS (price * qty) STORED
         );";
    let fixture = fixture(
        schema,
        "CREATE TABLE orders_new (
             id INTEGER PRIMARY KEY,
             price REAL NOT NULL,
             qty INTEGER NOT NULL,
             total REAL GENERATED ALWAYS AS (price * qty) STORED
         );
         INSERT INTO orders_new (id, price, qty) SELECT id, price, qty FROM orders;
         DROP TABLE orders;
         ALTER TABLE orders_new RENAME TO orders;",
    );
    let result = preview(&fixture);
    assert!(result.success, "{:?}", result.error);
    assert!(
        result.schema_changes.is_empty(),
        "{:#?}",
        result.schema_changes
    );
}

/// A virtual table's own hidden columns are module internals, not schema the
/// user wrote, and must not appear as columns.
#[test]
fn virtual_table_module_columns_are_not_reported_as_schema() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    let connection = rusqlite::Connection::open(&database).unwrap();
    let created = connection
        .execute_batch("CREATE VIRTUAL TABLE docs USING fts5(body);")
        .is_ok();
    connection.close().unwrap();
    if !created {
        return; // fts5 not compiled in; nothing to assert.
    }

    let workspace = workspace(&database);
    let snapshot =
        migradry_lib::schema::snapshot(&workspace.baseline().open_readonly().unwrap()).unwrap();
    if let Some(docs) = snapshot.table("docs") {
        let names: Vec<&str> = docs.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["body"],
            "fts5's own hidden columns leaked into the schema"
        );
    }
}

// ---------------------------------------------------------------------------
// Generated expression bodies
// ---------------------------------------------------------------------------
//
// `pragma_table_xinfo` reports nothing about the expression inside
// `GENERATED ALWAYS AS (...)`, so changing it leaves column metadata
// byte-identical. Before this was handled, such a migration reported zero
// changes — the user was told nothing had happened.

/// Rebuilds `t`, substituting a new definition for the generated column.
fn rebuild_t(new_columns: &str) -> String {
    format!(
        "CREATE TABLE t_new ({new_columns});
         INSERT INTO t_new (a) SELECT a FROM t;
         DROP TABLE t;
         ALTER TABLE t_new RENAME TO t;"
    )
}

#[test]
fn a_changed_stored_expression_is_not_reported_as_no_change() {
    let fixture = fixture(
        "CREATE TABLE t (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED);
         INSERT INTO t (a) VALUES (1), (2);",
        &rebuild_t("a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 3) STORED"),
    );

    let hash_before = sha256_of(&fixture.database);
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert!(
        !result.schema_changes.is_empty(),
        "a changed generated expression must never be reported as no change"
    );
    assert!(has_change(
        &result,
        SchemaChangeKind::TableDefinitionChanged,
        "t"
    ));
    assert_eq!(hash_before, sha256_of(&fixture.database));
}

#[test]
fn a_changed_virtual_expression_is_reported() {
    let fixture = fixture(
        "CREATE TABLE t (a INTEGER, b INTEGER GENERATED ALWAYS AS (a + 1) VIRTUAL);",
        &rebuild_t("a INTEGER, b INTEGER GENERATED ALWAYS AS (a + 100) VIRTUAL"),
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert!(has_change(
        &result,
        SchemaChangeKind::TableDefinitionChanged,
        "t"
    ));
}

/// The rebuild rewrites `CREATE TABLE t` as `CREATE TABLE "t"`, so a naive text
/// comparison would flag every rebuild. It must not.
#[test]
fn an_unchanged_generated_expression_produces_no_finding() {
    let fixture = fixture(
        "CREATE TABLE t (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED);
         INSERT INTO t (a) VALUES (1);",
        &rebuild_t("a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED"),
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert!(
        result.schema_changes.is_empty(),
        "a rebuild that changes nothing must report nothing: {:#?}",
        result.schema_changes
    );
}

/// Formatting-only rewrites are reported. This is deliberate.
///
/// The definition is compared verbatim, so re-spacing an expression produces an
/// advisory even though nothing about the schema changed. That is the
/// conservative direction: normalising whitespace away is what previously
/// erased it inside string literals and hid a real change. A visible false
/// advisory beats a silent miss.
#[test]
fn a_formatting_only_rewrite_is_conservatively_reported() {
    let fixture = fixture(
        "CREATE TABLE t (a INTEGER, b INTEGER GENERATED ALWAYS AS (a*2) STORED);",
        &rebuild_t("a INTEGER, b INTEGER GENERATED ALWAYS AS ( a * 2 ) STORED"),
    );
    let result = preview(&fixture);
    assert!(result.success, "{:?}", result.error);
    assert!(has_change(
        &result,
        SchemaChangeKind::TableDefinitionChanged,
        "t"
    ));
}

/// And it is deterministic: the same pair of definitions always agrees.
#[test]
fn the_definition_comparison_is_deterministic() {
    let fixture = fixture(
        "CREATE TABLE t (a INTEGER, b INTEGER GENERATED ALWAYS AS (a*2) STORED);",
        &rebuild_t("a INTEGER, b INTEGER GENERATED ALWAYS AS ( a * 2 ) STORED"),
    );
    let first = preview(&fixture);
    let second = preview(&fixture);
    assert_eq!(first.schema_changes, second.schema_changes);
}

// --- whitespace inside literals ------------------------------------------
//
// The reported bug: normalisation stripped whitespace everywhere, including
// inside string literals, so `'a b'` and `'ab'` compared equal and a
// semantically different generated expression produced no finding at all.

#[test]
fn whitespace_inside_a_virtual_expression_literal_is_a_change() {
    let fixture = fixture(
        "CREATE TABLE t (a TEXT, b TEXT GENERATED ALWAYS AS ('a b') VIRTUAL);",
        "CREATE TABLE t_new (a TEXT, b TEXT GENERATED ALWAYS AS ('ab') VIRTUAL);
         INSERT INTO t_new (a) SELECT a FROM t;
         DROP TABLE t;
         ALTER TABLE t_new RENAME TO t;",
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert!(
        has_change(&result, SchemaChangeKind::TableDefinitionChanged, "t"),
        "'a b' and 'ab' are different expressions: {:#?}",
        result.schema_changes
    );
}

#[test]
fn whitespace_inside_a_stored_expression_literal_is_a_change() {
    let fixture = fixture(
        "CREATE TABLE t (a TEXT, b TEXT GENERATED ALWAYS AS ('a b') STORED);",
        "CREATE TABLE t_new (a TEXT, b TEXT GENERATED ALWAYS AS ('ab') STORED);
         INSERT INTO t_new (a) SELECT a FROM t;
         DROP TABLE t;
         ALTER TABLE t_new RENAME TO t;",
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert!(
        has_change(&result, SchemaChangeKind::TableDefinitionChanged, "t"),
        "'a b' and 'ab' are different expressions: {:#?}",
        result.schema_changes
    );
}

/// A quoted identifier may legally contain a parenthesis. The body must be
/// located past it, not inside it.
#[test]
fn a_parenthesis_in_a_quoted_table_name_does_not_confuse_the_comparison() {
    let schema = "CREATE TABLE \"example(test)\" (
             a INTEGER,
             b INTEGER GENERATED ALWAYS AS (a * 2) STORED
         );";

    // Unchanged definition: no finding, despite the parenthesis in the name.
    let unchanged = fixture(
        schema,
        "CREATE TABLE tmp (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED);
         DROP TABLE tmp;",
    );
    let result = preview(&unchanged);
    assert!(result.success, "{:?}", result.error);
    assert!(
        !has_change(
            &result,
            SchemaChangeKind::TableDefinitionChanged,
            "example(test)"
        ),
        "an untouched table must not be flagged: {:#?}",
        result.schema_changes
    );

    // Changed expression on the same awkwardly-named table: reported.
    let changed = fixture(
        schema,
        "CREATE TABLE t_new (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 3) STORED);
         INSERT INTO t_new (a) SELECT a FROM \"example(test)\";
         DROP TABLE \"example(test)\";
         ALTER TABLE t_new RENAME TO \"example(test)\";",
    );
    let result = preview(&changed);
    assert!(result.success, "{:?}", result.error);
    assert!(
        has_change(
            &result,
            SchemaChangeKind::TableDefinitionChanged,
            "example(test)"
        ),
        "the real body was not located: {:#?}",
        result.schema_changes
    );
}

/// The definition check is scoped to tables with generated columns, so ordinary
/// tables gain no new findings.
#[test]
fn ordinary_tables_are_not_definition_compared() {
    let fixture = fixture(
        "CREATE TABLE plain (a INTEGER, b TEXT);
         INSERT INTO plain (a, b) VALUES (1, 'x');",
        "CREATE TABLE plain_new (a INTEGER, b TEXT);
         INSERT INTO plain_new (a, b) SELECT a, b FROM plain;
         DROP TABLE plain;
         ALTER TABLE plain_new RENAME TO plain;",
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert!(
        result.schema_changes.is_empty(),
        "a rebuilt ordinary table must not report a definition change: {:#?}",
        result.schema_changes
    );
}

/// A definition change is advisory and carries no data-impact count.
#[test]
fn a_definition_change_is_advisory_and_uncounted() {
    let fixture = fixture(
        "CREATE TABLE t (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED);
         INSERT INTO t (a) VALUES (1), (2), (3);",
        &rebuild_t("a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 9) STORED"),
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    let change = result
        .schema_changes
        .iter()
        .find(|c| c.kind == SchemaChangeKind::TableDefinitionChanged)
        .expect("a definition change");
    assert_eq!(change.impact, migradry_lib::ChangeImpact::Advisory);
    assert!(change.data_impact.is_none());
    assert_eq!(result.destructive_change_count, 0);
}

/// Ordinary column changes on a generated-column table still work, and the
/// definition advisory sits alongside them rather than replacing them.
#[test]
fn column_findings_still_appear_alongside_a_definition_change() {
    let fixture = fixture(
        "CREATE TABLE t (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED);",
        &rebuild_t("a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 3) STORED, c TEXT"),
    );
    let result = preview(&fixture);

    assert!(result.success, "{:?}", result.error);
    assert!(has_child_change(
        &result,
        SchemaChangeKind::ColumnAdded,
        "t",
        "c"
    ));
    assert!(has_change(
        &result,
        SchemaChangeKind::TableDefinitionChanged,
        "t"
    ));
}
