# Contributing to MigraDry

Thanks for taking a look. MigraDry is small on purpose, and the bar for adding
to it is deliberately high.

## The one rule

> **Migration SQL must never be able to reach the original database, and
> MigraDry must never present a preview it cannot vouch for.**

Any change that could let user-supplied SQL execute against the file the user
selected is a critical design violation, not a bug to be fixed later. A pull
request that does so will be rejected regardless of how useful the feature is.

Things that count as violating it:

- opening the original with anything other than `SQLITE_OPEN_READ_ONLY`
- adding `SQLITE_OPEN_CREATE` or `SQLITE_OPEN_URI` to that open
- giving `execute_on_clone` a `&Path` instead of a `&WorkingClone`
- constructing a `WorkingClone` or a `BaselineSnapshot` outside
  `PreviewWorkspace::create`
- relaxing the SQL authorizer so a named database can be attached
- removing the before/after hash comparison, or downgrading a mismatch from an
  error to a flag
- adding an "apply to original" path of any kind
- reintroducing a filesystem-copy snapshot fallback, or any other snapshot
  mechanism whose consistency cannot be proven — a refused preview is always
  preferable to an uncertain one
- giving `BaselineSnapshot` a writable opener, or otherwise letting anything
  write to the baseline
- taking impact counts from the original database, or from a second snapshot
  made after the migration — the counts must describe the state the preview is
  about
- reporting a count that could not be taken as zero
- writing to the source in order to make a snapshot possible: no checkpoint, no
  `VACUUM`, no journal-mode change
- deleting a `-shm` sidecar, which may be shared with another process
- describing the original as "unchanged" without qualification; the proven claim
  is that its *content* is unchanged
- blaming MigraDry, or anything else, for a source change it cannot attribute

If you think a change genuinely needs one of these, open an issue first and
explain the reasoning. Do not open the pull request.

## Prerequisites

