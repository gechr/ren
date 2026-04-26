// Interactive preview TUI: prompts the user y/n/A/q + back-arrow over each
// planned rename, returning the accepted subset for `apply_plan`.
//
// Derived from fastmod (Copyright Meta Platforms, Inc. and affiliates) by way
// of `rep/src/interactive.rs`, used under the Apache License, Version 2.0.
// See LICENSE and NOTICE at the repo root for details.
//
// The shape mirrors `rep`'s preview but is slimmed down: no editor action, no
// external diff renderer, no multi-line content. Each prompt covers a single
// `PlanEntry` with an inline-diff render of the basename change.

use std::io::Write as _;
use std::path::Path;

use anyhow::Context as _;
use anyhow::Result;
use crossterm::style::Stylize as _;

use crate::rename::PlanEntry;

mod terminal;

use self::terminal::Color;

/// RAII guard for crossterm raw mode.
///
/// `enter()` enables raw mode and returns a guard whose `Drop` disables it.
/// This is a deliberate divergence from `rep`'s preview, which manually
/// brackets `enable_raw_mode()` / `disable_raw_mode()` around the event loop:
/// `q` calls `std::process::exit(0)` from inside `prompt_plan`, bypassing the
/// post-loop disable. The guard's `Drop` runs during stack unwind on
/// `process::exit`, so the user's terminal returns to cooked mode no matter
/// how the prompt loop exits.
struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> Result<Self> {
        crossterm::terminal::enable_raw_mode().context("Unable to enable raw mode")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        drop(crossterm::terminal::disable_raw_mode());
    }
}

/// Sentinel chars for arrow-key navigation in interactive mode.
const NAV_BACK: char = '\x01';
const NAV_FORWARD: char = '\x02';

/// Render `prompt_text`, then read a single keystroke.
///
/// Accepts any character in `letters`. `Enter` selects `default` if provided;
/// `Left`/`Right` arrows return `NAV_BACK`/`NAV_FORWARD` (suppressed at the
/// first/last entry respectively). `Ctrl-C` exits the process with code 130.
fn prompt(
    prompt_text: &str,
    letters: &str,
    default: Option<char>,
    is_first: bool,
    is_last: bool,
) -> Result<char> {
    use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

    print!("{prompt_text}");
    std::io::stdout().flush()?;

    let _guard = RawModeGuard::enter()?;
    let result = loop {
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = event::read().context("Unable to read key event")?
        {
            match code {
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    // RawModeGuard's Drop restores cooked mode during unwind.
                    std::process::exit(130);
                }
                KeyCode::Enter => {
                    if let Some(default) = default {
                        break Ok(default);
                    }
                }
                KeyCode::Left if !is_first => {
                    break Ok(NAV_BACK);
                }
                KeyCode::Right if !is_last => {
                    break Ok(NAV_FORWARD);
                }
                KeyCode::Char(c) if letters.contains(c) => {
                    break Ok(c);
                }
                _ => {}
            }
        }
    };
    if let Ok(c) = result {
        match c {
            NAV_BACK => print!("{}\r", "←".yellow()),
            NAV_FORWARD => print!("{}\r", "→".yellow()),
            'y' | 'A' => println!("{}", c.to_string().green()),
            'n' | 'q' => println!("{}", c.to_string().red()),
            _ => println!("{c}"),
        }
    }
    result
}

/// Find the (prefix_bytes, suffix_bytes) overlap of two strings by walking
/// `char_indices` in both directions. Pure: no I/O.
fn common_prefix_suffix(old: &str, new: &str) -> (usize, usize) {
    let prefix_bytes = old
        .char_indices()
        .zip(new.char_indices())
        .take_while(|((_, a), (_, b))| a == b)
        .last()
        .map_or(0, |((i, c), _)| i + c.len_utf8());

    let old_rest = &old[prefix_bytes..];
    let new_rest = &new[prefix_bytes..];

    let suffix_bytes = old_rest
        .char_indices()
        .rev()
        .zip(new_rest.char_indices().rev())
        .take_while(|((_, a), (_, b))| a == b)
        .last()
        .map_or(0, |((i, _), _)| old_rest.len() - i);

    (prefix_bytes, suffix_bytes)
}

