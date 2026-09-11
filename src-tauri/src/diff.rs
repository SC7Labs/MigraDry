//! Comparison of two schema snapshots.
//!
//! Objects are compared **by name only**. A dropped table and a new one are
//! reported as a removal plus an addition, never as a rename. That is honest
//! and explainable, and it never invents a change the SQL did not clearly make.
//!
//! Columns that survive under the same name are compared attribute by
//! attribute, which is what makes the common SQLite table-rebuild pattern
//! legible: `users` is created afresh, filled, dropped and renamed back into
//! place, and because the final table is still called `users`, the columns line
//! up and the metadata differences fall out. No rename inference is involved.
//!
//! Not attempted here, on purpose: rename detection, semantic equivalence of
//! DDL text, foreign-key and constraint diffing, AST comparison, SQLite type
//! affinity analysis.

use crate::models::{
    ColumnInfo, ColumnKind, ColumnPropertyChange, SchemaChange, SchemaChangeKind, SchemaObjectKind,
    SchemaSnapshot,
};
use std::collections::BTreeMap;

/// Compares two snapshots and returns a deterministically ordered change list.
pub fn diff(before: &SchemaSnapshot, after: &SchemaSnapshot) -> Vec<SchemaChange> {
    let mut changes = Vec::new();

    for (kind, added, removed) in [
        (
            SchemaObjectKind::Table,
            SchemaChangeKind::TableAdded,
            SchemaChangeKind::TableRemoved,
        ),
        (
            SchemaObjectKind::Index,
            SchemaChangeKind::IndexAdded,
            SchemaChangeKind::IndexRemoved,
        ),
        (
            SchemaObjectKind::View,
            SchemaChangeKind::ViewAdded,
            SchemaChangeKind::ViewRemoved,
        ),
        (
            SchemaObjectKind::Trigger,
            SchemaChangeKind::TriggerAdded,
            SchemaChangeKind::TriggerRemoved,
        ),
    ] {
        diff_objects(before, after, kind, added, removed, &mut changes);
    }

    diff_columns(before, after, &mut changes);
    diff_generated_table_definitions(before, after, &mut changes);

    changes.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
    changes
}

fn diff_objects(
    before: &SchemaSnapshot,
    after: &SchemaSnapshot,
    kind: SchemaObjectKind,
    added_kind: SchemaChangeKind,
    removed_kind: SchemaChangeKind,
    changes: &mut Vec<SchemaChange>,
) {
    // `parent` is the table an index or trigger hangs off; for tables and views
    // it is not reported, since it would just repeat the object name.
    let parent_of = |snapshot: &SchemaSnapshot| -> BTreeMap<String, Option<String>> {
        snapshot
            .objects_of(kind)
            .map(|object| {
                let parent = match kind {
                    SchemaObjectKind::Index | SchemaObjectKind::Trigger => {
                        object.table_name.clone()
                    }
                    SchemaObjectKind::Table | SchemaObjectKind::View => None,
                };
                (object.name.clone(), parent)
            })
            .collect()
    };

    let before_objects = parent_of(before);
    let after_objects = parent_of(after);

    for (name, parent) in &after_objects {
        if !before_objects.contains_key(name) {
            changes.push(SchemaChange::new(added_kind, name).with_parent(parent.clone()));
        }
    }
    for (name, parent) in &before_objects {
        if !after_objects.contains_key(name) {
            changes.push(SchemaChange::new(removed_kind, name).with_parent(parent.clone()));
        }
    }
}

