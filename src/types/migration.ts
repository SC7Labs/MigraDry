/**
 * TypeScript mirror of the Rust data contract in `src-tauri/src/models.rs`.
 *
 * Rust serializes struct fields as camelCase and enum variants as snake_case,
 * so these names match the wire format exactly. Changing one side without the
 * other will break the bridge, and `preview_engine.rs::the_result_round_trips_through_json`
 * pins the format from the Rust side.
 */

export type SchemaObjectKind = "table" | "index" | "view" | "trigger";

export type SchemaChangeKind =
  | "table_added"
  | "table_removed"
  | "column_added"
  | "column_removed"
  | "column_modified"
  | "table_definition_changed"
  | "index_added"
  | "index_removed"
  | "view_added"
  | "view_removed"
  | "trigger_added"
  | "trigger_removed";

/** How much a change should worry the person reading the preview. */
export type ChangeImpact = "additive" | "advisory" | "destructive";

/**
 * How the disposable clone was produced.
 *
 * There is exactly one mechanism. MigraDry will not copy database files itself,
 * because such a copy cannot be proven consistent against a concurrent writer;
 * when SQLite cannot provide a snapshot, the preview fails with
 * `snapshot_not_possible` instead.
 */
export type CloneStrategy = "sqlite_backup_api";

/**
 * One attribute of a surviving column that the migration changed.
 *
 * A discriminated union rather than a pair of strings, so each property can be
 * rendered in its own terms.
 */
/**
 * How a column gets its value.
 *
 * Read from `pragma_table_xinfo`, which — unlike `pragma_table_info` — reports
 * generated columns at all.
 */
export type ColumnKind =
  | "ordinary"
  | "virtual_generated"
  | "stored_generated";

export type ColumnPropertyChange =
  | { property: "declared_type"; before: string; after: string }
  | { property: "not_null"; before: boolean; after: boolean }
  | {
      property: "default_value";
      before: string | null;
      after: string | null;
    }
  | { property: "primary_key_position"; before: number; after: number }
  | { property: "generated"; before: ColumnKind; after: ColumnKind };

/**
 * How much stored data a destructive change would discard.
 *
 * Every number is an exact `COUNT` taken from the baseline snapshot the preview
 * was computed from — not an estimate. `unavailable` exists so a count that
 * could not be taken is never mistaken for a measured zero.
 */
export type DataImpact =
  | {
      status: "measured";
      table: string;
      /** `null` when the whole table goes. */
      column: string | null;
      totalRows: number;
      affectedRows: number;
    }
  | {
      status: "unavailable";
      table: string;
      column: string | null;
      reason: string;
    };

export interface SchemaChange {
  kind: SchemaChangeKind;
  objectName: string;
  /** Owning table for column, index and trigger changes. */
  parentName: string | null;
  impact: ChangeImpact;
  /** Populated only for `column_modified`, and never empty when it is. */
  propertyChanges: ColumnPropertyChange[];
  /**
   * Populated only for changes that discard data, and only when the migration
   * ran to completion. `null` means "not applicable", which is a different
   * thing from a measured zero.
   */
  dataImpact: DataImpact | null;
}

export interface FileFingerprint {
  sizeBytes: number;
  sha256: string;
  modifiedUnixMs: number | null;
}

export interface DatabaseFingerprint {
  main: FileFingerprint;
  wal: FileFingerprint | null;
}

export interface OriginalIntegrity {
  /**
   * The durable *content* of the database — the main file and its `-wal` — was
   * byte-identical before and after.
   *
   * Deliberately narrower than "the directory is untouched": see
   * `shmCreatedByPreview`.
   */
  contentUnchanged: boolean;
  before: DatabaseFingerprint;
  after: DatabaseFingerprint;
  walChecked: boolean;
  /**
   * Reading a WAL database requires a shared-memory index, so SQLite creates a
   * `-shm` file beside it. Every reader does this; it holds no database
   * content. Reported so that "content unchanged" is never mistaken for "no
   * file appeared".
   */
  shmCreatedByPreview: boolean;
}

export interface MigrationFailure {
  message: string;
  sqliteCode: string | null;
  sqliteExtendedCode: number | null;
  /** 1-based line in the migration file, when SQLite reported an offset. */
  line: number | null;
}

export interface MigrationPreviewResult {
  success: boolean;
  databasePath: string;
  databaseName: string;
  migrationPath: string;
  migrationName: string;
  /** Time spent executing the migration against the clone. */
  durationMs: number;
  /** Time spent on the whole preview, including cloning and hashing. */
  totalDurationMs: number;
  schemaChanges: SchemaChange[];
  destructiveChangeCount: number;
  advisoryChangeCount: number;
  /** The durable content of the original was byte-identical before and after. */
  originalContentUnchanged: boolean;
  originalIntegrity: OriginalIntegrity;
  /** Whether `PRAGMA foreign_keys` was on while the migration ran. */
  foreignKeysEnforced: boolean;
  cloneStrategy: CloneStrategy;
  warnings: string[];
  error: MigrationFailure | null;
}

export type PreviewErrorKind =
  | "invalid_input"
  | "invalid_database"
  | "invalid_migration"
  | "not_sqlite_database"
  | "clone_failed"
  | "snapshot_not_possible"
  | "schema_read_failed"
  | "source_changed"
  | "safety_check";

/** The preview could not be produced at all. */
export interface PreviewError {
  kind: PreviewErrorKind;
  message: string;
}

/** Narrows an unknown rejection value into a `PreviewError`. */
export function asPreviewError(value: unknown): PreviewError {
  if (
    typeof value === "object" &&
    value !== null &&
    "kind" in value &&
    "message" in value &&
    typeof (value as PreviewError).message === "string"
  ) {
    return value as PreviewError;
  }
  return {
    kind: "invalid_input",
    message: value instanceof Error ? value.message : String(value),
  };
}
