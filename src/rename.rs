// Plan / validate / apply for `ren`. Takes already-walked paths and already-
// compiled expressions, produces a `Vec<PlanEntry>`, validates it against
// within-plan and existing-target conflicts, and applies it via a per-depth
// two-phase rename.
//
// The two-phase apply runs deepest-depth first. Each depth-group renames every
// `old` to a unique `<parent>/<basename>.ren-<pid>-<counter>` temp (phase 1),
// then renames every temp to its target `new` (phase 2). This handles chains,
// cycles, case-only renames (which would otherwise no-op on case-insensitive
// filesystems), and parent/child dependencies through the same mechanism.
//
// Rollback scope is per-failing-depth only: a phase-2 failure rolls back the
// failing depth's already-applied phase-2 ops (in reverse) and then all
// phase-1 temps (in reverse) to their `old`. Deeper depths stay applied. The
// error message says so.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::Result;
use anyhow::anyhow;

use crate::display_path;
use crate::expressions;
use crate::scan;
use crate::transforms;

/// A single planned rename.
///
/// `depth` is the path's component count **relative to the walk root** it was
/// found under (computed in `build_plan` via `strip_prefix(root)`). Carrying it
/// here avoids recomputation in `apply_plan` and keeps multi-root walks correct
/// (a file two levels deep under root `a` and a file two levels deep under root
/// `b` are both depth 2, regardless of absolute component counts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanEntry {
    pub old: PathBuf,
    pub new: PathBuf,
    pub depth: usize,
}

/// Common compound extensions recognised by stem/extension splitting.
/// Best-effort list - uncommon multi-dot patterns (`.tar.lzma`, `.spec.ts`,
/// `.d.ts`, etc.) fall through to single-extension semantics. ASCII
/// case-insensitive match.
const COMPOUND_EXTENSIONS: &[&str] = &["tar.gz", "tar.bz2", "tar.xz", "tar.zst", "tar.lz"];

/// Which part of each basename the rename pipeline operates on.
///
/// `Exclude` is the default: the pipeline runs on the stem only, and
/// `build_plan` reattaches the original extension unchanged. This prevents
/// accidents like `ren txt notes` rewriting `report.txt` to `report.notes`.
///
/// `Include` (`-x`) opts back into running the pipeline on the full basename.
///
/// `Only` (`-X`) flips the split: the pipeline runs on the extension only and
/// the stem is preserved. Files without an extension are skipped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ExtensionScope {
    #[default]
    Exclude,
    Include,
    Only,
}

/// Split a basename into `(stem, ext)` for stem/extension matching, where
/// `ext` is `Some("rs")` for `foo.rs`, `Some("tar.gz")` for `archive.tar.gz`
/// (compound-extension best effort), `Some("")` for `archive.` (empty after
/// the trailing dot - preserved on round-trip), and `None` for `Makefile`,
/// `.bashrc`, or any other name without a `.ext` suffix.
///
/// Recognises the compound extensions in `COMPOUND_EXTENSIONS`; all other
/// names defer to `Path::file_stem` / `Path::extension`:
///
/// - `foo.rs`          → `("foo", Some("rs"))`
/// - `archive.tar.gz`  → `("archive", Some("tar.gz"))`
/// - `archive.tar`     → `("archive", Some("tar"))`
/// - `.bashrc`         → `(".bashrc", None)`
/// - `Makefile`        → `("Makefile", None)`
/// - `archive.`        → `("archive", Some(""))`
fn split_stem_ext(basename: &str) -> (&str, Option<&str>) {
    // Compound extensions first: ASCII case-insensitive. Reject the case
    // where the entire basename IS the compound (would yield empty stem).
    let lower = basename.to_ascii_lowercase();
    for compound in COMPOUND_EXTENSIONS {
        let suffix_len = compound.len() + 1; // +1 for the leading '.'
        if basename.len() > suffix_len
            && basename.as_bytes()[basename.len() - suffix_len] == b'.'
            && lower.ends_with(compound)
        {
            let stem_len = basename.len() - suffix_len;
            return (&basename[..stem_len], Some(&basename[stem_len + 1..]));
        }
    }

    // Single-extension case: defer to `Path` semantics.
    let p = Path::new(basename);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(basename);
    // `Path::extension` returns `None` for `archive.` (empty extension) - to
    // round-trip the trailing dot we detect it separately by scanning the
    // basename directly. `Path::extension` also returns `None` for dotfiles
    // (`.bashrc`), which is the behavior we want.
    let ext = match p.extension().and_then(|s| s.to_str()) {
        Some(e) => Some(e),
        None => {
            if basename.ends_with('.') && stem != basename {
                Some("")
            } else {
                None
            }
        }
    };
    (stem, ext)
}

