//! Loading, executing and orchestrating a migration preview.
//!
//! # The safety invariant
//!
//! [`execute_on_clone`] is the only function in MigraDry that runs
//! user-supplied SQL, and it takes a [`WorkingClone`] rather than a path. Since
//! `WorkingClone` can only be produced by [`PreviewWorkspace::create`], there is
//! no expression anywhere in this crate that could aim migration SQL at the
//! original database — or at the baseline, which has no writable opener at all.
//! The connection it opens is additionally hardened so SQL cannot reach back
//! out to another database by other means.

use crate::database;
use crate::diff;
use crate::error::{display_name, MigraDryError, Result};
use crate::impact;
use crate::models::{MigrationFailure, MigrationPreviewResult, OriginalIntegrity, SchemaSnapshot};
use crate::schema;
use crate::temp::{PreviewWorkspace, WorkingClone};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::{Connection, ErrorCode};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Upper bound on a migration file. Migrations are hand-written SQL; anything
/// larger is far more likely to be the wrong file than a real migration.
pub const MAX_MIGRATION_BYTES: u64 = 16 * 1024 * 1024;

/// Default upper bound on migration SQL execution duration against the clone.
///
/// Prevents runaway recursive CTEs, Cartesian joins, or CPU spin loops from
/// hanging the application indefinitely.
pub const DEFAULT_MIGRATION_TIMEOUT: Duration = Duration::from_secs(15);

/// Number of VDBE instructions between checks of the execution deadline.
const PROGRESS_HANDLER_OP_INTERVAL: i32 = 1000;

