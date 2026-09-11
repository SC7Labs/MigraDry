import { describe, expect, it } from "vitest";
import type {
  ColumnPropertyChange,
  DataImpact,
  MigrationPreviewResult,
  SchemaChange,
} from "../types/migration";
import {
  changeLabel,
  changeListHeading,
  changeNote,
  changeSymbol,
  describeCloneStrategy,
  describeDataImpact,
  describeIntegrity,
  describePropertyChange,
  emptyChangeMessage,
  fileName,
  formatBytes,
  formatCount,
  formatDuration,
  isAddition,
  shortHash,
  summarizeChanges,
} from "./format";

function change(overrides: Partial<SchemaChange> = {}): SchemaChange {
  return {
    kind: "table_added",
    objectName: "orders",
    parentName: null,
    impact: "additive",
    propertyChanges: [],
    dataImpact: null,
    ...overrides,
  };
}

function measured(overrides: Partial<Extract<DataImpact, { status: "measured" }>> = {}): DataImpact {
  return {
    status: "measured",
    table: "events",
    column: null,
    totalRows: 10,
    affectedRows: 10,
    ...overrides,
  };
}

function result(
  overrides: Partial<MigrationPreviewResult> = {},
): MigrationPreviewResult {
  const fingerprint = {
    main: { sizeBytes: 4096, sha256: "a".repeat(64), modifiedUnixMs: null },
    wal: null,
  };
  return {
    success: true,
    databasePath: "/path/to/app.db",
    databaseName: "app.db",
    migrationPath: "/path/to/001.sql",
    migrationName: "001.sql",
    durationMs: 12,
    totalDurationMs: 30,
    schemaChanges: [],
    destructiveChangeCount: 0,
    advisoryChangeCount: 0,
    originalContentUnchanged: true,
    foreignKeysEnforced: true,
    originalIntegrity: {
      contentUnchanged: true,
      before: fingerprint,
      after: fingerprint,
      walChecked: false,
      shmCreatedByPreview: false,
    },
    cloneStrategy: "sqlite_backup_api",
    warnings: [],
    error: null,
    ...overrides,
  };
}

describe("changeSymbol", () => {
  it("marks creations with a plus", () => {
    expect(changeSymbol("table_added")).toBe("+");
    expect(changeSymbol("column_added")).toBe("+");
    expect(changeSymbol("trigger_added")).toBe("+");
  });

  it("marks removals with a minus", () => {
    expect(changeSymbol("table_removed")).toBe("-");
    expect(changeSymbol("index_removed")).toBe("-");
  });

  it("marks a column altered in place with a tilde", () => {
    expect(changeSymbol("column_modified")).toBe("~");
    expect(isAddition("column_modified")).toBe(false);
  });

  it("marks a changed table definition with a tilde", () => {
    expect(changeSymbol("table_definition_changed")).toBe("~");
    expect(isAddition("table_definition_changed")).toBe(false);
  });

  it("agrees with isAddition", () => {
    expect(isAddition("view_added")).toBe(true);
    expect(isAddition("view_removed")).toBe(false);
  });
});

describe("changeLabel", () => {
  it("names a table on its own", () => {
    expect(changeLabel(change())).toBe("TABLE orders");
  });

  it("qualifies a column with its table", () => {
    expect(
      changeLabel(
        change({
          kind: "column_added",
          objectName: "last_login",
          parentName: "users",
        }),
      ),
    ).toBe("COLUMN users.last_login");
  });

  it("qualifies a modified column with its table", () => {
    expect(
      changeLabel(
        change({
          kind: "column_modified",
          objectName: "email",
          parentName: "users",
        }),
      ),
    ).toBe("COLUMN users.email");
  });

  it("does not qualify an index with its table", () => {
    expect(
      changeLabel(
        change({
          kind: "index_removed",
          objectName: "idx_old_email",
          parentName: "users",
        }),
      ),
    ).toBe("INDEX idx_old_email");
  });

  it("survives a column with no recorded parent", () => {
    expect(
      changeLabel(change({ kind: "column_removed", objectName: "legacy" })),
    ).toBe("COLUMN ?.legacy");
  });
});

