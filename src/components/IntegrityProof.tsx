import type { OriginalIntegrity } from "../types/migration";
import { formatBytes, shortHash } from "../lib/format";

interface IntegrityProofProps {
  integrity: OriginalIntegrity;
}

/**
 * The evidence, not just the claim.
 *
 * Two different guarantees live here and they are deliberately not merged.
 * *Content integrity* is what MigraDry proves with a hash: the database file
 * and its write-ahead log are byte-identical. *Filesystem side effects* is a
 * weaker, separate statement: reading a WAL database makes SQLite create a
 * `-shm` shared-memory index beside it, as it does for every reader, and
 * saying "nothing was touched" would paper over that.
 */
export function IntegrityProof({ integrity }: IntegrityProofProps) {
  const { before, after, contentUnchanged, shmCreatedByPreview } = integrity;

  return (
    <details className="proof">
      <summary className={contentUnchanged ? "proof__ok" : "proof__bad"}>
        {contentUnchanged
          ? "✓ Original database content unchanged"
          : "✗ Original database content changed"}
        {shmCreatedByPreview && (
          <span className="proof__aside"> · SQLite added a -shm sidecar</span>
        )}
      </summary>

      <dl className="proof__grid">
        <dt>Database content</dt>
        <dd>
          {contentUnchanged
            ? "unchanged, verified by SHA-256"
            : "changed during the preview"}
        </dd>

        <dt>Filesystem side effects</dt>
        <dd>
          {shmCreatedByPreview
            ? "SQLite created a shared-memory sidecar (-shm) during read-only inspection"
            : "none"}
        </dd>

        <dt>SHA-256 before</dt>
        <dd className="mono">{shortHash(before.main.sha256)}…</dd>

        <dt>SHA-256 after</dt>
        <dd className="mono">{shortHash(after.main.sha256)}…</dd>

        <dt>Size</dt>
        <dd>
          {formatBytes(before.main.sizeBytes)}
          {before.main.sizeBytes !== after.main.sizeBytes &&
            ` → ${formatBytes(after.main.sizeBytes)}`}
        </dd>

        {integrity.walChecked && (
          <>
            <dt>Write-ahead log</dt>
            <dd>
              {before.wal && after.wal
                ? `checked, ${shortHash(after.wal.sha256)}…`
                : "presence changed"}
            </dd>
          </>
        )}
      </dl>

      <p className="proof__footnote">
        Migration SQL ran against a temporary clone and never against the file
        you selected. The hashes above are of that file, taken before the clone
        was made and again after it was destroyed.
        {shmCreatedByPreview && (
          <>
            {" "}
            The <code>-shm</code> holds no database content: SQLite rebuilds it
            on demand, and any process that reads a WAL database creates one.
            MigraDry does not delete it, because removing a sidecar another
            process may be using is unsafe.
          </>
        )}
      </p>
    </details>
  );
}
