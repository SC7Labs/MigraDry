# MigraDry

**Preview a SQLite migration before it touches your database.**

MigraDry is a local desktop developer tool that applies a SQLite migration to a
disposable clone, compares the schema before and after, counts exactly what the
migration would discard, and verifies by hash that the content of your original
database was left byte-identical.

```text
  original database
        │
        ├── SHA-256 + size recorded
        │
        ▼  SQLite online backup, read-only
  BASELINE  ── pristine, never written again
        │
        ▼  SQLite online backup
  WORKING CLONE
        │
        ▼
  migration executes ONLY here
        │
        ├── schema compared with the baseline
        │
        └── exact row counts for what would be
            discarded, queried from the BASELINE
        │
        ▼
  report + proof the original's
  content is byte-identical
```

> [!IMPORTANT]
> **Migration SQL is never executed against the original database.** It runs
> only against a temporary clone, which is deleted afterwards. There is no
> "apply to original" button, and v0.1 is not going to grow one.
>
> The original is opened read-only, and its *content* — the database file and
> its write-ahead log — is verified byte-identical by SHA-256 before and after
> every preview. Under WAL mode, SQLite creates a transient `-shm` sidecar next
> to any database it reads, MigraDry included; see
> [the WAL note](#a-note-on-wal-mode-and--shm).

---

## The problem

You have a migration and a database, and the only way to find out what the
migration does is to run it. On a dev database that is merely annoying; the
habit it builds is the dangerous part. `sqlite3 app.db < 004_orders.sql` looks
identical whether the migration adds a column or drops a table.

MigraDry answers the question without taking the risk: *what would this
migration do?*

## The safety invariant

Migration SQL never runs against the original database, and the original's
content is never modified — not on success, not on failure, not if the SQL is
destructive, not if it changes journal mode, not if it opens transactions, and
not if the app crashes half way through.

Three claims, kept separate on purpose:

| Claim | Strength |
|---|---|
| Migration SQL only ever executes against a disposable clone | structural — there is no code path that could do otherwise |
| The database file and its `-wal` are byte-identical afterwards | verified by SHA-256 on every preview |
| Nothing at all appears next to the database | **not claimed** — SQLite creates a `-shm` when reading a WAL database |

This is enforced in five independent ways rather than by careful coding alone:

1. **Read-only by construction.** The original is opened with
   `SQLITE_OPEN_READ_ONLY`, without `SQLITE_OPEN_CREATE` and without
   `SQLITE_OPEN_URI` (so a file literally named `file:...` cannot be
   reinterpreted as a URI carrying `mode=rwc`). `PRAGMA query_only` is set on top.

2. **The type system.** Migration SQL is executed by a function that takes a
   `WorkingClone`, not a path. `WorkingClone` has private fields and only one
   constructor, inside the workspace module, and it is the only type with a
   writable opener — the baseline has none. There is no expression in the
   codebase that could aim migration SQL at the original file or at the
   baseline.

3. **A SQL authorizer.** SQLite has exactly one way for SQL to reach a second
   database file — `ATTACH`, which `VACUUM INTO` also uses internally. An
   authorizer denies any attempt to attach a *named* database, and after the
   migration runs, `PRAGMA database_list` is checked to confirm the connection
   is still pointed at nothing but the clone.

4. **Verification.** The original is hashed with SHA-256 before the clone is
   made, again once the snapshot is complete, and again after the clone is
   destroyed. A mismatch at either checkpoint aborts the preview instead of
   being reported as a field on an otherwise cheerful result.

5. **Refusal.** When SQLite cannot give MigraDry a snapshot it vouches for, the
   preview fails rather than improvising one. See
   [How the clone is made](#how-the-clone-is-made).

There are tests for all of this. See [Tests](#tests).

## Scope

MigraDry v0.1 is **SQLite only**. It does not support PostgreSQL, MySQL, or any
other engine, and it does not integrate with Prisma, Alembic, Django, Rails or
any other migration framework. It reads a `.db` file and a `.sql` file.

This is a milestone-1 foundation, not a finished product. It is not production
software.

## What it detects

| Change | Reported | Treated as |
|---|---|---|
| Table added / removed | yes | removal is **destructive** |
| Column added / removed | yes | removal is **destructive** |
| Column altered in place | yes | advisory |
| Index added / removed | yes | removal is advisory |
| View added / removed | yes | removal is advisory |
| Trigger added / removed | yes | removal is advisory |

"Destructive" means stored data is discarded. Removing an index, view or trigger
removes a support object, and altering a column rewrites the schema — neither
loses rows on its own. The distinction is deliberately simple; there is no risk
scoring.

### Column changes

For a column that exists on both sides, MigraDry compares what SQLite reports
about it and lists every attribute that moved as a single finding — declared
type, nullability, default, primary-key position, and whether it is a generated
column:

```text
~ COLUMN users.status
    type       TEXT → VARCHAR(32)
    NOT NULL   false → true
    default    none → 'active'
```

Generated columns are included. Schema inspection uses `pragma_table_xinfo`
rather than `pragma_table_info`, because the latter omits `GENERATED ALWAYS AS`
columns entirely — a migration that added or dropped one would otherwise produce
an empty diff. Dropping a `STORED` generated column is destructive and counted;
dropping a `VIRTUAL` one is advisory, because its values are computed on read
and nothing is stored to lose.

**Generated expressions are compared conservatively.** The expression inside
`GENERATED ALWAYS AS (...)` is not part of any column metadata SQLite reports,
so MigraDry compares the stored `CREATE TABLE` definition instead and raises a
`TABLE ... definition changed` advisory when it differs. MigraDry does **not**
parse SQL and does not claim to understand what an expression means — only that
the text moved.

The comparison is verbatim from the opening parenthesis onwards. Only the
`CREATE TABLE <name>` prefix is skipped, because SQLite rewrites it when a table
is renamed into place. Two consequences worth knowing:

- A **formatting-only** rewrite of a generated-column table — different spacing,
  different capitalisation — produces an advisory even though nothing changed.
  This is deterministic and deliberate. Normalising whitespace away is what
  previously erased it *inside* string literals, so that `AS ('a b')` and
  `AS ('ab')` compared equal and a genuinely different expression was reported
  as no change. A visible false advisory beats a silent miss.
- The comparison applies **only** to tables that carry a generated column on
  either side, so ordinary tables gain no new findings.

SQLite has no `ALTER COLUMN`, so this normally happens through a table rebuild —
create a replacement, copy the rows across, drop the original, rename the
replacement into place. Because the final table keeps the old name, the columns
line up and the differences fall out. No rename inference is involved.

The declared type is compared as a *declaration*, not as a meaning. Case and
whitespace are normalised away (`text` and `TEXT` are the same declaration, as
are `VARCHAR (255)` and `VARCHAR(255)`), and anything else is reported with both
spellings shown. `TEXT` → `VARCHAR(255)` is reported even though SQLite gives
them the same affinity: the declaration genuinely changed, and MigraDry's job is
to say what the migration did rather than to decide on your behalf that it did
not matter. There is no affinity analysis and no claim that data was converted.

Defaults are compared as text, because that is what SQLite stores. Two spellings
of the same value — `'a'` and `"a"`, or `1` and `1.0` — therefore read as a
change. That is conservative on purpose: calling them equivalent would need an
expression evaluator, and a wrong "nothing changed" is worse than a noisy
"something did".

### Data impact

For changes that discard data, MigraDry reports exactly how much:

```text
! - TABLE old_events
      126,441 rows would be removed

! - COLUMN users.legacy_code
      48,102 non-null values across 51,337 rows would be removed
```

These are **exact `COUNT` results taken from the baseline snapshot**, not
estimates. A column whose values are all null still reports its row count and
still counts as destructive — the column disappears either way, and "no impact"
would be the wrong summary of that.

Counts are only taken for the objects the diff flagged, and only after the diff
has run, so a migration that removes nothing never reads a single row. If a
count cannot be taken, MigraDry says so; it never substitutes a zero.

**Not implemented, on purpose:** column and table rename detection, semantic
equivalence of DDL text, foreign-key and constraint diffing, AST comparison,
SQLite affinity analysis, data-type conversion simulation, row-by-row data
diffing. Objects are compared by name, so a rename shows up as a removal plus an
addition. That is honest and explainable, which matters more here than being
clever.

## How the clone is made

A SQLite database in WAL mode is `app.db` **plus** `app.db-wal`. Committed data
can live entirely in the log. Copying only the main file would silently discard
it and every conclusion drawn from the preview would be wrong.

There is exactly one snapshot mechanism: the **SQLite Online Backup API**, driven
from a read-only source connection in a single `step(-1)` so the whole copy
happens inside one read transaction. SQLite reads through the write-ahead log,
so uncheckpointed commits are included and the result is point-in-time
consistent.

Nothing is done to the source to make this work — no checkpoint, no `VACUUM`, no
journal-mode change, no write of any kind.

### Two databases, not one

A preview needs the schema *and the data* as they were before the migration, and
it needs them after a destructive migration has already thrown them away.
Reading the original again afterwards would answer a different question: it
describes a later moment, and the source may have moved on.

So a preview builds a workspace of two disposable databases in one private
temporary directory, never beside the original:

* the **baseline**, taken from the original and then never written again. It is
  the exact state the preview is *about*, and every impact count comes from it.
* the **working clone**, copied from the baseline by the same backup API. The
  migration runs here and nowhere else.

Both are deleted when the preview ends, on normal return and on a panic alike.

Baseline immutability is not a rule anyone has to remember. `WorkingClone`
exposes a read-write opener and `BaselineSnapshot` does not, so a writable handle
on the pre-migration state is not an expression that can be written. On top of
that the baseline file is marked read-only on disk, and the engine hashes it
before and after every preview and refuses to report a result if it moved.

### Why there is no fallback

An earlier version copied `app.db` and `app.db-wal` with `fs::copy` when SQLite
refused to open the database read-only. That is two non-atomic reads: a writer
committing between them produces a clone blended from two different moments, and
nothing afterwards can tell that it happened — `PRAGMA integrity_check` would
pass, because the result can be structurally valid and transactionally
incoherent at once.

MigraDry cannot detect a condition that rules this out, so it does not ship the
copy. When SQLite will not provide a snapshot, the preview fails:

```text
A consistent SQLite snapshot could not be created safely: SQLite could not open
the database read-only. A database using write-ahead logging needs a -shm
shared-memory index, and one could not be created — often because the directory
holding the database is not writable. MigraDry will not copy the database files
itself, because such a copy cannot be proven consistent.
```

A refused preview is an inconvenience. A confident, wrong preview is the failure
this program exists to prevent.

### If someone else is writing at the same time

WAL lets a reader and a writer work concurrently, so a preview usually succeeds
and the snapshot is consistent — that is what the backup API guarantees. If the
source's content changes between the "before" hash and the "after" hash,
MigraDry discards the result and says so, because it can no longer describe the
preview as a picture of a stable database.

The wording of that error is deliberately non-attributive. MigraDry can prove
what *it* does — it opens the source read-only and executes migration SQL only
against a clone — but it cannot prove what else on the machine touched the file,
so it does not claim to know who did.

### A note on WAL mode and `-shm`

Reading a WAL database requires a shared-memory index, so SQLite creates an
`app.db-shm` file beside it. Every reader does this, `sqlite3` running a plain
`SELECT` included; it is not something MigraDry can avoid while still reading the
log correctly. The `-shm` carries no database content and SQLite rebuilds it on
demand.

MigraDry reports it rather than hiding it: the result sets
`originalIntegrity.shmCreatedByPreview`, and the UI separates *database content
unchanged* (proven by hash) from *filesystem side effects* (a sidecar may
appear). MigraDry never deletes a `-shm`, because unlinking one while another
process is using it risks corrupting that process's view of the database.

## Migration execution

The whole migration file is handed to SQLite's own batch execution. MigraDry has
no SQL parser and no statement splitter: comments, semicolons inside trigger
bodies and every other quirk of SQL text are SQLite's business. Nothing is run
through a shell, a subprocess, or an external `sqlite3` binary.

Migration SQL is executed against the clone with these restrictions:

- `ATTACH` of a named database is refused — absolute, relative, URI-shaped and
  `:memory:` alike. A plain `VACUUM` works, because it attaches an *anonymous*
  temporary database internally; `VACUUM INTO 'file'` is refused for the same
  reason. After the migration runs, `PRAGMA database_list` is checked to confirm
  the connection still sees nothing but the clone.

  This is a restriction on naming SQLite databases, not a filesystem sandbox.
  See [What this does not protect against](#what-this-does-not-protect-against).
- Extension loading is never enabled, so `load_extension()` is unavailable.

### Foreign keys

Previews run with **`PRAGMA foreign_keys = ON`**, and the result says so in
`foreignKeysEnforced`.

The default is not something to inherit: upstream SQLite documents it as *off*,
while the bundled amalgamation this project links is compiled with
`SQLITE_DEFAULT_FOREIGN_KEYS` and is *on*. So the effective default is a property
of a dependency's build flags, and would flip if MigraDry were built against a
system SQLite. Setting it explicitly means a preview means the same thing
everywhere.

Enforcement is the value to pin because it is the stricter of the two: a preview
that says "this would fail" when your own tooling would have allowed it is a
false alarm you can inspect, while a preview that says "this is fine" about a
migration that breaks referential integrity is the failure this program exists
to prevent.

A migration keeps the last word. The standard SQLite table-rebuild recipe opens
with `PRAGMA foreign_keys = OFF`, and that works exactly as it does anywhere
else — MigraDry chooses the starting position, not the rules.

If the migration leaves a transaction open, MigraDry commits it on success and
rolls it back on failure, then says which it did. The clone is disposable, so
committing costs nothing and is the only way to inspect the resulting schema.

If a migration fails part way through, the changes that had already been applied
are still reported, with a warning explaining that the run stopped early.

## Getting started

### Prerequisites

- [Rust](https://rustup.rs/) 1.88 or newer
- [Node.js](https://nodejs.org/) 20 or newer
- The system libraries Tauri 2 needs. On Debian/Ubuntu:

  ```bash
  sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev patchelf build-essential curl wget file libssl-dev libayatana-appindicator3-dev
  ```

  See the [Tauri prerequisites](https://tauri.app/start/prerequisites/) for
  macOS and Windows.

SQLite itself is compiled in via `rusqlite`'s `bundled` feature, so no system
SQLite is required.

### Run it

```bash
npm install
npm run tauri dev
```

To produce installable packages:

```bash
npm run tauri build
```

On Linux this also attempts an AppImage, which needs `patchelf` on the `PATH`
and a temporary directory that is not mounted `noexec`. Where either is missing,
build just the packages that do not need it:

```bash
npm run tauri build -- --bundles deb,rpm
```

Point it at a database and a migration, and press **Preview Migration**. Both
fields accept a typed or pasted path as well as the native file picker.

## Tests

The backend safety engine is where the tests are concentrated. Fixtures are
created programmatically; nothing depends on checked-in database files.

```bash
cd src-tauri && cargo test
```

Test files:

| File | Covers |
|---|---|
| `tests/original_safety.rs` | the invariant: successful, failed, destructive, data-deleting, escaping and pragma-abusing migrations all leave the original byte-identical |
| `tests/concurrent_writer.rs` | snapshots taken while another connection commits continuously are consistent or refused, never torn — checked with SQLite queries against a cross-table invariant, not with hashes |
| `tests/foreign_keys.rs` | the foreign-key policy, including that the clone connection really starts with enforcement on |
| `tests/sql_containment.rs` | `ATTACH` and `VACUUM INTO` refused for absolute, relative, URI-shaped and `:memory:` names; `PRAGMA database_list` audited; URI-shaped *filenames* treated as paths |
| `tests/column_changes.rs` | type, nullability, default and primary-key changes; table rebuilds; spelling that is not a change |
| `tests/data_impact.rs` | exact counts for dropped tables and columns, all-null columns, awkward identifiers, baseline immutability, and impact omitted on a failed migration |
| `tests/failure_modes.rs` | malformed, truncated, unreadable, empty, locked and multi-megabyte databases |
| `tests/source_access.rs` | the read-only handle refuses every write, and a source-level audit that only the clone module can open a database for writing |
| `tests/wal_safety.rs` | data that exists only in a `-wal` is seen; an unopenable WAL database is refused; no sidecars left behind |
| `tests/temp_lifecycle.rs` | no temporary clone survives any path, including a panic mid-preview |
| `tests/temp_dir_unavailable.rs` | an unusable system temp directory is reported, not panicked over |
| `tests/preview_engine.rs` | input validation, empty and comment-only migrations, multi-statement migrations, every diff category, determinism, JSON contract |
| `tests/command_bridge.rs` | the command contract the frontend calls, including structured errors |
| unit tests in `src/` | hashing, schema snapshots, diffing, migration file validation |

Every test that claims the original's content was unchanged hashes the file
itself, before and after, rather than trusting the engine's own
`originalContentUnchanged` flag.

Frontend tests cover the pure presentation helpers:

```bash
npm test
```

### Quality checks

```bash
cd src-tauri
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

```bash
npm run lint
npm run typecheck
npm run build
```

## Architecture

```text
src-tauri/src/
├── main.rs         thin binary entry point
├── lib.rs          module wiring and the Tauri builder
├── commands.rs     the one command the frontend can call
├── migration.rs    loading, executing and orchestrating a preview
├── database.rs     every read of the original — all of it read-only
├── temp.rs         creating and destroying the disposable clone
├── schema.rs       deterministic schema snapshots
├── diff.rs         comparing two snapshots
├── models.rs       the serializable data contract
└── error.rs        structured errors
```

The frontend can call exactly one command:

```ts
invoke<MigrationPreviewResult>("preview_migration", {
  databasePath: string,
  migrationPath: string,
})
```

`Ok` means the preview ran — check `success` and `error` to find out whether the
*migration* worked. `Err` carries a structured `PreviewError` and means no
preview could be produced. No filesystem primitive, SQL entry point or clone
path is reachable from the webview.

## What this does not protect against

MigraDry constrains what a migration can do to your *database*. It is not a
sandbox for untrusted SQL, and the difference matters if you ever point it at a
migration you did not write.

**Constrained.** Migration SQL executes only against a disposable working clone.
The selected database is opened read-only and its content is hash-verified before
and after. `ATTACH` of a named database and `VACUUM INTO` are refused, so SQL
cannot name a second database file. Extension loading is never enabled.

**Not constrained.** A migration is arbitrary SQL running in-process with no
execution budget:

- **No time limit.** `WITH RECURSIVE` can loop indefinitely. There is no
  deadline, no progress handler and no cancel button — a pathological migration
  hangs the preview until you kill the application.
- **No memory limit.** A query that builds a large result set allocates as much
  as SQLite asks for.
- **No temporary-disk quota.** SQLite writes journal and temporary files while
  executing; a migration that inflates the clone can fill the temporary
  filesystem.
- **CPU is unbounded.** Preview execution is not metered.

None of these can modify the database you selected — that is what the read-only
open and the hash verification cover — but all of them can consume the machine
MigraDry is running on. Treat a migration file the way you would treat a script
you are about to run: MigraDry protects the database, not the host.

Adding a deadline was considered and rejected for v0.1: any threshold short
enough to help would eventually interrupt a legitimate migration on a large
database, and turning a correct preview into a false failure is the outcome this
project treats as worst.

---

## Limitations

- SQLite only.
- Objects are compared by name; renames appear as a removal plus an addition.
- Index, view and trigger *definitions* are not diffed — only their presence. An
  index that survives under the same name but now covers different columns is
  not reported.
- Virtual-table shadow tables appear as ordinary tables in the diff.
- The whole database is copied twice for every preview — once into the baseline,
  once into the working clone — so preview time and temporary disk both scale
  with database size. Peak temporary disk can exceed two copies if the migration
  grows the clone or SQLite writes journal files. Measured figures are below.
- Column changes are compared by declared metadata only. A column whose type
  declaration is respelled without changing meaning still reports a change, and
  two different expressions that produce the same default do too.
- Impact counts cover dropped tables and dropped columns. A migration that
  deletes rows without changing the schema reports no schema change and no
  count.
- If SQLite cannot open the database read-only — a `-wal` left by a crashed
  writer in a directory MigraDry cannot write to — the preview is refused rather
  than approximated.
- A preview taken while another process is committing may be refused, because
  the "before" and "after" hashes will not match.
- `ATTACH` is unavailable to migrations, including `:memory:`.

## Performance

A preview copies the database twice and hashes it three times, so cost scales
linearly with size. Measured on this machine:

| Database | Median | Min | Max |
|---|---|---|---|
| 10 MB | 174 ms | 113 ms | 224 ms |
| 100 MB | 1.58 s | 1.33 s | 1.85 s |
| 500 MB | 6.42 s | 5.44 s | 8.46 s |

**Method.** Release build (`cargo test --release`), 5 runs per size, single
threaded, warm page cache. Databases generated with
`INSERT INTO blobs SELECT randomblob(1000)` from a recursive CTE, in
rollback-journal mode. Migration applied: `CREATE INDEX idx_b ON blobs(id)`.
Timing covers the whole `MigrationService::preview` call — validation, both
copies, three hashes, both schema snapshots, migration execution and teardown.

**Hardware.** Intel Core Ultra 7 155H (22 threads), 30 GB RAM, Linux 6.x,
temporary directory on `tmpfs`.

**Temporary disk.** The workspace *starts* at roughly two database-sized copies:
a pristine baseline and a writable working clone. Peak use can exceed that — the
migration may grow the working clone, and SQLite writes its own journal and
temporary files while executing. Nothing bounds it: there is no temporary-disk
quota (see [What this does not protect against](#what-this-does-not-protect-against)).
The whole workspace is deleted when the preview ends.

These figures come from one machine and one storage configuration; treat them as
an order of magnitude, not a specification.

---

## Before this is published

The application identifier is configured as `io.github.sc7labs.migradry`. The repository
URL is configured as `https://github.com/SC7Labs/MigraDry` in `src-tauri/Cargo.toml`.
Remaining publication items are tracked in **[RELEASE_CHECKLIST.md](RELEASE_CHECKLIST.md)**.

---

## License

MIT — see [LICENSE](LICENSE).
