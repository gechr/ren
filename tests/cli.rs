//! End-to-end integration tests for the `ren` binary.
//!
//! These complement the unit tests in `src/main.rs` and the per-module test
//! suites by exercising the binary as a whole: argument parsing → walker →
//! plan build → validate → apply (or dry-run / list-files / preview-stub).
//! The binary is located by `assert_cmd::Command::cargo_bin("ren")`.

use std::collections::HashSet;
use std::fs;
use std::process::{Command, Stdio};

use assert_cmd::assert::OutputAssertExt as _;
use assert_cmd::cargo::CommandCargoExt as _;
use predicates::prelude::*;
use tempfile::TempDir;

/// `ren` binary cwd-set to `dir`. Stdin is nulled so `ren`'s auto-detection
/// doesn't trip on the FIFO that `assert_cmd::Command` would attach.
fn ren(dir: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("ren").expect("ren binary built by cargo");
    cmd.current_dir(dir.path()).stdin(Stdio::null());
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
fn transform_only_single_file_path_is_renamed() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("Cargo.lock"), "").unwrap();

    ren(&dir).args(["-L", "Cargo.lock"]).assert().success();

    let names = read_dir_basenames(dir.path());
    assert!(names.contains("cargo.lock"));
    assert!(!names.contains("Cargo.lock"));
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
        // `-x` so the regex anchors match the full basename, including the
        // `.txt` extension; the default Exclude scope would only see stems.
        .args(["-x", "--regex", r"^[ab]\.txt$", "c.txt"])
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
fn default_scope_protects_extension_from_rewrite() {
    // The default scope matches against the file stem only. With pattern
    // `txt → notes`:
    //
    //   - `report.txt`: stem `report` doesn't contain `txt`, ext `txt` is
    //     ignored by default → unchanged.
    //   - `notes.md`: stem `notes` doesn't contain `txt` → unchanged.
    //
    // With `-x`, `report.txt` would be rewritten to `report.notes`, which is
    // the accident the stem-only default is designed to prevent.
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("report.txt"), "").unwrap();
    fs::write(dir.path().join("notes.md"), "").unwrap();

    ren(&dir).args(["txt", "notes"]).assert().success();

    // Both files untouched: stem-only matching means `txt` in the extension
    // doesn't trigger a rename, and `notes.md` doesn't match anywhere.
    assert!(dir.path().join("report.txt").exists());
    assert!(dir.path().join("notes.md").exists());
    assert!(!dir.path().join("report.notes").exists());
}

#[test]
fn default_scope_renames_stem_and_reattaches_extension() {
    // Positive case: the default rewrites the stem and reattaches the
    // original ext unchanged. `foo.rs` becomes `bar.rs`.
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("foo.rs"), "").unwrap();
    fs::write(dir.path().join("foo.tar.gz"), "").unwrap();

    ren(&dir).args(["foo", "bar"]).assert().success();

    assert!(dir.path().join("bar.rs").exists());
    assert!(dir.path().join("bar.tar.gz").exists());
    assert!(!dir.path().join("foo.rs").exists());
    assert!(!dir.path().join("foo.tar.gz").exists());
}

#[test]
fn include_extension_flag_matches_full_basename() {
    // `-x` opts into matching the full basename, including the extension.
    // `report.txt` becomes `report.notes` because the ext matches `txt`.
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("report.txt"), "").unwrap();

    ren(&dir).args(["-x", "txt", "notes"]).assert().success();

    assert!(dir.path().join("report.notes").exists());
    assert!(!dir.path().join("report.txt").exists());
}

#[test]
fn only_extension_flag_matches_extension_and_preserves_stem() {
    // `-X` runs the pipeline on the extension only. `foo.rs` becomes
    // `foo.txt`; files without an extension (`Makefile`) are skipped.
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("foo.rs"), "").unwrap();
    fs::write(dir.path().join("Makefile"), "").unwrap();

    ren(&dir).args(["-X", "rs", "txt"]).assert().success();

    assert!(dir.path().join("foo.txt").exists());
    assert!(!dir.path().join("foo.rs").exists());
    assert!(dir.path().join("Makefile").exists());
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

/// `assert_cmd::Command` variant for `write_stdin`-using tests, which need
/// the wrapper's automatic `Stdio::piped()` on stdin.
fn ren_stdin(dir: &TempDir) -> assert_cmd::Command {
    let mut cmd = Command::cargo_bin("ren").expect("ren binary built by cargo");
    cmd.current_dir(dir.path());
    assert_cmd::Command::from_std(cmd)
}

#[test]
fn stdin_mode_reads_newline_separated_paths() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("foo_a.txt"), "").unwrap();
    fs::write(dir.path().join("foo_b.txt"), "").unwrap();
    // Omitted from stdin: must NOT be touched even though a cwd walk would match.
    fs::write(dir.path().join("foo_skip.txt"), "").unwrap();

    ren_stdin(&dir)
        .args(["foo", "bar"])
        .write_stdin("foo_a.txt\nfoo_b.txt\n")
        .assert()
        .success();

    assert!(dir.path().join("bar_a.txt").exists());
    assert!(dir.path().join("bar_b.txt").exists());
    assert!(dir.path().join("foo_skip.txt").exists());
}

#[test]
fn stdin_mode_with_null_splits_on_nul() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("foo_a.txt"), "").unwrap();
    fs::write(dir.path().join("foo_b.txt"), "").unwrap();

    ren_stdin(&dir)
        .args(["-0", "foo", "bar"])
        .write_stdin("foo_a.txt\0foo_b.txt\0")
        .assert()
        .success();

    assert!(dir.path().join("bar_a.txt").exists());
    assert!(dir.path().join("bar_b.txt").exists());
}

#[test]
fn null_with_explicit_paths_errors() {
    // `-0` only makes sense when reading from stdin. With an explicit path, we
    // bail rather than silently ignoring the flag.
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("foo.txt"), "").unwrap();

    ren(&dir)
        .args(["-0", "foo", "bar", "."])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--null"));
}
