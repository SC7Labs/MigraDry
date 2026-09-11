//! Serializable data model shared by the engine, the Tauri command layer and
//! the frontend.
//!
//! These types carry *facts*, never presentation. Symbols, colours and wording
//! for changes are the frontend's business.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Schema snapshot
// ---------------------------------------------------------------------------

/// Kind of top-level object stored in `sqlite_master`.
///
/// Declaration order is meaningful: it defines the deterministic ordering of a
/// [`SchemaSnapshot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaObjectKind {
    Table,
    Index,
    View,
    Trigger,
}

impl SchemaObjectKind {
    /// Maps the `type` column of `sqlite_master` onto a kind.
    pub fn from_sqlite_type(value: &str) -> Option<Self> {
        match value {
            "table" => Some(Self::Table),
            "index" => Some(Self::Index),
            "view" => Some(Self::View),
            "trigger" => Some(Self::Trigger),
            _ => None,
        }
    }
}

/// How a column gets its value.
///
/// SQLite reports this through the `hidden` flag of `pragma_table_xinfo`. It
/// matters here for one blunt reason: a `VIRTUAL` generated column stores
/// nothing, so removing one discards a definition rather than data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnKind {
    /// An ordinary stored column.
    Ordinary,
    /// `GENERATED ALWAYS AS (...) VIRTUAL` — computed on read, never stored.
    VirtualGenerated,
    /// `GENERATED ALWAYS AS (...) STORED` — computed on write and stored.
    StoredGenerated,
}

impl ColumnKind {
    /// Maps the `hidden` column of `pragma_table_xinfo`.
    pub fn from_hidden_flag(hidden: i64) -> Option<Self> {
        match hidden {
            0 => Some(Self::Ordinary),
            // 1 marks a virtual-table module's own hidden column (fts5's
            // `rank`, for instance). Those are not part of the schema anyone
            // wrote, so they are left out entirely.
            1 => None,
            2 => Some(Self::VirtualGenerated),
            3 => Some(Self::StoredGenerated),
            _ => Some(Self::Ordinary),
        }
    }

    /// Whether values in a column of this kind occupy storage.
    pub fn stores_data(self) -> bool {
        !matches!(self, Self::VirtualGenerated)
    }
}

/// Column metadata as reported by `pragma_table_xinfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnInfo {
    pub name: String,
    /// Declared type exactly as written in the DDL. SQLite typing is dynamic,
    /// so this string is reported verbatim and never interpreted.
    pub declared_type: String,
    pub not_null: bool,
    pub default_value: Option<String>,
    /// 0 when the column is not part of the primary key, otherwise its 1-based
    /// position within the primary key.
    pub primary_key_position: i32,
    /// Ordinary, or generated and how.
    ///
    /// Read from `pragma_table_xinfo` rather than `pragma_table_info`, because
    /// the latter omits generated columns altogether — a migration that added
    /// or dropped one would otherwise produce an empty diff.
    pub kind: ColumnKind,
}

/// One object in a schema snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaObject {
    pub object_type: SchemaObjectKind,
    pub name: String,
    /// The table an index or trigger is attached to. Equal to `name` for tables.
    pub table_name: Option<String>,
    /// Original DDL text from `sqlite_master`. `None` for objects SQLite creates
    /// implicitly.
    pub sql: Option<String>,
    /// Populated for tables only; empty for indexes, views and triggers.
    pub columns: Vec<ColumnInfo>,
}

/// A deterministic, ordered picture of a database schema.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaSnapshot {
    pub objects: Vec<SchemaObject>,
}

impl SchemaSnapshot {
    pub fn objects_of(&self, kind: SchemaObjectKind) -> impl Iterator<Item = &SchemaObject> {
        self.objects.iter().filter(move |o| o.object_type == kind)
    }

    pub fn table(&self, name: &str) -> Option<&SchemaObject> {
        self.objects
            .iter()
            .find(|o| o.object_type == SchemaObjectKind::Table && o.name == name)
    }
}

// ---------------------------------------------------------------------------
// Schema diff
// ---------------------------------------------------------------------------