/// Render `old → new` as a single side-by-side line with the changed slice
/// colored red on the left, green on the right, and the unchanged
/// prefix/suffix preserved on both sides.
fn print_inline_diff(old_path: &Path, new_path: &Path) {
    // Filenames are guaranteed UTF-8 by `build_plan` (non-UTF-8 basenames are
    // skipped at scan time). `to_string_lossy` is the conservative fallback if
    // a non-UTF-8 path slips through.
    let old_basename = old_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let new_basename = new_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let parent = old_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| {
            let s = p.to_string_lossy().into_owned();
            // Strip the leading "./" so cwd-rooted plans don't render "./foo/".
            s.strip_prefix("./").unwrap_or(&s).to_string()
        })
        .filter(|s| !s.is_empty());

    let (prefix_bytes, suffix_bytes) = common_prefix_suffix(&old_basename, &new_basename);
    let prefix = &old_basename[..prefix_bytes];
    let old_changed = &old_basename[prefix_bytes..old_basename.len() - suffix_bytes];
    let new_changed = &new_basename[prefix_bytes..new_basename.len() - suffix_bytes];
    let suffix = &old_basename[old_basename.len() - suffix_bytes..];

    if let Some(parent) = parent.as_deref() {
        let parent_with_sep = format!("{parent}/");
        print!("{}", parent_with_sep.as_str().dim());
    }

    print!("{prefix}");
    terminal::fg(Color::Red);
    print!("{old_changed}");
    terminal::reset();
    print!("{suffix}");

    print!("  {}  ", "→".dim());

    if let Some(parent) = parent.as_deref() {
        let parent_with_sep = format!("{parent}/");
        print!("{}", parent_with_sep.as_str().dim());
    }

    print!("{prefix}");
    terminal::fg(Color::Green);
    print!("{new_changed}");
    terminal::reset();
    println!("{suffix}");
}

/// Apply a user action to the decision stack and return the new index.
///
/// Pure: takes only the current state and returns the new index. Does NOT
/// read keys, render, or talk to the terminal.
///
/// Action characters:
/// - `'y'`: push `(idx, true)` (accept). Returns `idx` (caller advances).
/// - `'n'`: push `(idx, false)` (reject). Returns `idx`.
/// - `'A'`: same as `'y'` here; the caller flips `yes_to_all` on its side so
///   subsequent iterations skip the prompt.
/// - `'>'` (NAV_FORWARD): same as `'n'`.
/// - `'<'` (NAV_BACK): pop the last decision and return `idx - 1`. If the
///   stack is empty, returns 0 and does not pop.
fn decide(decisions: &mut Vec<(usize, bool)>, idx: usize, action: char) -> usize {
    match action {
        'y' | 'A' => {
            decisions.push((idx, true));
            idx
        }
        'n' | '>' => {
            decisions.push((idx, false));
            idx
        }
        '<' => {
            if decisions.pop().is_some() {
                idx.saturating_sub(1)
            } else {
                0
            }
        }
        _ => idx,
    }
}

/// Interactive preview driver.
pub(crate) struct PreviewPatcher {
    yes_to_all: bool,
}

impl PreviewPatcher {
    pub(crate) fn new() -> Self {
        Self { yes_to_all: false }
    }

