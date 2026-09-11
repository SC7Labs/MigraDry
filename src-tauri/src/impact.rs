//! Exact counts for the data a destructive migration would discard.
//!
//! # Where the numbers come from
//!
//! Every count is a `COUNT` run against the **baseline** — the disposable
//! snapshot the preview was computed from — and never against the original
//! database. That distinction is the whole reason the baseline exists. Counting
//! the original after the migration would answer a question nobody asked: it
//! describes a later moment, and the source may have changed in between.
//!
//! # When the counts are taken
//!
//! Only for changes that actually discard data, and only once the diff has
//! identified which ones those are. A migration that adds a column touches no
//! rows and must not provoke a full table scan of the database, so a preview
//! with nothing destructive in it never opens the baseline here at all.
//!
//! # What is never done
//!
//! A count that fails is reported as unavailable. It is never quietly reported
//! as zero: "nothing would be lost" and "MigraDry could not find out" are
//! different answers, and only one of them is safe to act on.

use crate::models::{DataImpact, SchemaChange};
use crate::temp::BaselineSnapshot;
use rusqlite::Connection;

/// Quotes an identifier for use in SQL.
///
/// SQLite will not accept a table or column name as a bound parameter — there
/// is no `SELECT count(*) FROM ?` — so the name has to be written into the
/// statement, and it has to be written safely.
///
/// Double quotes are SQLite's standard identifier delimiter, and an embedded
/// double quote is escaped by doubling it, so `foo"bar` becomes `"foo""bar"`.
/// That is the entire rule: SQLite accepts almost anything else between the
/// quotes, including spaces, punctuation, reserved words and any Unicode, so
/// there is nothing here that tries to validate a name against a character set.
/// A pattern like `[A-Za-z0-9_]` would reject perfectly legal schemas.
///
/// Names reaching this function come from SQLite's own catalogue by way of a
/// schema snapshot, never from anything the user typed into the interface.
pub fn quote_identifier(name: &str) -> String {
    let mut quoted = String::with_capacity(name.len() + 2);
    quoted.push('"');
    for character in name.chars() {
        if character == '"' {
            quoted.push('"');
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}

/// Attaches an exact [`DataImpact`] to every change that discards data.
///
/// Returns a warning for each impact that could not be measured. The changes
/// themselves are always left in a usable state: an unmeasurable count becomes
/// [`DataImpact::Unavailable`], never a fabricated zero.
pub fn measure(baseline: &BaselineSnapshot, changes: &mut [SchemaChange]) -> Vec<String> {
    let destructive: Vec<usize> = changes
        .iter()
        .enumerate()
        .filter(|(_, change)| change.data_target().is_some())
        .map(|(index, _)| index)
        .collect();

    // Nothing to count means nothing to open. A harmless migration never causes
    // a single row to be read.
    if destructive.is_empty() {
        return Vec::new();
    }

    let mut warnings = Vec::new();
    let connection = match baseline.open_readonly() {
        Ok(connection) => connection,
        Err(error) => {
            // The baseline was readable a moment ago, when the "before" schema
            // came out of it, so this is unexpected — which is exactly why it
            // is reported rather than absorbed.
            for index in destructive {
                let (table, column) = describe(&changes[index]);
                changes[index].data_impact = Some(DataImpact::Unavailable {
                    table: table.clone(),
                    column: column.clone(),
                    reason: error.to_string(),
                });
                warnings.push(unavailable_warning(
                    &table,
                    column.as_deref(),
                    &error.to_string(),
                ));
            }
            return warnings;
        }
    };

    for index in destructive {
        let Some((table, column)) = changes[index].data_target() else {
            continue;
        };
        let table = table.to_string();
        let column = column.map(str::to_string);

        match count(&connection, &table, column.as_deref()) {
            Ok((total_rows, affected_rows)) => {
                changes[index].data_impact = Some(DataImpact::Measured {
                    table,
                    column,
                    total_rows,
                    affected_rows,
                });
            }
            Err(reason) => {
                warnings.push(unavailable_warning(&table, column.as_deref(), &reason));
                changes[index].data_impact = Some(DataImpact::Unavailable {
                    table,
                    column,
                    reason,
                });
            }
        }
    }

    warnings
}

/// Counts rows, and non-null values in `column` when one is given.
///
/// For a dropped table every row is affected, so both numbers are the row
/// count. For a dropped column only the rows holding a value lose anything,
/// which is what `count(column)` measures — SQLite's `count(x)` ignores nulls.
fn count(
    connection: &Connection,
    table: &str,
    column: Option<&str>,
) -> std::result::Result<(u64, u64), String> {
    // A NUL byte cannot appear in a SQLite identifier and would truncate the
    // statement handed to the C API, so it is refused rather than quoted.
    if table.contains('\0') || column.is_some_and(|name| name.contains('\0')) {
        return Err("the name contains a NUL byte and cannot be queried".to_string());
    }

    let quoted_table = quote_identifier(table);
    let sql = match column {
        Some(column) => format!(
            "SELECT count(*), count({}) FROM {quoted_table}",
            quote_identifier(column)
        ),
        None => format!("SELECT count(*), count(*) FROM {quoted_table}"),
    };

    connection
        .query_row(&sql, [], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|error| error.to_string())
        .and_then(|(total, affected)| {
            let total = u64::try_from(total).map_err(|_| "negative row count".to_string())?;
            let affected = u64::try_from(affected).map_err(|_| "negative row count".to_string())?;
            Ok((total, affected))
        })
}

fn describe(change: &SchemaChange) -> (String, Option<String>) {
    match change.data_target() {
        Some((table, column)) => (table.to_string(), column.map(str::to_string)),
        None => (change.object_name.clone(), None),
    }
}

fn unavailable_warning(table: &str, column: Option<&str>, reason: &str) -> String {
    match column {
        Some(column) => {
            format!("Impact could not be calculated for {table}.{column}: {reason}")
        }
        None => format!("Impact could not be calculated for {table}: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_an_ordinary_name() {
        assert_eq!(quote_identifier("users"), "\"users\"");
    }

    #[test]
    fn doubles_an_embedded_quote() {
        assert_eq!(quote_identifier("foo\"bar"), "\"foo\"\"bar\"");
        assert_eq!(quote_identifier("\"\""), "\"\"\"\"\"\"");
    }

    #[test]
    fn leaves_spaces_punctuation_and_unicode_alone() {
        assert_eq!(
            quote_identifier("table with spaces"),
            "\"table with spaces\""
        );
        assert_eq!(quote_identifier("select"), "\"select\"");
        assert_eq!(quote_identifier("a;DROP TABLE x--"), "\"a;DROP TABLE x--\"");
        assert_eq!(
            quote_identifier("naïve_ünïcode_日本語"),
            "\"naïve_ünïcode_日本語\""
        );
        assert_eq!(quote_identifier(""), "\"\"");
    }

    /// The quoting is only worth anything if SQLite agrees with it.
    #[test]
    fn quoted_names_round_trip_through_sqlite() {
        let connection = Connection::open_in_memory().unwrap();
        for name in [
            "users",
            "table with spaces",
            "quote\"name",
            "select",
            "naïve_ünïcode_日本語",
            "a;DROP TABLE x--",
            "with'apostrophe",
            "[brackets]",
            "back`tick",
        ] {
            let quoted = quote_identifier(name);
            connection
                .execute_batch(&format!("CREATE TABLE {quoted} (value INTEGER)"))
                .unwrap_or_else(|error| panic!("create {name:?}: {error}"));
            connection
                .execute_batch(&format!("INSERT INTO {quoted} (value) VALUES (1), (NULL)"))
                .unwrap_or_else(|error| panic!("insert {name:?}: {error}"));

            let (total, affected) = count(&connection, name, Some("value")).unwrap();
            assert_eq!((total, affected), (2, 1), "counting {name:?}");

            let (total, affected) = count(&connection, name, None).unwrap();
            assert_eq!((total, affected), (2, 2), "counting all of {name:?}");
        }
    }

    #[test]
    fn a_name_carrying_a_nul_byte_is_refused_rather_than_quoted() {
        let connection = Connection::open_in_memory().unwrap();
        let error = count(&connection, "users\0; DROP TABLE x", None).unwrap_err();
        assert!(error.contains("NUL"), "unexpected reason: {error}");
    }

    #[test]
    fn a_missing_table_reports_a_reason_rather_than_zero() {
        let connection = Connection::open_in_memory().unwrap();
        let error = count(&connection, "not_here", None).unwrap_err();
        assert!(
            error.contains("no such table"),
            "unexpected reason: {error}"
        );
    }
}
