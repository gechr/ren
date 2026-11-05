//! End-to-end integration tests for the `ren` binary.
//!
//! These complement the unit tests in the individual modules and test
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
/// doesn't trip on the FIFO that `assert_cmd::Command` would attach. All
/// `REN_*` env vars are scrubbed so the developer's shell can't bleed into
/// snapshot assertions, and `REN_CONFIG_PATH=""` disables the
/// `~/.config/ren/config.toml` lookup. Tests that exercise env or config
/// behavior can `.env(...)` back in selectively.
fn ren(dir: &TempDir) -> Command {
    let mut cmd = Command::cargo_bin("ren").expect("ren binary built by cargo");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("REN_") {
            cmd.env_remove(&key);
        }
    }
    // Empty (not unset) means "no config file"; an unset value would fall
    // through to the default `~/.config/ren/config.toml` lookup.
    cmd.env("REN_CONFIG_PATH", "");
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

    ren(&dir).args(["-W", "foo", "qux"]).assert().success();

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

    ren(&dir).args(["-W", "foo", "bar"]).assert().success();

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

    ren(&dir)
        .args(["-W", "-R", "foo", "bar"])
        .assert()
        .success();

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
        .args(["-W", "--include-dirs", "-R", "foo", "qux"])
        .assert()
        .success();

    assert!(dir.path().join("qux_dir").is_dir());
    assert!(dir.path().join("qux_dir/qux_inner.txt").exists());
    assert!(!dir.path().join("foo_dir").exists());
}

#[test]
fn nested_directory_summary_uses_source_types_before_renaming() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("foo_parent/foo_child")).unwrap();
    ren(&dir)
        .args(["-W", "-D", "-R", "foo", "bar"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Renamed 2 directories"));
    assert!(dir.path().join("bar_parent/bar_child").is_dir());
}

#[cfg(unix)]
#[test]
fn closed_stdout_is_graceful_for_help_listing_and_apply() {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("old.txt"), "data").unwrap();
    for args in [vec!["--help"], vec!["-l"], vec!["-W", "old", "new"]] {
        let (reader, writer) = UnixStream::pair().unwrap();
        drop(reader);
        ren(&dir)
            .args(args)
            .stdout(OwnedFd::from(writer))
            .assert()
            .success()
            .stderr("");
    }
    assert_eq!(
        fs::read_to_string(dir.path().join("new.txt")).unwrap(),
        "data"
    );
}

#[cfg(unix)]
#[test]
fn closed_stdout_does_not_hide_invalid_input_paths() {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("file.txt"), "data").unwrap();
    let (reader, writer) = UnixStream::pair().unwrap();
    drop(reader);
    ren(&dir)
        .args(["-l", "", ".", "missing"])
        .stdout(OwnedFd::from(writer))
        .assert()
        .failure()
        .stderr(predicate::str::contains("1 path could not be processed"));
}

#[cfg(unix)]
#[test]
fn closed_stderr_preserves_failures_and_independent_renames() {
    use std::io::Write as _;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("old.txt"), "data").unwrap();
    for (args, code) in [(vec!["--unknown-option"], 2), (vec!["old"], 1)] {
        let (reader, writer) = UnixStream::pair().unwrap();
        drop(reader);
        ren(&dir)
            .args(args)
            .stderr(OwnedFd::from(writer))
            .assert()
            .code(code);
    }
    let (reader, writer) = UnixStream::pair().unwrap();
    drop(reader);
    let mut child = ren(&dir)
        .args(["-W", "old", "new"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(OwnedFd::from(writer))
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"old.txt\nmissing\n")
        .unwrap();
    assert_eq!(child.wait_with_output().unwrap().status.code(), Some(1));
    assert_eq!(
        fs::read_to_string(dir.path().join("new.txt")).unwrap(),
        "data"
    );
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
fn dry_run_is_the_default_without_write_flag() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("foo.txt"), "").unwrap();

    ren(&dir)
        .args(["foo", "bar"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Would rename"));

    assert!(dir.path().join("foo.txt").exists());
    assert!(!dir.path().join("bar.txt").exists());
}

#[test]
fn write_and_dry_run_together_is_a_mutex_error() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("foo.txt"), "").unwrap();

    ren(&dir)
        .args(["-W", "--dry-run", "foo", "bar"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--write"))
        .stderr(predicate::str::contains("--dry-run"));

    // FS untouched.
    assert!(dir.path().join("foo.txt").exists());
}

#[test]
fn transform_only_single_file_path_is_renamed() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("Cargo.lock"), "").unwrap();

    ren(&dir)
        .args(["-W", "-L", "Cargo.lock"])
        .assert()
        .success();

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

    ren(&dir).args(["-W", "tmp", "TMP"]).assert().success();

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
        .stderr(predicate::str::contains(
            "multiple files would be renamed to the same target",
        ));

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
        .args(["-W", "--smart", "foo_bar", "hello_world"])
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

    ren(&dir).args(["-W", "foo", "bar"]).assert().success();

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

    ren(&dir)
        .args(["-W", "-x", "txt", "notes"])
        .assert()
        .success();

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

    ren(&dir).args(["-W", "-X", "rs", "txt"]).assert().success();

    assert!(dir.path().join("foo.txt").exists());
    assert!(!dir.path().join("foo.rs").exists());
    assert!(dir.path().join("Makefile").exists());
}

