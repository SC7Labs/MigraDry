//! How the original database may be opened, enforced rather than documented.
//!
//! The rule is that MigraDry has exactly one way to open a database for
//! writing, it is a method on the clone type, and it therefore cannot name the
//! original. The first test checks that at runtime; the second checks the
//! source itself, so that a future change which quietly adds a second writable
//! opener fails the build rather than the user's database.

mod common;

use common::*;
use std::path::{Path, PathBuf};

/// Executable source of a module: test module and comments removed.
///
/// Two things have to go before the text means anything. Test modules
/// legitimately open databases for writing to build fixtures, and are always
/// last in the file, so everything from `#[cfg(test)]` onwards is dropped.
/// Comments are dropped because the modules explain at length which flags they
/// deliberately do *not* pass, and prose about `SQLITE_OPEN_CREATE` is the
/// opposite of a call to it.
fn production_source(module: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(module);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    let code = match text.find("#[cfg(test)]") {
        Some(index) => &text[..index],
        None => &text[..],
    };
    code.lines()
        .map(|line| match line.find("//") {
            Some(index) => &line[..index],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn modules() -> Vec<String> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".rs"))
        .collect();
    names.sort();
    names
}

/// The read-only handle really is read-only, as SQLite sees it.
#[test]
fn the_source_handle_cannot_write() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("app.db");
    seed_database(
        &database,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users (name) VALUES ('ada');",
    );

    let hash_before = sha256_of(&database);
    let connection = migradry_lib::database::open_readonly(&database).unwrap();

    // SQLite's own opinion of the handle.
    assert!(connection.is_readonly("main").unwrap());

    for statement in [
        "CREATE TABLE intruder (id INTEGER)",
        "INSERT INTO users (name) VALUES ('mallory')",
        "DELETE FROM users",
        "DROP TABLE users",
        "PRAGMA user_version = 99",
        "PRAGMA journal_mode = WAL",
        "VACUUM",
    ] {
        assert!(
            connection.execute_batch(statement).is_err(),
            "the source handle accepted: {statement}"
        );
    }

    drop(connection);
    assert_eq!(hash_before, sha256_of(&database));
    assert_eq!(row_count(&database, "users"), 1);
}

/// A source handle cannot be talked into existence for a file that is not there.
#[test]
fn the_source_handle_never_creates_a_database() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("absent.db");

    assert!(migradry_lib::database::open_readonly(&absent).is_err());
    assert!(
        !absent.exists(),
        "opening a missing database created one — SQLITE_OPEN_CREATE has crept in"
    );
}

/// Writable database handles may exist in exactly one module.
///
/// `temp.rs` owns the clone and is the only place allowed to open anything for
/// writing. If this fails, either move the code back into `temp.rs` or think
/// very hard about why a second writable opener is worth the risk.
#[test]
fn only_the_clone_module_opens_a_database_for_writing() {
    const WRITABLE: [&str; 3] = [
        "SQLITE_OPEN_READ_WRITE",
        "SQLITE_OPEN_CREATE",
        "Connection::open(",
    ];

    for module in modules() {
        let source = production_source(&module);
        for needle in WRITABLE {
            let found = source.contains(needle);
            if module == "temp.rs" {
                continue;
            }
            assert!(
                !found,
                "{module} can open a database for writing ({needle}); only temp.rs may"
            );
        }
    }

    // And the rule is not vacuous: temp.rs really does contain one.
    let temp = production_source("temp.rs");
    assert!(
        WRITABLE.iter().any(|needle| temp.contains(needle)),
        "temp.rs no longer opens anything for writing — has the rule moved?"
    );
}

/// The source is opened without URI interpretation, so a path can never carry
/// connection parameters such as `mode=rwc`.
#[test]
fn the_source_is_never_opened_with_uri_interpretation() {
    for module in modules() {
        assert!(
            !production_source(&module).contains("SQLITE_OPEN_URI"),
            "{module} enables URI interpretation; a file named `file:...?mode=rwc` \
             would then be reinterpreted as a writable connection"
        );
    }
}

