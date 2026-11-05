// Plan / validate / apply for `ren`. Takes already-walked paths and already-
// compiled expressions, produces a `Vec<PlanEntry>`, validates it against
// within-plan and existing-target conflicts, and applies it via a per-depth
// two-phase rename.
//
// The two-phase apply runs deepest-depth first. Each depth-group renames every
// `old` to a unique `<parent>/.ren-<pid>-<counter>` temp (phase 1),
// then renames every temp to its target `new` (phase 2). This handles chains,
// cycles, case-only renames (which would otherwise no-op on case-insensitive
// filesystems), and parent/child dependencies through the same mechanism.
//
// Rollback scope is per-failing-depth only: either failure restores staged
// sources, undoing completed placements first. Deeper depths stay applied.
// Recovery failures report the actual location of each stranded entry.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::anyhow;
use anyhow::{Context as _, Result};

use crate::display_path;
use crate::expressions;
use crate::scan;
use crate::transforms;

mod noreplace;

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
    change_ext: Option<&str>,
    transforms_opts: &transforms::TransformOptions,
    target_dir: Option<&Path>,
    parents: bool,
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
            .is_some_and(transforms::has_counter_placeholder)
        || transforms_opts
            .supplant
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
                // `-E` overrides the reattached extension. A single leading dot
                // is optional; an empty value drops the extension entirely.
                // Without `-E`, the original extension is preserved.
                suffix = match change_ext {
                    Some(new) => {
                        let e = new.strip_prefix('.').unwrap_or(new);
                        if e.is_empty() {
                            String::new()
                        } else {
                            format!(".{e}")
                        }
                    }
                    None => match ext {
                        Some(e) => format!(".{e}"),
                        None => String::new(),
                    },
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

        // `-d <dir>` relocates the renamed file under <dir>; otherwise the
        // rename happens in place, keeping the source's parent directory.
        //
        // Under `-d`, the default is to flatten - only the new basename lands
        // in <dir>, so `x.jpg` + `-d out` => `out/x.jpg`. This is deliberate:
        // the source path is frequently absolute (shell-expanded globs), and
        // `Path::join` discards its base when the argument is absolute, so
        // joining the full source path would silently ignore `-d` entirely.
        //
        // `--parents` opts into reproducing the source's parent structure
        // *relative to its walk root* under <dir>: a file found at
        // `photos/2024/x.jpg` under root `photos` lands at `out/2024/x.jpg`.
        // For shell-expanded globs each file is its own root, so the relative
        // parent is empty and `--parents` coincides with flatten.
        let new = match target_dir {
            Some(dir) => {
                let relative = if parents {
                    record
                        .path
                        .strip_prefix(&record.root)
                        .ok()
                        .and_then(Path::parent)
                        .filter(|p| !p.as_os_str().is_empty())
                        .map(|p| p.join(&new_basename))
                        .unwrap_or_else(|| PathBuf::from(&new_basename))
                } else {
                    PathBuf::from(&new_basename)
                };
                dir.join(relative)
            }
            None => record
                .path
                .parent()
                .map(|p| p.join(&new_basename))
                .unwrap_or_else(|| PathBuf::from(&new_basename)),
        };

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
///    actual directory entry of an `old` scheduled to move.
///
/// Case folding uses `String::to_lowercase`, which is approximate Unicode
/// (see the trade-offs section of the build plan). Adequate for ASCII-only
/// filenames.
pub(crate) fn validate_plan(plan: &[PlanEntry]) -> Result<()> {
    let cwd = std::env::current_dir()?;
    for entry in plan {
        let old = absolute_normalized(&cwd.join(&entry.old))?;
        let new = absolute_normalized(&cwd.join(&entry.new))?;
        if new != old
            && new.starts_with(&old)
            && entry
                .old
                .symlink_metadata()
                .with_context(|| format!("unable to inspect source {}", display_path(&entry.old)))?
                .is_dir()
        {
            return Err(anyhow!(
                "cannot move directory {} inside itself: {}",
                display_path(&entry.old),
                display_path(&entry.new)
            ));
        }
    }
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

    // Resolve actual directory entries, not lowercased spellings. On a
    // case-sensitive volume, Foo and foo can be unrelated existing files.
    let old_entries: std::collections::HashMap<PathBuf, (&Path, usize)> = plan
        .iter()
        .map(|e| {
            existing_entry(&e.old)
                .with_context(|| format!("unable to resolve source {}", display_path(&e.old)))
                .map(|path| (path, (e.old.as_path(), e.depth)))
        })
        .collect::<Result<_>>()?;
    let mut aliases = std::collections::HashMap::<_, Vec<_>>::new();
    for (path, source) in &old_entries {
        if let Some(key) = case_alias_key(path) {
            aliases.entry(key).or_default().push(*source);
        }
    }

    let mut external: Vec<(&Path, &Path)> = Vec::new();
    for entry in plan {
        match existing_entry(&entry.new) {
            Ok(destination) => {
                let mut freed = old_entries
                    .get(&destination)
                    .is_some_and(|(_, depth)| *depth >= entry.depth);
                if !freed
                    && let Some(candidates) =
                        case_alias_key(&destination).and_then(|key| aliases.get(&key))
                {
                    let destination_id = entry_identity(&entry.new)?;
                    for &(source, depth) in candidates {
                        if depth >= entry.depth && entry_identity(source)? == destination_id {
                            freed = true;
                            break;
                        }
                    }
                }
                if !freed {
                    external.push((entry.old.as_path(), entry.new.as_path()));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(anyhow::Error::from(e).context(format!(
                    "unable to resolve destination {}",
                    display_path(&entry.new)
                )));
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

fn case_alias_key(path: &Path) -> Option<(PathBuf, String)> {
    Some((
        path.parent()?.to_path_buf(),
        path.file_name()?.to_str()?.to_lowercase(),
    ))
}

fn entry_identity(path: &Path) -> Result<(u64, u64)> {
    noreplace::entry_identity(path)
        .with_context(|| format!("unable to inspect directory entry {}", display_path(path)))
}

fn absolute_normalized(path: &Path) -> io::Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in std::path::absolute(path)?.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

fn existing_entry(path: &Path) -> io::Result<PathBuf> {
    let metadata = path.symlink_metadata()?;
    if metadata.file_type().is_symlink() {
        // Resolving the final symlink would confuse its target with the link.
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        Ok(parent.canonicalize()?.join(
            path.file_name().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "path has no basename")
            })?,
        ))
    } else {
        path.canonicalize()
    }
}

/// Apply the plan via a per-depth two-phase rename, deepest-first.
///
/// Group entries by `depth` and walk descending. For each depth: rename every
/// `old` to a unique temp (phase 1), then rename every temp to its `new`
/// (phase 2). On either error, roll back the failing depth. Deeper depths
/// remain committed. Rollback never replaces an occupied path.
#[cfg(test)]
pub(crate) fn apply_plan(plan: &[PlanEntry]) -> Result<()> {
    validate_plan(plan)?;
    apply_plan_with(plan, real_rename)
}

pub(crate) fn apply_plan_interruptible(plan: &[PlanEntry]) -> Result<()> {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    let cancelled = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&cancelled);
    let active = Arc::new(Mutex::new(true));
    #[derive(Debug)]
    struct ApplyGuard(Arc<Mutex<bool>>);
    impl Drop for ApplyGuard {
        fn drop(&mut self) {
            *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
        }
    }
    // The caller owns reporting. Keep cancellation in recovery mode until it
    // has written/flushed the error and releases it, including errors naming
    // stranded files. Printing remains outside the rename engine.
    #[derive(Debug)]
    struct ApplyFailure {
        error: anyhow::Error,
        _guard: ApplyGuard,
    }
    impl std::fmt::Display for ApplyFailure {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "{}", self.error)
        }
    }
    impl std::error::Error for ApplyFailure {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.error.source()
        }
    }
    let guard = ApplyGuard(Arc::clone(&active));
    ctrlc::set_handler(move || {
        let applying = active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *applying {
            signal.store(true, Ordering::Relaxed);
        } else {
            // The handler outlives apply. Once recovery is finished, a signal
            // must still terminate even when the main thread is blocked on output.
            std::process::exit(130);
        }
    })?;
    let result = (|| {
        validate_plan(plan)?;
        apply_plan_with_cancellation(plan, real_rename, || cancelled.load(Ordering::Relaxed))
    })();
    match result {
        Ok(()) => {
            drop(guard);
            check_cancelled(&|| cancelled.load(Ordering::Relaxed)).map_err(anyhow::Error::from)
        }
        Err(error) => Err(anyhow::Error::new(ApplyFailure {
            error,
            _guard: guard,
        })),
    }
}

