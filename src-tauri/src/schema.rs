//! Deterministic schema snapshots read from SQLite's own catalogue.
//!
//! Nothing here parses SQL. Every fact comes from `sqlite_master` and from the
//! `pragma_table_info` table-valued function, which means MigraDry always
//! agrees with SQLite about what a schema contains.

use crate::error::{MigraDryError, Result};
use crate::models::{ColumnInfo, ColumnKind, SchemaObject, SchemaObjectKind, SchemaSnapshot};
use rusqlite::Connection;

/// Objects whose names begin with `sqlite_` are SQLite's own bookkeeping —
/// `sqlite_sequence`, `sqlite_stat1`, the `sqlite_autoindex_*` indexes created
/// implicitly by UNIQUE and PRIMARY KEY constraints. They are excluded so the
/// diff reports what the migration author wrote, not what SQLite maintains
/// underneath.
const CATALOGUE_QUERY: &str = "
    SELECT type, name, tbl_name, sql
    FROM sqlite_master
    WHERE type IN ('table', 'index', 'view', 'trigger')
      AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'
";

/// `pragma_table_xinfo` is used as a table-valued function so the table name is
/// a bound parameter. A `PRAGMA table_xinfo(...)` statement would require
/// splicing the name into SQL text.
///
/// `xinfo` rather than `info`: `pragma_table_info` omits generated columns
/// entirely. A migration that added or dropped a `GENERATED ALWAYS AS` column
/// would produce an empty diff, which is precisely the sort of silent omission
/// this program exists to avoid. `xinfo` adds a `hidden` flag that distinguishes
/// ordinary columns from `VIRTUAL` and `STORED` generated ones — and from a
/// virtual-table module's own hidden columns, which are dropped.
const COLUMN_QUERY: &str = "
    SELECT name, type, \"notnull\", dflt_value, pk, hidden
    FROM pragma_table_xinfo(?1)
    ORDER BY cid
";

/// Reads the complete schema of an open connection.
///
/// Ordering is fully deterministic: objects are sorted by kind (tables,
/// indexes, views, triggers) and then by name, and columns keep their
/// declaration order. Two snapshots of the same schema are always equal.
pub fn snapshot(connection: &Connection) -> Result<SchemaSnapshot> {
    let mut objects = read_catalogue(connection).map_err(schema_error)?;

    objects.sort_by(|a, b| {
        a.object_type
            .cmp(&b.object_type)
            .then_with(|| a.name.cmp(&b.name))
    });

    for object in &mut objects {
        if object.object_type == SchemaObjectKind::Table {
            object.columns = read_columns(connection, &object.name).map_err(schema_error)?;
        }
    }

    Ok(SchemaSnapshot { objects })
}

fn read_catalogue(connection: &Connection) -> rusqlite::Result<Vec<SchemaObject>> {
    let mut statement = connection.prepare(CATALOGUE_QUERY)?;
    let rows = statement.query_map([], |row| {
        let object_type: String = row.get(0)?;
        let name: String = row.get(1)?;
        let table_name: Option<String> = row.get(2)?;
        let sql: Option<String> = row.get(3)?;
        Ok((object_type, name, table_name, sql))
    })?;

    let mut objects = Vec::new();
    for row in rows {
        let (object_type, name, table_name, sql) = row?;
        // `type` is constrained by the query, so an unknown value cannot occur;
        // skipping rather than failing keeps the snapshot total.
        let Some(object_type) = SchemaObjectKind::from_sqlite_type(&object_type) else {
            continue;
        };
        objects.push(SchemaObject {
            object_type,
            name,
            table_name,
            sql,
            columns: Vec::new(),
        });
    }
    Ok(objects)
}

fn read_columns(connection: &Connection, table: &str) -> rusqlite::Result<Vec<ColumnInfo>> {
    let mut statement = connection.prepare(COLUMN_QUERY)?;
    let rows = statement.query_map([table], |row| {
        let kind = ColumnKind::from_hidden_flag(row.get::<_, i64>(5)?);
        Ok(kind.map(|kind| ColumnInfo {
            name: row.get_unwrap(0),
            // A column declared without a type has an empty string here, which
            // is exactly what SQLite reports. No inference is attempted.
            declared_type: row
                .get::<_, Option<String>>(1)
                .unwrap_or_default()
                .unwrap_or_default(),
            not_null: row.get_unwrap::<_, i64>(2) != 0,
            default_value: row.get_unwrap(3),
            primary_key_position: row.get_unwrap::<_, i64>(4) as i32,
            kind,
        }))
    })?;
    // `None` is a virtual-table module's own hidden column; those are skipped.
    rows.filter_map(|row| row.transpose()).collect()
}