/// Columns are only compared for tables that exist on both sides. Columns of a
/// dropped table are not reported individually — the dropped table already says
/// everything, and listing its columns would double-count the loss.
///
/// A column that survives under the same name is compared attribute by
/// attribute, and every attribute that moved is folded into a single
/// [`SchemaChangeKind::ColumnModified`] rather than emitted as separate
/// findings. One column changing three ways is one thing that happened.
fn diff_columns(before: &SchemaSnapshot, after: &SchemaSnapshot, changes: &mut Vec<SchemaChange>) {
    for before_table in before.objects_of(SchemaObjectKind::Table) {
        let Some(after_table) = after.table(&before_table.name) else {
            continue;
        };

        let before_columns: BTreeMap<&str, &ColumnInfo> = before_table
            .columns
            .iter()
            .map(|column| (column.name.as_str(), column))
            .collect();
        let after_columns: BTreeMap<&str, &ColumnInfo> = after_table
            .columns
            .iter()
            .map(|column| (column.name.as_str(), column))
            .collect();

        for name in after_columns.keys() {
            if !before_columns.contains_key(name) {
                changes.push(
                    SchemaChange::new(SchemaChangeKind::ColumnAdded, *name)
                        .with_parent(Some(before_table.name.clone())),
                );
            }
        }
        for (name, old) in &before_columns {
            match after_columns.get(name) {
                None => {
                    // A VIRTUAL generated column stores nothing: its values are
                    // computed on read from columns that remain. Dropping one
                    // discards a definition, not data, so it is not reported as
                    // data loss and carries no row count.
                    let change = SchemaChange::new(SchemaChangeKind::ColumnRemoved, *name)
                        .with_parent(Some(before_table.name.clone()));
                    changes.push(if old.kind.stores_data() {
                        change
                    } else {
                        change.as_advisory()
                    });
                }
                Some(new) => {
                    let properties = compare_columns(old, new);
                    if !properties.is_empty() {
                        changes.push(
                            SchemaChange::new(SchemaChangeKind::ColumnModified, *name)
                                .with_parent(Some(before_table.name.clone()))
                                .with_property_changes(properties),
                        );
                    }
                }
            }
        }
    }
}

/// Reports tables whose stored `CREATE TABLE` text changed while their columns
/// look identical.
///
/// This exists for one blind spot. The body of a generated column — the
/// expression in `GENERATED ALWAYS AS (...)` — is not part of what
/// `pragma_table_xinfo` reports, so changing `(a * 2)` to `(a * 3)` produces
/// byte-identical column metadata. Without this, such a migration is reported
/// as no change at all, which is the one outcome this program must never
/// produce.
///
/// Deliberately narrow: only tables that carry a generated column on either
/// side are compared this way, so ordinary tables gain no new failure mode. The
/// finding is advisory and says only that the definition text moved — MigraDry
/// does not parse SQL and will not claim to know how.
fn diff_generated_table_definitions(
    before: &SchemaSnapshot,
    after: &SchemaSnapshot,
    changes: &mut Vec<SchemaChange>,
) {
    for before_table in before.objects_of(SchemaObjectKind::Table) {
        let Some(after_table) = after.table(&before_table.name) else {
            continue;
        };

        let has_generated = before_table
            .columns
            .iter()
            .chain(after_table.columns.iter())
            .any(|column| column.kind != ColumnKind::Ordinary);
        if !has_generated {
            continue;
        }

        let (Some(old_sql), Some(new_sql)) = (&before_table.sql, &after_table.sql) else {
            continue;
        };
        if definition_body(old_sql) != definition_body(new_sql) {
            changes.push(SchemaChange::new(
                SchemaChangeKind::TableDefinitionChanged,
                &before_table.name,
            ));
        }
    }
}

/// Which kind of quoted region a scan is currently inside.
///
/// SQLite accepts four ways of quoting: string literals in single quotes, and
/// identifiers in double quotes, backticks or square brackets. The first three
/// escape their own delimiter by doubling it; brackets have no escape and end
/// at the first `]`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Quoted {
    No,
    Single,
    Double,
    Backtick,
    Bracket,
}

impl Quoted {
    fn opened_by(character: char) -> Option<Self> {
        match character {
            '\'' => Some(Self::Single),
            '"' => Some(Self::Double),
            '`' => Some(Self::Backtick),
            '[' => Some(Self::Bracket),
            _ => None,
        }
    }

    fn closing(self) -> Option<char> {
        match self {
            Self::No => None,
            Self::Single => Some('\''),
            Self::Double => Some('"'),
            Self::Backtick => Some('`'),
            Self::Bracket => Some(']'),
        }
    }

    /// Whether a doubled delimiter escapes itself rather than closing.
    fn doubles_to_escape(self) -> bool {
        !matches!(self, Self::Bracket | Self::No)
    }
}