#[test]
fn change_extension_flag_swaps_extension_and_keeps_stem() {
    // `-E` overrides the reattached extension while leaving the stem alone.
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("photo.jpg"), "").unwrap();

    ren(&dir).args(["-W", "-E", "png"]).assert().success();

    assert!(dir.path().join("photo.png").exists());
    assert!(!dir.path().join("photo.jpg").exists());
}

#[test]
fn change_extension_combines_with_supplant_to_rename_and_retype() {
    // The headline combination: renumber the stem AND change the extension in
    // a single pass. `image_a.jpg` / `image_b.jpg` become `1.png` / `2.png`.
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("image_a.jpg"), "").unwrap();
    fs::write(dir.path().join("image_b.jpg"), "").unwrap();

    ren(&dir)
        .args(["-W", "-s", "{n}", "-E", "png"])
        .assert()
        .success();

    assert!(dir.path().join("1.png").exists());
    assert!(dir.path().join("2.png").exists());
    assert!(!dir.path().join("image_a.jpg").exists());
    assert!(!dir.path().join("image_b.jpg").exists());
}

#[test]
fn change_extension_empty_strips_extension() {
    // `-E ''` drops the extension entirely.
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("notes.bak"), "").unwrap();

    ren(&dir).args(["-W", "-E", ""]).assert().success();

    assert!(dir.path().join("notes").exists());
    assert!(!dir.path().join("notes.bak").exists());
}

#[test]
fn change_extension_conflicts_with_only_extension() {
    // `-E` and `-X` both redefine extension handling, so combining them errs.
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("foo.jpg"), "").unwrap();

    ren(&dir)
        .args(["-W", "-E", "png", "-X", "rs", "txt"])
        .assert()
        .failure();

    assert!(dir.path().join("foo.jpg").exists());
}

#[test]
fn multiple_expressions_apply_in_order_in_one_pass() {
    // `-e foo bar -e baz qux` runs both substitutions in sequence on each
    // basename. `foo_baz.txt` becomes `bar_qux.txt`.
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("foo_baz.txt"), "").unwrap();

    ren(&dir)
        .args(["-W", "-e", "foo", "bar", "-e", "baz", "qux"])
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
        .args(["-W", "foo", "bar"])
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
        .args(["-W", "-0", "foo", "bar"])
        .write_stdin("foo_a.txt\0foo_b.txt\0")
        .assert()
        .success();

    assert!(dir.path().join("bar_a.txt").exists());
    assert!(dir.path().join("bar_b.txt").exists());
}

#[test]
fn create_dirs_makes_missing_parents_for_regex_target() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.log"), "").unwrap();
    fs::write(dir.path().join("b.log"), "").unwrap();

    ren(&dir)
        .args([
            "-W",
            "--create-dirs",
            "-x",
            "--regex",
            r"^(.*)\.log$",
            "logs/$1.log",
            ".",
        ])
        .assert()
        .success();

    assert!(dir.path().join("logs").is_dir());
    assert!(dir.path().join("logs/a.log").exists());
    assert!(dir.path().join("logs/b.log").exists());
    assert!(!dir.path().join("a.log").exists());
}