fn schema_error(error: rusqlite::Error) -> MigraDryError {
    MigraDryError::SchemaReadFailed {
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection_with(sql: &str) -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(sql).unwrap();
        connection
    }

    #[test]
    fn captures_tables_indexes_views_and_triggers() {
        let connection = connection_with(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
             CREATE INDEX idx_users_name ON users(name);
             CREATE VIEW active_users AS SELECT * FROM users;
             CREATE TRIGGER users_touch AFTER UPDATE ON users
               BEGIN SELECT 1; END;",
        );
        let snapshot = snapshot(&connection).unwrap();

        let kinds: Vec<_> = snapshot
            .objects
            .iter()
            .map(|o| (o.object_type, o.name.as_str()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (SchemaObjectKind::Table, "users"),
                (SchemaObjectKind::Index, "idx_users_name"),
                (SchemaObjectKind::View, "active_users"),
                (SchemaObjectKind::Trigger, "users_touch"),
            ]
        );
    }

    #[test]
    fn captures_column_metadata_in_declaration_order() {
        let connection = connection_with(
            "CREATE TABLE users (
                 id INTEGER PRIMARY KEY,
                 name TEXT NOT NULL,
                 nickname TEXT DEFAULT 'anon',
                 untyped
             );",
        );
        let snapshot = snapshot(&connection).unwrap();
        let users = snapshot.table("users").unwrap();

        let names: Vec<_> = users.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "name", "nickname", "untyped"]);

        assert_eq!(users.columns[0].declared_type, "INTEGER");
        assert_eq!(users.columns[0].primary_key_position, 1);
        assert!(users.columns[1].not_null);
        assert_eq!(users.columns[1].primary_key_position, 0);
        assert_eq!(users.columns[2].default_value.as_deref(), Some("'anon'"));
        assert_eq!(users.columns[3].declared_type, "");
    }

    #[test]
    fn records_composite_primary_key_positions() {
        let connection = connection_with(
            "CREATE TABLE memberships (
                 user_id INTEGER NOT NULL,
                 group_id INTEGER NOT NULL,
                 PRIMARY KEY (group_id, user_id)
             );",
        );
        let snapshot = snapshot(&connection).unwrap();
        let table = snapshot.table("memberships").unwrap();
        assert_eq!(table.columns[0].primary_key_position, 2);
        assert_eq!(table.columns[1].primary_key_position, 1);
    }

    #[test]
    fn excludes_sqlite_internal_objects() {
        let connection = connection_with(
            "CREATE TABLE counters (id INTEGER PRIMARY KEY AUTOINCREMENT, label TEXT UNIQUE);
             INSERT INTO counters (label) VALUES ('a');",
        );
        let snapshot = snapshot(&connection).unwrap();
        assert!(snapshot
            .objects
            .iter()
            .all(|o| !o.name.starts_with("sqlite_")));
        assert!(snapshot.table("sqlite_sequence").is_none());
    }

    #[test]
    fn ordering_is_stable_regardless_of_creation_order() {
        let forwards = connection_with(
            "CREATE TABLE alpha (id INTEGER);
             CREATE TABLE beta (id INTEGER);
             CREATE INDEX idx_a ON alpha(id);
             CREATE INDEX idx_b ON beta(id);",
        );
        let backwards = connection_with(
            "CREATE TABLE beta (id INTEGER);
             CREATE INDEX idx_b ON beta(id);
             CREATE TABLE alpha (id INTEGER);
             CREATE INDEX idx_a ON alpha(id);",
        );
        assert_eq!(snapshot(&forwards).unwrap(), snapshot(&backwards).unwrap());
    }

    #[test]
    fn an_empty_database_snapshots_to_nothing() {
        let connection = Connection::open_in_memory().unwrap();
        assert!(snapshot(&connection).unwrap().objects.is_empty());
    }
}
