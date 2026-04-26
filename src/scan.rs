// Walker + candidate-path check for `ren`.
//
// Adapted from the sibling crate `rep` (which itself derives from fastmod,
// Copyright Meta Platforms, Inc. and affiliates), used under the Apache
// License, Version 2.0. See LICENSE and NOTICE at the repo root for details.
//
// `ren` walks paths and renames basenames; unlike `rep` it does not need a
// grep-based content pre-filter, so the searcher/sink machinery is dropped.
// The walk is single-threaded - there is no expensive per-file work to
// parallelise.

use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context as _;
use anyhow::Error;
use anyhow::ensure;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;

type Result<T> = ::std::result::Result<T, Error>;

#[derive(Clone)]
pub(crate) struct FileSet {
    pub(crate) matches: Vec<String>,
    pub(crate) case_insensitive: bool,
}

/// A walked path tagged with the root it was found under.
///
/// Phase 3 uses `root` to compute the path's depth relative to the originating
/// walk root via `path.strip_prefix(root)`, which keeps multi-root walks
/// correct (a file two levels deep under root `a` and a file two levels deep
/// under root `b` are both depth 2, regardless of their absolute component
/// counts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathRecord {
    pub(crate) path: PathBuf,
    pub(crate) root: PathBuf,
}

/// Build a `WalkBuilder` configured with the supplied roots and (optional)
/// glob override set.
///
/// `recursive == false` clamps the walk to depth 1 (the root and its direct
/// children). `follow_links(false)` is unconditional - `ren` operates on the
/// link itself, not its target, and dangling symlinks should still surface
/// rather than being silently skipped.
pub(crate) fn walk_builder(
    dirs: Vec<&str>,
    file_set: Option<FileSet>,
    recursive: bool,
) -> Result<WalkBuilder> {
    ensure!(!dirs.is_empty(), "must provide at least one path to walk!");
    let mut builder = WalkBuilder::new(dirs[0]);
    for dir in &dirs[1..] {
        builder.add(dir);
    }
    builder.follow_links(false);
    if !recursive {
        builder.max_depth(Some(1));
    }
    if let Some(file_set) = file_set {
        let mut override_builder = OverrideBuilder::new(".");
        if file_set.case_insensitive {
            override_builder
                .case_insensitive(true)
                .context("Unable to toggle case sensitivity")?;
        }
        for file in file_set.matches {
            override_builder
                .add(&file)
                .context("Unable to register glob with directory walker")?;
        }
        builder.overrides(
            override_builder
                .build()
                .context("Unable to register glob with directory walker")?,
        );
    }
    Ok(builder)
}

pub(crate) fn apply_walk_flags(builder: &mut WalkBuilder, hidden: bool, no_ignore: bool) {
    builder.hidden(!hidden);
    if no_ignore {
        builder
            .ignore(false)
            .git_ignore(false)
            .git_exclude(false)
            .git_global(false);
    } else {
        builder.filter_entry(|entry| !is_vcs_path(entry.path()));
    }
}

pub(crate) fn is_candidate_path(path: &Path) -> bool {
    let bytes = path.as_os_str().as_encoded_bytes();
    !bytes.ends_with(b"~") && !bytes.ends_with(b"tags") && !bytes.ends_with(b"TAGS")
}

fn is_vcs_path(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component.as_os_str(),
            name if is_vcs_dir_name(name)
        )
    })
}

fn is_vcs_dir_name(name: &OsStr) -> bool {
    matches!(
        name.as_encoded_bytes(),
        b".git" | b".jj" | b".hg" | b".svn" | b"CVS"
    )
}

