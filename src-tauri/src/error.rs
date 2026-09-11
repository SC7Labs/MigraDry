//! Error types for the MigraDry preview engine.
//!
//! Two distinct failure channels exist and they must not be confused:
//!
//! * [`MigraDryError`] — the preview could not be *performed* (bad input, the
//!   file is not a SQLite database, the clone could not be created, ...). The
//!   Tauri command surfaces these as `Err`.
//! * [`crate::models::MigrationFailure`] — the preview *was* performed and the
//!   migration SQL itself failed on the disposable clone. That is a successful
//!   preview of an unsuccessful migration and is returned as `Ok`.

use serde::Serialize;
use std::path::Path;
use thiserror::Error;

/// Something went wrong before a preview could be produced.
#[derive(Debug, Error)]
pub enum MigraDryError {
    #[error("Database file does not exist: {name}")]
    DatabaseNotFound { name: String },

    #[error("Database path is not a file: {name}")]
    DatabaseNotAFile { name: String },

    #[error("Migration file does not exist: {name}")]
    MigrationNotFound { name: String },

    #[error("Migration path is not a file: {name}")]
    MigrationNotAFile { name: String },

    #[error("The database and the migration must be two different files")]
    SamePath,

    #[error("Selected database is not a valid SQLite database: {name}")]
    NotSqliteDatabase { name: String },

    #[error("Original database could not be read: {reason}")]
    OriginalUnreadable { reason: String },

    #[error("Migration file could not be read: {reason}")]
    MigrationUnreadable { reason: String },

    #[error("Migration file does not look like SQL text: {reason}")]
    MigrationNotSql { reason: String },

    #[error("Temporary database clone could not be created: {reason}")]
    CloneFailed { reason: String },

    /// SQLite could not give us a snapshot it can vouch for. MigraDry refuses
    /// rather than copying the files itself and hoping the result is coherent.
    #[error("A consistent SQLite snapshot could not be created safely: {reason}")]
    SnapshotNotPossible { reason: String },

    #[error("Schema could not be read from the database clone: {reason}")]
    SchemaReadFailed { reason: String },

    /// The source changed while the snapshot was being read, so the clone does
    /// not correspond to any single state of it.
    ///
    /// Note the deliberate absence of blame. MigraDry can prove what *it* does —
    /// it opens the source read-only and runs migration SQL only against a
    /// clone — but it cannot prove what else on the machine touched the file, so
    /// it does not claim to know.
    #[error(
        "The source database changed while the snapshot was being taken, so the preview would \
         not describe any single state of it and has been discarded. MigraDry opens the source \
         read-only and executes migration SQL only against a disposable clone."
    )]
    SourceChangedDuringSnapshot,

    /// The source changed after the snapshot was taken. The preview may well be
    /// accurate for the state that was read, but the original is no longer that
    /// state, so it is not presented as a finding.
    #[error(
        "The source database changed after the snapshot was taken and before the preview \
         finished, so the result has been discarded. MigraDry opens the source read-only and \
         executes migration SQL only against a disposable clone."
    )]
    SourceChangedAfterSnapshot,

    /// The file was removed or replaced mid-preview.
    #[error("The source database was removed or replaced while the preview was running")]
    SourceDisappeared,

    #[error("Internal safety check failed: {reason}")]
    SafetyCheck { reason: String },
}

impl MigraDryError {
    /// Stable, machine-readable discriminant for the frontend.
    pub fn kind(&self) -> PreviewErrorKind {
        match self {
            Self::DatabaseNotFound { .. } | Self::DatabaseNotAFile { .. } => {
                PreviewErrorKind::InvalidDatabase
            }
            Self::MigrationNotFound { .. } | Self::MigrationNotAFile { .. } => {
                PreviewErrorKind::InvalidMigration
            }
            Self::SamePath => PreviewErrorKind::InvalidInput,
            Self::NotSqliteDatabase { .. } => PreviewErrorKind::NotSqliteDatabase,
            Self::OriginalUnreadable { .. } => PreviewErrorKind::InvalidDatabase,
            Self::MigrationUnreadable { .. } | Self::MigrationNotSql { .. } => {
                PreviewErrorKind::InvalidMigration
            }
            Self::CloneFailed { .. } => PreviewErrorKind::CloneFailed,
            Self::SnapshotNotPossible { .. } => PreviewErrorKind::SnapshotNotPossible,
            Self::SchemaReadFailed { .. } => PreviewErrorKind::SchemaReadFailed,
            Self::SourceChangedDuringSnapshot
            | Self::SourceChangedAfterSnapshot
            | Self::SourceDisappeared => PreviewErrorKind::SourceChanged,
            Self::SafetyCheck { .. } => PreviewErrorKind::SafetyCheck,
        }
    }
}

/// Machine-readable error category handed to the frontend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewErrorKind {
    InvalidInput,
    InvalidDatabase,
    InvalidMigration,
    NotSqliteDatabase,
    CloneFailed,
    /// SQLite could not produce a snapshot whose consistency it guarantees.
    SnapshotNotPossible,
    SchemaReadFailed,
    /// The source database moved under us. Deliberately not named after a
    /// culprit: MigraDry does not know who wrote to the file.
    SourceChanged,
    SafetyCheck,
}

/// Serializable error shape returned to the frontend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewError {
    pub kind: PreviewErrorKind,
    pub message: String,
}

impl From<MigraDryError> for PreviewError {
    fn from(error: MigraDryError) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

/// Best-effort file name for user-facing messages.
///
/// Error text deliberately names the *file*, never the full path, so that
/// messages and logs do not carry a user's directory layout around.
pub(crate) fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<unnamed>".to_string())
}

pub type Result<T> = std::result::Result<T, MigraDryError>;

impl MigraDryError {
    /// Short, non-identifying label for logs. Never contains a path, SQL text
    /// or database content.
    pub fn kind_label(&self) -> &'static str {
        match self.kind() {
            PreviewErrorKind::InvalidInput => "invalid_input",
            PreviewErrorKind::InvalidDatabase => "invalid_database",
            PreviewErrorKind::InvalidMigration => "invalid_migration",
            PreviewErrorKind::NotSqliteDatabase => "not_sqlite_database",
            PreviewErrorKind::CloneFailed => "clone_failed",
            PreviewErrorKind::SnapshotNotPossible => "snapshot_not_possible",
            PreviewErrorKind::SchemaReadFailed => "schema_read_failed",
            PreviewErrorKind::SourceChanged => "source_changed",
            PreviewErrorKind::SafetyCheck => "safety_check",
        }
    }
}