/// The `CREATE TABLE` body, from its opening parenthesis onwards, verbatim.
///
/// Only the prefix is dropped, and only because SQLite rewrites it: a table
/// rebuilt and renamed into place comes back as `CREATE TABLE "t" (...)` where
/// it was `CREATE TABLE t (...)`, so comparing the whole statement would flag
/// every rebuild.
///
/// The parenthesis is found with a quote-aware scan rather than `find('(')`,
/// because a legal identifier may contain one: in
/// `CREATE TABLE "example(test)" (...)` the first `(` sits inside the quoted
/// name, and taking the substring from there would compare fragments of the
/// identifier instead of the definition.
///
/// Nothing else is normalised. An earlier version stripped whitespace, which
/// erased it *inside* string literals too — `AS ('a b')` and `AS ('ab')`
/// collapsed to the same text and a genuinely different generated expression
/// was reported as no change at all. Comparing verbatim cannot do that. The
/// cost is that a formatting-only rewrite produces an advisory, which is the
/// error worth making: a false advisory is visible, a missed change is not.
fn definition_body(sql: &str) -> &str {
    let mut state = Quoted::No;
    let mut characters = sql.char_indices().peekable();

    while let Some((index, character)) = characters.next() {
        match state {
            Quoted::No => {
                if character == '(' {
                    return &sql[index..];
                }
                if let Some(opened) = Quoted::opened_by(character) {
                    state = opened;
                }
            }
            quoted => {
                if Some(character) == quoted.closing() {
                    let doubled = quoted.doubles_to_escape()
                        && characters.peek().map(|(_, next)| *next) == quoted.closing();
                    if doubled {
                        characters.next();
                    } else {
                        state = Quoted::No;
                    }
                }
            }
        }
    }

    // No parenthesis outside a quoted region. Compare the whole statement
    // rather than nothing: two different texts must not look identical.
    sql
}

/// Every attribute of a surviving column that differs, in a fixed order./// Every attribute of a surviving column that differs, in a fixed order.
fn compare_columns(before: &ColumnInfo, after: &ColumnInfo) -> Vec<ColumnPropertyChange> {
    let mut properties = Vec::new();

    if declared_types_differ(&before.declared_type, &after.declared_type) {
        properties.push(ColumnPropertyChange::DeclaredType {
            // Reported exactly as written, however they were compared.
            before: before.declared_type.clone(),
            after: after.declared_type.clone(),
        });
    }
    if before.not_null != after.not_null {
        properties.push(ColumnPropertyChange::NotNull {
            before: before.not_null,
            after: after.not_null,
        });
    }
    if before.default_value != after.default_value {
        // Compared as text, because SQLite stores a default as the text of the
        // expression that produces it. Two spellings of the same value — `'a'`
        // and `"a"`, or `1` and `1.0` — therefore read as a change. That is
        // conservative on purpose: calling them equivalent would need an
        // expression evaluator, and a wrong "nothing changed" is worse than a
        // noisy "something did".
        properties.push(ColumnPropertyChange::DefaultValue {
            before: before.default_value.clone(),
            after: after.default_value.clone(),
        });
    }
    if before.primary_key_position != after.primary_key_position {
        properties.push(ColumnPropertyChange::PrimaryKeyPosition {
            before: before.primary_key_position,
            after: after.primary_key_position,
        });
    }
    if before.kind != after.kind {
        properties.push(ColumnPropertyChange::Generated {
            before: before.kind,
            after: after.kind,
        });
    }

    properties.sort_by_key(ColumnPropertyChange::order);
    properties
}

/// Whether two declared types differ by more than how they were typed out.
///
/// A declared type in SQLite is close to free text — the engine derives an
/// affinity from it and is otherwise indifferent — so the only sensible
/// comparison is of the declaration itself. Case and whitespace carry no
/// meaning (`text` versus `TEXT`, `VARCHAR (255)` versus `VARCHAR(255)`), so
/// they are normalised away; everything else is reported.
///
/// This is deliberately not affinity analysis. `TEXT` and `VARCHAR(255)` share
/// an affinity and are still reported as a change, because the declaration
/// genuinely changed. MigraDry's job is to say what the migration did, not to
/// decide on the author's behalf that it did not matter.
fn declared_types_differ(before: &str, after: &str) -> bool {
    normalise_declared_type(before) != normalise_declared_type(after)
}

fn normalise_declared_type(declared: &str) -> String {
    declared
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_uppercase)
        .collect()
}

/// Number of changes that discard stored data.
pub fn destructive_count(changes: &[SchemaChange]) -> usize {
    changes
        .iter()
        .filter(|change| change.impact == crate::models::ChangeImpact::Destructive)
        .count()
}