/// Parse the `-f` smart glob mini-DSL into the iglob patterns consumed
/// by `walk_builder`.
///
/// Supports comma-separated patterns:
///   `txt`         → `*.txt`        (extension)
///   `=Dockerfile` → `Dockerfile`   (exact filename)
///   `!=Makefile`  → `!Makefile`    (exclude exact filename)
///   `*.json`      → `*.json`       (glob as-is)
///   `!rs`         → `!*.rs`        (exclude extension)
pub(crate) fn parse_file_globs(input: &str) -> Vec<String> {
    let mut globs = Vec::new();
    for part in input.split(',') {
        let pattern = part.trim();
        if pattern.is_empty() || pattern == "." {
            continue;
        }
        let glob = if let Some(rest) = pattern.strip_prefix("!=") {
            format!("!{rest}")
        } else if let Some(rest) = pattern.strip_prefix('=') {
            rest.to_string()
        } else if pattern.contains('*') {
            pattern.to_string()
        } else if let Some(rest) = pattern.strip_prefix('!') {
            format!("!*.{rest}")
        } else {
            format!("*.{pattern}")
        };
        if !glob.is_empty() {
            globs.push(glob);
        }
    }
    globs
}

/// Walk every path in `dirs`, returning every entry that should be considered
/// for renaming, naturally sorted by path.
///
/// Files are admitted unconditionally; directories only when `include_dirs` is
/// true. Directory roots themselves (depth 0 of the walk) are always skipped -
/// `ren` only renames things *inside* directories it was pointed at. File roots
/// are admitted so `ren -L path/to/File` can rename that file directly.
///
/// The returned `Vec<PathRecord>` is sorted by `path` using `natord::compare`
/// on `to_string_lossy()`. This deterministic ordering is a load-bearing
/// invariant for Phase 3, which preserves it through plan construction and
/// per-depth grouping so temp-name allocation and rollback ordering are
/// reproducible across runs.
pub(crate) fn walk_paths(
    dirs: Vec<&str>,
    file_set: Option<FileSet>,
    hidden: bool,
    no_ignore: bool,
    recursive: bool,
    include_dirs: bool,
) -> Vec<PathRecord> {
    let mut records: Vec<PathRecord> = Vec::new();

    for dir in dirs {
        let mut builder = match walk_builder(vec![dir], file_set.clone(), recursive) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("Warning: {dir}: {e}");
                continue;
            }
        };
        apply_walk_flags(&mut builder, hidden, no_ignore);

        for result in builder.build() {
            let dirent = match result {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("Warning: {e}");
                    continue;
                }
            };
            let path = dirent.path();
            if !is_candidate_path(path) {
                continue;
            }
            let admit = match dirent.file_type() {
                Some(ft) if ft.is_file() => true,
                Some(ft) if ft.is_dir() && dirent.depth() == 0 => false,
                Some(ft) if ft.is_dir() => include_dirs,
                // Symlinks (since follow_links is off) and other entries are
                // skipped - Phase 3 only handles regular files and dirs.
                _ => false,
            };
            if !admit {
                continue;
            }
            records.push(PathRecord {
                path: path.to_path_buf(),
                root: PathBuf::from(dir),
            });
        }
    }

    records.sort_by(|a, b| natord::compare(&a.path.to_string_lossy(), &b.path.to_string_lossy()));
    records
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::FileSet;
    use super::is_candidate_path;
    use super::is_vcs_path;
    use super::parse_file_globs;
    use super::walk_paths;

    // ---- is_candidate_path / is_vcs_path (lifted from rep) -------------

    #[test]
    fn test_is_candidate_path_accepts_regular_source_file() {
        assert!(is_candidate_path(Path::new("src/main.rs")));
        assert!(is_candidate_path(Path::new("README.md")));
        assert!(is_candidate_path(Path::new("Makefile")));
    }

    #[test]
    fn test_is_candidate_path_rejects_tilde_backup() {
        assert!(!is_candidate_path(Path::new("main.rs~")));
        assert!(!is_candidate_path(Path::new("some/dir/file.txt~")));
    }

    #[test]
    fn test_is_candidate_path_rejects_ctags_files() {
        assert!(!is_candidate_path(Path::new("tags")));
        assert!(!is_candidate_path(Path::new("TAGS")));
        assert!(!is_candidate_path(Path::new("./tags")));
    }

    #[test]
    fn test_is_vcs_path_rejects_vcs_directories() {
        assert!(is_vcs_path(Path::new(".git/config")));
        assert!(is_vcs_path(Path::new("repo/.jj/working_copy")));
        assert!(is_vcs_path(Path::new("./nested/.svn/entries")));
        assert!(is_vcs_path(Path::new("vendor/CVS/Entries")));
    }

    #[test]
    fn test_is_vcs_path_accepts_regular_hidden_paths() {
        assert!(!is_vcs_path(Path::new(".env")));
        assert!(!is_vcs_path(Path::new(".config/app.toml")));
        assert!(!is_vcs_path(Path::new("src/.hidden/file.txt")));
    }

    // ---- parse_file_globs (lifted from rep) ----------------------------

    #[test]
    fn test_parse_file_globs_extension() {
        assert_eq!(parse_file_globs("txt"), vec!["*.txt"]);
        assert_eq!(parse_file_globs("rs,go"), vec!["*.rs", "*.go"]);
    }

    #[test]
    fn test_parse_file_globs_exact_filename() {
        assert_eq!(parse_file_globs("=Dockerfile"), vec!["Dockerfile"]);
    }

    #[test]
    fn test_parse_file_globs_negation() {
        assert_eq!(parse_file_globs("!rs"), vec!["!*.rs"]);
        assert_eq!(parse_file_globs("!=Makefile"), vec!["!Makefile"]);
    }

    #[test]
    fn test_parse_file_globs_wildcard() {
        assert_eq!(parse_file_globs("*.json"), vec!["*.json"]);
    }

    #[test]
    fn test_parse_file_globs_dot_ignored() {
        assert!(parse_file_globs(".").is_empty());
    }

    #[test]
    fn test_parse_file_globs_mixed() {
        assert_eq!(
            parse_file_globs("rs, =Dockerfile, !txt"),
            vec!["*.rs", "Dockerfile", "!*.txt"]
        );
    }

    #[test]
    fn test_parse_file_globs_empty_string_is_empty() {
        assert!(parse_file_globs("").is_empty());
    }

    #[test]
    fn test_parse_file_globs_only_commas_is_empty() {
        assert!(parse_file_globs(",,,").is_empty());
    }

    // ---- walk_paths fixtures ------------------------------------------

    /// Build a fixture rooted at `tmp` with this layout:
    ///
    /// ```text
    /// <tmp>/
    /// ├── a.txt
    /// ├── b.txt
    /// ├── ignored.log         # excluded by .ignore (treated like .gitignore)
    /// ├── .hidden.txt         # hidden file
    /// ├── .ignore             # contains "ignored.log"
    /// ├── sub/
    /// │   ├── c.txt
    /// │   └── nested/
    /// │       └── d.txt
    /// └── .hidden_dir/
    ///     └── e.txt
    /// ```
    fn make_fixture(tmp: &Path) {
        fs::write(tmp.join("a.txt"), "a").unwrap();
        fs::write(tmp.join("b.txt"), "b").unwrap();
        fs::write(tmp.join("ignored.log"), "ignored").unwrap();
        fs::write(tmp.join(".hidden.txt"), "hidden").unwrap();
        fs::write(tmp.join(".ignore"), "ignored.log\n").unwrap();

        let sub = tmp.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("c.txt"), "c").unwrap();
        let nested = sub.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("d.txt"), "d").unwrap();

        let hidden_dir = tmp.join(".hidden_dir");
        fs::create_dir(&hidden_dir).unwrap();
        fs::write(hidden_dir.join("e.txt"), "e").unwrap();
    }

    /// Path basenames returned by a walk, for compact assertions.
    fn basenames(records: &[super::PathRecord]) -> Vec<String> {
        records
            .iter()
            .map(|r| {
                r.path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    // ---- walk_paths behaviours ----------------------------------------

    #[test]
    fn test_walk_paths_default_files_only_depth1() {
        let tmp = TempDir::new().unwrap();
        make_fixture(tmp.path());

        let dir = tmp.path().to_string_lossy().into_owned();
        let records = walk_paths(vec![&dir], None, false, false, false, false);

        assert_eq!(basenames(&records), vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn test_walk_paths_recursive_files_only() {
        let tmp = TempDir::new().unwrap();
        make_fixture(tmp.path());

        let dir = tmp.path().to_string_lossy().into_owned();
        let records = walk_paths(vec![&dir], None, false, false, true, false);

        // Depth-1 plus subtree files; hidden + gitignored excluded.
        assert_eq!(
            basenames(&records),
            vec!["a.txt", "b.txt", "c.txt", "d.txt"]
        );
    }

    #[test]
    fn test_walk_paths_include_dirs_depth1() {
        let tmp = TempDir::new().unwrap();
        make_fixture(tmp.path());

        let dir = tmp.path().to_string_lossy().into_owned();
        let records = walk_paths(vec![&dir], None, false, false, false, true);

        // Direct children of the root: files + the `sub` dir, root excluded.
        assert_eq!(basenames(&records), vec!["a.txt", "b.txt", "sub"]);
    }

    #[test]
    fn test_walk_paths_recursive_include_dirs() {
        let tmp = TempDir::new().unwrap();
        make_fixture(tmp.path());

        let dir = tmp.path().to_string_lossy().into_owned();
        let records = walk_paths(vec![&dir], None, false, false, true, true);

        // Everything visible (i.e. non-hidden, non-gitignored), root excluded.
        assert_eq!(
            basenames(&records),
            vec!["a.txt", "b.txt", "sub", "c.txt", "nested", "d.txt"]
        );
    }

    #[test]
    fn test_walk_paths_hidden_includes_dotfiles() {
        let tmp = TempDir::new().unwrap();
        make_fixture(tmp.path());

        let dir = tmp.path().to_string_lossy().into_owned();
        let records = walk_paths(vec![&dir], None, true, false, false, false);
        let names = basenames(&records);

        assert!(names.contains(&".hidden.txt".to_string()));
        assert!(names.contains(&".ignore".to_string()));
        assert!(names.contains(&"a.txt".to_string()));
        assert!(names.contains(&"b.txt".to_string()));
    }

    #[test]
    fn test_walk_paths_no_ignore_admits_gitignored() {
        let tmp = TempDir::new().unwrap();
        make_fixture(tmp.path());

        let dir = tmp.path().to_string_lossy().into_owned();
        let records = walk_paths(vec![&dir], None, false, true, false, false);
        let names = basenames(&records);

        assert!(names.contains(&"ignored.log".to_string()));
        assert!(names.contains(&"a.txt".to_string()));
    }

    #[test]
    fn test_walk_paths_with_file_set_glob() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("foo.rs"), "").unwrap();
        fs::write(tmp.path().join("bar.rs"), "").unwrap();
        fs::write(tmp.path().join("README.md"), "").unwrap();

        let file_set = FileSet {
            matches: parse_file_globs("rs"),
            case_insensitive: false,
        };
        let dir = tmp.path().to_string_lossy().into_owned();
        let records = walk_paths(vec![&dir], Some(file_set), false, false, false, false);

        assert_eq!(basenames(&records), vec!["bar.rs", "foo.rs"]);
    }

    #[test]
    fn test_walk_paths_multi_root_tags_each_record() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        fs::write(tmp1.path().join("one.txt"), "").unwrap();
        fs::write(tmp2.path().join("two.txt"), "").unwrap();

        let d1 = tmp1.path().to_string_lossy().into_owned();
        let d2 = tmp2.path().to_string_lossy().into_owned();
        let records = walk_paths(vec![&d1, &d2], None, false, false, false, false);

        let by_name: std::collections::HashMap<String, PathBuf> = records
            .iter()
            .map(|r| {
                (
                    r.path.file_name().unwrap().to_string_lossy().into_owned(),
                    r.root.clone(),
                )
            })
            .collect();

        assert_eq!(by_name.get("one.txt"), Some(&PathBuf::from(&d1)));
        assert_eq!(by_name.get("two.txt"), Some(&PathBuf::from(&d2)));
    }

    #[test]
    fn test_walk_paths_natord_sort() {
        let tmp = TempDir::new().unwrap();
        // Create out of order; natord should produce a.txt < aa.txt < b.txt.
        fs::write(tmp.path().join("b.txt"), "").unwrap();
        fs::write(tmp.path().join("a.txt"), "").unwrap();
        fs::write(tmp.path().join("aa.txt"), "").unwrap();

        let dir = tmp.path().to_string_lossy().into_owned();
        let records = walk_paths(vec![&dir], None, false, false, false, false);

        assert_eq!(basenames(&records), vec!["a.txt", "aa.txt", "b.txt"]);
    }
}
