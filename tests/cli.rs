//! End-to-end integration tests for the `ren` binary.
//!
//! These complement the unit tests in `src/main.rs` and the per-module test
//! suites by exercising the binary as a whole: argument parsing → walker →
//! plan build → validate → apply (or dry-run / list-files / preview-stub).
//! The binary is located by `assert_cmd::Command::cargo_bin("ren")`.

use std::collections::HashSet;
use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// Convenience: build a `Command` that invokes the `ren` binary with `cwd`
/// set to the given temp dir. All arguments are passed as `&str`.
fn ren(dir: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("ren").expect("ren binary built by cargo");
    cmd.current_dir(dir.path());
    cmd
}

/// Read the immediate children of `dir` (basenames only) into a `HashSet`,
/// preserving the on-disk byte casing. Used by the case-only test to assert
/// the dirent literally flipped - `fs::metadata("TMP").is_ok()` would pass
/// on case-insensitive filesystems even if the dirent still says `tmp`.
fn read_dir_basenames(dir: &std::path::Path) -> HashSet<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn basic_rename_renames_only_matching_files() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("foo.txt"), "").unwrap();
    fs::write(dir.path().join("bar.txt"), "").unwrap();

    ren(&dir).args(["foo", "qux"]).assert().success();

    assert!(dir.path().join("qux.txt").exists());
    assert!(!dir.path().join("foo.txt").exists());
    // Untouched neighbour stays intact.
    assert!(dir.path().join("bar.txt").exists());
}

#[test]
fn default_scope_is_depth_one() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("foo.txt"), "").unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("sub/foo.txt"), "").unwrap();

    ren(&dir).args(["foo", "bar"]).assert().success();

    // Top-level was renamed, nested file was not.
    assert!(dir.path().join("bar.txt").exists());
    assert!(!dir.path().join("foo.txt").exists());
    assert!(dir.path().join("sub/foo.txt").exists());
    assert!(!dir.path().join("sub/bar.txt").exists());
}

#[test]
fn recursive_renames_at_all_depths() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("foo.txt"), "").unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("sub/foo.txt"), "").unwrap();

    ren(&dir).args(["-R", "foo", "bar"]).assert().success();

    assert!(dir.path().join("bar.txt").exists());
    assert!(!dir.path().join("foo.txt").exists());
    assert!(dir.path().join("sub/bar.txt").exists());
    assert!(!dir.path().join("sub/foo.txt").exists());
}

#[test]
fn include_dirs_with_recursive_renames_dir_then_nested_file() {
    // `foo_dir` and `foo_dir/foo_inner.txt` both match `foo`. With
    // `--include-dirs -R`, the deepest-first apply order means the inner file
    // is renamed first (under its old parent name), then the directory is
    // renamed. End state: `qux_dir/qux_inner.txt`.
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("foo_dir")).unwrap();
    fs::write(dir.path().join("foo_dir/foo_inner.txt"), "").unwrap();

    ren(&dir)
        .args(["--include-dirs", "-R", "foo", "qux"])
        .assert()
        .success();

    assert!(dir.path().join("qux_dir").is_dir());
    assert!(dir.path().join("qux_dir/qux_inner.txt").exists());
    assert!(!dir.path().join("foo_dir").exists());
}

#[test]
fn dry_run_makes_no_filesystem_changes() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("foo.txt"), "").unwrap();

    ren(&dir)
        .args(["--dry-run", "foo", "bar"])
        .assert()
        .success()
        .stdout(predicate::str::contains("→"))
        .stdout(predicate::str::contains("Would rename"));

    // FS untouched.
    assert!(dir.path().join("foo.txt").exists());
    assert!(!dir.path().join("bar.txt").exists());
}

#[test]
fn list_files_prints_only_matching_basenames() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("foo.txt"), "").unwrap();
    fs::write(dir.path().join("other.txt"), "").unwrap();

    let assert = ren(&dir).args(["--list-files", "foo"]).assert().success();
    let output = assert.get_output();
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();

    // Only `foo.txt` matched. `display_path` strips a leading `./`, so the
    // output is the bare basename relative to cwd.
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines, vec!["foo.txt"], "stdout: {stdout:?}");
}

