//! MigraDry — preview a SQLite migration before it touches your database.
//!
//! The engine takes a disposable baseline copy of a database, migrates a second
//! copy of that baseline, compares the two schemas, counts exactly what the
//! migration would discard, and proves by hash that the original's content was
//! left byte-identical.
//!
//! Where it cannot guarantee that, it refuses. A preview MigraDry cannot vouch
//! for is worse than no preview: the whole value of the tool is that its answer
//! can be believed.
//!
//! Module responsibilities:
//!
//! | Module | Owns |
//! |---|---|
//! | [`database`] | every read of the *original* file, all of it read-only |
//! | [`temp`] | the preview workspace: baseline, working clone, and their lifetime |
//! | [`impact`] | exact row counts for destructive changes, read from the baseline |
//! | [`migration`] | reading, executing and orchestrating the migration |
//! | [`schema`] | reading a deterministic schema snapshot |
//! | [`diff`] | comparing two snapshots |
//! | [`models`] | the serializable data contract |
//! | [`commands`] | the single Tauri command exposed to the frontend |

#![deny(unsafe_code)]

pub mod commands;
pub mod database;
pub mod diff;
pub mod error;
pub mod impact;
pub mod migration;
pub mod models;
pub mod schema;
pub mod temp;

pub use error::{MigraDryError, PreviewError, PreviewErrorKind};
pub use migration::MigrationService;
pub use models::*;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![commands::preview_migration])
        .run(tauri::generate_context!())
        .expect("error while running the MigraDry application");
}
