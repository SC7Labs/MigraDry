import type { MigrationPreviewResult } from "../types/migration";
import {
  changeListHeading,
  describeCloneStrategy,
  emptyChangeMessage,
  formatDuration,
  summarizeChanges,
} from "../lib/format";
import { ChangeList } from "./ChangeList";
import { IntegrityProof } from "./IntegrityProof";

interface PreviewReportProps {
  result: MigrationPreviewResult;
}

/** Everything the engine found, once a preview has run. */
export function PreviewReport({ result }: PreviewReportProps) {
  return (
    <section className="report" aria-live="polite">
      <header
        className={`report__banner report__banner--${result.success ? "ok" : "failed"}`}
      >
        <span className="report__banner-mark" aria-hidden="true">
          {result.success ? "✓" : "✗"}
        </span>
        <div>
          <h2 className="report__headline">
            {result.success
              ? "Migration would succeed"
              : "Migration failed on the clone"}
          </h2>
          <p className="report__subhead">
            {result.migrationName} against {result.databaseName} ·{" "}
            {summarizeChanges(result)}
          </p>
        </div>
      </header>

      {result.error && (
        <div className="notice notice--error">
          <p className="notice__title">
            {result.error.sqliteCode ?? "SQLite error"}
            {result.error.line !== null && ` · line ${result.error.line}`}
          </p>
          <p className="mono notice__body">{result.error.message}</p>
        </div>
      )}

      {result.destructiveChangeCount > 0 && (
        <div className="notice notice--warning">
          <p className="notice__title">
            ! {result.destructiveChangeCount} potentially destructive{" "}
            {result.destructiveChangeCount === 1 ? "change" : "changes"}
          </p>
          <p className="notice__body">
            Dropping a table or a column discards the data stored in it. Nothing
            has happened to your database — this is what <em>would</em> happen.
          </p>
        </div>
      )}

      <ChangeList
        changes={result.schemaChanges}
        heading={changeListHeading(result)}
        emptyMessage={emptyChangeMessage(result)}
      />

      {result.warnings.length > 0 && (
        <ul className="warnings">
          {result.warnings.map((warning) => (
            <li key={warning}>{warning}</li>
          ))}
        </ul>
      )}

      <IntegrityProof integrity={result.originalIntegrity} />

      <dl className="meta">
        <div>
          <dt>Migration</dt>
          <dd>{formatDuration(result.durationMs)}</dd>
        </div>
        <div>
          <dt>Total</dt>
          <dd>{formatDuration(result.totalDurationMs)}</dd>
        </div>
        <div>
          <dt>Foreign keys</dt>
          <dd>{result.foreignKeysEnforced ? "enforced" : "not enforced"}</dd>
        </div>
        <div>
          <dt>Clone</dt>
          <dd>{describeCloneStrategy(result.cloneStrategy)}</dd>
        </div>
      </dl>
    </section>
  );
}