/// Outcome of running the migration SQL against the clone.
#[derive(Debug)]
pub struct MigrationRun {
    pub duration_ms: u64,
    /// `None` when every statement ran. The preview itself succeeded either way.
    pub failure: Option<MigrationFailure>,
    pub warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Migration file input
// ---------------------------------------------------------------------------

/// Reads and sanity-checks a migration file.
///
/// File extensions are not trusted, and no attempt is made to parse SQL. The
/// checks only rule out inputs that clearly cannot be a migration: a directory,
/// something implausibly large, a SQLite database picked by mistake, and bytes
/// that are not text.
pub fn load_migration_sql(path: &Path) -> Result<String> {
    if !path.exists() {
        return Err(MigraDryError::MigrationNotFound {
            name: display_name(path),
        });
    }
    if !path.is_file() {
        return Err(MigraDryError::MigrationNotAFile {
            name: display_name(path),
        });
    }

    let Some(bytes) = read_bounded(path, MAX_MIGRATION_BYTES)? else {
        return Err(MigraDryError::MigrationNotSql {
            reason: format!(
                "it is larger than the {MAX_MIGRATION_BYTES} byte limit for a migration"
            ),
        });
    };

    if bytes.starts_with(b"SQLite format 3\0") {
        return Err(MigraDryError::MigrationNotSql {
            reason: "it is a SQLite database, not a SQL script".to_string(),
        });
    }
    if bytes.contains(&0u8) {
        return Err(MigraDryError::MigrationNotSql {
            reason: "it contains binary data".to_string(),
        });
    }

    let text = String::from_utf8(bytes).map_err(|_| MigraDryError::MigrationNotSql {
        reason: "it is not valid UTF-8 text".to_string(),
    })?;
    // Editors on some platforms prefix SQL files with a byte-order mark, which
    // SQLite would reject as a syntax error.
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
}

/// Reads at most `limit` bytes, reporting `None` when the file has more.
///
/// The size is decided by what was actually read, never by a prior `stat`.
/// Checking `metadata().len()` and then calling `fs::read` looks equivalent and
/// is not: the two are separate syscalls, so a file can grow in between, and
/// some perfectly ordinary files report a length that has nothing to do with
/// their contents. `/proc/self/status` passes `is_file()`, reports a length of
/// zero, and yields well over a kilobyte when read.
///
/// Reading `limit + 1` bytes is what makes the distinction: if the extra byte
/// arrives, the file is over the limit, and at most one byte beyond the bound
/// is ever allocated no matter how large the file really is.
fn read_bounded(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    let file = std::fs::File::open(path).map_err(|error| MigraDryError::MigrationUnreadable {
        reason: error.to_string(),
    })?;

    let mut bytes = Vec::new();
    std::io::Read::take(file, limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| MigraDryError::MigrationUnreadable {
            reason: error.to_string(),
        })?;

    if bytes.len() as u64 > limit {
        return Ok(None);
    }
    Ok(Some(bytes))
}

// ---------------------------------------------------------------------------
// Migration execution
// ---------------------------------------------------------------------------

/// Runs the migration SQL against the disposable clone.
///
/// The whole script is handed to SQLite's own batch execution, so statement
/// splitting, comments, triggers containing semicolons and every other quirk of
/// SQL text are SQLite's problem rather than a hand-rolled parser's.
pub fn execute_on_clone(working: &WorkingClone, sql: &str) -> Result<MigrationRun> {
    execute_on_clone_with_timeout(working, sql, DEFAULT_MIGRATION_TIMEOUT)
}

/// Runs the migration SQL against the disposable clone with a bounded execution timeout.
pub fn execute_on_clone_with_timeout(
    working: &WorkingClone,
    sql: &str,
    timeout: Duration,
) -> Result<MigrationRun> {
    let connection = working.open_read_write()?;

    harden(&connection)?;
    enforce_foreign_keys(&connection)?;

    // Attach a progress handler to enforce a bounded execution budget.
    // If a migration exceeds the deadline, the progress handler returns true,
    // signaling SQLite to interrupt statement execution and return SQLITE_INTERRUPT.
    let deadline = Instant::now() + timeout;
    connection
        .progress_handler(
            PROGRESS_HANDLER_OP_INTERVAL,
            Some(move || Instant::now() >= deadline),
        )
        .map_err(|error| MigraDryError::SafetyCheck {
            reason: format!("could not install progress handler on clone: {error}"),
        })?;

    let mut warnings = Vec::new();
    let started = Instant::now();
    let mut outcome = connection.execute_batch(sql);

    // Remove the progress handler so cleanup operations (such as ROLLBACK or
    // reading database_list) are not interrupted if the deadline was reached.
    let _ = connection.progress_handler(0, None::<fn() -> bool>);

    // A migration may open a transaction and never close it. Resolve it here so
    // the schema that gets inspected is a settled one rather than the inside of
    // a transaction that is about to be discarded when the handle closes.
    if !connection.is_autocommit() {
        match outcome {
            Ok(()) => {
                warnings.push(
                    "The migration left a transaction open. MigraDry committed it on the clone \
                     so the resulting schema could be inspected."
                        .to_string(),
                );
                outcome = connection.execute_batch("COMMIT");
            }
            Err(_) => {
                warnings.push(
                    "The migration failed inside an open transaction. MigraDry rolled it back on \
                     the clone, so the schema below reflects the rollback."
                        .to_string(),
                );
                let _ = connection.execute_batch("ROLLBACK");
            }
        }
    }
    let duration_ms = elapsed_ms(started);

    // Prevention is checked by verification: whatever the SQL did, the only
    // database this connection may still be holding is the clone itself.
    assert_only_clone_is_open(&connection, working)?;

    let failure = outcome.err().map(|error| describe_failure(error, sql));
    // Close before the schema is read back, so the after-snapshot sees durable
    // state rather than anything still held by this handle.
    drop(connection);

    Ok(MigrationRun {
        duration_ms,
        failure,
        warnings,
    })
}

/// Message shown when the authorizer refuses to let SQL name a second database.
const NAMED_DATABASE_REFUSED: &str =
    "Naming another database file is not permitted during a preview: migration SQL may only \
     touch the disposable clone. ATTACH and VACUUM INTO are refused for this reason; a plain \
     VACUUM is fine.";

/// Message shown when the migration execution exceeds the allowed execution time limit.
const MIGRATION_TIMEOUT_MESSAGE: &str =
    "The migration exceeded the maximum execution time limit and was interrupted to prevent the \
     application from hanging.";

/// Locks down the connection that executes untrusted SQL.
///
/// SQLite has exactly one way for SQL to reach a second database file:
/// `ATTACH`. `VACUUM INTO` uses it internally too. Both are refused by
/// [`authorize_statement`].
///
/// The attached-database limit is lowered to one as a second, independent
/// barrier. It cannot be zero: a plain `VACUUM` needs a single slot for the
/// anonymous temporary database SQLite builds the rebuilt file in. One slot is
/// enough for SQLite's own use and leaves no room for a migration to hold
/// several databases open at once.
///
/// Extension loading stays disabled — rusqlite never calls
/// `sqlite3_enable_load_extension`, so `load_extension()` is unavailable and no
/// extension can introduce a filesystem-writing SQL function.
fn harden(connection: &Connection) -> Result<()> {
    connection
        .set_limit(Limit::SQLITE_LIMIT_ATTACHED, 1)
        .map_err(|error| MigraDryError::SafetyCheck {
            reason: format!("could not limit ATTACH on the clone connection: {error}"),
        })?;
    connection
        .authorizer(Some(authorize_statement))
        .map_err(|error| MigraDryError::SafetyCheck {
            reason: format!("could not install the SQL authorizer on the clone: {error}"),
        })?;
    Ok(())
}

/// Whether MigraDry runs migrations with referential integrity enforced.
///
/// See [`enforce_foreign_keys`] for why this is on.
pub const FOREIGN_KEYS_ENFORCED: bool = true;

/// Turns foreign-key enforcement on before the migration runs.
///
/// The value is set explicitly because the default cannot be reasoned about.
/// Upstream SQLite documents `PRAGMA foreign_keys` as **off**, for
/// compatibility with databases written before foreign keys existed — but the
/// amalgamation MigraDry links is compiled with `SQLITE_DEFAULT_FOREIGN_KEYS`,
/// which makes it **on**. So the effective default is a property of a
/// dependency's build flags, and it would silently invert if this crate were
/// ever built against a system SQLite. A migration-preview tool cannot have its
/// semantics decided that way, and a user cannot be expected to know which
/// build they got.
///
/// Enforcement is the right value to pin. Inheriting "off" would make MigraDry
/// wrong in the expensive direction: a migration that orphans rows, or drops a
/// table other tables depend on, would run cleanly on the clone and be reported
/// as safe, then fail in the application — whose connection almost certainly
/// has `foreign_keys = ON`, because every mainstream SQLite binding sets it.
///
/// Of the two ways to be wrong, a preview that says "this would fail" when the
/// user's own tooling would have let it through is a false alarm they can
/// inspect. A preview that says "this is fine" about a migration that breaks
/// referential integrity is the failure this program exists to prevent. So
/// enforcement is on, and [`crate::models::MigrationPreviewResult::foreign_keys_enforced`]
/// says so rather than leaving it to be guessed.
///
/// A migration retains the last word. The standard SQLite table-rebuild recipe
/// opens with `PRAGMA foreign_keys = OFF`, and that still works exactly as it
/// would anywhere else: the pragma is executed by SQLite as part of the batch.
/// MigraDry chooses the starting position, not the rules.
fn enforce_foreign_keys(connection: &Connection) -> Result<()> {
    // `PRAGMA foreign_keys` is a silent no-op inside a transaction, so it has
    // to be set here, before any of the migration's SQL has run.
    connection
        .pragma_update(None, "foreign_keys", FOREIGN_KEYS_ENFORCED)
        .map_err(|error| MigraDryError::SafetyCheck {
            reason: format!("could not enable foreign keys on the clone: {error}"),
        })?;

    let effective: i64 = connection
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .map_err(|error| MigraDryError::SafetyCheck {
            reason: format!("could not confirm the foreign-key setting on the clone: {error}"),
        })?;
    if (effective != 0) != FOREIGN_KEYS_ENFORCED {
        return Err(MigraDryError::SafetyCheck {
            reason: "SQLite did not apply the foreign-key setting to the clone".to_string(),
        });
    }
    Ok(())
}

/// Refuses any attempt to attach a *named* database.
///
/// SQLite raises `SQLITE_ATTACH` with an empty filename for its own private
/// temporary databases — that is how a plain `VACUUM` works internally — so the
/// empty name is allowed and everything else is denied. `VACUUM INTO 'file'`
/// arrives here carrying the target path and is therefore refused too.
///
/// This is a rule about naming SQLite databases, not a filesystem sandbox. It
/// closes the only route by which SQL could reach the original database; it is
/// not a claim that nothing reaches the disk, since SQLite still writes its own
/// journal and temporary files while executing a migration.
///
/// `DETACH` is allowed: with no named database ever attached, it cannot reach a
/// file, and denying it would break SQLite's own internal cleanup.
fn authorize_statement(context: AuthContext<'_>) -> Authorization {
    match context.action {
        AuthAction::Attach { filename } if !filename.is_empty() => Authorization::Deny,
        _ => Authorization::Allow,
    }
}

/// Verifies, after the SQL has run, that the connection is still attached to
/// nothing but the clone.
///
/// `PRAGMA database_list` reports every database the connection can see. `main`
/// must still resolve to the clone file, `temp` has no file of its own, and no
/// third entry naming a file may exist. If this ever fires, the authorizer was
/// bypassed and the run is treated as a safety failure rather than a result.
fn assert_only_clone_is_open(connection: &Connection, working: &WorkingClone) -> Result<()> {
    let mut statement = connection
        .prepare("PRAGMA database_list")
        .map_err(|error| MigraDryError::SafetyCheck {
            reason: format!("could not list the databases open on the clone: {error}"),
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?))
        })
        .map_err(|error| MigraDryError::SafetyCheck {
            reason: format!("could not list the databases open on the clone: {error}"),
        })?;

    let expected = working
        .path()
        .canonicalize()
        .unwrap_or_else(|_| working.path().to_path_buf());

    for row in rows {
        let (name, file) = row.map_err(|error| MigraDryError::SafetyCheck {
            reason: format!("could not read the database list of the clone: {error}"),
        })?;
        let file = file.unwrap_or_default();
        match name.as_str() {
            // `temp` is an anonymous scratch database and has no file.
            "temp" => {}
            "main" => {
                let actual = Path::new(&file)
                    .canonicalize()
                    .unwrap_or_else(|_| PathBuf::from(&file));
                if actual != expected {
                    return Err(MigraDryError::SafetyCheck {
                        reason:
                            "the migration connection is no longer pointed at the working clone"
                                .to_string(),
                    });
                }
            }
            _ if file.is_empty() => {}
            _ => {
                return Err(MigraDryError::SafetyCheck {
                    reason: "the migration attached another database file".to_string(),
                })
            }
        }
    }
    Ok(())
}

