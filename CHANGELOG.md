# Changelog

All notable changes to MigraDry are recorded here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.1.0

First release. SQLite only, and not yet published anywhere.

### Pre-release corrections

Found while auditing before publication; each is covered by a regression test.

- **Generated columns were invisible.** Schema inspection used
  `pragma_table_info`, which omits `GENERATED ALWAYS AS` columns entirely, so a
  migration that added or dropped one produced an empty diff. Now uses
  `pragma_table_xinfo` and records whether a column is ordinary, `VIRTUAL` or
  `STORED`. Dropping a `VIRTUAL` generated column is reported as advisory rather
  than data loss, because nothing is stored to lose.
- **The migration size limit was not a bound.** `metadata().len()` was checked
  and then `fs::read` read the file whole, so the limit could be bypassed by a
  file that grows between the two calls — or simply by one whose reported length
  does not match its contents. `/proc/self/status` passes `is_file()`, reports
  zero length and yields 1558 bytes. Reads are now bounded to the limit plus one
  byte, and the decision is made on what was actually read.
- **Generated-expression comparison made literal-safe.** The definition
  comparison stripped whitespace, which erased it inside string literals too:
  `AS ('a b')` and `AS ('ab')` normalised to the same text and a different
  expression produced no finding. The body is now compared verbatim, and it is
  located with a quote-aware scan so a legal identifier such as
  `"example(test)"` cannot be mistaken for the start of the definition.
- **Claims narrowed.** "Stops migration SQL writing new files anywhere on disk"
  overstated what the SQL authorizer does; it refuses named SQLite databases and
  is not a filesystem sandbox. A "What this does not protect against" section now
  states plainly that there is no time, memory or temporary-disk bound on
  migration execution.
- **Benchmark numbers documented.** Replaced an unqualified "developer laptop"
  figure with medians over five runs per size, plus the CPU, generation method
  and build mode.
- **Dev origins removed from the production CSP.** The Vite dev-server websocket
  and origin now live in `devCsp`, which Tauri applies only to `tauri dev`.
- **Application identifier set to reverse-DNS.** Configured as `io.github.sc7labs.migradry`
  under the SC7Labs organization namespace.

### Preview engine

- Preview what a SQLite migration would do, without running it against your
  database. Migration SQL executes only against a disposable clone.
- Every preview builds a two-database workspace in a private temporary
  directory: a **baseline** taken from the original with the SQLite Online
  Backup API, and a **working clone** taken from the baseline. The baseline is
  never written to; the working clone is the only writable database.
- The workspace is deleted when the preview ends, on success, on failure and on
  a panic alike.
- Migrations run with `PRAGMA foreign_keys = ON`, set explicitly and read back
  rather than inherited from a build-dependent default. The result reports the
  policy in `foreignKeysEnforced`.

### Schema differences

- Detects tables, columns, indexes, views and triggers being added or removed.
- Detects changes to a column that survives the migration — declared type,
  nullability, default and primary-key position — reported as one finding
  carrying each individual delta. This makes the usual SQLite table-rebuild
  pattern legible without any rename inference.
- Declared types are compared as declarations, with case and whitespace
  normalised away. No affinity analysis, and no claim that data was converted.
- Ordering is deterministic: the same inputs always produce the same report.

### Data impact

- Exact counts for what a destructive migration would discard: the row count for
  a dropped table, and the non-null count alongside the row count for a dropped
  column. These are `COUNT` results taken from the baseline, not estimates.
- Counts are taken only for the objects the diff flagged, so a migration that
  removes nothing never reads a row.
- A column whose values are all null still reports its row count and still
  counts as destructive.
- A count that cannot be taken is reported as unavailable, never as zero. A
  migration that failed part-way reports no impact rather than guessing.
- Table and column names are quoted for counting with embedded quotes doubled,
  so names containing spaces, quotes, Unicode, punctuation or reserved words are
  handled safely.

### Original-database integrity

- The original is opened read-only, with no `CREATE` and no URI reinterpretation,
  and SQLite is asked to confirm the handle really is read-only.
- Its content is fingerprinted with SHA-256 three times — before the snapshot,
  immediately after it, and after the workspace is destroyed. A change at either
  checkpoint discards the preview rather than reporting it.
- Source-change errors state what MigraDry can prove about its own behaviour and
  do not attribute the change to anyone.
- Reading a WAL database makes SQLite create a `-shm` sidecar, as it does for
  every reader. That is reported rather than hidden, and the interface separates
  proven *content* integrity from filesystem side effects.

### Containment

- Migration SQL cannot name a second database. `ATTACH` is refused for absolute,
  relative, URI-shaped and `:memory:` targets, and `VACUUM INTO` with it. A
  plain `VACUUM` still works.
- After every migration, `PRAGMA database_list` is checked to confirm the
  connection still sees nothing but the working clone.
- No shell, no subprocess, no external `sqlite3`, and extension loading is never
  enabled.

### Snapshot consistency

- The SQLite Online Backup API is the only snapshot mechanism. MigraDry does not
  copy database files itself, because such a copy cannot be proven consistent
  against a concurrent writer.
- Where SQLite cannot provide a snapshot it vouches for, the preview fails
  closed with a structured error instead of improvising one.

### Interface

- A minimal Tauri desktop application: pick a database and a migration, press
  preview, read the report. There is deliberately no "apply to original" button.
