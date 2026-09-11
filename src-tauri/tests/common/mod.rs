//! Helpers shared by the integration tests.
//!
//! Everything a test asserts about the original database is computed *here*,
//! independently of the engine. A test never takes the engine's word for it:
//! `original_content_unchanged` being `true` is checked against a hash the test
//! computed itself, and schema claims are checked by querying the original
//! directly.

#![allow(dead_code)]

use rusqlite::{Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// A database file and a migration file in a throwaway directory.
pub struct Fixture {
    /// Kept alive so the directory outlives the test body.
    pub _dir: TempDir,
    pub database: PathBuf,
    pub migration: PathBuf,
}

/// Builds a database from `schema_sql` and a migration file from `migration_sql`.
pub fn fixture(schema_sql: &str, migration_sql: &str) -> Fixture {
    let dir = tempfile::tempdir().expect("temp dir");
    let database = dir.path().join("app.db");
    let migration = dir.path().join("001_change.sql");

    seed_database(&database, schema_sql);
    std::fs::write(&migration, migration_sql).expect("write migration");

    Fixture {
        _dir: dir,
        database,
        migration,
    }
}

/// Creates a database and runs `sql` against it.
pub fn seed_database(path: &Path, sql: &str) {
    let connection = Connection::open(path).expect("create database");
    if !sql.trim().is_empty() {
        connection.execute_batch(sql).expect("seed schema");
    }
    connection.close().expect("close seed connection");
}

/// SHA-256 of a file, computed by the test rather than by the engine.
pub fn sha256_of(path: &Path) -> String {
    let mut file = std::fs::File::open(path).expect("open for hashing");
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = file.read(&mut buffer).expect("read for hashing");
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Opens a database read-only. Test verification must never be able to alter
/// the very file it is checking.
pub fn open_readonly(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("open read-only")
}

pub fn table_names(path: &Path) -> Vec<String> {
    names_of(path, "table")
}

pub fn index_names(path: &Path) -> Vec<String> {
    names_of(path, "index")
}

pub fn view_names(path: &Path) -> Vec<String> {
    names_of(path, "view")
}

pub fn trigger_names(path: &Path) -> Vec<String> {
    names_of(path, "trigger")
}

fn names_of(path: &Path, object_type: &str) -> Vec<String> {
    let connection = open_readonly(path);
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = ?1 AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'
             ORDER BY name",
        )
        .expect("prepare catalogue query");
    let rows = statement
        .query_map([object_type], |row| row.get::<_, String>(0))
        .expect("query catalogue");
    rows.map(|row| row.expect("catalogue row")).collect()
}

pub fn column_names(path: &Path, table: &str) -> Vec<String> {
    let connection = open_readonly(path);
    let mut statement = connection
        .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
        .expect("prepare column query");
    let rows = statement
        .query_map([table], |row| row.get::<_, String>(0))
        .expect("query columns");
    rows.map(|row| row.expect("column row")).collect()
}

pub fn row_count(path: &Path, table: &str) -> i64 {
    let connection = open_readonly(path);
    connection
        .query_row(&format!("SELECT count(*) FROM \"{table}\""), [], |row| {
            row.get(0)
        })
        .expect("count rows")
}

/// Builds a preview workspace (baseline + working clone) for a database.
pub fn workspace(database: &Path) -> migradry_lib::temp::PreviewWorkspace {
    migradry_lib::temp::PreviewWorkspace::create(database).expect("workspace should be creatable")
}

/// Convenience wrapper: run a preview and require that it produced a result.
pub fn preview(fixture: &Fixture) -> migradry_lib::MigrationPreviewResult {
    migradry_lib::MigrationService::preview(&fixture.database, &fixture.migration)
        .expect("the preview itself should succeed")
}

/// The single `ColumnModified` change for a column, or a useful failure.
pub fn modification<'a>(
    result: &'a migradry_lib::MigrationPreviewResult,
    table: &str,
    column: &str,
) -> &'a migradry_lib::SchemaChange {
    let matches: Vec<&migradry_lib::SchemaChange> = result
        .schema_changes
        .iter()
        .filter(|change| {
            change.kind == migradry_lib::SchemaChangeKind::ColumnModified
                && change.object_name == column
                && change.parent_name.as_deref() == Some(table)
        })
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one modification of {table}.{column}, got {:#?}",
        result.schema_changes
    );
    matches[0]
}

/// The measured impact of a destructive change, as `(total_rows, affected_rows)`.
pub fn measured_impact(
    result: &migradry_lib::MigrationPreviewResult,
    table: &str,
    column: Option<&str>,
) -> (u64, u64) {
    for change in &result.schema_changes {
        if let Some(migradry_lib::DataImpact::Measured {
            table: measured_table,
            column: measured_column,
            total_rows,
            affected_rows,
        }) = &change.data_impact
        {
            if measured_table == table && measured_column.as_deref() == column {
                return (*total_rows, *affected_rows);
            }
        }
    }
    panic!(
        "no measured impact for {table}{}: {:#?}",
        column.map(|c| format!(".{c}")).unwrap_or_default(),
        result.schema_changes
    );
}

/// True when the change list contains this kind for this object.
pub fn has_change(
    result: &migradry_lib::MigrationPreviewResult,
    kind: migradry_lib::SchemaChangeKind,
    object: &str,
) -> bool {
    result
        .schema_changes
        .iter()
        .any(|change| change.kind == kind && change.object_name == object)
}

/// True when the change list contains this kind for this object on this parent.
pub fn has_child_change(
    result: &migradry_lib::MigrationPreviewResult,
    kind: migradry_lib::SchemaChangeKind,
    parent: &str,
    object: &str,
) -> bool {
    result.schema_changes.iter().any(|change| {
        change.kind == kind
            && change.object_name == object
            && change.parent_name.as_deref() == Some(parent)
    })
}