    /// Walk `plan` interactively and return the accepted subset.
    ///
    /// Each iteration prompts y/n/A/q with left-arrow to go back. The decision
    /// stack records `(index, accepted)` pairs so back-arrow can pop and
    /// re-prompt. After `A`, subsequent iterations auto-accept without
    /// prompting. `q` calls `process::exit(0)`; the `RawModeGuard` Drop
    /// restores cooked mode during unwind.
    pub(crate) fn prompt_plan(&mut self, plan: &[PlanEntry]) -> Result<Vec<PlanEntry>> {
        let total = plan.len();
        let mut decisions: Vec<(usize, bool)> = Vec::with_capacity(total);
        let mut idx = 0;

        while idx < total {
            // Auto-accept after the user pressed `A`. The decision is recorded
            // but no UI is rendered, matching rep's behavior.
            if self.yes_to_all {
                decisions.push((idx, true));
                idx += 1;
                continue;
            }

            let entry = &plan[idx];
            terminal::hide_cursor();
            terminal::clear();

            // Header: position counter + parent context.
            let header = format!("Rename [{}/{}]", idx + 1, total);
            println!("{}", header.yellow().bold());
            print_inline_diff(&entry.old, &entry.new);

            let prompt_text = format!(
                "\n{} {}{}{}{}{}{}{}{}{}{}{}{}",
                "Apply?".yellow().bold(),
                "y".green().bold(),
                "es ".white(),
                "· ".dim(),
                "n".red(),
                "o ".white(),
                "· ".dim(),
                "A".green(),
                "ll ".white(),
                "· ".dim(),
                "q".red(),
                "uit\n".white(),
                "❯ ".yellow(),
            );

            terminal::show_cursor();
            let action = prompt(&prompt_text, "ynAq", Some('y'), idx == 0, idx + 1 == total)?;

            match action {
                'y' => {
                    decide(&mut decisions, idx, 'y');
                    idx += 1;
                }
                'n' => {
                    decide(&mut decisions, idx, 'n');
                    idx += 1;
                }
                'A' => {
                    self.yes_to_all = true;
                    decide(&mut decisions, idx, 'A');
                    idx += 1;
                }
                'q' => {
                    // RawModeGuard from `prompt` has already dropped (returned
                    // a result), so cooked mode is restored. Exit cleanly.
                    std::process::exit(0);
                }
                NAV_BACK => {
                    idx = decide(&mut decisions, idx, '<');
                }
                NAV_FORWARD => {
                    decide(&mut decisions, idx, '>');
                    idx += 1;
                }
                _ => {
                    // Unreachable per `prompt`'s contract, but stay safe.
                    idx += 1;
                }
            }
        }

        Ok(plan
            .iter()
            .enumerate()
            .filter(|(i, _)| decisions.iter().any(|(j, accepted)| *j == *i && *accepted))
            .map(|(_, e)| e.clone())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::common_prefix_suffix;
    use super::decide;
    use crate::rename::PlanEntry;

    // ---- decide -----------------------------------------------------------

    #[test]
    fn decide_y_pushes_accept_and_returns_idx() {
        let mut decisions = Vec::new();
        let new_idx = decide(&mut decisions, 3, 'y');
        assert_eq!(new_idx, 3);
        assert_eq!(decisions, vec![(3, true)]);
    }

    #[test]
    fn decide_n_pushes_reject() {
        let mut decisions = Vec::new();
        let new_idx = decide(&mut decisions, 2, 'n');
        assert_eq!(new_idx, 2);
        assert_eq!(decisions, vec![(2, false)]);
    }

    #[test]
    fn decide_capital_a_pushes_accept() {
        let mut decisions = Vec::new();
        let new_idx = decide(&mut decisions, 0, 'A');
        assert_eq!(new_idx, 0);
        assert_eq!(decisions, vec![(0, true)]);
    }

    #[test]
    fn decide_forward_arrow_is_reject() {
        let mut decisions = Vec::new();
        let new_idx = decide(&mut decisions, 4, '>');
        assert_eq!(new_idx, 4);
        assert_eq!(decisions, vec![(4, false)]);
    }

    #[test]
    fn decide_back_arrow_pops_and_decrements() {
        let mut decisions = vec![(0, true), (1, false)];
        let new_idx = decide(&mut decisions, 2, '<');
        assert_eq!(new_idx, 1);
        assert_eq!(decisions, vec![(0, true)]);
    }

    #[test]
    fn decide_back_arrow_on_empty_stack_returns_zero_and_keeps_stack() {
        let mut decisions: Vec<(usize, bool)> = Vec::new();
        let new_idx = decide(&mut decisions, 0, '<');
        assert_eq!(new_idx, 0);
        assert!(decisions.is_empty());
    }

    /// Walk a representative input sequence step-by-step, mirroring how
    /// `prompt_plan` advances `idx` after each `decide` call.
    #[test]
    fn decide_sequence_y_n_y_back_y_capital_a_n_back_y() {
        // Plan has 6 hypothetical entries. Sequence:
        //  idx 0: 'y' → accept; advance
        //  idx 1: 'n' → reject; advance
        //  idx 2: 'y' → accept; advance
        //  idx 3: '<' → pop accept@2, idx becomes 2
        //  idx 2: 'y' → accept; advance
        //  idx 3: 'A' → accept (yes_to_all flag set externally); advance
        //  idx 4: 'n' → reject; advance
        //  idx 5: '<' → pop reject@4, idx becomes 4
        //  idx 4: 'y' → accept; advance
        let mut decisions: Vec<(usize, bool)> = Vec::new();
        let mut idx = 0;

        // y
        decide(&mut decisions, idx, 'y');
        idx += 1;
        // n
        decide(&mut decisions, idx, 'n');
        idx += 1;
        // y
        decide(&mut decisions, idx, 'y');
        idx += 1;
        // <
        idx = decide(&mut decisions, idx, '<');
        // y
        decide(&mut decisions, idx, 'y');
        idx += 1;
        // A
        decide(&mut decisions, idx, 'A');
        idx += 1;
        // n
        decide(&mut decisions, idx, 'n');
        idx += 1;
        // <
        idx = decide(&mut decisions, idx, '<');
        // y
        decide(&mut decisions, idx, 'y');
        idx += 1;

        assert_eq!(idx, 5);
        assert_eq!(
            decisions,
            vec![(0, true), (1, false), (2, true), (3, true), (4, true)],
        );
    }

    /// The accepted-subset filter (mirrors the closing expression of
    /// `prompt_plan`) yields the entries whose index appears with
    /// `accepted = true` in the decision stack.
    #[test]
    fn accepted_subset_filter_matches_decision_stack() {
        let plan = [
            PlanEntry {
                old: PathBuf::from("a"),
                new: PathBuf::from("A"),
                depth: 1,
            },
            PlanEntry {
                old: PathBuf::from("b"),
                new: PathBuf::from("B"),
                depth: 1,
            },
            PlanEntry {
                old: PathBuf::from("c"),
                new: PathBuf::from("C"),
                depth: 1,
            },
            PlanEntry {
                old: PathBuf::from("d"),
                new: PathBuf::from("D"),
                depth: 1,
            },
        ];
        // Accept 0 and 2; reject 1 and 3.
        let decisions = [(0, true), (1, false), (2, true), (3, false)];

        let accepted: Vec<PlanEntry> = plan
            .iter()
            .enumerate()
            .filter(|(i, _)| decisions.iter().any(|(j, ok)| *j == *i && *ok))
            .map(|(_, e)| e.clone())
            .collect();

        assert_eq!(accepted.len(), 2);
        assert_eq!(accepted[0].old, PathBuf::from("a"));
        assert_eq!(accepted[1].old, PathBuf::from("c"));
    }

    // ---- common_prefix_suffix --------------------------------------------

    #[test]
    fn common_prefix_suffix_test_to_spec() {
        // "test_foo.rs" vs "spec_foo.rs": prefix is empty, suffix "_foo.rs"
        let (p, s) = common_prefix_suffix("test_foo.rs", "spec_foo.rs");
        assert_eq!(p, 0, "no common prefix");
        assert_eq!(s, "_foo.rs".len(), "suffix is _foo.rs");
    }

    #[test]
    fn common_prefix_suffix_shared_prefix_and_extension() {
        // "foo_old.txt" vs "foo_new.txt": prefix "foo_", suffix ".txt"
        let (p, s) = common_prefix_suffix("foo_old.txt", "foo_new.txt");
        assert_eq!(p, "foo_".len());
        assert_eq!(s, ".txt".len());
    }

    #[test]
    fn common_prefix_suffix_identical_strings() {
        let (p, s) = common_prefix_suffix("foo.rs", "foo.rs");
        // Identical strings: the entire string is both prefix and suffix.
        // `common_prefix_suffix` walks prefix to end-of-shorter then walks the
        // remaining empty suffix.
        assert_eq!(p, "foo.rs".len());
        assert_eq!(s, 0);
    }

    #[test]
    fn common_prefix_suffix_handles_multibyte_chars() {
        // "café_a.rs" vs "café_b.rs": prefix "café_" (5 bytes for café + _),
        // suffix ".rs". The 'é' is 2 bytes so the prefix byte count is 6.
        let (p, s) = common_prefix_suffix("café_a.rs", "café_b.rs");
        assert_eq!(p, "café_".len());
        assert_eq!(s, ".rs".len());
    }

    #[test]
    fn common_prefix_suffix_empty_strings() {
        let (p, s) = common_prefix_suffix("", "");
        assert_eq!(p, 0);
        assert_eq!(s, 0);
    }

    #[test]
    fn common_prefix_suffix_no_overlap() {
        let (p, s) = common_prefix_suffix("abc", "xyz");
        assert_eq!(p, 0);
        assert_eq!(s, 0);
    }
}