#[test]
fn missing_parent_without_create_dirs_fails_and_rolls_back() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("a.log"), "").unwrap();

    ren(&dir)
        .args(["-W", "-x", "--regex", r"^(.*)\.log$", "logs/$1.log", "."])
        .assert()
        .failure();

    // Rollback restores the original file; no `logs/` directory was created.
    assert!(dir.path().join("a.log").exists());
    assert!(!dir.path().join("logs").exists());
}

#[test]
fn directory_relocates_flattening_by_default() {
    // `a/b/foo.txt` renamed with `-d out` flattens to `out/bar.txt` by default;
    // the source's `a/b` parents are dropped. The `out` dir is created
    // automatically (no `--create-dirs`).
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("a/b")).unwrap();
    fs::write(dir.path().join("a/b/foo.txt"), "").unwrap();

    ren(&dir)
        .args(["-W", "-R", "-d", "out", "foo", "bar"])
        .assert()
        .success();

    assert!(dir.path().join("out/bar.txt").exists());
    assert!(!dir.path().join("out/a/b/bar.txt").exists());
    assert!(!dir.path().join("a/b/foo.txt").exists());
}

#[test]
fn directory_relocates_preserving_subtree_with_parents() {
    // `--parents` recreates the source's `a/b` structure under `-d out`, so
    // `a/b/foo.txt` lands at `out/a/b/bar.txt`; intermediate dirs are created.
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("a/b")).unwrap();
    fs::write(dir.path().join("a/b/foo.txt"), "").unwrap();

    ren(&dir)
        .args(["-W", "-R", "-d", "out", "--parents", "foo", "bar"])
        .assert()
        .success();

    assert!(dir.path().join("out/a/b/bar.txt").exists());
    assert!(!dir.path().join("a/b/foo.txt").exists());
}

#[test]
fn parents_without_directory_errors() {
    // `--parents` only makes sense alongside `-d`; used alone it must error
    // rather than silently do nothing.
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("foo.txt"), "").unwrap();

    ren(&dir)
        .args(["-W", "--parents", "foo", "bar", "."])
        .assert()
        .failure();
}

#[test]
fn directory_short_flag_no_longer_means_include_dirs() {
    // `-d` selects `--directory`; `--include-dirs` moved to `-D`. A bare `-d`
    // with a directory value must not toggle directory admission.
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("foodir")).unwrap();
    fs::write(dir.path().join("foodir/inner.txt"), "").unwrap();

    // `-D` admits directories into the plan and renames `foodir` → `bardir`.
    ren(&dir)
        .args(["-W", "-D", "foo", "bar"])
        .assert()
        .success();
    assert!(dir.path().join("bardir").is_dir());
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

#[test]
fn missing_explicit_roots_abort_but_stdin_continues_valid_inputs() {
    for stdin in [false, true] {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("foo.txt"), "content").unwrap();
        if stdin {
            ren_stdin(&dir)
                .args(["-W", "foo", "bar"])
                .write_stdin("missing.txt\nfoo.txt\n")
                .assert()
                .failure();
        } else {
            ren(&dir)
                .args(["-W", "foo", "bar", "missing.txt", "foo.txt"])
                .assert()
                .failure();
        }
        assert_eq!(
            fs::read_to_string(dir.path().join(if stdin { "bar.txt" } else { "foo.txt" })).unwrap(),
            "content"
        );
        assert!(
            !dir.path()
                .join(if stdin { "foo.txt" } else { "bar.txt" })
                .exists()
        );
    }
}

#[test]
fn transform_defaults_with_invalid_roots_cannot_modify_other_roots() {
    for from_config in [false, true] {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("Foo.txt"), "first").unwrap();
        fs::write(dir.path().join("Other.TXT"), "second").unwrap();
        let mut command = ren(&dir);
        if from_config {
            let config = dir.path().join("defaults.toml");
            fs::write(&config, "lower = true\nwrite = true\n").unwrap();
            command.env("REN_CONFIG_PATH", config);
        } else {
            command.env("REN_PREPEND", "x");
        }
        command
            .args(["-W", "Foo", "Q", "."])
            .assert()
            .failure()
            .stderr(predicate::str::contains("Unable to resolve Foo"));
        let names = read_dir_basenames(dir.path());
        assert!(names.contains("Foo.txt"));
        assert!(names.contains("Other.TXT"));
        assert_eq!(names.len(), if from_config { 3 } else { 2 });
        assert_eq!(
            fs::read_to_string(dir.path().join("Foo.txt")).unwrap(),
            "first"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("Other.TXT")).unwrap(),
            "second"
        );
    }
}