/// Build a rename plan from walked records and compiled expressions.
///
/// Non-UTF-8 basenames are warned and skipped (parity with `rep`'s permissive
/// philosophy). Records whose basename does not change after applying every
/// expression - including those with zero matches - are filtered out.
///
/// The output preserves the input order. `walk_paths` already sorts records
/// naturally; `build_plan` does not re-sort. This deterministic ordering is a
/// load-bearing invariant for `apply_plan`'s per-depth grouping.
///
/// `scope` selects which part of the basename the pipeline operates on:
/// `Exclude` (default) runs on the stem only and reattaches the extension;
/// `Include` runs on the full basename; `Only` runs on the extension and
/// preserves the stem. See `ExtensionScope` for the precise semantics.
///
/// The transform pipeline runs after find/replace in fixed canonical order
/// (see `transforms::apply`). When `--prepend` or `--append` references the
/// counter (`{n}`, `{n:0W}`, or `{N}`), the counter resets per parent
/// directory and increments only for entries that actually land in the plan;
/// both affixes share the same per-record value.
pub(crate) fn build_plan(
    records: &[scan::PathRecord],
    exprs: &[expressions::CompiledExpression],
    scope: ExtensionScope,
    transforms_opts: &transforms::TransformOptions,
) -> Vec<PlanEntry> {
    let mut plan = Vec::with_capacity(records.len());
    let mut dir_counts: BTreeMap<PathBuf, usize> = BTreeMap::new();
    let mut dir_indices: BTreeMap<PathBuf, usize> = BTreeMap::new();

    let uses_counter = transforms_opts
        .prepend
        .as_deref()
        .is_some_and(transforms::has_counter_placeholder)
        || transforms_opts
            .append
            .as_deref()
            .is_some_and(transforms::has_counter_placeholder);

    if uses_counter {
        for record in records {
            if record.path.file_name().and_then(|n| n.to_str()).is_some() {
                *dir_counts.entry(parent_key(&record.path)).or_default() += 1;
            }
        }
    }

    for record in records {
        let Some(basename) = record.path.file_name().and_then(|n| n.to_str()) else {
            eprintln!(
                "warning: {}: skipping (non-UTF-8 basename)",
                record.path.display()
            );
            continue;
        };
        let parent = parent_key(&record.path);

        // The pipeline (find/replace + transforms) runs on `working_input`,
        // and the result is reassembled as `prefix + after_transforms + suffix`.
        //
        //   Exclude (default): pipeline on stem; reattach `.ext`
        //   Include (`-x`):    pipeline on full basename
        //   Only    (`-X`):    pipeline on ext; reattach `stem.` prefix
        let prefix: String;
        let suffix: String;
        let working_input: &str;
        match scope {
            ExtensionScope::Include => {
                prefix = String::new();
                suffix = String::new();
                working_input = basename;
            }
            ExtensionScope::Exclude => {
                let (stem, ext) = split_stem_ext(basename);
                prefix = String::new();
                suffix = match ext {
                    Some(e) => format!(".{e}"),
                    None => String::new(),
                };
                working_input = stem;
            }
            ExtensionScope::Only => {
                let (stem, ext) = split_stem_ext(basename);
                let Some(e) = ext else {
                    // No extension to operate on; skip.
                    continue;
                };
                prefix = format!("{stem}.");
                suffix = String::new();
                working_input = e;
            }
        }

        let (after_expr, _) = expressions::apply_to_basename(working_input, exprs);
        let counter_value = dir_indices.get(&parent).copied().unwrap_or(0) + 1;
        let dir_count = dir_counts.get(&parent).copied().unwrap_or(0);
        let ctx = transforms::CounterContext {
            n: counter_value,
            dir_count,
        };
        let after_transforms = transforms::apply(&after_expr, transforms_opts, ctx);

        let new_basename = format!("{prefix}{after_transforms}{suffix}");

        if new_basename == basename {
            continue;
        }

        let new = record
            .path
            .parent()
            .map(|p| p.join(&new_basename))
            .unwrap_or_else(|| PathBuf::from(&new_basename));

        let depth = record
            .path
            .strip_prefix(&record.root)
            .map(|p| p.components().count())
            .unwrap_or_else(|_| record.path.components().count());

        plan.push(PlanEntry {
            old: record.path.clone(),
            new,
            depth,
        });
        if uses_counter {
            *dir_indices.entry(parent).or_default() += 1;
        }
    }

    plan
}

fn parent_key(path: &Path) -> PathBuf {
    path.parent().map(Path::to_path_buf).unwrap_or_default()
}