/// Native no-overwrite rename adapter for the production apply path.
///
/// `apply_plan_with` takes the rename op as a parameter so tests can inject a
/// fake to exercise the rollback branch deterministically.
fn real_rename(from: &Path, to: &Path) -> io::Result<()> {
    noreplace::rename(from, to)
}

/// Apply a plan using a caller-supplied rename op. The production entry point
/// `apply_plan` wires this to `real_rename`; tests inject a fake to drive the
/// phase-2 rollback path.
#[cfg(test)]
fn apply_plan_with<F>(plan: &[PlanEntry], rename_op: F) -> Result<()>
where
    F: Fn(&Path, &Path) -> io::Result<()>,
{
    apply_plan_with_cancellation(plan, rename_op, || false)
}

fn apply_plan_with_cancellation<F, C>(plan: &[PlanEntry], rename_op: F, cancelled: C) -> Result<()>
where
    F: Fn(&Path, &Path) -> io::Result<()>,
    C: Fn() -> bool,
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

    let mut completed: Vec<&PlanEntry> = Vec::new();
    for (_, group) in by_depth.iter().rev() {
        if let Err(error) = apply_depth_group_cancellable(group, &rename_op, &cancelled) {
            if completed.is_empty() {
                return Err(error);
            }
            let mut message = String::from("earlier depth groups remain applied:");
            for entry in completed {
                let _ = write!(
                    message,
                    "\n  {} → {}",
                    display_path(&entry.old),
                    display_path(&entry.new)
                );
            }
            return Err(error.context(message));
        }
        completed.extend(group.iter().copied());
    }

    Ok(())
}