/// Turns a rusqlite error into the structured failure the frontend renders.
///
/// SQLite reports statement problems through two different error shapes, and
/// both carry a result code. The one that matters extra is `SqlInputError`,
/// which also carries a byte offset into the SQL still to be executed — enough
/// to name a line without inspecting the SQL in any other way.
fn describe_failure(error: rusqlite::Error, sql: &str) -> MigrationFailure {
    let (code, message, line) = match &error {
        rusqlite::Error::SqliteFailure(failure, message) => (
            Some(*failure),
            message.clone().unwrap_or_else(|| error.to_string()),
            None,
        ),
        rusqlite::Error::SqlInputError {
            error: failure,
            msg,
            sql: remaining,
            offset,
        } => (
            Some(*failure),
            msg.clone(),
            line_of(sql, remaining, *offset),
        ),
        other => (None, other.to_string(), None),
    };

    match code {
        Some(failure) => {
            let message = if failure.code == ErrorCode::AuthorizationForStatementDenied {
                // Attaching a named database is the only thing the authorizer
                // ever denies, so the code alone identifies the cause.
                NAMED_DATABASE_REFUSED.to_string()
            } else if failure.code == ErrorCode::OperationInterrupted {
                MIGRATION_TIMEOUT_MESSAGE.to_string()
            } else {
                message
            };
            MigrationFailure {
                message,
                sqlite_code: Some(symbolic_code(failure.code).to_string()),
                sqlite_extended_code: Some(failure.extended_code),
                line,
            }
        }
        None => MigrationFailure {
            message,
            sqlite_code: None,
            sqlite_extended_code: None,
            line: None,
        },
    }
}