/// A single difference between the before and after snapshots.
///
/// Declaration order defines the deterministic ordering of a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaChangeKind {
    TableAdded,
    TableRemoved,
    ColumnAdded,
    ColumnRemoved,
    /// A column that exists on both sides, whose declared metadata differs.
    ColumnModified,
    /// The `CREATE TABLE` text of a table carrying generated columns changed in
    /// a way column-level metadata does not show.
    TableDefinitionChanged,
    IndexAdded,
    IndexRemoved,
    ViewAdded,
    ViewRemoved,
    TriggerAdded,
    TriggerRemoved,
}

/// How much a change should worry the person reading the preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeImpact {
    /// Creates something new. No existing data is at risk.
    Additive,
    /// Removes a support object. Nothing stored in a row is lost, but query
    /// plans or behaviour may change.
    Advisory,
    /// Discards stored data.
    Destructive,
}

impl SchemaChangeKind {
    /// A deliberately simple, explainable rule: only dropping a table or a
    /// column destroys stored data. Removing an index, view or trigger removes
    /// a support object, and changing a column's declared metadata rewrites the
    /// schema — both are worth reading, neither loses rows on its own.
    ///
    /// There is no risk score here and there is not going to be one.
    pub fn impact(self) -> ChangeImpact {
        match self {
            Self::TableRemoved | Self::ColumnRemoved => ChangeImpact::Destructive,
            Self::IndexRemoved
            | Self::ViewRemoved
            | Self::TriggerRemoved
            | Self::ColumnModified
            | Self::TableDefinitionChanged => ChangeImpact::Advisory,
            Self::TableAdded
            | Self::ColumnAdded
            | Self::IndexAdded
            | Self::ViewAdded
            | Self::TriggerAdded => ChangeImpact::Additive,
        }
    }
}

// ---------------------------------------------------------------------------
// Column modifications
// ---------------------------------------------------------------------------

/// One attribute of a column that the migration changed.
///
/// A tagged union rather than a stringly-typed pair, so the frontend can render
/// each property in its own terms and the engine never has to decide how a
/// value should look.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "property",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ColumnPropertyChange {
    /// The type as written in the DDL, reported verbatim on both sides.
    ///
    /// MigraDry compares declarations, not semantics. SQLite's typing is
    /// dynamic and a declared type is closer to documentation than to a
    /// constraint, so this says the declaration changed — never that data was
    /// converted.
    DeclaredType {
        before: String,
        after: String,
    },
    NotNull {
        before: bool,
        after: bool,
    },
    /// The default expression exactly as SQLite reports it, or `None` for a
    /// column with no default at all.
    DefaultValue {
        before: Option<String>,
        after: Option<String>,
    },
    /// 0 when the column is not part of the primary key, otherwise its 1-based
    /// position within it.
    PrimaryKeyPosition {
        before: i32,
        after: i32,
    },
    /// A column becoming generated, ceasing to be, or moving between `VIRTUAL`
    /// and `STORED`.
    Generated {
        before: ColumnKind,
        after: ColumnKind,
    },
}

impl ColumnPropertyChange {
    /// Ordering key, so a column's property changes always list in the same
    /// order regardless of how they were discovered.
    pub(crate) fn order(&self) -> u8 {
        match self {
            Self::DeclaredType { .. } => 0,
            Self::NotNull { .. } => 1,
            Self::DefaultValue { .. } => 2,
            Self::PrimaryKeyPosition { .. } => 3,
            Self::Generated { .. } => 4,
        }
    }
}

// ---------------------------------------------------------------------------
// Data impact
// ---------------------------------------------------------------------------

