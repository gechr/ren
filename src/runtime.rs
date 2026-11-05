//! Discover, plan and execute using resolved runtime settings.
use crate::expressions::compile_expressions;
use crate::options::{Mode, Options};
use crate::rename::{PlanEntry, apply_plan_interruptible as apply_plan, build_plan, validate_plan};
use crate::{io, output, preview, scan};
use anyhow::{Context as _, Result, bail};
use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal as _, Write};
use std::path::{Path, PathBuf};

pub(crate) fn run(
    options: Options,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    colored: bool,
) -> Result<()> {
    let from_stdin = options.paths.is_empty() && io::stdin_has_input();
    if options.null && !from_stdin {
        bail!("--null requires reading filenames from stdin");
    }
    if options.mode == Mode::Preview && !std::io::stdin().is_terminal() {
        bail!("--preview requires an interactive terminal");
    }
    let exprs = compile_expressions(&options.matching)?;
    let found = if from_stdin {
        io::records_from_paths(io::read_paths_from_stdin(options.null)?)
    } else {
        let roots = if options.paths.is_empty() {
            vec![Path::new(".")]
        } else {
            options.paths.iter().map(|path| path.as_path()).collect()
        };
        scan::walk_paths(
            roots,
            options.file_set,
            options.hidden,
            options.no_ignore,
            options.recursive,
            options.include_dirs,
        )
    };
    // Preserve diagnostic failures until independent valid work is complete.
    let mut diagnostics = Ok(());
    for error in &found.errors {
        if diagnostics.is_ok() {
            diagnostics = output::write_error(stderr, error, false);
        }
    }
    for warning in &found.warnings {
        if diagnostics.is_ok() {
            diagnostics = writeln!(stderr, "Warning: {warning}");
        }
    }
    let result = (|| -> Result<()> {
        if options.mode == Mode::List {
            for record in &found.records {
                let Some(name) = record.path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if exprs.is_empty() || exprs.iter().any(|expr| expr.regex.is_match(name)) {
                    writeln!(stdout, "{}", output::display_path(&record.path))?;
                }
            }
            return Ok(());
        }
        for record in &found.records {
            if record
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .is_none()
                && diagnostics.is_ok()
            {
                diagnostics = writeln!(
                    stderr,
                    "warning: {}: skipping (non-UTF-8 basename)",
                    record.path.display()
                );
            }
        }
        let plan = build_plan(
            &found.records,
            &exprs,
            options.scope,
            options.change_extension.as_deref(),
            &options.transforms,
            options.directory.as_deref(),
            options.parents,
        );
        validate_plan(&plan)?;
        if plan.is_empty() {
            return Ok(());
        }
        let plan = if options.mode == Mode::Preview {
            let accepted = preview::PreviewPatcher::new().prompt_plan(&plan)?;
            validate_plan(&accepted)?;
            accepted
        } else {
            plan
        };
        if plan.is_empty() {
            return Ok(());
        }
        let counts = output::count_files_and_dirs(&plan)?;
        let dry = options.mode == Mode::DryRun;
        if !dry {
            if options.create_dirs {
                create_missing_parents(&plan)?;
            }
            apply_plan(&plan)?;
        }
        output::write_summary(stdout, &plan, counts, dry, colored)?;
        Ok(())
    })();
    if !found.errors.is_empty() && result.as_ref().err().is_none_or(output::is_broken_pipe) {
        let count = found.errors.len();
        let noun = if count == 1 { "path" } else { "paths" };
        bail!("{count} {noun} could not be processed");
    }
    output::finish(result, diagnostics.map_err(output::diagnostic_error))
}

/// Create any missing parent directories for the plan's `new` paths. Called
/// just before `apply_plan` when `--create-dirs` is set.
///
/// If a parent path already exists but is not a directory (e.g. a regular file
/// blocking the intended `mkdir -p`), this returns an actionable error before
/// `apply_plan` runs. Without this check the failure would surface later as a
/// confusing `ENOTDIR`/`ENOENT` from inside the two-phase rename - by which
/// point phase 1 has already moved the source to a temp.
fn create_missing_parents(plan: &[PlanEntry]) -> Result<()> {
    let mut parents = HashMap::new();
    let mut targets = HashSet::new();
    // Resolve aliases before comparing missing directories with destinations.
    // Cache repeated parents, the common case when relocating many files.
    for entry in plan {
        let parent = entry
            .new
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        // `metadata` follows symlinks, so a symlink-to-dir counts as a dir
        // (correct) and a symlink-to-file is reported as the non-dir it
        // resolves to (also correct). Missing ancestors and dangling links
        // are checked by `resolve_future_directory` before any creation.
        match std::fs::metadata(parent) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                bail!(
                    "cannot create parent directory {}: a non-directory file already exists at that path (rename target: {})",
                    parent.display(),
                    entry.new.display(),
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect parent directory: {}", parent.display()));
            }
        }
        let resolved = if let Some(resolved) = parents.get(parent) {
            resolved
        } else {
            parents
                .entry(parent.to_owned())
                .or_insert(resolve_future_directory(parent)?)
        };
        if let Some(name) = entry.new.file_name() {
            // Match validate_plan's conservative case-insensitive collision
            // policy even when missing paths cannot yet be canonicalized.
            targets.insert(resolved.join(name).to_string_lossy().to_lowercase());
        }
    }
    // Preflight every required directory before the first mkdir. A directory
    // made for one entry would otherwise occupy another entry's destination.
    for entry in plan {
        let parent = entry
            .new
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        // create_dir_all traverses literal ancestors, including a directory
        // subsequently cancelled by `..`. Check those before normalization.
        for ancestor in parent.ancestors().filter(|p| !p.as_os_str().is_empty()) {
            let resolved = resolve_future_directory(ancestor)?;
            for required in resolved.ancestors() {
                if targets.contains(&required.to_string_lossy().to_lowercase()) {
                    bail!(
                        "rename target {} is also required as a parent directory (rename target: {})",
                        required.display(),
                        entry.new.display()
                    );
                }
            }
        }
    }
    for parent in parents.keys() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent directory: {}", parent.display()))?;
    }
    Ok(())
}