describe("changeNote", () => {
  it("separates data loss from support objects", () => {
    expect(changeNote(change({ impact: "destructive" }))).toBe("data loss");
    expect(
      changeNote(change({ kind: "index_removed", impact: "advisory" })),
    ).toBe("support object");
    expect(changeNote(change({ impact: "additive" }))).toBeNull();
  });

  it("calls a column modification what it is", () => {
    expect(
      changeNote(change({ kind: "column_modified", impact: "advisory" })),
    ).toBe("metadata change");
  });

  it("labels a table definition change without claiming to know the cause", () => {
    expect(
      changeNote(change({ kind: "table_definition_changed", impact: "advisory" })),
    ).toBe("definition changed");
  });

  it("labels a changed table definition as a table", () => {
    expect(
      changeLabel(change({ kind: "table_definition_changed", objectName: "orders" })),
    ).toBe("TABLE orders");
  });
});

describe("describePropertyChange", () => {
  it("shows both declarations verbatim", () => {
    expect(
      describePropertyChange({
        property: "declared_type",
        before: "TEXT",
        after: "VARCHAR(255)",
      }),
    ).toEqual({ label: "type", before: "TEXT", after: "VARCHAR(255)" });
  });

  it("renders nullability as a boolean pair", () => {
    expect(
      describePropertyChange({
        property: "not_null",
        before: false,
        after: true,
      }),
    ).toEqual({ label: "NOT NULL", before: "false", after: "true" });
  });

  it("names the absence of a default rather than printing null", () => {
    expect(
      describePropertyChange({
        property: "default_value",
        before: null,
        after: "'active'",
      }),
    ).toEqual({ label: "default", before: "none", after: "'active'" });
  });

  it("renders primary-key position", () => {
    expect(
      describePropertyChange({
        property: "primary_key_position",
        before: 0,
        after: 1,
      }),
    ).toEqual({ label: "primary key position", before: "0", after: "1" });
  });

  it("renders a column becoming generated", () => {
    expect(
      describePropertyChange({
        property: "generated",
        before: "ordinary",
        after: "stored_generated",
      }),
    ).toEqual({ label: "generated", before: "no", after: "STORED" });
  });

  it("distinguishes virtual from stored", () => {
    expect(
      describePropertyChange({
        property: "generated",
        before: "virtual_generated",
        after: "stored_generated",
      }),
    ).toEqual({ label: "generated", before: "VIRTUAL", after: "STORED" });
  });

  it("covers every property in the union", () => {
    const all: ColumnPropertyChange[] = [
      { property: "declared_type", before: "a", after: "b" },
      { property: "not_null", before: true, after: false },
      { property: "default_value", before: "1", after: "2" },
      { property: "primary_key_position", before: 1, after: 2 },
      { property: "generated", before: "ordinary", after: "virtual_generated" },
    ];
    for (const property of all) {
      const rendered = describePropertyChange(property);
      expect(rendered.label).not.toBe("");
      expect(rendered.label).toBeDefined();
      expect(rendered.before).toBeDefined();
      expect(rendered.after).toBeDefined();
    }
  });
});

describe("formatCount", () => {
  it("groups thousands", () => {
    expect(formatCount(0)).toBe("0");
    expect(formatCount(999)).toBe("999");
    expect(formatCount(1000)).toBe("1,000");
    expect(formatCount(48102)).toBe("48,102");
    expect(formatCount(126441)).toBe("126,441");
    expect(formatCount(1234567)).toBe("1,234,567");
  });
});

describe("describeDataImpact", () => {
  it("counts every row of a dropped table", () => {
    expect(
      describeDataImpact(measured({ totalRows: 126441, affectedRows: 126441 })),
    ).toBe("126,441 rows would be removed");
  });

  it("says an empty table is still removed", () => {
    expect(describeDataImpact(measured({ totalRows: 0, affectedRows: 0 }))).toBe(
      "The table is empty; it would still be removed",
    );
  });

  it("counts non-null values for a dropped column", () => {
    expect(
      describeDataImpact(
        measured({
          table: "users",
          column: "legacy_code",
          totalRows: 51337,
          affectedRows: 48102,
        }),
      ),
    ).toBe("48,102 non-null values across 51,337 rows would be removed");
  });

  it("never reduces an all-null column to no impact", () => {
    const sentence = describeDataImpact(
      measured({
        table: "users",
        column: "unused",
        totalRows: 40,
        affectedRows: 0,
      }),
    );
    expect(sentence).toBe(
      "No non-null values across 40 rows; the column would still be removed",
    );
    expect(sentence).not.toContain("no impact");
  });

  it("uses singulars where they read better", () => {
    expect(describeDataImpact(measured({ totalRows: 1, affectedRows: 1 }))).toBe(
      "1 row would be removed",
    );
  });

  it("reports an unavailable count as unavailable, never as zero", () => {
    const sentence = describeDataImpact({
      status: "unavailable",
      table: "users",
      column: "legacy_code",
      reason: "no such table: users",
    });
    expect(sentence).toBe(
      "Impact could not be calculated: no such table: users",
    );
    expect(sentence).not.toContain("0");
  });
});