/// How much stored data a destructive change would discard.
///
/// Every number here is an exact `COUNT` run against the baseline snapshot —
/// the same disposable copy the preview was computed from — not an estimate and
/// not a reading of the live database taken at some other moment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DataImpact {
    Measured {
        table: String,
        /// `None` when the whole table goes.
        column: Option<String>,
        /// Rows in the table, in the baseline.
        total_rows: u64,
        /// Rows that actually lose something: every row for a dropped table,
        /// and the rows holding a non-null value for a dropped column.
        affected_rows: u64,
    },
    /// The count could not be taken. Reported as such, never as zero: a
    /// fabricated zero is exactly the kind of confident wrong answer this
    /// program exists to avoid.
    Unavailable {
        table: String,
        column: Option<String>,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaChange {
    pub kind: SchemaChangeKind,
    pub object_name: String,
    /// Owning table for column changes, and for indexes and triggers when
    /// SQLite reports one.
    pub parent_name: Option<String>,
    pub impact: ChangeImpact,
    /// Populated only for [`SchemaChangeKind::ColumnModified`], and never
    /// empty when it is: a modification with nothing modified is not a change.
    #[serde(default)]
    pub property_changes: Vec<ColumnPropertyChange>,
    /// Populated only for changes that discard data, and only when the
    /// migration ran to completion. `None` means "not applicable", which is a
    /// different thing from a measured zero.
    #[serde(default)]
    pub data_impact: Option<DataImpact>,
}

impl SchemaChange {
    pub fn new(kind: SchemaChangeKind, object_name: impl Into<String>) -> Self {
        Self {
            kind,
            object_name: object_name.into(),
            parent_name: None,
            impact: kind.impact(),
            property_changes: Vec::new(),
            data_impact: None,
        }
    }

    pub fn with_parent(mut self, parent: Option<String>) -> Self {
        self.parent_name = parent;
        self
    }

    pub fn with_property_changes(mut self, changes: Vec<ColumnPropertyChange>) -> Self {
        self.property_changes = changes;
        self
    }

    /// Downgrades a change from destructive to advisory.
    ///
    /// Used for exactly one case: dropping a `VIRTUAL` generated column, which
    /// removes a computed definition rather than stored data. Reporting it as
    /// data loss, with a count of values that were never stored, would be a
    /// false alarm of the kind this program is supposed to prevent.
    pub fn as_advisory(mut self) -> Self {
        self.impact = ChangeImpact::Advisory;
        self
    }

    /// The table this change touches, for changes that discard data.
    ///
    /// A dropped table is named by `object_name`; a dropped column is named by
    /// its parent. Anything else has no data to count.
    pub fn data_target(&self) -> Option<(&str, Option<&str>)> {
        // A change downgraded to advisory discards no stored data, so there is
        // nothing to count.
        if self.impact != ChangeImpact::Destructive {
            return None;
        }
        match self.kind {
            SchemaChangeKind::TableRemoved => Some((self.object_name.as_str(), None)),
            SchemaChangeKind::ColumnRemoved => self
                .parent_name
                .as_deref()
                .map(|table| (table, Some(self.object_name.as_str()))),
            _ => None,
        }
    }

    /// Deterministic ordering key: kind first, then owner, then object name.
    pub(crate) fn sort_key(&self) -> (SchemaChangeKind, &str, &str) {
        (
            self.kind,
            self.parent_name.as_deref().unwrap_or(""),
            self.object_name.as_str(),
        )
    }
}

// ---------------------------------------------------------------------------
// Original-database integrity
// ---------------------------------------------------------------------------

/// Content fingerprint of a single file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileFingerprint {
    pub size_bytes: u64,
    /// Lowercase hex SHA-256 of the whole file. Authoritative.
    pub sha256: String,
    /// Reported for information only; never used to decide `unchanged`.
    pub modified_unix_ms: Option<u64>,
}

impl FileFingerprint {
    /// Content equality. `modified_unix_ms` is deliberately excluded: a
    /// filesystem may report a new mtime without any content change, and a
    /// content change without a new mtime. The hash decides.
    pub fn content_matches(&self, other: &Self) -> bool {
        self.size_bytes == other.size_bytes && self.sha256 == other.sha256
    }
}

/// Fingerprint of a SQLite database as a whole.
///
/// The `-shm` file is intentionally absent: it is a transient shared-memory
/// index that SQLite rebuilds from the `-wal` at will, so including it would
/// produce false alarms. The main file and the `-wal` hold all durable content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseFingerprint {
    pub main: FileFingerprint,
    pub wal: Option<FileFingerprint>,
}