/// Apply a single depth-group: phase 1 (old → temp) then phase 2 (temp → new),
/// with per-depth rollback on either failure.
#[cfg(test)]
fn apply_depth_group<F>(group: &[&PlanEntry], rename_op: &F) -> Result<()>
where
    F: Fn(&Path, &Path) -> io::Result<()>,
{
    apply_depth_group_cancellable(group, rename_op, &|| false)
}

fn check_cancelled(cancelled: &impl Fn() -> bool) -> io::Result<()> {
    if cancelled() {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "rename interrupted",
        ))
    } else {
        Ok(())
    }
}

fn apply_depth_group_cancellable<F>(
    group: &[&PlanEntry],
    rename_op: &F,
    cancelled: &impl Fn() -> bool,
) -> Result<()>
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
            match check_cancelled(cancelled).and_then(|()| rename_op(&entry.old, &temp)) {
                Ok(()) => break,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt < 16 => {
                    attempt += 1;
                    temp = unique_temp_path(&entry.old);
                }
                Err(e) => {
                    // User-facing: the rename of `old` to `new` failed. The
                    // temp hop is an implementation detail; the user wrote a
                    // find/replace, they see "rename A → B".
                    let recovery = rollback(&applied, &[], rename_op);
                    return Err(anyhow::Error::from(e).context(
                        format!(
                            "could not rename {} → {}",
                            entry.old.display(),
                            entry.new.display(),
                        ) + &recovery,
                    ));
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
        if let Err(e) = check_cancelled(cancelled).and_then(|()| rename_op(temp, &entry.new)) {
            let recovery = rollback(&applied, &phase2_done, rename_op);
            return Err(anyhow::Error::from(e).context(
                format!(
                    "could not rename {} → {}",
                    entry.old.display(),
                    entry.new.display(),
                ) + &recovery,
            ));
        }
        phase2_done.push((entry, temp.clone()));
    }

    Ok(())
}

fn rollback<F>(
    staged: &[(&PlanEntry, PathBuf)],
    placed: &[(&PlanEntry, PathBuf)],
    rename_op: &F,
) -> String
where
    F: Fn(&Path, &Path) -> io::Result<()>,
{
    let mut recovery = String::new();
    let mut stranded = std::collections::HashSet::new();
    // Vacate every completed destination before restoring sources, so chains
    // and cycles can be undone without overwriting another entry.
    for (entry, temp) in placed.iter().rev() {
        if let Err(error) = rename_op(&entry.new, temp) {
            stranded.insert(temp);
            let _ = write!(
                recovery,
                "\nrecovery required: {} remains at {} ({error})",
                entry.old.display(),
                entry.new.display()
            );
        }
    }
    for (entry, temp) in staged.iter().rev() {
        if stranded.contains(temp) {
            continue;
        }
        if let Err(error) = rename_op(temp, &entry.old) {
            let _ = write!(
                recovery,
                "\nrecovery required: {} remains at {} ({error})",
                entry.old.display(),
                temp.display()
            );
        }
    }
    recovery
}