/// Number of changes that alter the schema without losing row data.
pub fn advisory_count(changes: &[SchemaChange]) -> usize {
    changes
        .iter()
        .filter(|change| change.impact == crate::models::ChangeImpact::Advisory)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;
    use rusqlite::Connection;

    fn snapshot_of(sql: &str) -> SchemaSnapshot {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(sql).unwrap();
        schema::snapshot(&connection).unwrap()
    }

    fn kinds(changes: &[SchemaChange]) -> Vec<(SchemaChangeKind, String, Option<String>)> {
        changes
            .iter()
            .map(|c| (c.kind, c.object_name.clone(), c.parent_name.clone()))
            .collect()
    }

    #[test]
    fn identical_schemas_produce_no_changes() {
        let sql = "CREATE TABLE users (id INTEGER PRIMARY KEY);";
        assert!(diff(&snapshot_of(sql), &snapshot_of(sql)).is_empty());
    }

    #[test]
    fn detects_added_and_removed_tables() {
        let before = snapshot_of("CREATE TABLE users (id INTEGER); CREATE TABLE old (id INTEGER);");
        let after =
            snapshot_of("CREATE TABLE users (id INTEGER); CREATE TABLE orders (id INTEGER);");
        assert_eq!(
            kinds(&diff(&before, &after)),
            vec![
                (SchemaChangeKind::TableAdded, "orders".into(), None),
                (SchemaChangeKind::TableRemoved, "old".into(), None),
            ]
        );
    }

    #[test]
    fn detects_added_and_removed_columns_with_their_table() {
        let before = snapshot_of("CREATE TABLE users (id INTEGER, legacy TEXT);");
        let after = snapshot_of("CREATE TABLE users (id INTEGER, email TEXT);");
        assert_eq!(
            kinds(&diff(&before, &after)),
            vec![
                (
                    SchemaChangeKind::ColumnAdded,
                    "email".into(),
                    Some("users".into())
                ),
                (
                    SchemaChangeKind::ColumnRemoved,
                    "legacy".into(),
                    Some("users".into())
                ),
            ]
        );
    }

    #[test]
    fn columns_of_a_dropped_table_are_not_reported_separately() {
        let before = snapshot_of("CREATE TABLE old_data (id INTEGER, payload TEXT);");
        let after = snapshot_of("");
        assert_eq!(
            kinds(&diff(&before, &after)),
            vec![(SchemaChangeKind::TableRemoved, "old_data".into(), None)]
        );
    }

    #[test]
    fn detects_index_view_and_trigger_changes() {
        let before = snapshot_of(
            "CREATE TABLE users (id INTEGER, email TEXT);
             CREATE INDEX idx_old_email ON users(email);
             CREATE VIEW old_view AS SELECT id FROM users;",
        );
        let after = snapshot_of(
            "CREATE TABLE users (id INTEGER, email TEXT);
             CREATE INDEX idx_users_id ON users(id);
             CREATE TRIGGER users_guard AFTER INSERT ON users BEGIN SELECT 1; END;",
        );
        assert_eq!(
            kinds(&diff(&before, &after)),
            vec![
                (
                    SchemaChangeKind::IndexAdded,
                    "idx_users_id".into(),
                    Some("users".into())
                ),
                (
                    SchemaChangeKind::IndexRemoved,
                    "idx_old_email".into(),
                    Some("users".into())
                ),
                (SchemaChangeKind::ViewRemoved, "old_view".into(), None),
                (
                    SchemaChangeKind::TriggerAdded,
                    "users_guard".into(),
                    Some("users".into())
                ),
            ]
        );
    }

    #[test]
    fn impact_separates_data_loss_from_support_object_removal() {
        let before = snapshot_of(
            "CREATE TABLE users (id INTEGER, legacy TEXT);
             CREATE TABLE gone (id INTEGER);
             CREATE INDEX idx_gone ON users(legacy);",
        );
        let after = snapshot_of("CREATE TABLE users (id INTEGER);");
        let changes = diff(&before, &after);

        // Dropped table + dropped column are destructive; the dropped index is not.
        assert_eq!(destructive_count(&changes), 2);
        assert_eq!(advisory_count(&changes), 1);
    }

    #[test]
    fn ordering_is_deterministic() {
        let before =
            snapshot_of("CREATE TABLE zeta (id INTEGER); CREATE TABLE alpha (id INTEGER);");
        let after = snapshot_of(
            "CREATE TABLE zeta (id INTEGER, b TEXT, a TEXT);
             CREATE TABLE alpha (id INTEGER);
             CREATE TABLE mid (id INTEGER);",
        );
        let first = diff(&before, &after);
        let second = diff(&before, &after);
        assert_eq!(first, second);
        assert_eq!(
            kinds(&first),
            vec![
                (SchemaChangeKind::TableAdded, "mid".into(), None),
                (
                    SchemaChangeKind::ColumnAdded,
                    "a".into(),
                    Some("zeta".into())
                ),
                (
                    SchemaChangeKind::ColumnAdded,
                    "b".into(),
                    Some("zeta".into())
                ),
            ]
        );
    }
}
