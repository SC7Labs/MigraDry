# Release checklist

Tracking the final publication items. This file is the single place that records them.

---

## REQUIRED BEFORE FIRST PUBLIC BUILD

### 1. Application identifier (SET)

**File:** `src-tauri/tauri.conf.json` → `identifier`
**Current value:** `io.github.sc7labs.migradry`

Set to the reverse-DNS identifier under the `sc7labs` namespace (`io.github.sc7labs.migradry`).

### 2. Repository URL (SET)

**File:** `src-tauri/Cargo.toml` → `[package]`
**Current value:** `repository = "https://github.com/SC7Labs/MigraDry"`

Configured to the repository URL (`https://github.com/SC7Labs/MigraDry`).

---

## Dependency audit status

Recorded so the next audit does not have to rediscover it.

**`npm audit`** — 0 vulnerabilities, with and without dev dependencies.

**`cargo audit`** — 0 vulnerabilities. 17 warnings: 16 `unmaintained` and 1
`unsound`. Every one is a transitive dependency of Tauri, and none is a direct
dependency of MigraDry (`tauri`, `tauri-plugin-dialog`, `serde`, `serde_json`,
`rusqlite`, `sha2`, `tempfile`, `thiserror`):

| Advisory group | Crates | Reached via |
|---|---|---|
| gtk-rs GTK3 bindings no longer maintained | `atk`, `gdk`, `gtk`, `gdkx11`, `gdkwayland-sys`, and their `-sys` crates | `tauri` → `muda` / `tao` |
| `unic-*` unmaintained | `unic-char-property`, `unic-char-range`, `unic-common`, `unic-ucd-ident`, `unic-ucd-version` | `tauri` |
| `proc-macro-error` unmaintained | `proc-macro-error` | build-time macro dependency |
| RUSTSEC-2024-0429 `glib` unsound | `glib` 0.18.5 | `tauri` → GTK3 stack |

No dependency was changed. None of these is a vulnerability, no fix is available
to this project, and resolving them requires Tauri to migrate off GTK3 — pinning
or forking around it would be worse than recording it. The `glib` unsoundness is
in `VariantStrIter`, which MigraDry does not use; it has no direct `glib`
dependency.

Re-run before release:

```bash
cargo audit          # needs: cargo install cargo-audit
npm audit
```

---

## Not blockers

These are known and documented, and do not need resolving before publication:

- **AppImage** does not build on hosts without `patchelf`, or where the
  temporary directory is mounted `noexec`. `.deb` and `.rpm` build normally.
  See CONTRIBUTING for the `--bundles deb,rpm` invocation.
- **`cargo audit`** is not part of the local gate set unless installed; see the
  audit notes in CONTRIBUTING.

---

## Verifying before release

```bash
npm ci && npm run lint && npm run typecheck && npm test && npm run build
cd src-tauri
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Then, from the repository root:

```bash
RUSTFLAGS="--remap-path-prefix=$HOME=~" npm run tauri build -- --bundles deb,rpm
```

The `--remap-path-prefix` flag keeps the builder's home directory out of the
shipped binary; see CONTRIBUTING for why.