impl DatabaseFingerprint {
    pub fn content_matches(&self, other: &Self) -> bool {
        if !self.main.content_matches(&other.main) {
            return false;
        }
        match (&self.wal, &other.wal) {
            (None, None) => true,
            (Some(a), Some(b)) => a.content_matches(b),
            // A `-wal` that appeared or vanished means something checkpointed
            // or wrote the original. That is a failure.
            _ => false,
        }
    }
}

/// The before/after evidence that the original database was left alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OriginalIntegrity {
    /// The durable *content* of the database — the main file and its `-wal` —
    /// is byte-identical before and after.
    ///
    /// This is deliberately narrower than "the directory is untouched". See
    /// [`Self::shm_created_by_preview`].
    pub content_unchanged: bool,
    pub before: DatabaseFingerprint,
    pub after: DatabaseFingerprint,
    /// True when a `-wal` sidecar was present on either side and therefore
    /// took part in the comparison.
    pub wal_checked: bool,
    /// True when reading the original caused SQLite to create a `-shm` file
    /// beside it.
    ///
    /// This happens for WAL-mode databases and is unavoidable for *any* reader:
    /// SQLite requires a shared-memory index to read a write-ahead log. The
    /// `-shm` holds no database content, is rebuilt from the `-wal` on demand,
    /// and is excluded from the fingerprint for that reason. It is reported
    /// rather than hidden, because it is the one visible trace a preview can
    /// leave in the original's directory.
    pub shm_created_by_preview: bool,
}

// ---------------------------------------------------------------------------
// Preview result
// ---------------------------------------------------------------------------

/// How the disposable clone was produced.
///
/// There is exactly one mechanism, and that is the point. MigraDry will not
/// copy database files itself: a plain filesystem copy of `app.db` and
/// `app.db-wal` cannot be proven consistent against a concurrent writer, and a
/// preview built on a torn snapshot is worse than no preview at all. When
/// SQLite cannot produce a snapshot it vouches for, the preview fails closed
/// with [`crate::error::MigraDryError::SnapshotNotPossible`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloneStrategy {
    /// SQLite Online Backup API, driven from a read-only source connection.
    /// Page-exact and transactionally consistent, and it reads through any
    /// uncheckpointed `-wal` content.
    SqliteBackupApi,
}

/// The migration SQL itself failed. The preview around it still succeeded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationFailure {
    pub message: String,
    /// Symbolic SQLite primary result code, e.g. `SQLITE_ERROR`, when the
    /// driver reported one.
    pub sqlite_code: Option<String>,
    pub sqlite_extended_code: Option<i32>,
    /// 1-based line in the migration file that SQLite objected to.
    ///
    /// Derived from the byte offset SQLite itself reports, never from parsing
    /// the SQL. It is absent whenever SQLite does not supply an offset.
    pub line: Option<u32>,
}

/// The complete answer to "what would this migration do?".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationPreviewResult {
    /// True when the migration SQL ran to completion on the clone.
    ///
    /// A preview result only ever exists when the original's content was
    /// verified unchanged, so this says nothing about the original — it is
    /// purely the migration's own verdict.
    pub success: bool,
    pub database_path: String,
    pub database_name: String,
    pub migration_path: String,
    pub migration_name: String,
    /// Time spent executing the migration SQL against the clone.
    pub duration_ms: u64,
    /// Time spent on the whole preview, including cloning and hashing.
    pub total_duration_ms: u64,
    pub schema_changes: Vec<SchemaChange>,
    pub destructive_change_count: usize,
    pub advisory_change_count: usize,
    /// The durable content of the original was byte-identical before and after.
    /// Always `true` on a result: any deviation aborts the preview instead.
    pub original_content_unchanged: bool,
    pub original_integrity: OriginalIntegrity,
    /// Whether `PRAGMA foreign_keys` was on while the migration ran.
    ///
    /// SQLite's own default is off, which would quietly let a migration that
    /// breaks referential integrity look fine. MigraDry enforces foreign keys
    /// and says so, rather than inheriting a default nobody chose.
    pub foreign_keys_enforced: bool,
    pub clone_strategy: CloneStrategy,
    /// Engine-level notices (an empty migration, a `-shm` sidecar created while
    /// reading a WAL database, a migration that left a transaction open, ...).
    pub warnings: Vec<String>,
    pub error: Option<MigrationFailure>,
}