/// Converts SQLite's offset into a line number in the migration file.
///
/// `remaining` is the portion of the script SQLite had left to execute, and is
/// therefore a suffix of the whole file; `offset` is measured from its start.
/// If that relationship does not hold, no line is reported rather than a wrong
/// one.
///
/// The offset arrives from SQLite as a byte count, and the slice it indexes is
/// UTF-8. In practice SQLite points at the start of a token, which is always a
/// character boundary — but "in practice" is not a guarantee worth staking a
/// panic on, so the slice is taken with `get`, and an offset that lands mid
/// character yields no line number rather than a crash.
fn line_of(full_sql: &str, remaining: &str, offset: i32) -> Option<u32> {
    let offset = usize::try_from(offset).ok()?;
    let consumed = full_sql.len().checked_sub(remaining.len())?;
    if !full_sql.ends_with(remaining) {
        return None;
    }
    let absolute = consumed.checked_add(offset)?;
    let preceding = full_sql.get(..absolute)?;
    let line = preceding.bytes().filter(|byte| *byte == b'\n').count() + 1;
    u32::try_from(line).ok()
}

fn symbolic_code(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::InternalMalfunction => "SQLITE_INTERNAL",
        ErrorCode::PermissionDenied => "SQLITE_PERM",
        ErrorCode::OperationAborted => "SQLITE_ABORT",
        ErrorCode::DatabaseBusy => "SQLITE_BUSY",
        ErrorCode::DatabaseLocked => "SQLITE_LOCKED",
        ErrorCode::OutOfMemory => "SQLITE_NOMEM",
        ErrorCode::ReadOnly => "SQLITE_READONLY",
        ErrorCode::OperationInterrupted => "SQLITE_INTERRUPT",
        ErrorCode::SystemIoFailure => "SQLITE_IOERR",
        ErrorCode::DatabaseCorrupt => "SQLITE_CORRUPT",
        ErrorCode::NotFound => "SQLITE_NOTFOUND",
        ErrorCode::DiskFull => "SQLITE_FULL",
        ErrorCode::CannotOpen => "SQLITE_CANTOPEN",
        ErrorCode::FileLockingProtocolFailed => "SQLITE_PROTOCOL",
        ErrorCode::SchemaChanged => "SQLITE_SCHEMA",
        ErrorCode::TooBig => "SQLITE_TOOBIG",
        ErrorCode::ConstraintViolation => "SQLITE_CONSTRAINT",
        ErrorCode::TypeMismatch => "SQLITE_MISMATCH",
        ErrorCode::ApiMisuse => "SQLITE_MISUSE",
        ErrorCode::NoLargeFileSupport => "SQLITE_NOLFS",
        ErrorCode::AuthorizationForStatementDenied => "SQLITE_AUTH",
        ErrorCode::ParameterOutOfRange => "SQLITE_RANGE",
        ErrorCode::NotADatabase => "SQLITE_NOTADB",
        ErrorCode::Unknown => "SQLITE_ERROR",
        _ => "SQLITE_ERROR",
    }
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// Turns a database and a migration file into a [`MigrationPreviewResult`].
///
/// The sequence is fixed:
///
///  1. validate both inputs;
///  2. fingerprint the original *before* anything opens it;
///  3. build the workspace — a baseline taken from the original, and a working
///     clone taken from the baseline (read-only source access only);
///  4. fingerprint the original again — if it moved while it was being read,
///     stop, because the baseline would describe no single state of it;
///  5. fingerprint the baseline;
///  6. snapshot the baseline's schema — this is the "before";
///  7. execute the migration against the working clone;
///  8. snapshot the working clone — this is the "after";
///  9. diff, then count what the diff says would be discarded, reading the
///     baseline and only the objects the diff flagged;
/// 10. fingerprint the baseline again and require it unchanged;
/// 11. destroy the workspace;
/// 12. fingerprint the original a third time and compare.
///
/// The "before" schema comes from the baseline rather than from the original.
/// That is deliberate twice over: it is the same content, and it means the
/// original is opened exactly once, for the copy alone.
///
/// Step 4 is the one that earns its keep. Without it, a source modified during
/// the copy would only be caught at step 12, after a migration had been run
/// against a snapshot that was already meaningless.
///
/// Step 9 is why the baseline exists at all. By the time the diff knows a table
/// is being dropped, the working clone no longer has it — and the original may
/// have moved on. Only the baseline still holds the exact state the preview is
/// describing.
pub struct MigrationService;

