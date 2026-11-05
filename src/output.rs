//! Fallible command output and rename rendering.
use crate::rename::PlanEntry;
use anyhow::{Context as _, Result};
use std::io::{self, Write};

pub(crate) fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
}

/// Only a closed stdout is a graceful early exit. Failed diagnostics remain
/// failures even when their underlying error is a broken pipe.
pub(crate) fn diagnostic_error(error: io::Error) -> io::Error {
    io::Error::other(format!("write diagnostics: {error}"))
}

/// Keep an earlier operation error, but don't let a broken pipe hide a later
/// flush failure on another stream.
pub(crate) fn finish<T>(result: Result<T>, flushed: io::Result<()>) -> Result<T> {
    match (result, flushed) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) => Err(error.into()),
        (Err(error), Err(flush)) if is_broken_pipe(&error) => Err(flush.into()),
        (Err(error), _) => Err(error),
    }
}

pub(crate) fn write_error(
    out: &mut impl Write,
    error: &anyhow::Error,
    colored: bool,
) -> io::Result<()> {
    if colored {
        writeln!(out, "\x1b[1;31merror:\x1b[m {error:#}")
    } else {
        writeln!(out, "error: {error:#}")
    }
}

/// Keep ownership through the flush attempt. Apply errors may hold the signal
/// guard protecting their recovery report. Reporting itself must never panic.
pub(crate) fn report_error(out: &mut impl Write, error: anyhow::Error, colored: bool) {
    drop(write_error(out, &error, colored));
    drop(out.flush());
}

pub(crate) fn display_path(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    s.strip_prefix("./").unwrap_or(&s).to_string()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffOp {
    Same(char),
    Old(char),
    New(char),
}

fn diff_chars(old: &str, new: &str) -> Vec<DiffOp> {
    let old_chars: Vec<char> = old.chars().collect();
    let new_chars: Vec<char> = new.chars().collect();
    let mut dp = vec![vec![0usize; new_chars.len() + 1]; old_chars.len() + 1];

    for (i, old_char) in old_chars.iter().enumerate() {
        for (j, new_char) in new_chars.iter().enumerate() {
            dp[i + 1][j + 1] = if old_char == new_char {
                dp[i][j] + 1
            } else {
                dp[i][j + 1].max(dp[i + 1][j])
            };
        }
    }

    let mut ops = Vec::with_capacity(old_chars.len().max(new_chars.len()));
    let mut i = old_chars.len();
    let mut j = new_chars.len();
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && old_chars[i - 1] == new_chars[j - 1] {
            ops.push(DiffOp::Same(old_chars[i - 1]));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] > dp[i - 1][j]) {
            ops.push(DiffOp::New(new_chars[j - 1]));
            j -= 1;
        } else {
            ops.push(DiffOp::Old(old_chars[i - 1]));
            i -= 1;
        }
    }
    ops.reverse();
    ops
}

/// Which side of the rename a span is being rendered for. `Old` is the
/// red/removed side, `New` the green/added side.
#[derive(Clone, Copy)]
enum Side {
    Old,
    New,
}

impl Side {
    /// ANSI color used to highlight changed content on this side.
    const fn color(self) -> &'static str {
        match self {
            Self::Old => "\x1b[31m",
            Self::New => "\x1b[32m",
        }
    }
}