/// Validate the plan against two classes of conflict:
///
/// 1. Within-plan: two entries with the same `new` (compared
///    case-insensitively, which catches collisions on case-insensitive
///    filesystems like macOS APFS).
/// 2. Existing target: a `new` that already exists on disk and is not the
///    `old` of some entry in the plan (also case-insensitive).
///
/// Case folding uses `String::to_lowercase`, which is approximate Unicode
/// (see the trade-offs section of the build plan). Adequate for ASCII-only
/// filenames.
pub(crate) fn validate_plan(plan: &[PlanEntry]) -> Result<()> {
    // 1. Within-plan duplicates: bucket entries by lowercased `new` and
    //    surface every group with more than one source path.
    let mut by_new: BTreeMap<String, Vec<&PlanEntry>> = BTreeMap::new();
    for entry in plan {
        by_new
            .entry(entry.new.to_string_lossy().to_lowercase())
            .or_default()
            .push(entry);
    }
    let dup_groups: Vec<_> = by_new.iter().filter(|(_, v)| v.len() > 1).collect();
    if !dup_groups.is_empty() {
        let mut msg = String::from("multiple files would be renamed to the same target:\n");
        for (i, (_, entries)) in dup_groups.iter().enumerate() {
            if i > 0 {
                msg.push('\n');
            }
            for entry in *entries {
                write!(
                    msg,
                    "\n  {} → {}",
                    display_path(&entry.old),
                    display_path(&entry.new)
                )
                .expect("write to String never fails");
            }
        }
        return Err(anyhow!("{msg}"));
    }

    // 2. Existing-target collisions: a `new` that exists on disk but isn't the
    //    `old` of any entry in the plan. The lowercase set lets us match
    //    case-insensitive filesystems where `tmp` and `TMP` resolve to the
    //    same dirent.
    let olds_lower: std::collections::HashSet<String> = plan
        .iter()
        .map(|e| e.old.to_string_lossy().to_lowercase())
        .collect();

    let mut external: Vec<(&Path, &Path)> = Vec::new();
    for entry in plan {
        if entry.new.symlink_metadata().is_ok() {
            let new_lower = entry.new.to_string_lossy().to_lowercase();
            if !olds_lower.contains(&new_lower) {
                external.push((entry.old.as_path(), entry.new.as_path()));
            }
        }
    }
    if !external.is_empty() {
        let mut msg = String::from("renames would overwrite existing files:\n");
        for (old, new) in external {
            write!(msg, "\n  {} → {}", display_path(old), display_path(new))
                .expect("write to String never fails");
        }
        return Err(anyhow!("{msg}"));
    }

    Ok(())
}

/// Apply the plan via a per-depth two-phase rename, deepest-first.
///
/// Group entries by `depth` and walk descending. For each depth: rename every
/// `old` to a unique temp (phase 1), then rename every temp to its `new`
/// (phase 2). On a phase-2 error, roll back the failing depth (and only the
/// failing depth - deeper depths are already committed). On a phase-1 error,
/// bubble the error: at the failure point only some entries are at temps and
/// the remainder are still at `old`, so no rollback is required.
pub(crate) fn apply_plan(plan: &[PlanEntry]) -> Result<()> {
    apply_plan_with(plan, real_rename)
}