#[test]
fn configuration_can_change_extensions_and_cli_overrides_it() {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join("settings.toml");
    fs::write(&config, "change-extension = \"png\"\n").unwrap();
    fs::write(dir.path().join("a.jpg"), "content").unwrap();
    ren(&dir)
        .env("REN_CONFIG_PATH", &config)
        .args(["-W", "a.jpg"])
        .assert()
        .success();
    assert!(dir.path().join("a.png").exists());
    ren(&dir)
        .env("REN_CONFIG_PATH", &config)
        .args(["-W", "-E", "webp", "a.png"])
        .assert()
        .success();
    assert!(dir.path().join("a.webp").exists());
}

#[test]
fn overlapping_roots_keep_first_root_and_number_each_entry_once() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("sub/nested")).unwrap();
    fs::write(dir.path().join("sub/nested/a.txt"), "a").unwrap();
    ren(&dir)
        .args([
            "-W",
            "-R",
            "--parents",
            "-d",
            "out",
            "--prepend",
            "{n}",
            "sub",
            "./sub/nested",
            "sub/nested/a.txt",
        ])
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(dir.path().join("out/nested/1a.txt")).unwrap(),
        "a"
    );
}

#[test]
fn stdin_parent_and_child_apply_in_ancestry_order() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("d")).unwrap();
    fs::write(dir.path().join("d/dx.txt"), "content").unwrap();
    ren_stdin(&dir)
        .args(["-W", "d", "e"])
        .write_stdin("d\nd/dx.txt\n")
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(dir.path().join("e/ex.txt")).unwrap(),
        "content"
    );
}

#[test]
fn candidate_exclusions_are_exact_and_explicit_files_are_honored() {
    let dir = TempDir::new().unwrap();
    for name in ["tags", "TAGS", "mytags", "backup~"] {
        fs::write(dir.path().join(name), "content").unwrap();
    }
    ren(&dir)
        .args(["-W", "--prepend", "x", "."])
        .assert()
        .success();
    assert!(dir.path().join("xmytags").exists());
    for name in ["tags", "TAGS", "backup~"] {
        fs::write(dir.path().join(name), "content").unwrap();
        ren(&dir)
            .args(["-W", "--prepend", "x", name])
            .assert()
            .success();
        assert!(dir.path().join(format!("x{name}")).exists());
        fs::remove_file(dir.path().join(format!("x{name}"))).unwrap();
    }
}

#[test]
fn no_ignore_does_not_admit_vcs_metadata() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join(".git/HEAD"), "content").unwrap();
    ren(&dir)
        .args(["-W", "-H", "-R", "--no-ignore", "--prepend", "x", "."])
        .assert()
        .success();
    assert!(dir.path().join(".git/HEAD").exists());
}

#[test]
fn expression_preprocessing_respects_argument_terminator() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("-Example"), "content").unwrap();
    ren(&dir)
        .args(["-W", "--lower", "--", "-Example"])
        .assert()
        .success();
    assert!(dir.path().join("-example").exists());
}

#[cfg(unix)]
#[test]
fn aliases_deduplicate_but_distinct_hardlinks_remain() {
    let dir = TempDir::new().unwrap();
    fs::create_dir(dir.path().join("real")).unwrap();
    fs::write(dir.path().join("real/a.txt"), "content").unwrap();
    fs::hard_link(dir.path().join("real/a.txt"), dir.path().join("real/b.txt")).unwrap();
    std::os::unix::fs::symlink("real", dir.path().join("alias")).unwrap();
    ren(&dir)
        .args([
            "-W",
            "--prepend",
            "{n}",
            "real/a.txt",
            "alias/a.txt",
            "alias/b.txt",
        ])
        .assert()
        .success();
    // The retained alias/b path sorts before real/a, but both share a counter.
    assert!(dir.path().join("real/2a.txt").exists());
    assert!(dir.path().join("real/1b.txt").exists());
}

