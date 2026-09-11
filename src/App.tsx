import { invoke } from "@tauri-apps/api/core";
import { useState } from "react";
import { FileField } from "./components/FileField";
import { PreviewReport } from "./components/PreviewReport";
import {
  asPreviewError,
  type MigrationPreviewResult,
  type PreviewError,
  type PreviewErrorKind,
} from "./types/migration";

/**
 * Headline for a preview that could not be produced.
 *
 * A refusal is a normal, correct outcome — MigraDry declines rather than
 * guessing — so most of these read as information, not alarm. Only a failed
 * internal safety check is presented as critical.
 */
function errorTitle(kind: PreviewErrorKind): string {
  switch (kind) {
    case "snapshot_not_possible":
      return "No safe snapshot could be taken";
    case "source_changed":
      return "The database changed while the preview was running";
    case "safety_check":
      return "Critical safety failure";
    default:
      return "Could not run the preview";
  }
}

/**
 * The five states the UI can be in. Plain React state is enough for this —
 * there is one action and one result.
 */
type Status =
  | { phase: "idle" }
  | { phase: "running" }
  | { phase: "done"; result: MigrationPreviewResult }
  | { phase: "error"; error: PreviewError };

export default function App() {
  const [databasePath, setDatabasePath] = useState("");
  const [migrationPath, setMigrationPath] = useState("");
  const [status, setStatus] = useState<Status>({ phase: "idle" });

  const running = status.phase === "running";
  const ready =
    databasePath.trim().length > 0 && migrationPath.trim().length > 0;

  async function runPreview() {
    setStatus({ phase: "running" });
    try {
      const result = await invoke<MigrationPreviewResult>("preview_migration", {
        databasePath,
        migrationPath,
      });
      setStatus({ phase: "done", result });
    } catch (error) {
      setStatus({ phase: "error", error: asPreviewError(error) });
    }
  }

  return (
    <main className="app">
      <header className="app__header">
        <h1 className="app__title">MigraDry</h1>
        <p className="app__tagline">
          Preview a SQLite migration before it touches your database.
        </p>
      </header>

      <form
        className="panel"
        onSubmit={(event) => {
          event.preventDefault();
          if (ready && !running) {
            void runPreview();
          }
        }}
      >
        <FileField
          label="Database"
          value={databasePath}
          placeholder="/path/to/app.db"
          extensions={["db", "sqlite", "sqlite3", "db3"]}
          filterName="SQLite database"
          disabled={running}
          onChange={setDatabasePath}
        />
        <FileField
          label="Migration"
          value={migrationPath}
          placeholder="/path/to/004_orders.sql"
          extensions={["sql"]}
          filterName="SQL migration"
          disabled={running}
          onChange={setMigrationPath}
        />

        <div className="panel__actions">
          <button
            type="submit"
            className="button button--primary"
            disabled={!ready || running}
          >
            {running ? "Previewing…" : "Preview Migration"}
          </button>
          <p className="panel__reassurance">
            Migration SQL never runs against the file you select. It runs
            against a disposable copy, which is then deleted.
          </p>
        </div>
      </form>

      {status.phase === "running" && (
        <p className="muted" aria-live="polite">
          Cloning the database and applying the migration to the copy…
        </p>
      )}

      {status.phase === "error" && (
        <div
          className={`notice ${
            status.error.kind === "safety_check"
              ? "notice--critical"
              : "notice--error"
          }`}
          role="alert"
        >
          <p className="notice__title">{errorTitle(status.error.kind)}</p>
          <p className="notice__body">{status.error.message}</p>
        </div>
      )}

      {status.phase === "done" && <PreviewReport result={status.result} />}

      <footer className="app__footer">
        MigraDry v0.1 · SQLite only · Foreign keys enforced · There is
        deliberately no “apply to original” button.
      </footer>
    </main>
  );
}
