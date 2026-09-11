/**
 * Presentation helpers.
 *
 * All display wording lives here rather than in the Rust result model, and all
 * of it is pure so it can be unit tested without rendering anything.
 */

import type {
  CloneStrategy,
  ColumnKind,
  ColumnPropertyChange,
  DataImpact,
  MigrationPreviewResult,
  SchemaChange,
  SchemaChangeKind,
} from "../types/migration";

const ADDED_KINDS: ReadonlySet<SchemaChangeKind> = new Set([
  "table_added",
  "column_added",
  "index_added",
  "view_added",
  "trigger_added",
]);

const COLUMN_KINDS: ReadonlySet<SchemaChangeKind> = new Set([
  "column_added",
  "column_removed",
  "column_modified",
]);

const OBJECT_LABELS: Record<SchemaChangeKind, string> = {
  table_added: "TABLE",
  table_removed: "TABLE",
  column_added: "COLUMN",
  column_removed: "COLUMN",
  column_modified: "COLUMN",
  table_definition_changed: "TABLE",
  index_added: "INDEX",
  index_removed: "INDEX",
  view_added: "VIEW",
  view_removed: "VIEW",
  trigger_added: "TRIGGER",
  trigger_removed: "TRIGGER",
};

/** `+` created, `-` removed, `~` altered in place. */
export function changeSymbol(kind: SchemaChangeKind): "+" | "-" | "~" {
  if (kind === "column_modified" || kind === "table_definition_changed") {
    return "~";
  }
  return ADDED_KINDS.has(kind) ? "+" : "-";
}

export function isAddition(kind: SchemaChangeKind): boolean {
  return ADDED_KINDS.has(kind);
}

/** `TABLE orders`, `COLUMN users.last_login`, `INDEX idx_old_email`. */
export function changeLabel(change: SchemaChange): string {
  const object = OBJECT_LABELS[change.kind];
  const name = COLUMN_KINDS.has(change.kind)
    ? `${change.parentName ?? "?"}.${change.objectName}`
    : change.objectName;
  return `${object} ${name}`;
}

/** Short note explaining why a change is or is not worth worrying about. */
export function changeNote(change: SchemaChange): string | null {
  switch (change.impact) {
    case "destructive":
      return "data loss";
    case "advisory":
      switch (change.kind) {
        case "column_modified":
          return "metadata change";
        case "table_definition_changed":
          return "definition changed";
        default:
          return "support object";
      }
    case "additive":
      return null;
  }
}

const PROPERTY_LABELS: Record<ColumnPropertyChange["property"], string> = {
  declared_type: "type",
  not_null: "NOT NULL",
  default_value: "default",
  primary_key_position: "primary key position",
  generated: "generated",
};

const COLUMN_KIND_LABELS: Record<ColumnKind, string> = {
  ordinary: "no",
  virtual_generated: "VIRTUAL",
  stored_generated: "STORED",
};

/**
 * A property change as three pieces the UI lays out itself.
 *
 * The declared type is shown exactly as it was written in both schemas.
 * MigraDry compares declarations, not semantics, so this says the declaration
 * changed — never that data was converted.
 */
export function describePropertyChange(change: ColumnPropertyChange): {
  label: string;
  before: string;
  after: string;
} {
  const label = PROPERTY_LABELS[change.property];
  switch (change.property) {
    case "declared_type":
      return { label, before: change.before, after: change.after };
    case "not_null":
      return {
        label,
        before: String(change.before),
        after: String(change.after),
      };
    case "default_value":
      return {
        label,
        before: change.before ?? "none",
        after: change.after ?? "none",
      };
    case "primary_key_position":
      return {
        label,
        before: String(change.before),
        after: String(change.after),
      };
    case "generated":
      return {
        label,
        before: COLUMN_KIND_LABELS[change.before],
        after: COLUMN_KIND_LABELS[change.after],
      };
  }
}

/** `48102` becomes `48,102`. Deterministic, and not a localization system. */
export function formatCount(value: number): string {
  return String(value).replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}

/**
 * What a destructive change would actually cost, in rows.
 *
 * A column whose values are all null still disappears, so the wording never
 * reduces that case to "no impact" — the schema change is real either way.
 */
export function describeDataImpact(impact: DataImpact): string {
  if (impact.status === "unavailable") {
    return `Impact could not be calculated: ${impact.reason}`;
  }

  const rows = formatCount(impact.totalRows);
  if (impact.column === null) {
    return impact.totalRows === 0
      ? "The table is empty; it would still be removed"
      : `${rows} ${impact.totalRows === 1 ? "row" : "rows"} would be removed`;
  }

  if (impact.affectedRows === 0) {
    return `No non-null values across ${rows} ${
      impact.totalRows === 1 ? "row" : "rows"
    }; the column would still be removed`;
  }
  return `${formatCount(impact.affectedRows)} non-null ${
    impact.affectedRows === 1 ? "value" : "values"
  } across ${rows} ${impact.totalRows === 1 ? "row" : "rows"} would be removed`;
}

export function formatDuration(milliseconds: number): string {
  if (milliseconds < 1000) {
    return `${milliseconds} ms`;
  }
  return `${(milliseconds / 1000).toFixed(2)} s`;
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) {
    return `${bytes} B`;
  }
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(1)} ${units[unit]}`;
}

/** First 12 hex characters, which is plenty for a human eyeball comparison. */
export function shortHash(sha256: string): string {
  return sha256.slice(0, 12);
}

/** One-line summary of what the migration would do. */
export function summarizeChanges(result: MigrationPreviewResult): string {
  const total = result.schemaChanges.length;
  if (total === 0) {
    return "No schema changes";
  }
  const parts = [`${total} schema ${total === 1 ? "change" : "changes"}`];
  if (result.destructiveChangeCount > 0) {
    parts.push(`${result.destructiveChangeCount} potentially destructive`);
  }
  if (result.advisoryChangeCount > 0) {
    parts.push(
      `${result.advisoryChangeCount} advisory ${
        result.advisoryChangeCount === 1 ? "change" : "changes"
      }`,
    );
  }
  return parts.join(" · ");
}

export function describeCloneStrategy(strategy: CloneStrategy): string {
  switch (strategy) {
    case "sqlite_backup_api":
      return "SQLite online backup (consistent snapshot)";
  }
}

/**
 * Heading for the change list.
 *
 * A failed migration still has changes worth showing — the statements that ran
 * before it stopped — but they must never read as a migration that worked.
 */
export function changeListHeading(result: MigrationPreviewResult): string {
  return result.success
    ? "Changes this migration would make"
    : "Changes applied to the disposable clone before failure";
}

/** What to say when the change list is empty. */
export function emptyChangeMessage(result: MigrationPreviewResult): string {
  return result.success
    ? "This migration would not add, remove or alter any tables, columns, indexes, views or triggers."
    : "The migration failed before it changed the schema of the clone.";
}

/** One line covering both halves of the integrity claim. */
export function describeIntegrity(result: MigrationPreviewResult): string {
  const content = result.originalIntegrity.contentUnchanged
    ? "Original database content unchanged"
    : "Original database content CHANGED";
  return result.originalIntegrity.shmCreatedByPreview
    ? `${content} · SQLite added a -shm sidecar`
    : content;
}

/** Basename of a path, for showing a file rather than a directory layout. */
export function fileName(path: string): string {
  const trimmed = path.trim().replace(/[\\/]+$/, "");
  const separator = Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\"));
  return separator === -1 ? trimmed : trimmed.slice(separator + 1);
}