#[test]
fn malformed_ignore_warns_without_failing_valid_renames() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join(".ignore"), "{a,b\n").unwrap();
    fs::write(dir.path().join("foo.txt"), "content").unwrap();
    ren(&dir)
        .args(["-W", "foo", "bar", "."])
        .assert()
        .success()
        .stderr(predicate::str::contains("Warning:"));
    assert!(dir.path().join("bar.txt").exists());
}

#[cfg(unix)]
#[test]
fn closed_stderr_during_warnings_still_processes_valid_files() {
    use std::os::fd::OwnedFd;
    #[cfg(target_os = "linux")]
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::net::UnixStream;

    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join(".ignore"), "{a,b\n").unwrap();
    // APFS rejects invalid UTF-8 basenames before ren can discover them.
    #[cfg(target_os = "linux")]
    let invalid = dir.path().join(std::ffi::OsStr::from_bytes(b"\xff.txt"));
    #[cfg(target_os = "linux")]
    fs::write(&invalid, "unchanged").unwrap();
    fs::write(dir.path().join("foo.txt"), "content").unwrap();
    let (reader, writer) = UnixStream::pair().unwrap();
    drop(reader);
    ren(&dir)
        .args(["-W", "foo", "bar", "."])
        .stderr(OwnedFd::from(writer))
        .assert()
        .code(1);
    assert_eq!(
        fs::read_to_string(dir.path().join("bar.txt")).unwrap(),
        "content"
    );
    #[cfg(target_os = "linux")]
    assert_eq!(fs::read_to_string(invalid).unwrap(), "unchanged");
}

#[test]
fn relocation_parent_collision_fails_before_creating_directories() {
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("photos/x")).unwrap();
    fs::create_dir_all(dir.path().join("photos/y")).unwrap();
    fs::write(dir.path().join("photos/y/z"), "content").unwrap();
    ren(&dir)
        .args([
            "-W",
            "-R",
            "-D",
            "-d",
            "out",
            "--parents",
            "-e",
            "x",
            "y",
            "-e",
            "z",
            "w",
            "photos",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "also required as a parent directory",
        ));
    assert!(!dir.path().join("out").exists());
    assert!(dir.path().join("photos/x").is_dir());
    assert!(dir.path().join("photos/y/z").is_file());
}

#[test]
fn parent_alias_collisions_leave_no_created_directories() {
    for target in ["out/Y/w", "out/y/../w"] {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("x"), "first").unwrap();
        fs::write(dir.path().join("z"), "second").unwrap();
        ren(&dir)
            .args([
                "-W",
                "--create-dirs",
                "-e",
                "x",
                "out/y",
                "-e",
                "z",
                target,
                "x",
                "z",
            ])
            .assert()
            .failure()
            .stderr(predicate::str::contains(
                "also required as a parent directory",
            ));
        assert!(!dir.path().join("out").exists());
        assert_eq!(fs::read_to_string(dir.path().join("x")).unwrap(), "first");
        assert_eq!(fs::read_to_string(dir.path().join("z")).unwrap(), "second");
    }
}

#[cfg(unix)]
#[test]
fn invalid_utf8_pattern_is_an_error_without_panicking() {
    use std::os::unix::ffi::OsStrExt as _;
    let dir = TempDir::new().unwrap();
    ren(&dir)
        .arg(std::ffi::OsStr::from_bytes(b"\xff"))
        .args(["x", "."])
        .assert()
        .failure()
        .stderr(predicate::str::contains("find pattern must be valid UTF-8"));
}

#[cfg(target_os = "linux")]
#[test]
fn non_utf8_parent_path_survives_argument_parsing() {
    use std::os::unix::ffi::OsStrExt as _;
    let dir = TempDir::new().unwrap();
    let parent = dir.path().join(std::ffi::OsStr::from_bytes(b"dir\xff"));
    fs::create_dir(&parent).unwrap();
    fs::write(parent.join("Foo.txt"), "content").unwrap();
    ren(&dir)
        .args(["-W", "--lower"])
        .arg(parent.join("Foo.txt"))
        .assert()
        .success();
    assert!(parent.join("foo.txt").exists());
}