impl MigrationService {
    pub fn preview(database_path: &Path, migration_path: &Path) -> Result<MigrationPreviewResult> {
        Self::preview_with_timeout(database_path, migration_path, DEFAULT_MIGRATION_TIMEOUT)
    }

    pub fn preview_with_timeout(
        database_path: &Path,
        migration_path: &Path,
        timeout: Duration,
    ) -> Result<MigrationPreviewResult> {
        let started = Instant::now();

        database::validate_database_path(database_path)?;
        let sql = load_migration_sql(migration_path)?;
        ensure_distinct_inputs(database_path, migration_path)?;

        let mut warnings = Vec::new();
        if sql.trim().is_empty() {
            warnings.push(
                "The migration file contains no SQL statements, so nothing would change."
                    .to_string(),
            );
        }

        // Fingerprint before anything opens the file, so the window under
        // observation starts before MigraDry's first read.
        let before_fingerprint = database::fingerprint(database_path)?;
        let shm_path = database::sidecar_path(database_path, "-shm");
        let shm_existed = shm_path.exists();

        let workspace = PreviewWorkspace::create(database_path)?;
        let clone_strategy = workspace.strategy();

        // Checkpoint one: did the source hold still while it was being read?
        //
        // If it did not, the baseline is a blend of two states and everything
        // downstream would be describing a database that never existed. Fail
        // here, before spending time running a migration on it.
        let snapshot_fingerprint = require_fingerprint(database_path)?;
        if !before_fingerprint.content_matches(&snapshot_fingerprint) {
            return Err(MigraDryError::SourceChangedDuringSnapshot);
        }

        let shm_created_by_preview = !shm_existed && shm_path.exists();
        if shm_created_by_preview {
            warnings.push(
                "This database uses write-ahead logging, so SQLite created a -shm shared-memory \
                 index beside it in order to read the log. Every reader of a WAL database does \
                 this, and the -shm holds no database content. The database file and its -wal \
                 were left byte-identical."
                    .to_string(),
            );
        }

        // The baseline is the state this preview is *about*. Fingerprinting it
        // here, and again once every count has been taken, turns "nothing
        // writes to the baseline" from a claim about the code into something
        // the engine checks on every run.
        let baseline_before = database::fingerprint(workspace.baseline().path())?;

        let schema_before = schema::snapshot(&workspace.baseline().open_readonly()?)?;
        let run = execute_on_clone_with_timeout(workspace.working(), &sql, timeout)?;
        let schema_after = snapshot_working(workspace.working())?;

        let mut schema_changes = diff::diff(&schema_before, &schema_after);

        // Exact counts for whatever the migration would discard, taken from the
        // baseline and only for the objects the diff actually flagged. A
        // migration with nothing destructive in it never reads a single row.
        if run.failure.is_none() {
            warnings.extend(impact::measure(workspace.baseline(), &mut schema_changes));
        } else if schema_changes
            .iter()
            .any(|change| change.data_target().is_some())
        {
            // A migration that stopped part-way has no settled answer to "how
            // much would this remove?", because it is not going to remove it.
            // Saying nothing beats guessing.
            warnings.push(
                "Data impact was not calculated: the migration did not run to completion, so \
                 there is no final state to measure against."
                    .to_string(),
            );
        }

        let baseline_after = database::fingerprint(workspace.baseline().path())?;
        if !baseline_before.content_matches(&baseline_after) {
            return Err(MigraDryError::SafetyCheck {
                reason: "the baseline snapshot changed during the preview; the impact counts \
                         would describe a different database from the one that was compared"
                    .to_string(),
            });
        }

        // The workspace has served its purpose. Dropping it deletes the
        // temporary directory and both databases inside it.
        drop(workspace);

        // Checkpoint two: is the source still what it was when we read it?
        let after_fingerprint = require_fingerprint(database_path)?;
        let content_unchanged = before_fingerprint.content_matches(&after_fingerprint);
        if !content_unchanged {
            // Never downgraded to a field on an otherwise cheerful result, and
            // never blamed on anyone: MigraDry can prove what it does, not what
            // the rest of the machine did.
            return Err(MigraDryError::SourceChangedAfterSnapshot);
        }

        warnings.extend(run.warnings);
        if run.failure.is_some() && !schema_changes.is_empty() {
            warnings.push(
                "The migration failed part-way through. The changes below are the statements \
                 that had already been applied to the disposable clone when it stopped. They \
                 are not a migration that succeeded."
                    .to_string(),
            );
        }

        let wal_checked = before_fingerprint.wal.is_some() || after_fingerprint.wal.is_some();

        Ok(MigrationPreviewResult {
            success: run.failure.is_none(),
            database_path: database_path.to_string_lossy().into_owned(),
            database_name: display_name(database_path),
            migration_path: migration_path.to_string_lossy().into_owned(),
            migration_name: display_name(migration_path),
            duration_ms: run.duration_ms,
            total_duration_ms: elapsed_ms(started),
            destructive_change_count: diff::destructive_count(&schema_changes),
            advisory_change_count: diff::advisory_count(&schema_changes),
            schema_changes,
            original_content_unchanged: content_unchanged,
            foreign_keys_enforced: FOREIGN_KEYS_ENFORCED,
            original_integrity: OriginalIntegrity {
                content_unchanged,
                before: before_fingerprint,
                after: after_fingerprint,
                wal_checked,
                shm_created_by_preview,
            },
            clone_strategy,
            warnings,
            error: run.failure,
        })
    }
}