#[test]
fn case_only_rename_flips_dirent_via_temp_hop() {
    // The temp-hop is unconditional in `ren`, so this test exercises and
    // proves the algorithm regardless of the filesystem's case sensitivity.
    // On macOS APFS (where /var/folders typically lives) a naive single
    // `fs::rename("tmp", "TMP")` is a silent no-op; the dirent will remain
    // `tmp`. With the temp-hop, the dirent literally flips to `TMP`.
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("tmp"), "").unwrap();

    ren(&dir).args(["tmp", "TMP"]).assert().success();

    let names = read_dir_basenames(dir.path());
    assert!(
        names.contains("TMP"),
        "expected dirent to be 'TMP' after rename, got: {names:?}"
    );
    assert!(
        !names.contains("tmp"),
        "expected old 'tmp' dirent to be gone, got: {names:?}"
    );
}

#[test]
fn duplicate_target_is_a_validation_error() {
    // Both `a.txt` and `b.txt` map to `c.txt` via the regex `^[ab]\.txt$`,
    // which `validate_plan` should reject as a within-plan conflict before
    // any rename touches the disk.
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.txt"), "").unwrap();
    fs::write(dir.path().join("b.txt"), "").unwrap();

    ren(&dir)
        .args(["--regex", r"^[ab]\.txt$", "c.txt"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("within-plan conflicts"));

    // Validation must reject before applying anything.
    assert!(dir.path().join("a.txt").exists());
    assert!(dir.path().join("b.txt").exists());
    assert!(!dir.path().join("c.txt").exists());
}

#[test]
fn smart_mode_renames_all_case_variants() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("foo_bar.rs"), "").unwrap();
    fs::write(dir.path().join("FooBar.tsx"), "").unwrap();

    ren(&dir)
        .args(["--smart", "foo_bar", "hello_world"])
        .assert()
        .success();

    assert!(dir.path().join("hello_world.rs").exists());
    assert!(dir.path().join("HelloWorld.tsx").exists());
    assert!(!dir.path().join("foo_bar.rs").exists());
    assert!(!dir.path().join("FooBar.tsx").exists());
}

#[test]
fn short_help_short_circuits_with_usage() {
    let mut cmd = Command::cargo_bin("ren").unwrap();
    cmd.arg("-h")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage"))
        .stdout(predicate::str::contains("--preview"));
}

#[test]
fn long_help_short_circuits_with_examples() {
    let mut cmd = Command::cargo_bin("ren").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage"))
        .stdout(predicate::str::contains("Examples"));
}

#[test]
fn no_extension_protects_extension_from_rewrite() {
    // `-E` matches against the file stem only. With pattern `txt → notes`:
    //
    //   - `report.txt`: stem `report` doesn't contain `txt`, ext `txt` is
    //     ignored under `-E` → unchanged.
    //   - `notes.md`: stem `notes` doesn't contain `txt` → unchanged.
    //
    // Without `-E`, `report.txt` would be rewritten to `report.notes`, which
    // is the accident `-E` is designed to prevent.
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("report.txt"), "").unwrap();
    fs::write(dir.path().join("notes.md"), "").unwrap();

    ren(&dir).args(["-E", "txt", "notes"]).assert().success();

    // Both files untouched: stem-only matching means `txt` in the extension
    // doesn't trigger a rename, and `notes.md` doesn't match anywhere.
    assert!(dir.path().join("report.txt").exists());
    assert!(dir.path().join("notes.md").exists());
    assert!(!dir.path().join("report.notes").exists());
}

#[test]
fn no_extension_renames_stem_and_reattaches_extension() {
    // Positive case: `-E` rewrites the stem and reattaches the original ext
    // unchanged. `foo.rs` becomes `bar.rs`.
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("foo.rs"), "").unwrap();
    fs::write(dir.path().join("foo.tar.gz"), "").unwrap();

    ren(&dir).args(["-E", "foo", "bar"]).assert().success();

    assert!(dir.path().join("bar.rs").exists());
    assert!(dir.path().join("bar.tar.gz").exists());
    assert!(!dir.path().join("foo.rs").exists());
    assert!(!dir.path().join("foo.tar.gz").exists());
}

#[test]
fn multiple_expressions_apply_in_order_in_one_pass() {
    // `-e foo bar -e baz qux` runs both substitutions in sequence on each
    // basename. `foo_baz.txt` becomes `bar_qux.txt`.
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("foo_baz.txt"), "").unwrap();

    ren(&dir)
        .args(["-e", "foo", "bar", "-e", "baz", "qux"])
        .assert()
        .success();

    assert!(dir.path().join("bar_qux.txt").exists());
    assert!(!dir.path().join("foo_baz.txt").exists());
}