/// A token-level diff item. `Both` is an unchanged token (identical text on
/// both sides); `Left`/`Right` are tokens present only on the old/new side.
#[derive(Clone, Copy)]
enum TokenDiff<'a> {
    Both(&'a str),
    Left(&'a str),
    Right(&'a str),
}

/// Render one side of the `old → new` rename line.
///
/// The diff is *token-driven* (see `tokenize`), and character-level
/// highlighting is only used when a change forms a single contiguous run per
/// side - otherwise the whole token/block is highlighted. This mirrors `rep`'s
/// diff: wholesale renames (e.g. `--supplant`) collapse to one solid
/// red-blob → green-blob rather than scattering color across coincidentally
/// shared characters.
fn render_inline_side(old_line: &str, new_line: &str, side: Side) -> String {
    let old_tokens = tokenize(old_line);
    let new_tokens = tokenize(new_line);
    let diffs = diff_tokens(&old_tokens, &new_tokens);

    let mut out = String::new();
    let mut i = 0;
    while i < diffs.len() {
        match diffs[i] {
            TokenDiff::Both(tok) => {
                out.push_str(tok);
                i += 1;
            }
            TokenDiff::Left(_) | TokenDiff::Right(_) => {
                // Gather the whole consecutive run of changed tokens so the
                // block can be classified (balanced replace vs lopsided
                // insert/delete) as a unit.
                let mut lefts: Vec<&str> = Vec::new();
                let mut rights: Vec<&str> = Vec::new();
                while i < diffs.len() {
                    match diffs[i] {
                        TokenDiff::Left(t) => {
                            lefts.push(t);
                            i += 1;
                        }
                        TokenDiff::Right(t) => {
                            rights.push(t);
                            i += 1;
                        }
                        TokenDiff::Both(..) => break,
                    }
                }
                write_change_block(&mut out, &lefts, &rights, side);
            }
        }
    }
    out
}

/// Render a block of changed tokens for one side.
fn write_change_block(out: &mut String, lefts: &[&str], rights: &[&str], side: Side) {
    // Equal token counts: treat as positionally-paired replacements, each of
    // which may char-diff if it's a single-run word edit.
    if lefts.len() == rights.len() {
        for (left, right) in lefts.iter().zip(rights) {
            if should_intra_word_diff(left, right) {
                write_char_diff(out, left, right, side);
            } else {
                write_highlighted(
                    out,
                    if matches!(side, Side::Old) {
                        left
                    } else {
                        right
                    },
                    side,
                );
            }
        }
        return;
    }

    // Lopsided block (insert/delete heavy). Char-diff only if the concatenated
    // texts form a single changed run per side; otherwise highlight the whole
    // side's tokens as one solid block.
    let old_text = lefts.concat();
    let new_text = rights.concat();
    if should_block_char_diff(&old_text, &new_text) {
        write_char_diff(out, &old_text, &new_text, side);
        return;
    }
    for tok in if matches!(side, Side::Old) {
        lefts
    } else {
        rights
    } {
        write_highlighted(out, tok, side);
    }
}

/// Push `text` wrapped in this side's highlight color.
fn write_highlighted(out: &mut String, text: &str, side: Side) {
    out.push_str(side.color());
    out.push_str(text);
    out.push_str("\x1b[m");
}

/// Character-level highlight of `old`→`new` for one side, coloring only the
/// changed characters (shared characters are emitted plain).
fn write_char_diff(out: &mut String, old: &str, new: &str, side: Side) {
    let color = side.color();
    let mut colored = false;
    for op in diff_chars(old, new) {
        let (ch, changed) = match (op, side) {
            (DiffOp::Same(ch), _) => (Some(ch), false),
            (DiffOp::Old(ch), Side::Old) | (DiffOp::New(ch), Side::New) => (Some(ch), true),
            _ => (None, false),
        };
        let Some(ch) = ch else { continue };
        if changed && !colored {
            out.push_str(color);
            colored = true;
        } else if !changed && colored {
            out.push_str("\x1b[m");
            colored = false;
        }
        out.push(ch);
    }
    if colored {
        out.push_str("\x1b[m");
    }
}

/// True when a lopsided block should still char-diff: both sides within the
/// length cap and the change forms a single contiguous run per side.
fn should_block_char_diff(old: &str, new: &str) -> bool {
    const MAX_BLOCK_CHAR_DIFF_LEN: usize = 1024;
    if old.len() > MAX_BLOCK_CHAR_DIFF_LEN || new.len() > MAX_BLOCK_CHAR_DIFF_LEN {
        return false;
    }
    has_single_changed_run_per_side(old, new)
}

/// True when a balanced word-pair should char-diff rather than be highlighted
/// wholesale: both tokens are words within the cap and the edit is a single
/// contiguous run per side (so colored chars are never broken up by shared
/// ones - the anti-speckle guard).
fn should_intra_word_diff(old_tok: &str, new_tok: &str) -> bool {
    const MAX_INTRA_WORD_LEN: usize = 1024;
    if token_kind(old_tok) != TokenKind::Word || token_kind(new_tok) != TokenKind::Word {
        return false;
    }
    if old_tok.len() > MAX_INTRA_WORD_LEN || new_tok.len() > MAX_INTRA_WORD_LEN {
        return false;
    }
    has_single_changed_run_per_side(old_tok, new_tok)
}

/// True when each side of the char diff has at most one contiguous changed run.
fn has_single_changed_run_per_side(old: &str, new: &str) -> bool {
    let mut left_runs = 0usize;
    let mut right_runs = 0usize;
    let mut in_left = false;
    let mut in_right = false;
    for op in diff_chars(old, new) {
        match op {
            DiffOp::Old(_) => {
                if !in_left {
                    left_runs += 1;
                    in_left = true;
                }
                in_right = false;
            }
            DiffOp::New(_) => {
                if !in_right {
                    right_runs += 1;
                    in_right = true;
                }
                in_left = false;
            }
            DiffOp::Same(_) => {
                in_left = false;
                in_right = false;
            }
        }
        if left_runs > 1 || right_runs > 1 {
            return false;
        }
    }
    true
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Whitespace,
    Word,
    Symbol,
}

fn classify(c: char) -> TokenKind {
    if c.is_whitespace() {
        TokenKind::Whitespace
    } else if c.is_alphanumeric() {
        TokenKind::Word
    } else {
        TokenKind::Symbol
    }
}

fn token_kind(tok: &str) -> TokenKind {
    tok.chars().next().map_or(TokenKind::Symbol, classify)
}

/// Split a name into whitespace runs, single symbols, and words (words further
/// split at sub-word boundaries like `camelCase`/`word2`), so the diff aligns
/// on meaningful units instead of individual characters.
fn tokenize(line: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut iter = line.char_indices().peekable();
    while let Some((start, c)) = iter.next() {
        match classify(c) {
            TokenKind::Symbol => {
                let end = iter.peek().map_or(line.len(), |&(i, _)| i);
                tokens.push(&line[start..end]);
            }
            TokenKind::Whitespace => {
                let mut end = line.len();
                while let Some(&(i, next)) = iter.peek() {
                    if classify(next) != TokenKind::Whitespace {
                        end = i;
                        break;
                    }
                    iter.next();
                }
                tokens.push(&line[start..end]);
            }
            TokenKind::Word => {
                let mut sub_start = start;
                let mut prev = c;
                while let Some(&(i, cur)) = iter.peek() {
                    if classify(cur) != TokenKind::Word {
                        break;
                    }
                    iter.next();
                    let next = iter.peek().map(|&(_, n)| n);
                    if is_subword_boundary(prev, cur, next) {
                        tokens.push(&line[sub_start..i]);
                        sub_start = i;
                    }
                    prev = cur;
                }
                let end = iter.peek().map_or(line.len(), |&(i, _)| i);
                tokens.push(&line[sub_start..end]);
            }
        }
    }
    tokens
}

fn is_subword_boundary(prev: char, cur: char, next: Option<char>) -> bool {
    if prev.is_alphabetic() != cur.is_alphabetic() {
        return true;
    }
    if prev.is_lowercase() && cur.is_uppercase() {
        return true;
    }
    prev.is_uppercase() && cur.is_uppercase() && matches!(next, Some(n) if n.is_lowercase())
}

/// Token-level LCS diff. Mirrors `diff_chars`' DP but over `&str` tokens.
fn diff_tokens<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<TokenDiff<'a>> {
    let mut dp = vec![vec![0usize; new.len() + 1]; old.len() + 1];
    for i in (0..old.len()).rev() {
        for j in (0..new.len()).rev() {
            dp[i][j] = if old[i] == new[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let mut ops = Vec::with_capacity(old.len().max(new.len()));
    let (mut i, mut j) = (0, 0);
    while i < old.len() && j < new.len() {
        if old[i] == new[j] {
            ops.push(TokenDiff::Both(old[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push(TokenDiff::Left(old[i]));
            i += 1;
        } else {
            ops.push(TokenDiff::Right(new[j]));
            j += 1;
        }
    }
    while i < old.len() {
        ops.push(TokenDiff::Left(old[i]));
        i += 1;
    }
    while j < new.len() {
        ops.push(TokenDiff::Right(new[j]));
        j += 1;
    }
    ops
}

fn colorized_rename_line(old: &str, new: &str) -> String {
    let old_side = render_inline_side(old, new, Side::Old);
    let new_side = render_inline_side(old, new, Side::New);
    format!("{old_side} \x1b[2m→\x1b[m {new_side}")
}

/// Render `n` using the system locale's thousands separator. Locales whose
/// separator is whitespace fall back to `,` because a space inside a count is
/// ambiguous in CLI output. Same fallback when the system locale is unreadable.
fn format_count<F>(n: usize, format: &F) -> String
where
    F: num_format::Format,
{
    use num_format::ToFormattedString as _;
    n.to_formatted_string(format)
}

#[cfg(not(windows))]
fn has_ambiguous_digit_group_separator(separator: &str) -> bool {
    separator.chars().all(char::is_whitespace)
}

#[cfg(not(windows))]
fn with_commas(n: usize) -> String {
    let fallback = || format_count(n, &num_format::Locale::en);
    let Ok(loc) = num_format::SystemLocale::default() else {
        return fallback();
    };
    if has_ambiguous_digit_group_separator(loc.separator()) {
        return fallback();
    }
    format_count(n, &loc)
}

// Windows has no `num-format` system-locale support (it needs the Windows SDK at
// build time, unavailable when cross-compiling), so use the static English
// locale.
#[cfg(windows)]
fn with_commas(n: usize) -> String {
    format_count(n, &num_format::Locale::en)
}

fn summary_message_with_formatter<F>(
    files: usize,
    dirs: usize,
    dry: bool,
    format_count: F,
) -> String
where
    F: Fn(usize) -> String,
{
    let verb = if dry { "Would rename" } else { "Renamed" };
    let file_part = (files > 0).then(|| {
        format!(
            "{} file{}",
            format_count(files),
            if files == 1 { "" } else { "s" },
        )
    });
    let dir_part = (dirs > 0).then(|| {
        format!(
            "{} director{}",
            format_count(dirs),
            if dirs == 1 { "y" } else { "ies" },
        )
    });
    let body = match (file_part, dir_part) {
        (Some(f), Some(d)) => format!("{f} and {d}"),
        (Some(f), None) => f,
        (None, Some(d)) => d,
        // Caller suppresses the summary when the plan is empty, but keep a
        // sensible fallback so the function is total.
        (None, None) => format!("{} items", format_count(0)),
    };
    format!("{verb} {body}")
}

fn summary_message(files: usize, dirs: usize, dry: bool) -> String {
    summary_message_with_formatter(files, dirs, dry, with_commas)
}

/// Count the sources before applying the plan. Renamed parents can make both
/// paths of a nested entry unavailable after apply. Symlinks count as files.
pub(crate) fn count_files_and_dirs(plan: &[PlanEntry]) -> Result<(usize, usize)> {
    let mut files = 0;
    let mut dirs = 0;
    for entry in plan {
        let metadata = entry
            .old
            .symlink_metadata()
            .with_context(|| format!("inspect source: {}", entry.old.display()))?;
        if metadata.is_dir() {
            dirs += 1;
        } else {
            files += 1;
        }
    }
    Ok((files, dirs))
}

pub(crate) fn write_summary(
    out: &mut impl Write,
    plan: &[PlanEntry],
    counts: (usize, usize),
    dry: bool,
    colored: bool,
) -> io::Result<()> {
    for entry in plan {
        let old = display_path(&entry.old);
        let new = display_path(&entry.new);
        if colored {
            writeln!(out, "{}", colorized_rename_line(&old, &new))?;
        } else {
            writeln!(out, "{old} → {new}")?;
        }
    }
    if !plan.is_empty() {
        let msg = summary_message(counts.0, counts.1, dry);
        if colored {
            let color = if dry { "\x1b[33m" } else { "\x1b[32m" };
            writeln!(out, "\n\x1b[1m{color}{msg}\x1b[m")?;
        } else {
            writeln!(out, "\n{msg}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fatal_diagnostics_flush_before_releasing_error() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        #[derive(Debug)]
        struct HeldError(Arc<AtomicBool>, Arc<AtomicBool>);
        impl std::fmt::Display for HeldError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("pending recovery report")
            }
        }
        impl std::error::Error for HeldError {}
        impl Drop for HeldError {
            fn drop(&mut self) {
                assert!(
                    self.0.load(Ordering::Relaxed),
                    "error released before flush"
                );
                self.1.store(true, Ordering::Relaxed);
            }
        }
        struct Writer {
            flushed: Arc<AtomicBool>,
            fail_write: bool,
        }
        impl Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.fail_write {
                    Err(io::ErrorKind::BrokenPipe.into())
                } else {
                    Ok(bytes.len())
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                self.flushed.store(true, Ordering::Relaxed);
                Err(io::ErrorKind::BrokenPipe.into())
            }
        }
        for fail_write in [false, true] {
            let flushed = Arc::new(AtomicBool::new(false));
            let released = Arc::new(AtomicBool::new(false));
            let error = anyhow::Error::new(HeldError(Arc::clone(&flushed), Arc::clone(&released)));
            report_error(
                &mut Writer {
                    flushed,
                    fail_write,
                },
                error,
                false,
            );
            assert!(released.load(Ordering::Relaxed));
        }
    }

    #[test]
    fn source_types_are_counted_before_swapping_with_different_types() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("file");
        let directory = dir.path().join("directory");
        std::fs::write(&file, "data").unwrap();
        std::fs::create_dir(&directory).unwrap();
        let plan = [PlanEntry {
            old: file,
            new: directory,
            depth: 1,
        }];
        assert_eq!(count_files_and_dirs(&plan).unwrap(), (1, 0));
    }

    #[test]
    fn summary_propagates_destination_write_failure() {
        struct Full;
        impl Write for Full {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::StorageFull.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let plan = [PlanEntry {
            old: "old".into(),
            new: "new".into(),
            depth: 1,
        }];
        let error = write_summary(&mut Full, &plan, (1, 0), true, false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
    }
    // ---- display_path -----------------------------------------------------

    #[test]
    fn test_display_path_strips_leading_dot_slash() {
        assert_eq!(
            display_path(std::path::Path::new("./src/main.rs")),
            "src/main.rs"
        );
    }

    #[test]
    fn test_display_path_preserves_plain_path() {
        assert_eq!(
            display_path(std::path::Path::new("src/main.rs")),
            "src/main.rs"
        );
        assert_eq!(display_path(std::path::Path::new("/abs/path")), "/abs/path");
    }

    #[test]
    fn test_colorized_rename_line_highlights_only_changed_slice() {
        assert_eq!(
            colorized_rename_line("Cargo.lock", "Cbrgo.lock"),
            "C\x1b[31ma\x1b[mrgo.lock \x1b[2m→\x1b[m C\x1b[32mb\x1b[mrgo.lock"
        );
    }

    #[test]
    fn test_colorized_rename_line_handles_lowercase_transform() {
        assert_eq!(
            colorized_rename_line("Cargo.lock", "cargo.lock"),
            "\x1b[31mC\x1b[margo.lock \x1b[2m→\x1b[m \x1b[32mc\x1b[margo.lock"
        );
    }

    #[test]
    fn test_colorized_rename_line_handles_counter_transform() {
        assert_eq!(
            colorized_rename_line("Cargo.lock", "01_Cargo.lock"),
            "Cargo.lock \x1b[2m→\x1b[m \x1b[32m01_\x1b[mCargo.lock"
        );
    }

    #[test]
    fn test_colorized_rename_line_highlights_whole_word_on_multi_run_edit() {
        // `Cargo` → `Cbrgb` has two separate changed chars (a→b, o→b): rather
        // than speckle them, the whole word is highlighted. `lock` → `lbck` is
        // a single run, so it still char-diffs to just the `o`/`b`.
        assert_eq!(
            colorized_rename_line("Cargo.lock", "Cbrgb.lbck"),
            "\x1b[31mCargo\x1b[m.l\x1b[31mo\x1b[mck \x1b[2m→\x1b[m \x1b[32mCbrgb\x1b[m.l\x1b[32mb\x1b[mck"
        );
    }

    #[test]
    fn test_colorized_rename_line_wholesale_replacement_not_speckled() {
        // The `--supplant` case: the stem is replaced outright. The shared
        // `.jpg` extension stays plain, and the stem is a single solid
        // red → green block - no per-character confetti from coincidentally
        // shared digits/letters.
        assert_eq!(
            colorized_rename_line("photo.jpg", "01.jpg"),
            "\x1b[31mphoto\x1b[m.jpg \x1b[2m→\x1b[m \x1b[32m01\x1b[m.jpg"
        );
    }

    #[test]
    fn test_tokenize_splits_on_subword_boundaries() {
        assert_eq!(tokenize("WhatsApp"), vec!["Whats", "App"]);
        assert_eq!(tokenize("img2"), vec!["img", "2"]);
        assert_eq!(tokenize("a-b.c"), vec!["a", "-", "b", ".", "c"]);
    }

    // ---- summary message -------------------------------------------------

    #[test]
    fn test_summary_message_singular_renamed() {
        assert_eq!(summary_message(1, 0, false), "Renamed 1 file");
        assert_eq!(summary_message(0, 1, false), "Renamed 1 directory");
    }

    #[test]
    fn test_summary_message_plural_renamed() {
        assert_eq!(summary_message(5, 0, false), "Renamed 5 files");
        assert_eq!(summary_message(0, 3, false), "Renamed 3 directories");
    }

    #[test]
    fn test_summary_message_dry_run_uses_would_rename() {
        assert_eq!(summary_message(1, 0, true), "Would rename 1 file");
        assert_eq!(summary_message(7, 0, true), "Would rename 7 files");
        assert_eq!(summary_message(0, 1, true), "Would rename 1 directory");
        assert_eq!(summary_message(0, 4, true), "Would rename 4 directories");
    }

    #[test]
    fn test_summary_message_combines_files_and_dirs() {
        assert_eq!(
            summary_message(9, 2, true),
            "Would rename 9 files and 2 directories"
        );
        assert_eq!(
            summary_message(1, 1, true),
            "Would rename 1 file and 1 directory"
        );
        assert_eq!(
            summary_message(2, 1, false),
            "Renamed 2 files and 1 directory"
        );
    }

    #[test]
    fn test_summary_message_large_counts_use_thousands_separators() {
        assert_eq!(
            summary_message_with_formatter(1_000, 0, false, |n| format_count(
                n,
                &num_format::Locale::en
            )),
            "Renamed 1,000 files"
        );
        assert_eq!(
            summary_message_with_formatter(2_500_000, 0, true, |n| format_count(
                n,
                &num_format::Locale::en
            )),
            "Would rename 2,500,000 files"
        );
        assert_eq!(
            summary_message_with_formatter(1_000, 25, true, |n| format_count(
                n,
                &num_format::Locale::en
            )),
            "Would rename 1,000 files and 25 directories"
        );
    }

    #[test]
    fn test_format_count_uses_requested_locale() {
        assert_eq!(format_count(0, &num_format::Locale::en), "0");
        assert_eq!(format_count(999, &num_format::Locale::en), "999");
        assert_eq!(format_count(1_000, &num_format::Locale::en), "1,000");
        assert_eq!(format_count(648_098, &num_format::Locale::en), "648,098");
    }

    #[cfg(not(windows))]
    #[test]
    fn test_has_ambiguous_digit_group_separator() {
        assert!(!has_ambiguous_digit_group_separator(","));
        assert!(has_ambiguous_digit_group_separator(" "));
        assert!(has_ambiguous_digit_group_separator("\u{00a0}"));
    }

    #[test]
    fn test_with_commas_preserves_small_values_without_grouping() {
        assert_eq!(with_commas(0), "0");
        assert_eq!(with_commas(7), "7");
        assert_eq!(with_commas(999), "999");
    }
}