/// Real `fs::rename` adapter for the production apply path.
///
/// `apply_plan_with` takes the rename op as a parameter so tests can inject a
/// fake to exercise the rollback branch deterministically.
fn real_rename(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

/// Apply a plan using a caller-supplied rename op. The production entry point
/// `apply_plan` wires this to `fs::rename`; tests inject a fake to drive the
/// phase-2 rollback path.
fn apply_plan_with<F>(plan: &[PlanEntry], rename_op: F) -> Result<()>
where
    F: Fn(&Path, &Path) -> io::Result<()>,
{
    // Group by depth. `BTreeMap` gives ordered iteration; `Vec` insertion order
    // within each group is preserved and matches `build_plan`'s output order
    // (which itself preserves `walk_paths`'s natord ordering). This is a
    // load-bearing invariant: it makes temp-name allocation and rollback order
    // reproducible across runs.
    let mut by_depth: BTreeMap<usize, Vec<&PlanEntry>> = BTreeMap::new();
    for entry in plan {
        by_depth.entry(entry.depth).or_default().push(entry);
    }

    for (_, group) in by_depth.iter().rev() {
        apply_depth_group(group, &rename_op)?;
    }

    Ok(())
}

/// Apply a single depth-group: phase 1 (old → temp) then phase 2 (temp → new),
/// with per-depth rollback on phase-2 failure.
fn apply_depth_group<F>(group: &[&PlanEntry], rename_op: &F) -> Result<()>
where
    F: Fn(&Path, &Path) -> io::Result<()>,
{
    // Phase 1: rename every `old` to a unique temp.
    let mut applied: Vec<(&PlanEntry, PathBuf)> = Vec::with_capacity(group.len());
    for entry in group {
        let mut temp = unique_temp_path(&entry.old);
        // Bounded retry loop guards against the race window between
        // `unique_temp_path`'s best-effort existence check and the actual
        // `rename` syscall. 16 iterations is overkill in practice (the counter
        // is process-wide unique).
        let mut attempt = 0;
        loop {
            match rename_op(&entry.old, &temp) {
                Ok(()) => break,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt < 16 => {
                    attempt += 1;
                    temp = unique_temp_path(&entry.old);
                }
                Err(e) => {
                    // User-facing: the rename of `old` to `new` failed. The
                    // temp hop is an implementation detail; the user wrote a
                    // find/replace, they see "rename A → B".
                    return Err(anyhow::Error::from(e).context(format!(
                        "could not rename {} → {}",
                        entry.old.display(),
                        entry.new.display(),
                    )));
                }
            }
        }
        applied.push((entry, temp));
    }

    // Phase 2: rename every temp to its target `new`. On the first error,
    // attempt a best-effort rollback of the depth's already-applied phase-2
    // ops and then all phase-1 temps.
    let mut phase2_done: Vec<(&PlanEntry, PathBuf)> = Vec::with_capacity(applied.len());
    for (entry, temp) in &applied {
        if let Err(e) = rename_op(temp, &entry.new) {
            // Rollback: undo phase-2 successes in reverse, then phase-1 temps
            // in reverse. Failures during rollback are warned but never mask
            // the original phase-2 error.
            //
            // The two warning shapes differ because the file's stuck location
            // does: a failed phase-2 undo leaves the file at `new` (already
            // renamed), while a failed phase-1 undo leaves it at the internal
            // temp path - which the user has to know about to recover it.
            for (done_entry, done_temp) in phase2_done.iter().rev() {
                if let Err(re) = rename_op(&done_entry.new, done_temp) {
                    eprintln!(
                        "warning: could not undo rename of {} → {}: {re}",
                        done_entry.old.display(),
                        done_entry.new.display(),
                    );
                }
            }
            for (a_entry, a_temp) in applied.iter().rev() {
                if let Err(re) = rename_op(a_temp, &a_entry.old) {
                    eprintln!(
                        "warning: {} could not be restored and is currently at {}: {re}",
                        a_entry.old.display(),
                        a_temp.display(),
                    );
                }
            }
            return Err(anyhow::Error::from(e).context(format!(
                "could not rename {} → {}",
                entry.old.display(),
                entry.new.display(),
            )));
        }
        phase2_done.push((entry, temp.clone()));
    }

    Ok(())
}

/// Build a unique-looking temp path next to `old`. The pre-check is
/// best-effort for human-readable temp names; `apply_depth_group` retries on
/// `ErrorKind::AlreadyExists` from the actual rename to handle the race.
fn unique_temp_path(old: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let parent = old.parent().unwrap_or(Path::new("."));
    let base = old
        .file_name()
        .expect("rename source must have a basename")
        .to_string_lossy();
    let pid = std::process::id();

    loop {
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!("{base}.ren-{pid}-{counter:08}"));
        if candidate.symlink_metadata().is_err() {
            return candidate;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::io;
    use std::path::Path;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::ExtensionScope;
    use super::PlanEntry;
    use super::apply_depth_group;
    use super::apply_plan;
    use super::build_plan;
    use super::split_stem_ext;
    use super::validate_plan;
    use crate::expressions;
    use crate::scan;
    use crate::transforms;

    /// Build compiled expressions from a literal find/replace.
    fn compile(find: &str, replace: &str) -> Vec<expressions::CompiledExpression> {
        let opts = expressions::CompileOptions {
            regex: false,
            ignore_case: false,
            greedy: false,
            word_regexp: false,
            smart: false,
            expressions: Vec::new(),
            positional_find: Some(find.into()),
            positional_replace: Some(replace.into()),
            list_files_find_only: false,
        };
        expressions::compile_expressions(&opts).unwrap()
    }

    /// Build a `PathRecord` rooted at `root`.
    fn record(path: PathBuf, root: PathBuf) -> scan::PathRecord {
        scan::PathRecord { path, root }
    }

    // ---- end-to-end apply -------------------------------------------------

    #[test]
    fn simple_rename_end_to_end() {
        let tmp = TempDir::new().unwrap();
        let foo = tmp.path().join("foo.txt");
        fs::write(&foo, "x").unwrap();

        let exprs = compile("foo", "bar");
        let records = vec![record(foo.clone(), tmp.path().to_path_buf())];
        let plan = build_plan(
            &records,
            &exprs,
            ExtensionScope::Include,
            &transforms::TransformOptions::default(),
        );
        assert_eq!(plan.len(), 1);
        validate_plan(&plan).unwrap();
        apply_plan(&plan).unwrap();

        assert!(!foo.exists());
        assert!(tmp.path().join("bar.txt").exists());
    }

    #[test]
    fn chain_a_to_b_b_to_c() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        fs::write(&a, "A").unwrap();
        fs::write(&b, "B").unwrap();

        // Construct PlanEntries by hand so the chain is unambiguous.
        let plan = vec![
            PlanEntry {
                old: a.clone(),
                new: b.clone(),
                depth: 1,
            },
            PlanEntry {
                old: b.clone(),
                new: tmp.path().join("c.txt"),
                depth: 1,
            },
        ];

        validate_plan(&plan).unwrap();
        apply_plan(&plan).unwrap();

        assert!(!a.exists());
        assert!(b.exists());
        assert_eq!(fs::read_to_string(&b).unwrap(), "A");
        let c = tmp.path().join("c.txt");
        assert!(c.exists());
        assert_eq!(fs::read_to_string(&c).unwrap(), "B");
    }

    #[test]
    fn cycle_a_to_b_b_to_a_swaps_contents() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        fs::write(&a, "A").unwrap();
        fs::write(&b, "B").unwrap();

        let plan = vec![
            PlanEntry {
                old: a.clone(),
                new: b.clone(),
                depth: 1,
            },
            PlanEntry {
                old: b.clone(),
                new: a.clone(),
                depth: 1,
            },
        ];

        validate_plan(&plan).unwrap();
        apply_plan(&plan).unwrap();

        assert!(a.exists());
        assert!(b.exists());
        // After the swap: a holds B's old contents, b holds A's.
        assert_eq!(fs::read_to_string(&a).unwrap(), "B");
        assert_eq!(fs::read_to_string(&b).unwrap(), "A");
    }

    #[test]
    fn case_only_rename_observably_flips_dirent() {
        let tmp = TempDir::new().unwrap();

        // Probe whether the temp dir is on a case-insensitive FS. Either way
        // the assertion at the end must hold - the temp-hop is unconditional.
        let probe = tmp.path().join("Probe");
        fs::write(&probe, "p").unwrap();
        let case_insensitive = fs::metadata(tmp.path().join("probe")).is_ok();
        fs::remove_file(&probe).unwrap();

        let lower = tmp.path().join("tmp");
        fs::write(&lower, "original").unwrap();

        let plan = vec![PlanEntry {
            old: lower.clone(),
            new: tmp.path().join("TMP"),
            depth: 1,
        }];
        validate_plan(&plan).unwrap();
        apply_plan(&plan).unwrap();

        // Read the dir back and confirm the literal byte-form of the name.
        let entries: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries.len(), 1, "exactly one entry remains");
        assert_eq!(
            entries[0], "TMP",
            "case-only rename must flip the dirent (case_insensitive_fs={case_insensitive})",
        );
    }

    // ---- validate ---------------------------------------------------------

    #[test]
    fn validate_errors_on_within_plan_duplicate_targets() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        let c = tmp.path().join("c.txt");

        let plan = vec![
            PlanEntry {
                old: a.clone(),
                new: c.clone(),
                depth: 1,
            },
            PlanEntry {
                old: b.clone(),
                new: c.clone(),
                depth: 1,
            },
        ];

        let err = validate_plan(&plan).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains(&a.display().to_string()), "msg: {msg}");
        assert!(msg.contains(&b.display().to_string()), "msg: {msg}");
    }

    #[test]
    fn validate_errors_on_existing_external_target() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a.txt");
        let occupant = tmp.path().join("b.txt");
        fs::write(&a, "").unwrap();
        fs::write(&occupant, "").unwrap();

        let plan = vec![PlanEntry {
            old: a.clone(),
            new: occupant.clone(),
            depth: 1,
        }];

        let err = validate_plan(&plan).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("would overwrite"));
        assert!(msg.contains(&occupant.display().to_string()));
    }

    #[test]
    fn validate_passes_when_existing_target_is_an_old_in_plan() {
        // Cycle case: b.txt exists, but it's also the old of another entry.
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        fs::write(&a, "").unwrap();
        fs::write(&b, "").unwrap();

        let plan = vec![
            PlanEntry {
                old: a.clone(),
                new: b.clone(),
                depth: 1,
            },
            PlanEntry {
                old: b.clone(),
                new: a.clone(),
                depth: 1,
            },
        ];

        validate_plan(&plan).unwrap();
    }

    // ---- depth ordering with --include-dirs ------------------------------

    #[test]
    fn include_dirs_depth_ordering_renames_inner_first() {
        let tmp = TempDir::new().unwrap();
        let foo_dir = tmp.path().join("foo_dir");
        fs::create_dir(&foo_dir).unwrap();
        let bar = foo_dir.join("bar.txt");
        fs::write(&bar, "B").unwrap();

        // Construct PlanEntries by hand to pin exact depth values.
        let plan = vec![
            PlanEntry {
                old: bar.clone(),
                new: foo_dir.join("baz.txt"),
                depth: 2,
            },
            PlanEntry {
                old: foo_dir.clone(),
                new: tmp.path().join("qux_dir"),
                depth: 1,
            },
        ];

        validate_plan(&plan).unwrap();
        apply_plan(&plan).unwrap();

        let qux_dir = tmp.path().join("qux_dir");
        assert!(qux_dir.is_dir());
        assert!(qux_dir.join("baz.txt").exists());
        assert!(!foo_dir.exists());
    }

    // ---- noop -------------------------------------------------------------

    #[test]
    fn noop_plan_filters_out_unchanged_basenames_and_apply_is_ok() {
        let tmp = TempDir::new().unwrap();
        let foo = tmp.path().join("foo.txt");
        fs::write(&foo, "").unwrap();

        // find/replace both "foo": no expressions compile (filtered as noop in
        // `compile_expressions`), so build_plan emits nothing.
        let exprs = compile("foo", "foo");
        let records = vec![record(foo.clone(), tmp.path().to_path_buf())];
        let plan = build_plan(
            &records,
            &exprs,
            ExtensionScope::Include,
            &transforms::TransformOptions::default(),
        );
        assert!(plan.is_empty());

        validate_plan(&plan).unwrap();
        apply_plan(&plan).unwrap();
        assert!(foo.exists());
    }

    // ---- depth derivation -------------------------------------------------

    #[test]
    fn build_plan_derives_depth_relative_to_root() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a").join("b").join("c.txt");
        fs::create_dir_all(nested.parent().unwrap()).unwrap();
        fs::write(&nested, "").unwrap();

        let exprs = compile("c", "z");
        let records = vec![record(nested, tmp.path().to_path_buf())];
        let plan = build_plan(
            &records,
            &exprs,
            ExtensionScope::Include,
            &transforms::TransformOptions::default(),
        );

        assert_eq!(plan.len(), 1);
        // a/b/c.txt under tmp/ → depth 3 relative to root.
        assert_eq!(plan[0].depth, 3);
    }

    // ---- counter ----------------------------------------------------------

    #[test]
    fn build_plan_counter_in_prepend_resets_per_parent_directory() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("sub");
        let records = vec![
            record(tmp.path().join("a.txt"), tmp.path().to_path_buf()),
            record(tmp.path().join("b.txt"), tmp.path().to_path_buf()),
            record(sub.join("c.txt"), tmp.path().to_path_buf()),
            record(sub.join("d.txt"), tmp.path().to_path_buf()),
        ];

        let opts = transforms::TransformOptions {
            prepend: Some("{n:02}_".into()),
            ..Default::default()
        };
        let plan = build_plan(&records, &[], ExtensionScope::Include, &opts);

        assert_eq!(plan[0].new, tmp.path().join("01_a.txt"));
        assert_eq!(plan[1].new, tmp.path().join("02_b.txt"));
        assert_eq!(plan[2].new, sub.join("01_c.txt"));
        assert_eq!(plan[3].new, sub.join("02_d.txt"));
    }

    #[test]
    fn build_plan_counter_in_append_resets_per_parent_directory() {
        let tmp = TempDir::new().unwrap();
        let records = vec![
            record(tmp.path().join("a.txt"), tmp.path().to_path_buf()),
            record(tmp.path().join("b.txt"), tmp.path().to_path_buf()),
        ];

        // Append runs on the stem (Exclude default reattaches `.txt`), so
        // `-{n}` lands between stem and extension.
        let opts = transforms::TransformOptions {
            append: Some("-{n}".into()),
            ..Default::default()
        };
        let plan = build_plan(&records, &[], ExtensionScope::Exclude, &opts);

        assert_eq!(plan[0].new, tmp.path().join("a-1.txt"));
        assert_eq!(plan[1].new, tmp.path().join("b-2.txt"));
    }

    #[test]
    fn build_plan_smart_marker_uses_directory_entry_count_for_width() {
        let tmp = TempDir::new().unwrap();
        let records: Vec<_> = (0..100)
            .map(|i| {
                record(
                    tmp.path().join(format!("file-{i}.txt")),
                    tmp.path().to_path_buf(),
                )
            })
            .collect();

        let opts = transforms::TransformOptions {
            prepend: Some("{N}_".into()),
            ..Default::default()
        };
        let plan = build_plan(&records, &[], ExtensionScope::Include, &opts);

        assert_eq!(plan[0].new, tmp.path().join("001_file-0.txt"));
        assert_eq!(plan[98].new, tmp.path().join("099_file-98.txt"));
        assert_eq!(plan[99].new, tmp.path().join("100_file-99.txt"));
    }

    #[test]
    fn build_plan_smart_marker_width_is_per_parent_directory() {
        let tmp = TempDir::new().unwrap();
        let large = tmp.path().join("large");
        let small = tmp.path().join("small");
        let mut records = vec![record(small.join("only.txt"), tmp.path().to_path_buf())];
        records.extend((0..100).map(|i| {
            record(
                large.join(format!("file-{i}.txt")),
                tmp.path().to_path_buf(),
            )
        }));

        let opts = transforms::TransformOptions {
            prepend: Some("{N}_".into()),
            ..Default::default()
        };
        let plan = build_plan(&records, &[], ExtensionScope::Include, &opts);

        assert_eq!(plan[0].new, small.join("01_only.txt"));
        assert_eq!(plan[1].new, large.join("001_file-0.txt"));
        assert_eq!(plan[100].new, large.join("100_file-99.txt"));
    }

    #[test]
    fn build_plan_prepend_and_append_share_per_parent_counter() {
        let tmp = TempDir::new().unwrap();
        let records = vec![
            record(tmp.path().join("a.txt"), tmp.path().to_path_buf()),
            record(tmp.path().join("b.txt"), tmp.path().to_path_buf()),
        ];

        let opts = transforms::TransformOptions {
            prepend: Some("{n}-".into()),
            append: Some("-{n}".into()),
            ..Default::default()
        };
        let plan = build_plan(&records, &[], ExtensionScope::Exclude, &opts);

        assert_eq!(plan[0].new, tmp.path().join("1-a-1.txt"));
        assert_eq!(plan[1].new, tmp.path().join("2-b-2.txt"));
    }

    // ---- ExtensionScope::Exclude stem-only matching -----------------------

    #[test]
    fn split_stem_ext_handles_edge_cases() {
        // Plain extension: stem + last `.ext` only.
        assert_eq!(split_stem_ext("foo.rs"), ("foo", Some("rs")));
        // Compound extension: `.tar.gz` is recognised as a unit.
        assert_eq!(
            split_stem_ext("archive.tar.gz"),
            ("archive", Some("tar.gz"))
        );
        // Compound extension is ASCII case-insensitive.
        assert_eq!(
            split_stem_ext("archive.TAR.GZ"),
            ("archive", Some("TAR.GZ"))
        );
        // Single `.tar` is single-extension (no compound on suffix).
        assert_eq!(split_stem_ext("archive.tar"), ("archive", Some("tar")));
        // Uncommon multi-dot pattern not in compound list: defers to Path semantics.
        assert_eq!(split_stem_ext("test.spec.ts"), ("test.spec", Some("ts")));
        // Dotfile (no extension per `Path::extension`).
        assert_eq!(split_stem_ext(".bashrc"), (".bashrc", None));
        // No-dot basename.
        assert_eq!(split_stem_ext("Makefile"), ("Makefile", None));
        // Trailing dot: extension is empty string, must round-trip the dot.
        assert_eq!(split_stem_ext("archive."), ("archive", Some("")));
        // Compound suffix as the entire basename: not a match (empty stem
        // rejected); falls through to single-ext Path semantics.
        assert_eq!(split_stem_ext(".tar.gz"), (".tar", Some("gz")));
    }

    #[test]
    fn build_plan_exclude_scope_matches_stem_only() {
        // Concrete proof: with the default `Exclude` scope, a literal pattern
        // that only appears in an extension does not match. With `Include`
        // (`-x`), the same pattern would rewrite the basename.
        let tmp = TempDir::new().unwrap();
        let report = tmp.path().join("report.txt");
        let txt_named = tmp.path().join("txt_notes.md");
        fs::write(&report, "").unwrap();
        fs::write(&txt_named, "").unwrap();

        let exprs = compile("txt", "notes");
        let records = vec![
            record(report.clone(), tmp.path().to_path_buf()),
            record(txt_named.clone(), tmp.path().to_path_buf()),
        ];

        // With Exclude: only `txt_notes.md` matches (stem contains `txt`);
        // `report.txt` is unchanged because `txt` only appears in the ext.
        let plan = build_plan(
            &records,
            &exprs,
            ExtensionScope::Exclude,
            &transforms::TransformOptions::default(),
        );
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].old, txt_named);
        assert_eq!(plan[0].new, tmp.path().join("notes_notes.md"));

        // With Include: both match.
        let plan_full = build_plan(
            &records,
            &exprs,
            ExtensionScope::Include,
            &transforms::TransformOptions::default(),
        );
        assert_eq!(plan_full.len(), 2);
    }

    #[test]
    fn build_plan_exclude_scope_handles_dotfiles_and_extensionless() {
        // `.bashrc` and `Makefile` have no extension; the Exclude scope should
        // still match the stem, which IS the whole basename. `archive.tar.gz`
        // is split as a compound extension.
        let tmp = TempDir::new().unwrap();
        let bashrc = tmp.path().join(".bashrc");
        let makefile = tmp.path().join("Makefile");
        let archive = tmp.path().join("archive.tar.gz");
        fs::write(&bashrc, "").unwrap();
        fs::write(&makefile, "").unwrap();
        fs::write(&archive, "").unwrap();

        // Match `archive` literally → expect `archive.tar.gz` to become
        // `release.tar.gz`. `.bashrc` and `Makefile` don't contain `archive`,
        // so they're filtered out.
        let exprs = compile("archive", "release");
        let records = vec![
            record(bashrc.clone(), tmp.path().to_path_buf()),
            record(makefile.clone(), tmp.path().to_path_buf()),
            record(archive.clone(), tmp.path().to_path_buf()),
        ];
        let plan = build_plan(
            &records,
            &exprs,
            ExtensionScope::Exclude,
            &transforms::TransformOptions::default(),
        );
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].old, archive);
        assert_eq!(plan[0].new, tmp.path().join("release.tar.gz"));
    }

    #[test]
    fn build_plan_exclude_scope_preserves_trailing_dot() {
        // `archive.` has stem `archive` and an empty-string extension. After
        // a stem-only rename, the trailing dot must round-trip.
        let tmp = TempDir::new().unwrap();
        let weird = tmp.path().join("archive.");
        fs::write(&weird, "").unwrap();

        let exprs = compile("archive", "release");
        let records = vec![record(weird.clone(), tmp.path().to_path_buf())];
        let plan = build_plan(
            &records,
            &exprs,
            ExtensionScope::Exclude,
            &transforms::TransformOptions::default(),
        );

        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].new, tmp.path().join("release."));
    }

    // ---- ExtensionScope::Only extension-only matching ---------------------

    #[test]
    fn build_plan_only_scope_matches_extension_and_preserves_stem() {
        // With `Only`, the pipeline runs on the extension only. `foo.rs`
        // becomes `foo.txt` under `rs → txt`; `foo.txt` is unchanged because
        // its ext is already `txt`; the stem is preserved verbatim.
        let tmp = TempDir::new().unwrap();
        let foo_rs = tmp.path().join("foo.rs");
        let bar_rs = tmp.path().join("bar.rs");
        fs::write(&foo_rs, "").unwrap();
        fs::write(&bar_rs, "").unwrap();

        let exprs = compile("rs", "txt");
        let records = vec![
            record(foo_rs.clone(), tmp.path().to_path_buf()),
            record(bar_rs.clone(), tmp.path().to_path_buf()),
        ];
        let plan = build_plan(
            &records,
            &exprs,
            ExtensionScope::Only,
            &transforms::TransformOptions::default(),
        );

        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].new, tmp.path().join("foo.txt"));
        assert_eq!(plan[1].new, tmp.path().join("bar.txt"));
    }

    #[test]
    fn build_plan_only_scope_skips_files_without_extension() {
        // Files with no extension (`Makefile`, `.bashrc`) have nothing for the
        // pipeline to operate on under `Only`; they're skipped entirely.
        let tmp = TempDir::new().unwrap();
        let makefile = tmp.path().join("Makefile");
        let bashrc = tmp.path().join(".bashrc");
        fs::write(&makefile, "").unwrap();
        fs::write(&bashrc, "").unwrap();

        let exprs = compile("a", "z");
        let records = vec![
            record(makefile.clone(), tmp.path().to_path_buf()),
            record(bashrc.clone(), tmp.path().to_path_buf()),
        ];
        let plan = build_plan(
            &records,
            &exprs,
            ExtensionScope::Only,
            &transforms::TransformOptions::default(),
        );
        assert!(plan.is_empty());
    }

    // ---- phase-2 rollback (via injected fake rename) ---------------------

    /// Drive `apply_depth_group` with a fake rename op that fails on a chosen
    /// phase-2 step. Exercises the rollback path deterministically without
    /// needing platform-specific permission tricks.
    #[test]
    fn phase2_failure_triggers_per_depth_rollback() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        fs::write(&a, "A").unwrap();
        fs::write(&b, "B").unwrap();

        let entry_a = PlanEntry {
            old: a.clone(),
            new: tmp.path().join("a-new.txt"),
            depth: 1,
        };
        let entry_b = PlanEntry {
            old: b.clone(),
            new: tmp.path().join("b-new.txt"),
            depth: 1,
        };
        let group: Vec<&PlanEntry> = vec![&entry_a, &entry_b];

        // Track calls so the fake can fail on a specific phase-2 invocation.
        // Phase 1 makes 2 calls (old → temp for a, then for b). Phase 2 makes
        // calls 3 and 4 (temp → new). We fail on call 4 (the second phase-2
        // op), which exercises both the phase-2 reverse undo and the
        // phase-1-temp reverse undo.
        let calls = RefCell::new(0usize);
        let rename_op = |from: &Path, to: &Path| -> io::Result<()> {
            let mut n = calls.borrow_mut();
            *n += 1;
            let this = *n;
            drop(n);
            if this == 4 {
                return Err(io::Error::other("fake rename failure"));
            }
            fs::rename(from, to)
        };

        let err = apply_depth_group(&group, &rename_op).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            format!(
                "could not rename {} → {}: fake rename failure",
                entry_b.old.display(),
                entry_b.new.display(),
            ),
        );

        // After rollback, both originals should be back at their starting
        // paths with their starting contents, and neither `new` should exist.
        assert!(a.exists(), "a.txt should be restored");
        assert!(b.exists(), "b.txt should be restored");
        assert_eq!(fs::read_to_string(&a).unwrap(), "A");
        assert_eq!(fs::read_to_string(&b).unwrap(), "B");
        assert!(!entry_a.new.exists());
        assert!(!entry_b.new.exists());
    }
}