describe("formatDuration", () => {
  it("uses milliseconds below a second", () => {
    expect(formatDuration(0)).toBe("0 ms");
    expect(formatDuration(38)).toBe("38 ms");
    expect(formatDuration(999)).toBe("999 ms");
  });

  it("switches to seconds at a second", () => {
    expect(formatDuration(1000)).toBe("1.00 s");
    expect(formatDuration(2500)).toBe("2.50 s");
  });
});

describe("formatBytes", () => {
  it("keeps small sizes in bytes", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(1023)).toBe("1023 B");
  });

  it("scales up through the units", () => {
    expect(formatBytes(1024)).toBe("1.0 KB");
    expect(formatBytes(1024 * 1024)).toBe("1.0 MB");
    expect(formatBytes(1024 * 1024 * 1024)).toBe("1.0 GB");
  });
});

describe("shortHash", () => {
  it("keeps twelve characters", () => {
    expect(shortHash("0123456789abcdef".repeat(4))).toBe("0123456789ab");
  });
});

describe("summarizeChanges", () => {
  it("says so when nothing would change", () => {
    expect(summarizeChanges(result())).toBe("No schema changes");
  });

  it("counts a single change in the singular", () => {
    expect(summarizeChanges(result({ schemaChanges: [change()] }))).toBe(
      "1 schema change",
    );
  });

  it("calls out destructive and advisory counts", () => {
    const summary = summarizeChanges(
      result({
        schemaChanges: [change(), change(), change()],
        destructiveChangeCount: 1,
        advisoryChangeCount: 2,
      }),
    );
    expect(summary).toBe(
      "3 schema changes · 1 potentially destructive · 2 advisory changes",
    );
  });
});

describe("describeCloneStrategy", () => {
  it("names the one snapshot mechanism there is", () => {
    expect(describeCloneStrategy("sqlite_backup_api")).toContain("backup");
    expect(describeCloneStrategy("sqlite_backup_api")).toContain("consistent");
  });
});

describe("changeListHeading", () => {
  it("presents a successful preview as what would happen", () => {
    expect(changeListHeading(result())).toBe(
      "Changes this migration would make",
    );
  });

  it("never lets a failed migration read as a completed one", () => {
    const heading = changeListHeading(result({ success: false }));
    expect(heading).toBe(
      "Changes applied to the disposable clone before failure",
    );
    expect(heading).not.toContain("would make");
  });
});

describe("emptyChangeMessage", () => {
  it("distinguishes nothing-to-do from stopped-early", () => {
    expect(emptyChangeMessage(result())).toContain(
      "would not add, remove or alter",
    );
    expect(emptyChangeMessage(result({ success: false }))).toContain(
      "failed before it changed",
    );
  });
});

describe("describeIntegrity", () => {
  it("claims content integrity, not an untouched directory", () => {
    const line = describeIntegrity(result());
    expect(line).toBe("Original database content unchanged");
    expect(line).not.toContain("database unchanged");
  });

  it("declares the shared-memory sidecar separately", () => {
    const line = describeIntegrity(
      result({
        originalIntegrity: {
          ...result().originalIntegrity,
          shmCreatedByPreview: true,
        },
      }),
    );
    expect(line).toContain("content unchanged");
    expect(line).toContain("-shm");
  });

  it("says so when the content did change", () => {
    expect(
      describeIntegrity(
        result({
          originalContentUnchanged: false,
          originalIntegrity: {
            ...result().originalIntegrity,
            contentUnchanged: false,
          },
        }),
      ),
    ).toContain("CHANGED");
  });
});

describe("fileName", () => {
  it("takes the last segment of a posix path", () => {
    expect(fileName("/path/to/app.db")).toBe("app.db");
  });

  it("takes the last segment of a windows path", () => {
    expect(fileName("C:\\data\\app.db")).toBe("app.db");
  });

  it("leaves a bare name alone", () => {
    expect(fileName("app.db")).toBe("app.db");
  });

  it("ignores a trailing separator", () => {
    expect(fileName("/path/to/dir/")).toBe("dir");
  });
});