/// Re-reads the source fingerprint during verification.
///
/// A database that has vanished gets its own error: "you gave me a path that
/// does not exist" and "the file went away underneath me" are different
/// problems and deserve different words.
fn require_fingerprint(path: &Path) -> Result<crate::models::DatabaseFingerprint> {
    database::fingerprint_if_present(path)?.ok_or(MigraDryError::SourceDisappeared)
}

fn snapshot_working(working: &WorkingClone) -> Result<SchemaSnapshot> {
    schema::snapshot(&working.open_readonly()?)
}

/// Rejects the case where the same file was chosen twice.
fn ensure_distinct_inputs(database_path: &Path, migration_path: &Path) -> Result<()> {
    let database = database_path
        .canonicalize()
        .unwrap_or_else(|_| database_path.into());
    let migration = migration_path
        .canonicalize()
        .unwrap_or_else(|_| migration_path.into());
    if database == migration {
        return Err(MigraDryError::SamePath);
    }
    Ok(())
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &[u8]) {
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn reads_plain_sql() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("001_init.sql");
        write(&path, b"CREATE TABLE t (id INTEGER);\n");
        assert_eq!(
            load_migration_sql(&path).unwrap(),
            "CREATE TABLE t (id INTEGER);\n"
        );
    }

    #[test]
    fn strips_a_byte_order_mark() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bom.sql");
        write(&path, "\u{feff}SELECT 1;".as_bytes());
        assert_eq!(load_migration_sql(&path).unwrap(), "SELECT 1;");
    }

    #[test]
    fn rejects_a_missing_migration() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            load_migration_sql(&dir.path().join("nope.sql")),
            Err(MigraDryError::MigrationNotFound { .. })
        ));
    }

    #[test]
    fn rejects_a_directory_as_a_migration() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            load_migration_sql(dir.path()),
            Err(MigraDryError::MigrationNotAFile { .. })
        ));
    }

    #[test]
    fn rejects_a_database_picked_as_a_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE t (id INTEGER);")
            .unwrap();
        assert!(matches!(
            load_migration_sql(&path),
            Err(MigraDryError::MigrationNotSql { .. })
        ));
    }

    #[test]
    fn rejects_binary_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob.sql");
        write(&path, &[0x53, 0x00, 0x51, 0x4c]);
        assert!(matches!(
            load_migration_sql(&path),
            Err(MigraDryError::MigrationNotSql { .. })
        ));
    }

    #[test]
    fn rejects_invalid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("latin1.sql");
        write(&path, &[0x53, 0x45, 0x4c, 0xff, 0xfe, 0x45]);
        assert!(matches!(
            load_migration_sql(&path),
            Err(MigraDryError::MigrationNotSql { .. })
        ));
    }

    // --- bounded reading -------------------------------------------------

    #[test]
    fn a_file_exactly_at_the_limit_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exact.sql");
        write(&path, &[b'-'; 10]);
        assert_eq!(read_bounded(&path, 10).unwrap(), Some(vec![b'-'; 10]));
    }

    #[test]
    fn a_file_one_byte_over_the_limit_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("over.sql");
        write(&path, &[b'-'; 11]);
        assert_eq!(read_bounded(&path, 10).unwrap(), None);
    }

    #[test]
    fn an_empty_file_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.sql");
        write(&path, b"");
        assert_eq!(read_bounded(&path, 10).unwrap(), Some(Vec::new()));
    }

    /// The bound is on bytes actually read, so it holds for a file far larger
    /// than the limit without the whole file ever being allocated.
    #[test]
    fn a_much_larger_file_is_refused_without_being_read_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.sql");
        write(&path, &vec![b'x'; 1_000_000]);
        assert_eq!(read_bounded(&path, 64).unwrap(), None);
    }

    /// The reason a prior `stat` is not a bound: this file reports a length of
    /// zero and yields over a kilobyte. A size check followed by an unbounded
    /// read would have let all of it through.
    #[test]
    fn a_file_whose_metadata_understates_its_size_is_still_bounded() {
        let path = Path::new("/proc/self/status");
        if !path.is_file() {
            return; // not Linux; nothing to demonstrate.
        }
        assert_eq!(
            std::fs::metadata(path).unwrap().len(),
            0,
            "fixture assumption: this file reports zero length"
        );
        let actual = std::fs::read(path).unwrap().len();
        assert!(actual > 64, "fixture assumption: it yields real content");

        // The old shape — trust `metadata().len()`, then read — would have
        // accepted all of it. The bound is what stops that.
        assert_eq!(read_bounded(path, 64).unwrap(), None);
        let bounded = read_bounded(path, actual as u64 + 100).unwrap().unwrap();
        assert!(bounded.len() > 64);
        assert!(
            (bounded.len() as isize - actual as isize).abs() < 100,
            "procfs status length is bounded and read matches actual status length within jitter"
        );
    }

    #[test]
    fn a_migration_over_the_real_limit_is_refused_with_a_clear_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.sql");
        write(&path, &vec![b'-'; (MAX_MIGRATION_BYTES + 1) as usize]);
        match load_migration_sql(&path) {
            Err(MigraDryError::MigrationNotSql { reason }) => {
                assert!(reason.contains("larger than"), "reason: {reason}");
                assert!(reason.contains(&MAX_MIGRATION_BYTES.to_string()));
            }
            other => panic!("expected a size refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_migration_exactly_at_the_real_limit_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("max.sql");
        // Valid UTF-8 SQL comment padding, exactly at the limit.
        let mut content = b"-- ".to_vec();
        content.resize(MAX_MIGRATION_BYTES as usize, b'x');
        write(&path, &content);
        let sql = load_migration_sql(&path).expect("a file at the limit is allowed");
        assert_eq!(sql.len() as u64, MAX_MIGRATION_BYTES);
    }

    #[test]
    fn accepts_an_empty_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.sql");
        write(&path, b"");
        assert_eq!(load_migration_sql(&path).unwrap(), "");
    }
}