/// Build a bounded temp name next to `old`. The atomic rename reserves it;
/// `apply_depth_group` retries collisions without overwriting the occupant.
fn unique_temp_path(old: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let parent = old.parent().unwrap_or(Path::new("."));
    let pid = std::process::id();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".ren-{pid}-{counter:08x}"))
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

    fn two_entry_plan(dir: &Path) -> Vec<PlanEntry> {
        ["a", "b"]
            .into_iter()
            .map(|name| {
                fs::write(dir.join(name), name).unwrap();
                PlanEntry {
                    old: dir.join(name),
                    new: dir.join(format!("new-{name}")),
                    depth: 1,
                }
            })
            .collect()
    }

    #[test]
    fn cancellation_during_staging_restores_sources() {
        use std::cell::Cell;
        let dir = TempDir::new().unwrap();
        let plan = two_entry_plan(dir.path());
        let cancelled = Cell::new(false);
        let rename = |from: &Path, to: &Path| {
            super::real_rename(from, to)?;
            cancelled.set(true);
            Ok(())
        };
        let error =
            super::apply_plan_with_cancellation(&plan, rename, || cancelled.get()).unwrap_err();
        assert!(format!("{error:#}").contains("rename interrupted"));
        assert_eq!(fs::read_to_string(&plan[0].old).unwrap(), "a");
        assert_eq!(fs::read_to_string(&plan[1].old).unwrap(), "b");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn cancellation_during_placement_restores_sources() {
        use std::cell::Cell;
        let dir = TempDir::new().unwrap();
        let plan = two_entry_plan(dir.path());
        let cancelled = Cell::new(false);
        let rename = |from: &Path, to: &Path| {
            super::real_rename(from, to)?;
            if to == plan[0].new {
                cancelled.set(true);
            }
            Ok(())
        };
        assert!(super::apply_plan_with_cancellation(&plan, rename, || cancelled.get()).is_err());
        assert_eq!(fs::read_to_string(&plan[0].old).unwrap(), "a");
        assert_eq!(fs::read_to_string(&plan[1].old).unwrap(), "b");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn failure_reports_earlier_completed_depth_groups() {
        let dir = TempDir::new().unwrap();
        let mut plan = two_entry_plan(dir.path());
        plan[0].depth = 2;
        let rename = |from: &Path, to: &Path| {
            if from == plan[1].old {
                Err(io::Error::other("injected failure"))
            } else {
                super::real_rename(from, to)
            }
        };
        let error = super::apply_plan_with(&plan, rename).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("earlier depth groups remain applied"));
        assert!(message.contains(&format!(
            "{} → {}",
            plan[0].old.display(),
            plan[0].new.display()
        )));
        assert_eq!(fs::read_to_string(&plan[0].new).unwrap(), "a");
        assert_eq!(fs::read_to_string(&plan[1].old).unwrap(), "b");
    }

    #[test]
    fn target_in_later_depth_group_is_not_yet_vacant() {
        let dir = TempDir::new().unwrap();
        let mut plan = two_entry_plan(dir.path());
        plan[0].depth = 2;
        plan[0].new = plan[1].old.clone();
        assert!(validate_plan(&plan).is_err());
    }

    #[test]
    fn directory_cannot_move_inside_its_descendant() {
        let dir = TempDir::new().unwrap();
        let old = dir.path().join("folder");
        fs::create_dir(&old).unwrap();
        let plan = [PlanEntry {
            old: old.clone(),
            new: old.join("child"),
            depth: 1,
        }];
        assert!(
            validate_plan(&plan)
                .unwrap_err()
                .to_string()
                .contains("inside itself")
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn case_only_symlink_rename_preserves_link_identity() {
        for target_exists in [true, false] {
            let dir = TempDir::new().unwrap();
            let target = dir.path().join("target.txt");
            if target_exists {
                fs::write(&target, "target").unwrap();
            }
            let old = dir.path().join("Link.txt");
            let new = dir.path().join("link.txt");
            #[cfg(unix)]
            std::os::unix::fs::symlink("target.txt", &old).unwrap();
            #[cfg(windows)]
            if let Err(error) = std::os::windows::fs::symlink_file("target.txt", &old) {
                if error.raw_os_error() == Some(1314) {
                    return;
                }
                panic!("unable to create test symlink: {error}");
            }
            apply_plan(&[PlanEntry {
                old,
                new: new.clone(),
                depth: 1,
            }])
            .unwrap();
            assert_eq!(fs::read_link(&new).unwrap(), PathBuf::from("target.txt"));
            let names: Vec<_> = fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert!(names.contains(&std::ffi::OsString::from("link.txt")));
            assert!(!names.contains(&std::ffi::OsString::from("Link.txt")));
            assert_eq!(target.exists(), target_exists);
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_directory_is_not_a_self_descendant_move() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("target")).unwrap();
        let old = dir.path().join("link");
        std::os::unix::fs::symlink("target", &old).unwrap();
        let plan = [PlanEntry {
            new: old.join("child"),
            old: old.clone(),
            depth: 1,
        }];
        validate_plan(&plan).unwrap();
        assert!(apply_plan(&plan).is_err());
        assert_eq!(fs::read_link(old).unwrap(), PathBuf::from("target"));
    }

    #[test]
    fn metadata_errors_identify_the_affected_path() {
        let dir = TempDir::new().unwrap();
        let old = dir.path().join("source");
        let new = dir.path().join("z".repeat(300));
        fs::write(&old, "content").unwrap();
        let error = validate_plan(&[PlanEntry {
            old: old.clone(),
            new: new.clone(),
            depth: 1,
        }])
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&new.to_string_lossy().to_string())
        );
        fs::remove_file(&old).unwrap();
        let error = validate_plan(&[PlanEntry {
            old: old.clone(),
            new: dir.path().join("new"),
            depth: 1,
        }])
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(&old.to_string_lossy().to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn signal_after_apply_child() {
        use std::io::Write as _;
        let Some(root) = std::env::var_os("REN_SIGNAL_TEST_DIR") else {
            return;
        };
        let dir = PathBuf::from(root);
        let expect_error = dir.join("expect-error").exists();
        let result = super::apply_plan_interruptible(&[PlanEntry {
            old: dir.join("source"),
            new: dir.join(if expect_error {
                "destination/child"
            } else {
                "destination"
            }),
            depth: 1,
        }]);
        if expect_error {
            let error = result.unwrap_err();
            fs::write(dir.join("completed"), "").unwrap();
            let started = std::time::Instant::now();
            while !dir.join("report-now").exists() {
                assert!(started.elapsed() < std::time::Duration::from_secs(10));
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            writeln!(std::io::stderr(), "{error:#}").unwrap();
            std::io::stderr().flush().unwrap();
            drop(error);
            fs::write(dir.join("reported"), "").unwrap();
        } else {
            result.unwrap();
            fs::write(dir.join("completed"), "").unwrap();
        }
        std::io::stdout()
            .lock()
            .write_all(&vec![b'x'; 1024 * 1024])
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn signal_after_apply_terminates_blocked_output() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("source"), "content").unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "rename::tests::signal_after_apply_child",
                "--nocapture",
            ])
            .env("REN_SIGNAL_TEST_DIR", dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        while !dir.path().join("completed").exists() && started.elapsed() < Duration::from_secs(10)
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !dir.path().join("completed").exists() {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("child did not finish apply");
        }
        std::thread::sleep(Duration::from_millis(50));
        let signal = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap();
        assert!(signal.success());
        let signalled = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if signalled.elapsed() >= Duration::from_secs(3) {
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        if status.is_none() {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        assert_eq!(status.and_then(|status| status.code()), Some(130));
        assert_eq!(
            fs::read_to_string(dir.path().join("destination")).unwrap(),
            "content"
        );
    }

    #[cfg(unix)]
    #[test]
    fn apply_error_keeps_signal_protection_until_reported() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("source"), "source").unwrap();
        fs::write(dir.path().join("destination"), "occupied").unwrap();
        fs::write(dir.path().join("expect-error"), "").unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "rename::tests::signal_after_apply_child",
                "--nocapture",
            ])
            .env("REN_SIGNAL_TEST_DIR", dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let started = Instant::now();
        while !dir.path().join("completed").exists() && started.elapsed() < Duration::from_secs(10)
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !dir.path().join("completed").exists() {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("child did not return an apply error");
        }
        assert!(
            Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            child.try_wait().unwrap().is_none(),
            "signal discarded the pending error"
        );
        fs::write(dir.path().join("report-now"), "").unwrap();
        let started = Instant::now();
        while !dir.path().join("reported").exists() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !dir.path().join("reported").exists() {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("child did not flush its apply error");
        }
        assert!(
            Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if started.elapsed() >= Duration::from_secs(3) {
                break None;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        if status.is_none() {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        assert_eq!(status.and_then(|status| status.code()), Some(130));
        let output = child.wait_with_output().unwrap();
        let report = String::from_utf8(output.stderr).unwrap();
        assert_eq!(report.matches("unable to resolve destination").count(), 1);
        assert_eq!(report.matches("os error").count(), 1);
        assert_eq!(
            fs::read_to_string(dir.path().join("source")).unwrap(),
            "source"
        );
    }

    #[test]
    fn unicode_alias_collision_rolls_back_without_overwriting() {
        let dir = TempDir::new().unwrap();
        let nfc = dir.path().join("é");
        let nfd = dir.path().join("e\u{301}");
        fs::write(&nfc, "probe").unwrap();
        let aliases = nfd.exists();
        fs::remove_file(&nfc).unwrap();
        if !aliases {
            return;
        }
        let mut plan = two_entry_plan(dir.path());
        plan[0].new = nfc;
        plan[1].new = nfd;
        assert!(apply_plan(&plan).is_err());
        assert_eq!(fs::read_to_string(&plan[0].old).unwrap(), "a");
        assert_eq!(fs::read_to_string(&plan[1].old).unwrap(), "b");
    }

    #[test]
    fn applying_preview_subset_cannot_overwrite_rejected_source() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a");
        let aa = dir.path().join("aa");
        fs::write(&a, "first").unwrap();
        fs::write(&aa, "second").unwrap();
        let plan = vec![
            PlanEntry {
                old: a.clone(),
                new: aa.clone(),
                depth: 1,
            },
            PlanEntry {
                old: aa.clone(),
                new: dir.path().join("aaaa"),
                depth: 1,
            },
        ];
        validate_plan(&plan).unwrap();
        assert!(apply_plan(&plan[..1]).is_err());
        assert_eq!(fs::read_to_string(a).unwrap(), "first");
        assert_eq!(fs::read_to_string(aa).unwrap(), "second");
    }

    #[test]
    fn staging_failure_restores_earlier_sources() {
        let dir = TempDir::new().unwrap();
        let plan = two_entry_plan(dir.path());
        let rename = |from: &Path, to: &Path| {
            if from == plan[1].old {
                Err(io::Error::other("staging failed"))
            } else {
                super::real_rename(from, to)
            }
        };
        assert!(super::apply_plan_with(&plan, rename).is_err());
        assert_eq!(fs::read_to_string(&plan[0].old).unwrap(), "a");
        assert_eq!(fs::read_to_string(&plan[1].old).unwrap(), "b");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn concurrent_destination_is_preserved_and_sources_restored() {
        let dir = TempDir::new().unwrap();
        let plan = two_entry_plan(dir.path());
        let rename = |from: &Path, to: &Path| {
            if to == plan[1].new {
                fs::write(to, "concurrent").unwrap();
            }
            super::real_rename(from, to)
        };
        assert!(super::apply_plan_with(&plan, rename).is_err());
        assert_eq!(fs::read_to_string(&plan[0].old).unwrap(), "a");
        assert_eq!(fs::read_to_string(&plan[1].old).unwrap(), "b");
        assert_eq!(fs::read_to_string(&plan[1].new).unwrap(), "concurrent");
        assert!(!plan[0].new.exists());
    }

    #[test]
    fn rollback_preserves_occupied_source_and_reports_temp_location() {
        let dir = TempDir::new().unwrap();
        let plan = two_entry_plan(dir.path());
        let rename = |from: &Path, to: &Path| {
            if from == plan[1].old {
                fs::write(&plan[0].old, "concurrent").unwrap();
                Err(io::Error::other("staging failed"))
            } else {
                super::real_rename(from, to)
            }
        };
        let error = super::apply_plan_with(&plan, rename)
            .unwrap_err()
            .to_string();
        let stranded = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".ren-")
            })
            .unwrap();
        assert_eq!(fs::read_to_string(&plan[0].old).unwrap(), "concurrent");
        assert_eq!(fs::read_to_string(&stranded).unwrap(), "a");
        assert!(error.contains(&format!("remains at {}", stranded.display())));
    }

    #[test]
    fn failed_placement_undo_reports_destination_not_temp() {
        let dir = TempDir::new().unwrap();
        let plan = two_entry_plan(dir.path());
        let rename = |from: &Path, to: &Path| {
            if to == plan[1].new || from == plan[0].new {
                Err(io::Error::other("injected failure"))
            } else {
                super::real_rename(from, to)
            }
        };
        let error = super::apply_plan_with(&plan, rename)
            .unwrap_err()
            .to_string();
        assert_eq!(fs::read_to_string(&plan[0].new).unwrap(), "a");
        assert_eq!(fs::read_to_string(&plan[1].old).unwrap(), "b");
        assert!(error.contains(&format!("remains at {}", plan[0].new.display())));
        assert_eq!(error.matches("recovery required:").count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn long_basename_uses_bounded_temporary_name() {
        let dir = TempDir::new().unwrap();
        let old = dir.path().join("z".repeat(249));
        let new = dir.path().join("x".repeat(250));
        fs::write(&old, "content").unwrap();
        apply_plan(&[PlanEntry {
            old: old.clone(),
            new: new.clone(),
            depth: 1,
        }])
        .unwrap();
        assert!(!old.exists());
        assert_eq!(fs::read_to_string(new).unwrap(), "content");
    }

    #[test]
    fn hardlink_target_is_not_a_scheduled_source_entry() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::write(&a, "content").unwrap();
        fs::hard_link(&a, &b).unwrap();
        assert!(
            validate_plan(&[PlanEntry {
                old: a,
                new: b,
                depth: 1
            }])
            .is_err()
        );
    }

    #[test]
    fn case_fold_does_not_admit_unrelated_existing_destination() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a");
        let upper = dir.path().join("A");
        fs::write(&a, "first").unwrap();
        if upper.exists() {
            return;
        } // This distinction requires a case-sensitive volume.
        fs::write(&upper, "second").unwrap();
        assert!(
            validate_plan(&[PlanEntry {
                old: a,
                new: upper,
                depth: 1
            }])
            .is_err()
        );
    }

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
            None,
            &transforms::TransformOptions::default(),
            None,
            false,
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
            None,
            &transforms::TransformOptions::default(),
            None,
            false,
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
            None,
            &transforms::TransformOptions::default(),
            None,
            false,
        );

        assert_eq!(plan.len(), 1);
        // a/b/c.txt under tmp/ → depth 3 relative to root.
        assert_eq!(plan[0].depth, 3);
    }

    // ---- target directory (`-d`) -----------------------------------------

    #[test]
    fn build_plan_target_dir_flattens_by_default() {
        // `build_plan` is pure path/string manipulation (no fs access), so
        // synthetic paths exercise the relocation directly. The default drops
        // only the renamed basename into <dir>, discarding the source's parents.
        let records = vec![record(
            PathBuf::from("photos/a/b/c.txt"),
            PathBuf::from("photos"),
        )];
        let exprs = compile("c", "z");
        let plan = build_plan(
            &records,
            &exprs,
            ExtensionScope::Exclude,
            None,
            &transforms::TransformOptions::default(),
            Some(Path::new("foo/bar")),
            false,
        );

        assert_eq!(plan.len(), 1);
        // Flatten: foo/bar + z.txt => foo/bar/z.txt (parents dropped).
        assert_eq!(plan[0].new, PathBuf::from("foo/bar/z.txt"));
        // The source is untouched by `-d`.
        assert_eq!(plan[0].old, PathBuf::from("photos/a/b/c.txt"));
    }

    #[test]
    fn build_plan_target_dir_absolute_source_flattens() {
        // Regression: shell-expanded globs yield absolute source paths, where
        // each file is its own walk root. `Path::join` discards its base when
        // the argument is absolute, so the pre-flatten `dir.join(full_path)`
        // silently ignored `-d`. Flattening on the basename avoids that trap.
        let src = PathBuf::from("/home/u/Downloads/x.jpeg");
        let records = vec![record(src.clone(), src.clone())];
        let exprs = compile("x", "01");
        let plan = build_plan(
            &records,
            &exprs,
            ExtensionScope::Exclude,
            None,
            &transforms::TransformOptions::default(),
            Some(Path::new("/mnt/images")),
            false,
        );

        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].new, PathBuf::from("/mnt/images/01.jpeg"));
    }

    #[test]
    fn build_plan_target_dir_parents_preserves_structure() {
        // `--parents` reproduces the source's parents relative to its walk root.
        let records = vec![record(
            PathBuf::from("photos/a/b/c.txt"),
            PathBuf::from("photos"),
        )];
        let exprs = compile("c", "z");
        let plan = build_plan(
            &records,
            &exprs,
            ExtensionScope::Exclude,
            None,
            &transforms::TransformOptions::default(),
            Some(Path::new("foo/bar")),
            true,
        );

        assert_eq!(plan.len(), 1);
        // Preserve the `a/b` structure below the root under <dir>.
        assert_eq!(plan[0].new, PathBuf::from("foo/bar/a/b/z.txt"));
    }

    #[test]
    fn build_plan_target_dir_trailing_slash_is_equivalent() {
        let records = vec![record(PathBuf::from("a/b/c.txt"), PathBuf::from("."))];
        let exprs = compile("c", "z");
        let with = |dir| {
            build_plan(
                &records,
                &exprs,
                ExtensionScope::Exclude,
                None,
                &transforms::TransformOptions::default(),
                Some(Path::new(dir)),
                false,
            )[0]
            .new
            .clone()
        };

        assert_eq!(with("foo/bar"), with("foo/bar/"));
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
        let plan = build_plan(
            &records,
            &[],
            ExtensionScope::Include,
            None,
            &opts,
            None,
            false,
        );

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
        let plan = build_plan(
            &records,
            &[],
            ExtensionScope::Exclude,
            None,
            &opts,
            None,
            false,
        );

        assert_eq!(plan[0].new, tmp.path().join("a-1.txt"));
        assert_eq!(plan[1].new, tmp.path().join("b-2.txt"));
    }

    #[test]
    fn build_plan_supplant_replaces_stem_and_preserves_extension() {
        // The headline use case: `--supplant '{n:02}'` renumbers every file to
        // 01, 02, … while the default (Exclude) scope reattaches the extension.
        let tmp = TempDir::new().unwrap();
        let records = vec![
            record(tmp.path().join("photo_a.jpg"), tmp.path().to_path_buf()),
            record(tmp.path().join("photo_b.jpg"), tmp.path().to_path_buf()),
        ];

        let opts = transforms::TransformOptions {
            supplant: Some("{n:02}".into()),
            ..Default::default()
        };
        let plan = build_plan(
            &records,
            &[],
            ExtensionScope::Exclude,
            None,
            &opts,
            None,
            false,
        );

        assert_eq!(plan[0].new, tmp.path().join("01.jpg"));
        assert_eq!(plan[1].new, tmp.path().join("02.jpg"));
    }

    #[test]
    fn build_plan_supplant_with_include_extension_replaces_entire_basename() {
        // `-x` (Include scope) makes the working segment the FULL basename, so
        // `--supplant` overwrites the extension too: `photo_a.jpg` → `01`.
        let tmp = TempDir::new().unwrap();
        let records = vec![
            record(tmp.path().join("photo_a.jpg"), tmp.path().to_path_buf()),
            record(tmp.path().join("photo_b.png"), tmp.path().to_path_buf()),
        ];

        let opts = transforms::TransformOptions {
            supplant: Some("{n:02}".into()),
            ..Default::default()
        };
        let plan = build_plan(
            &records,
            &[],
            ExtensionScope::Include,
            None,
            &opts,
            None,
            false,
        );

        assert_eq!(plan[0].new, tmp.path().join("01"));
        assert_eq!(plan[1].new, tmp.path().join("02"));
    }

    #[test]
    fn build_plan_change_extension_replaces_extension() {
        let tmp = TempDir::new().unwrap();
        let records = vec![
            record(tmp.path().join("foo.jpg"), tmp.path().to_path_buf()),
            record(tmp.path().join("bar.jpeg"), tmp.path().to_path_buf()),
        ];

        let opts = transforms::TransformOptions::default();
        let plan = build_plan(
            &records,
            &[],
            ExtensionScope::Exclude,
            Some("png"),
            &opts,
            None,
            false,
        );

        assert_eq!(plan[0].new, tmp.path().join("foo.png"));
        assert_eq!(plan[1].new, tmp.path().join("bar.png"));
    }

    #[test]
    fn build_plan_change_extension_leading_dot_is_optional() {
        // `png` and `.png` are equivalent; a *second* dot is preserved verbatim.
        let tmp = TempDir::new().unwrap();
        let records = vec![record(tmp.path().join("foo.jpg"), tmp.path().to_path_buf())];
        let opts = transforms::TransformOptions::default();

        let dotted = build_plan(
            &records,
            &[],
            ExtensionScope::Exclude,
            Some(".png"),
            &opts,
            None,
            false,
        );
        assert_eq!(dotted[0].new, tmp.path().join("foo.png"));

        let double = build_plan(
            &records,
            &[],
            ExtensionScope::Exclude,
            Some("..png"),
            &opts,
            None,
            false,
        );
        assert_eq!(double[0].new, tmp.path().join("foo..png"));
    }

    #[test]
    fn build_plan_change_extension_combines_with_supplant() {
        // The headline use case: renumber the stem AND change the extension in
        // one pass. `--supplant` rewrites the stem; `-E` overrides the suffix.
        let tmp = TempDir::new().unwrap();
        let records = vec![
            record(tmp.path().join("photo_a.jpg"), tmp.path().to_path_buf()),
            record(tmp.path().join("photo_b.jpg"), tmp.path().to_path_buf()),
        ];

        let opts = transforms::TransformOptions {
            supplant: Some("{n:02}".into()),
            ..Default::default()
        };
        let plan = build_plan(
            &records,
            &[],
            ExtensionScope::Exclude,
            Some("webp"),
            &opts,
            None,
            false,
        );

        assert_eq!(plan[0].new, tmp.path().join("01.webp"));
        assert_eq!(plan[1].new, tmp.path().join("02.webp"));
    }

    #[test]
    fn build_plan_change_extension_adds_to_extensionless_files() {
        // Files without an extension gain one; dotfiles are treated as having
        // no extension (the leading dot is part of the name).
        let tmp = TempDir::new().unwrap();
        let records = vec![
            record(tmp.path().join("Makefile"), tmp.path().to_path_buf()),
            record(tmp.path().join(".bashrc"), tmp.path().to_path_buf()),
        ];

        let opts = transforms::TransformOptions::default();
        let plan = build_plan(
            &records,
            &[],
            ExtensionScope::Exclude,
            Some("png"),
            &opts,
            None,
            false,
        );

        assert_eq!(plan[0].new, tmp.path().join("Makefile.png"));
        assert_eq!(plan[1].new, tmp.path().join(".bashrc.png"));
    }

    #[test]
    fn build_plan_change_extension_empty_strips_extension() {
        // An empty value drops the extension; a file that already lacks one is
        // unchanged and so is skipped by the no-op guard.
        let tmp = TempDir::new().unwrap();
        let records = vec![
            record(tmp.path().join("notes.bak"), tmp.path().to_path_buf()),
            record(tmp.path().join("Makefile"), tmp.path().to_path_buf()),
        ];

        let opts = transforms::TransformOptions::default();
        let plan = build_plan(
            &records,
            &[],
            ExtensionScope::Exclude,
            Some(""),
            &opts,
            None,
            false,
        );

        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].old, tmp.path().join("notes.bak"));
        assert_eq!(plan[0].new, tmp.path().join("notes"));
    }

    #[test]
    fn build_plan_change_extension_replaces_whole_compound_extension() {
        // The entire extension is replaced, including recognised compound ones.
        let tmp = TempDir::new().unwrap();
        let records = vec![record(
            tmp.path().join("archive.tar.gz"),
            tmp.path().to_path_buf(),
        )];

        let opts = transforms::TransformOptions::default();
        let plan = build_plan(
            &records,
            &[],
            ExtensionScope::Exclude,
            Some("zip"),
            &opts,
            None,
            false,
        );

        assert_eq!(plan[0].new, tmp.path().join("archive.zip"));
    }

    #[test]
    fn build_plan_supplant_literal_collapse_is_caught_by_validate() {
        // A literal `--supplant` (no counter) maps every file in a directory to
        // the same target; `validate_plan` must reject it rather than silently
        // clobbering. This is the "dry-run detects the conflict" guarantee.
        let tmp = TempDir::new().unwrap();
        let records = vec![
            record(tmp.path().join("a.txt"), tmp.path().to_path_buf()),
            record(tmp.path().join("b.txt"), tmp.path().to_path_buf()),
        ];

        let opts = transforms::TransformOptions {
            supplant: Some("same".into()),
            ..Default::default()
        };
        let plan = build_plan(
            &records,
            &[],
            ExtensionScope::Exclude,
            None,
            &opts,
            None,
            false,
        );

        // Both collapse to `same.txt`.
        assert_eq!(plan[0].new, tmp.path().join("same.txt"));
        assert_eq!(plan[1].new, tmp.path().join("same.txt"));
        let err = validate_plan(&plan).unwrap_err();
        assert!(format!("{err}").contains("same target"));
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
        let plan = build_plan(
            &records,
            &[],
            ExtensionScope::Include,
            None,
            &opts,
            None,
            false,
        );

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
        let plan = build_plan(
            &records,
            &[],
            ExtensionScope::Include,
            None,
            &opts,
            None,
            false,
        );

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
        let plan = build_plan(
            &records,
            &[],
            ExtensionScope::Exclude,
            None,
            &opts,
            None,
            false,
        );

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
            None,
            &transforms::TransformOptions::default(),
            None,
            false,
        );
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].old, txt_named);
        assert_eq!(plan[0].new, tmp.path().join("notes_notes.md"));

        // With Include: both match.
        let plan_full = build_plan(
            &records,
            &exprs,
            ExtensionScope::Include,
            None,
            &transforms::TransformOptions::default(),
            None,
            false,
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
            None,
            &transforms::TransformOptions::default(),
            None,
            false,
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
            None,
            &transforms::TransformOptions::default(),
            None,
            false,
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
            None,
            &transforms::TransformOptions::default(),
            None,
            false,
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
            None,
            &transforms::TransformOptions::default(),
            None,
            false,
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