/// Migration SQL is executed in exactly one place, and it takes the working
/// clone — not a path, and not the baseline.
#[test]
fn user_sql_is_executed_only_against_a_clone() {
    let migration = production_source("migration.rs");
    assert!(
        migration.contains("pub fn execute_on_clone(working: &WorkingClone, sql: &str)"),
        "the migration executor must take a WorkingClone, never a path or the baseline"
    );
    assert!(
        !migration.contains("execute_on_clone(path"),
        "the migration executor must never accept a path"
    );

    // No other module executes a batch of caller-supplied SQL.
    for module in modules() {
        if module == "migration.rs" || module == "temp.rs" {
            continue;
        }
        assert!(
            !production_source(&module).contains("execute_batch("),
            "{module} executes SQL; migration SQL belongs in migration.rs alone"
        );
    }
}

/// The baseline offers no way to write to it.
///
/// Immutability is not a rule anyone has to remember: `WorkingClone` has a
/// read-write opener and `BaselineSnapshot` does not, so a writable handle on
/// the pre-migration state is not an expression that can be written.
#[test]
fn the_baseline_has_no_writable_opener() {
    let temp = production_source("temp.rs");

    let baseline_impl = temp
        .split("impl BaselineSnapshot {")
        .nth(1)
        .expect("BaselineSnapshot must have an impl block");
    let baseline_impl = baseline_impl
        .split("\n}")
        .next()
        .expect("impl block must be closed");
    assert!(
        !baseline_impl.contains("open_read_write"),
        "BaselineSnapshot gained a writable opener"
    );
    assert!(
        !baseline_impl.contains("READ_WRITE") && !baseline_impl.contains("OPEN_CREATE"),
        "BaselineSnapshot gained writable open flags"
    );
    assert!(
        baseline_impl.contains("open_readonly"),
        "BaselineSnapshot must still be readable"
    );

    // And the working clone is the one that does have it.
    assert!(
        temp.contains("pub fn open_read_write(&self)"),
        "the working clone must expose the only writable opener"
    );
}

/// Data-impact counting reads the baseline and nothing else.
///
/// The whole reason the baseline exists is that counting the original after a
/// migration would answer a question about a different moment in time.
#[test]
fn impact_counting_never_reaches_the_original() {
    let impact = production_source("impact.rs");

    assert!(
        impact.contains("pub fn measure(baseline: &BaselineSnapshot"),
        "impact counting must be handed the baseline, never a path"
    );
    for forbidden in [
        "open_read_write",
        "READ_WRITE",
        "OPEN_CREATE",
        "database_path",
        "original",
        "fingerprint",
    ] {
        assert!(
            !impact.contains(forbidden),
            "impact.rs mentions {forbidden}; it may only read the baseline"
        );
    }
    // The counts are taken through the baseline's own read-only handle.
    assert!(impact.contains("baseline.open_readonly()"));
}

/// Identifiers are quoted, never interpolated raw.
#[test]
fn impact_queries_quote_every_identifier() {
    let impact = production_source("impact.rs");
    assert!(impact.contains("fn quote_identifier"));
    // Every table or column name reaching a statement goes through the quoter.
    for line in impact.lines() {
        let line = line.trim();
        if line.contains("format!(") && line.contains("FROM") {
            assert!(
                line.contains("quoted_table") || line.contains("quote_identifier"),
                "an identifier is interpolated unquoted: {line}"
            );
        }
    }
}

/// Nothing in the engine writes to a path it was given.
#[test]
fn no_module_writes_to_a_caller_supplied_path() {
    for module in modules() {
        let source = production_source(&module);
        for needle in [
            "fs::write(",
            "fs::remove_file(",
            "fs::remove_dir",
            "fs::copy(",
        ] {
            assert!(
                !source.contains(needle),
                "{module} writes to the filesystem ({needle}); clone lifetime is TempDir's job"
            );
        }
    }
}

/// Sanity check on the helper itself: it must actually be stripping test code,
/// or every assertion above would be checking an empty string.
#[test]
fn the_source_audit_reads_real_code() {
    let database = production_source("database.rs");
    assert!(database.contains("SQLITE_OPEN_READ_ONLY"));
    assert!(!database.contains("#[cfg(test)]"));
    // Comment stripping must not have eaten the code with it.
    assert!(database.contains("pub fn open_readonly"));
    assert!(!database.contains("Defence in depth"));
    assert!(Path::new(env!("CARGO_MANIFEST_DIR")).join("src").is_dir());
    assert!(modules().len() >= 8, "modules: {:?}", modules());
}
