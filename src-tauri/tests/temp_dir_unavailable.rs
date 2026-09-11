//! What happens when the system temporary directory itself is unusable.
//!
//! A single test in its own binary: it changes a process-wide environment
//! variable, so it must not run beside anything that creates a clone.

mod common;

use common::*;
use migradry_lib::{MigraDryError, MigrationService};

#[cfg(unix)]
#[test]
fn a_temporary_directory_that_cannot_be_written_is_reported_not_panicked() {
    use std::os::unix::fs::PermissionsExt;

    let work = tempfile::tempdir().unwrap();
    let database = work.path().join("app.db");
    seed_database(&database, "CREATE TABLE users (id INTEGER PRIMARY KEY);");
    let migration = work.path().join("001.sql");
    std::fs::write(&migration, "CREATE TABLE orders (id INTEGER PRIMARY KEY);").unwrap();

    let hash_before = sha256_of(&database);

    let unusable = tempfile::tempdir().unwrap();
    std::fs::set_permissions(unusable.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    // Root ignores permission bits, in which case there is nothing to test.
    let enforced = std::fs::write(unusable.path().join("probe"), b"x").is_err();

    let restore = std::env::var_os("TMPDIR");
    std::env::set_var("TMPDIR", unusable.path());
    let outcome = MigrationService::preview(&database, &migration);
    match restore {
        Some(value) => std::env::set_var("TMPDIR", value),
        None => std::env::remove_var("TMPDIR"),
    }
    std::fs::set_permissions(unusable.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

    if enforced {
        let error = outcome.expect_err("a clone cannot be created with nowhere to put it");
        assert!(
            matches!(error, MigraDryError::CloneFailed { .. }),
            "unexpected error: {error}"
        );
        // The failure is about MigraDry's own scratch space, not a verdict on
        // the user's database.
        assert!(
            error.to_string().contains("Temporary database clone"),
            "unexpected message: {error}"
        );
    }

    // Either way the original is untouched.
    assert_eq!(hash_before, sha256_of(&database));
    assert_eq!(table_names(&database), vec!["users"]);
}
