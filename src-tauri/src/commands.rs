//! The frontend's entire view of the backend.
//!
//! Exactly one command is exposed. No filesystem primitive, no SQL entry point
//! and no clone path is reachable from the webview, so the UI cannot be talked
//! into doing anything the engine would not do on its own.

use crate::error::PreviewError;
use crate::migration::MigrationService;
use crate::models::MigrationPreviewResult;
use std::path::PathBuf;

/// Previews what a migration would do to a database, without touching it.
///
/// `Ok` means the preview ran. It does *not* mean the migration succeeded —
/// check `success` and `error` for that. `Err` means no preview could be
/// produced at all.
#[tauri::command]
pub fn preview_migration(
    database_path: String,
    migration_path: String,
) -> Result<MigrationPreviewResult, PreviewError> {
    let database = PathBuf::from(database_path.trim());
    let migration = PathBuf::from(migration_path.trim());

    log_start();
    match MigrationService::preview(&database, &migration) {
        Ok(result) => {
            log_finish(&result);
            Ok(result)
        }
        Err(error) => {
            // Paths, SQL and database contents are never logged.
            eprintln!("[migradry] preview failed: {}", error.kind_label());
            Err(error.into())
        }
    }
}

fn log_start() {
    eprintln!("[migradry] preview started");
}

fn log_finish(result: &MigrationPreviewResult) {
    eprintln!(
        "[migradry] preview finished: migration_ok={} changes={} destructive={} \
         original_content_unchanged={} migration_ms={} total_ms={}",
        result.success,
        result.schema_changes.len(),
        result.destructive_change_count,
        result.original_content_unchanged,
        result.duration_ms,
        result.total_duration_ms,
    );
}