/// Resolve existing directory components, retaining missing suffixes. This
/// follows parent symlinks and catches blocked ancestors before any creation.
fn resolve_future_directory(path: &Path) -> Result<PathBuf> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => path
            .canonicalize()
            .with_context(|| format!("resolve parent directory: {}", path.display())),
        Ok(_) => bail!(
            "cannot create parent directory {}: a non-directory file already exists at that path",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if path.symlink_metadata().is_ok() {
                bail!(
                    "cannot create parent directory {}: a dangling symlink already exists at that path",
                    path.display()
                );
            }
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let mut resolved = resolve_future_directory(parent)?;
            if path.components().next_back() == Some(std::path::Component::ParentDir) {
                resolved.pop();
            } else if let Some(name) = path.file_name() {
                resolved.push(name);
            } else {
                bail!("cannot resolve parent directory: {}", path.display());
            }
            Ok(resolved)
        }
        Err(error) => {
            Err(error).with_context(|| format!("inspect parent directory: {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // ---- create_missing_parents ------------------------------------------

    #[test]
    fn required_parent_cannot_occupy_another_destination() {
        let dir = tempfile::TempDir::new().unwrap();
        let out = dir.path().join("out");
        let plan = [
            PlanEntry {
                old: dir.path().join("x"),
                new: out.join("y"),
                depth: 1,
            },
            PlanEntry {
                old: dir.path().join("z"),
                new: out.join("./y/w"),
                depth: 2,
            },
        ];
        assert!(
            create_missing_parents(&plan)
                .unwrap_err()
                .to_string()
                .contains("also required as a parent directory")
        );
        assert!(!out.exists());
    }

    #[test]
    fn required_parent_checks_case_aliases_and_cancelled_components() {
        for suffix in ["Y/w", "y/../w"] {
            let dir = tempfile::TempDir::new().unwrap();
            let out = dir.path().join("out");
            let plan = [
                PlanEntry {
                    old: dir.path().join("x"),
                    new: out.join("y"),
                    depth: 1,
                },
                PlanEntry {
                    old: dir.path().join("z"),
                    new: out.join(suffix),
                    depth: 1,
                },
            ];
            assert!(
                create_missing_parents(&plan)
                    .unwrap_err()
                    .to_string()
                    .contains("also required as a parent directory")
            );
            assert!(!out.exists());
        }
    }

    #[test]
    fn blocked_later_parent_is_checked_before_creating_earlier_directories() {
        let dir = tempfile::TempDir::new().unwrap();
        let out = dir.path().join("out");
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "data").unwrap();
        let plan = [
            PlanEntry {
                old: dir.path().join("x"),
                new: out.join("y"),
                depth: 1,
            },
            PlanEntry {
                old: dir.path().join("z"),
                new: blocked.join("w"),
                depth: 1,
            },
        ];
        assert!(create_missing_parents(&plan).is_err());
        assert!(!out.exists());
    }

    #[test]
    fn test_create_missing_parents_errors_when_parent_is_a_file() {
        // Reproduces `ren a a/b` with create-dirs on, where `a` exists as a
        // regular file. The plan asks us to mkdir -p `a/`, but `a` is blocked
        // by the file. Previously this slipped past `parent.exists()` and the
        // failure surfaced from inside the two-phase rename after phase 1 had
        // already moved the source to a temp. Now we fail upfront with an
        // actionable message naming the offending path.
        let tmp = tempfile::TempDir::new().unwrap();
        let a = tmp.path().join("a");
        std::fs::write(&a, "").unwrap();
        let plan = vec![PlanEntry {
            old: a.clone(),
            new: a.join("b"),
            depth: 1,
        }];

        let err = create_missing_parents(&plan).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            format!(
                "cannot create parent directory {}: a non-directory file already exists at that path (rename target: {})",
                a.display(),
                a.join("b").display(),
            ),
        );
    }

    #[test]
    fn test_create_missing_parents_creates_missing_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let parent = tmp.path().join("nested/sub");
        let plan = vec![PlanEntry {
            old: tmp.path().join("src"),
            new: parent.join("dst"),
            depth: 1,
        }];

        create_missing_parents(&plan).unwrap();
        assert!(parent.is_dir());
    }

    #[test]
    fn test_create_missing_parents_skips_existing_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let parent = tmp.path().join("already");
        std::fs::create_dir(&parent).unwrap();
        let plan = vec![PlanEntry {
            old: tmp.path().join("src"),
            new: parent.join("dst"),
            depth: 1,
        }];

        create_missing_parents(&plan).unwrap();
    }
}