- [Rust](https://rustup.rs/) 1.88 or newer
- [Node.js](https://nodejs.org/) 20 or newer
- Tauri 2's system libraries — see
  [Tauri prerequisites](https://tauri.app/start/prerequisites/). On
  Debian/Ubuntu:

  ```bash
  sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev patchelf build-essential curl wget file libssl-dev libayatana-appindicator3-dev
  ```

SQLite is compiled in through `rusqlite`'s `bundled` feature, so no system
SQLite is needed.

## Setup

```bash
npm install
npm run tauri dev
```

## Architecture

```text
Tauri command (commands.rs)
      │
      ▼
MigrationService::preview (migration.rs)
      │
      ├── validate inputs ............... database.rs, migration.rs
      ├── fingerprint the original ...... database.rs
      ├── build the workspace ........... temp.rs
      │     original → baseline → working clone
      ├── snapshot schema (baseline) .... schema.rs
      ├── execute migration on working .. migration.rs
      ├── snapshot schema (working) ..... schema.rs
      ├── diff the snapshots ............ diff.rs
      ├── count destructive impact ...... impact.rs (reads the baseline)
      ├── verify the baseline unchanged . database.rs
      ├── destroy the workspace ......... temp.rs (Drop)
      └── re-fingerprint and compare .... database.rs
```

Each module owns one responsibility:

| Module | Owns |
|---|---|
| `database.rs` | every read of the *original* file, all of it read-only |
| `temp.rs` | the preview workspace: baseline, working clone, and their lifetime |
| `impact.rs` | exact row counts for destructive changes, read from the baseline |
| `migration.rs` | reading, executing and orchestrating the migration |
| `schema.rs` | deterministic schema snapshots from SQLite's catalogue |
| `diff.rs` | comparing two snapshots |
| `models.rs` | the serializable data contract |
| `error.rs` | structured errors |
| `commands.rs` | the single Tauri command exposed to the webview |

Keep them that way. `database.rs` is the only module that should ever name the
original path, and `commands.rs` should stay thin.

## Icons

`src-tauri/icons/icon-source.png` is the 1024×1024 master. Every other icon in
that directory is generated from it:

```bash
npm run tauri icon -- src-tauri/icons/icon-source.png
```

That command also writes Android and iOS icon sets. MigraDry is desktop-only, so
delete `src-tauri/icons/android` and `src-tauri/icons/ios` afterwards.

## Tests

```bash
cd src-tauri && cargo test
```

Tests live in `src-tauri/tests/`:

| File | Covers |
|---|---|
| `original_safety.rs` | the safety invariant |
| `concurrent_writer.rs` | snapshots taken while another connection writes |
| `foreign_keys.rs` | the foreign-key policy and its consequences |
| `sql_containment.rs` | `ATTACH` / `VACUUM INTO` refusal, in every spelling |
| `column_changes.rs` | column metadata changes and table rebuilds |
| `data_impact.rs` | exact counts, awkward identifiers, baseline immutability |
| `failure_modes.rs` | malformed, unreadable, locked and very large databases |
| `source_access.rs` | how the original may be opened — including a source audit |
| `wal_safety.rs` | write-ahead-log correctness and the refusal path |
| `temp_lifecycle.rs` | no temporary clone survives any path (runs alone) |
| `temp_dir_unavailable.rs` | an unusable temp directory (runs alone) |
| `preview_engine.rs` | validation, diffing, determinism, cleanup |
| `command_bridge.rs` | the frontend-facing command contract |

`source_access.rs` reads the engine's own source and asserts that writable
database handles appear only in `temp.rs`, that `SQLITE_OPEN_URI` appears
nowhere, and that migration SQL is executed in one place. If it fails, the fix
is almost always to move code rather than to relax the test.

`temp_lifecycle.rs` and `temp_dir_unavailable.rs` each hold a single test on
purpose: one reads the shared system temporary directory and the other changes a
process-wide environment variable, so neither may run beside a test that creates
a clone. Cargo runs integration binaries one at a time, which is what makes that
safe. Do not add a second test to either file.

Conventions worth keeping:

- **Build fixtures in code.** No checked-in database files.
- **Verify independently.** A test that claims the original was untouched must
  hash the file itself, before and after. Never assert only on the engine's own
  `original_content_unchanged` flag — that is the thing under test.
- **Check consistency with SQLite, not hashes.** A clone can be byte-perfect
  against nothing in particular. `concurrent_writer.rs` maintains a cross-table
  invariant and queries it, which is the only way to catch a torn snapshot.
- **Name the behaviour, not the function.** Test names read as sentences.

Any change to the engine's safety behaviour needs a test that would fail without
it.

Frontend tests cover the pure helpers in `src/lib/`:

```bash
npm test
```

Components are not tested; they contain no logic worth pinning. If you move
logic into one, move it back out into `src/lib/` and test it there.

## Building a release

```bash
RUSTFLAGS="--remap-path-prefix=$HOME=~" npm run tauri build -- --bundles deb,rpm
```

Two things about that command are deliberate.

`--remap-path-prefix` keeps the builder's home directory out of the shipped
binary. Rust bakes a source path into every panic location, so an ordinary
release build embeds the build machine's layout several hundred times over.
Cargo's `trim-paths` profile option would be the tidier fix, but it is not
stabilised in Cargo 1.97, so the flag does the job for now.

One build-machine path survives it: `tauri-build` records the crate directory in
generated code, and that is a string constant rather than a source path, so the
remap does not reach it. Build releases from a neutral directory — or from CI —
if that matters to you.

`--bundles deb,rpm` skips the AppImage, which needs `patchelf` on the `PATH` and
a temporary directory that is not mounted `noexec`. Drop the flag on a machine
that has both.

## Publication configuration

The repository URL is tracked in [RELEASE_CHECKLIST.md](RELEASE_CHECKLIST.md) —
add it to `src-tauri/Cargo.toml` once the public repository is created.

The production CSP in `src-tauri/tauri.conf.json` contains no dev-server origins;
the Vite HMR websocket lives in `devCsp`, which Tauri applies only to
`tauri dev`. Keep them separate.

## Continuous integration

`.github/workflows/ci.yml` runs the same checks on every push and pull request,
split into a frontend job and a Rust job. It deliberately does not run
`npm run tauri build`: native bundling needs the full desktop toolchain, and on
Linux the AppImage step also needs `patchelf` and a temporary directory that is
not mounted `noexec`. Packaging is verified locally before a release instead.

## Checks before opening a pull request

```bash
cd src-tauri
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

```bash
npm run lint
npm run typecheck
npm run build
npm test
```

All of these must pass.

## Style

- Avoid `unwrap()` and `expect()` anywhere an error is realistic. Tests are
  exempt.
- Errors that reach a user should read like sentences, and should name the file
  rather than its full path.
- Never log database contents, migration SQL, or file paths.
- Say only what can be proven. "The source changed" is a fact; "another process
  changed it" is a guess.
- Comment the *why*. The what is already in the code.

## Scope

MigraDry v0.1 is SQLite-only and stays that way. Out of scope for now:
other database engines, ORM and migration-framework integrations, migration
generation or rollback, LLM features, remote or containerised databases, schema
diagrams, a SQL editor, telemetry, and auto-update.

Open an issue before building anything substantial, so nobody spends a weekend
on something that will be turned down on scope.
