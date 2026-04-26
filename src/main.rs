mod expressions;
mod preview;
mod rename;
mod scan;
mod transforms;

use std::io::IsTerminal as _;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{CommandFactory as _, Parser};
use clap_complete::Shell;

use crate::expressions::{CompileOptions, EXPR_SEP, compile_expressions};
use crate::rename::{PlanEntry, apply_plan, build_plan, validate_plan};

#[derive(Parser)]
#[command(name = "ren", version, disable_help_flag = true)]
struct Cli {
    #[arg(value_name = "arg")]
    args: Vec<String>,

    #[arg(short = 'h', help = "Print help")]
    help: bool,

    #[arg(long = "help", hide = true)]
    help_long: bool,

    /// File glob patterns
    #[arg(short = 'f', long = "files")]
    files: Option<String>,

    /// Include hidden files
    #[arg(short = 'H', long = "hidden")]
    hidden: bool,

    /// Ignore .gitignore / .ignore / .git/info/exclude
    #[arg(long = "no-ignore")]
    no_ignore: bool,

    /// Recurse into subdirectories
    #[arg(short = 'R', long = "recursive")]
    recursive: bool,

    /// Admit directories into the rename plan
    #[arg(long = "include-dirs")]
    include_dirs: bool,

    /// Exclude file extension from rename
    ///
    /// Match against the file stem only; the extension (per
    /// `Path::extension`) is reattached unchanged. Prevents accidents like
    /// `ren txt notes` rewriting `report.txt` to `report.notes`.
    #[arg(short = 'E', long = "no-extension")]
    no_extension: bool,

    /// Greedy matching
    #[arg(short = 'G', long = "greedy")]
    greedy: bool,

    /// Case-insensitive
    #[arg(short = 'i', long = "ignore-case")]
    ignore_case: bool,

    /// Use regex
    #[arg(short = 'r', long = "regex", alias = "regexp")]
    regexp: bool,

    /// Preserve-case replacement
    #[arg(short = 'S', long = "smart")]
    smart: bool,

    /// Lowercase the name
    #[arg(short = 'L', long = "lower", conflicts_with = "upper")]
    lower: bool,

    /// Uppercase the name
    #[arg(short = 'U', long = "upper")]
    upper: bool,

    /// Prepend literal string to each name
    #[arg(short = 'A', long = "prepend", value_name = "STR")]
    prepend: Option<String>,

    /// Append literal string to each name (or stem with -E)
    #[arg(short = 'a', long = "append", value_name = "STR")]
    append: Option<String>,

    /// Prepend a sequential counter
    ///
    /// Use `--counter` alone for the smart default format, or
    /// `--counter=FORMAT` to customize. `{n}` substitutes the index;
    /// `{n:0WIDTH}` zero-pads to WIDTH digits. For example,
    /// `--counter='{n:03}_'` prefixes `001_`, `002_`, ....
    #[arg(
        short = 'c',
        long = "counter",
        value_name = "FORMAT",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = transforms::SMART_COUNTER_TEMPLATE,
    )]
    counter: Option<String>,

    /// Find replace expression
    #[arg(short = 'e', long = "expression", value_name = "<find> <replace>")]
    expressions: Vec<String>,

    /// Whole words only
    #[arg(short = 'w', long = "word-regexp")]
    word_regexp: bool,

    /// Print matching file paths
    #[arg(short = 'l', long = "list-files")]
    list_files: bool,

    /// Dry run
    ///
    /// Composes with `--preview`: prompts the user to accept/reject each entry,
    /// then prints the would-rename plan instead of applying it.
    #[arg(short = 'n', long = "dry-run", alias = "dry")]
    dry_run: bool,

    /// Interactive preview
    #[arg(short = 'p', long = "preview")]
    preview: bool,

    #[arg(long = "completions", value_name = "SHELL", hide = true)]
    completions: Option<Shell>,
}

fn print_help() {
    let is_tty = std::io::stdout().is_terminal();

    let (bold, dim, red, green, yellow, blue, magenta, _white, grey, reset) = if is_tty {
        (
            "\x1b[1m",
            "\x1b[2m",
            "\x1b[31m",
            "\x1b[32m",
            "\x1b[33m",
            "\x1b[34m",
            "\x1b[35m",
            "\x1b[37m",
            "\x1b[38;5;248m",
            "\x1b[m",
        )
    } else {
        ("", "", "", "", "", "", "", "", "", "")
    };

    let text = format!(
        "\
{yellow}{bold}Usage{reset}

  {green}{bold}ren{reset} {red}[options]{reset} {blue}<find> <replace>{reset} {magenta}[<path>…]{reset}

    {blue}<find>{reset}     String to find in names
    {blue}<replace>{reset}  String to replace with
    {magenta}<path>…{reset}    Paths to walk {grey}(optional){reset}

{yellow}{bold}Filter{reset}

  {red}-f{reset}, {red}--files {dim}<glob>{reset}        Smart glob patterns to match files against
  {red}-H{reset}, {red}--hidden{reset}              Include hidden files and directories

{yellow}{bold}Replace{reset}

  {red}-e{reset}, {red}--expression {dim}<f> <r>{reset}  Find/replace expression
  {red}-S{reset}, {red}--smart{reset}               Replace all case variants of the pattern

{yellow}{bold}Regex{reset}

  {red}-G{reset}, {red}--greedy{reset}              Use greedy matching for regular expressions
  {red}-i{reset}, {red}--ignore-case{reset}         Case-insensitive matching
  {red}-r{reset}, {red}--regex{reset}               Treat patterns as regular expressions
  {red}-w{reset}, {red}--word-regexp{reset}         Match only whole words

{yellow}{bold}Scope{reset}

  {red}-R{reset}, {red}--recursive{reset}           Recurse into subdirectories
      {red}--include-dirs{reset}        Also rename directories
  {red}-E{reset}, {red}--no-extension{reset}        Exclude file extension from rename

{yellow}{bold}Transforms{reset}

  {red}-L{reset}, {red}--lower{reset}               Lowercase names
  {red}-U{reset}, {red}--upper{reset}               Uppercase names
  {red}-A{reset}, {red}--prepend {dim}<str>{reset}       Prepend literal string
  {red}-a{reset}, {red}--append {dim}<str>{reset}        Append literal string
  {red}-c{reset}, {red}--counter {dim}[<fmt>]{reset}     Prepend counter

{yellow}{bold}Behavior{reset}

  {red}-l{reset}, {red}--list-files{reset}          Print only file paths whose names match

  {red}-n{reset}, {red}--dry-run{reset}             Show what would be changed without renaming
  {red}-p{reset}, {red}--preview{reset}             Preview the changes before applying them

{yellow}{bold}Miscellaneous{reset}

  {red}-V{reset}, {red}--version{reset}             Print version

  {red}-h{reset}                        Print short help
      {red}--help{reset}                Print long help with examples
"
    );
    print!("{text}");
}

fn print_help_long() {
    let is_tty = std::io::stdout().is_terminal();

    let (bold, green, yellow, grey, reset) = if is_tty {
        (
            "\x1b[1m",
            "\x1b[32m",
            "\x1b[33m",
            "\x1b[38;5;248m",
            "\x1b[m",
        )
    } else {
        ("", "", "", "", "")
    };

    print_help();

    let text = format!(
        "
{yellow}{bold}Examples{reset}

  {grey}# Replace \"foo\" with \"bar\" anywhere in names (cwd, files only){reset}
  {green}${reset} ren foo bar

  {grey}# Recurse into subdirectories{reset}
  {green}${reset} ren -R foo bar

  {grey}# Also rename matching directories{reset}
  {green}${reset} ren --include-dirs foo bar

  {grey}# Restrict to .rs files{reset}
  {green}${reset} ren -f rs old new

  {grey}# Match the stem only; leave extensions alone (api_v1.json → api_v2.json){reset}
  {green}${reset} ren -E v1 v2

  {grey}# Smart-rename across case variants:{reset}
  {grey}#  \"foo_bar\" → \"hello_world\", \"FooBar\" → \"HelloWorld\", etc.{reset}
  {green}${reset} ren --smart foo_bar hello_world

  {grey}# Regex rename: replace test_ prefix with spec_{reset}
  {green}${reset} ren --regex '^test_' 'spec_'

  {grey}# Interactive preview before applying{reset}
  {green}${reset} ren --preview foo bar

  {grey}# Plan only, don't touch the filesystem{reset}
  {green}${reset} ren --dry-run foo bar

  {grey}# Apply multiple replacements in a single pass{reset}
  {green}${reset} ren -e foo bar -e baz qux

  {grey}# Number all files: 01_foo.txt, 02_bar.txt, ... (smart default){reset}
  {green}${reset} ren --counter

  {grey}# Custom counter format with zero-padding{reset}
  {green}${reset} ren --counter='{{n:03}}_'

  {grey}# Lowercase every name in cwd{reset}
  {green}${reset} ren --lower

  {grey}# Compose: find/replace → lower → counter (in fixed order){reset}
  {green}${reset} ren --counter --lower foo bar
"
    );
    print!("{text}");
}

impl Cli {
    /// Fill in defaults from `REN_*` env vars. CLI flags take precedence:
    /// for booleans, an explicit `--flag` (true) is never overridden; for
    /// `Option<T>`, env only fills when the flag is absent (`None`).
    fn apply_env_defaults(&mut self) {
        self.apply_env_defaults_with(|k| std::env::var(k).ok());
    }

    /// Testable core of `apply_env_defaults`.
    fn apply_env_defaults_with(&mut self, get: impl Fn(&str) -> Option<String>) {
        // Truthy: "1", "true" (case-insensitive). Anything else is false.
        let bool_var = |k| {
            get(k)
                .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true"))
                .unwrap_or(false)
        };
        // String env vars: empty string is treated as "unset".
        let str_var = |k| get(k).filter(|v| !v.is_empty());

        self.hidden |= bool_var("REN_HIDDEN");
        self.no_ignore |= bool_var("REN_NO_IGNORE");
        self.recursive |= bool_var("REN_RECURSIVE");
        self.include_dirs |= bool_var("REN_INCLUDE_DIRS");
        self.no_extension |= bool_var("REN_NO_EXTENSION");
        self.greedy |= bool_var("REN_GREEDY");
        self.ignore_case |= bool_var("REN_IGNORE_CASE");
        self.regexp |= bool_var("REN_REGEXP");

        self.smart |= bool_var("REN_SMART");
        self.preview |= bool_var("REN_PREVIEW");

        // Transforms. `--lower` and `--upper` are mutex at the clap layer;
        // env-side we just OR each in independently. If both env vars are
        // set the user gets a runtime conflict caught downstream by
        // `transforms::apply` (lower wins, see its tests).
        self.lower |= bool_var("REN_LOWER");
        self.upper |= bool_var("REN_UPPER");
        // For `Option<String>` env vars, only fill when the CLI flag is absent.
        // Counter env value is the literal template string - there's no
        // shorthand "1 means default" because the value IS the config.
        if self.append.is_none() {
            self.append = str_var("REN_APPEND");
        }
        if self.prepend.is_none() {
            self.prepend = str_var("REN_PREPEND");
        }
        if self.counter.is_none() {
            self.counter = str_var("REN_COUNTER");
        }
    }

    fn uses_expressions(&self) -> bool {
        !self.expressions.is_empty()
    }

    /// True when the CLI takes only `<find>` (no `<replace>`).
    ///
    /// `-l`/`--list-files` without `-e` consumes only `<find>`; all remaining
    /// positionals are search roots.
    fn is_find_only(&self) -> bool {
        !self.uses_expressions() && self.list_files
    }

    /// Documented introspection helper: true when any flag forces regex
    /// semantics. Currently exercised only via tests - `expressions.rs`
    /// reads the underlying booleans through `CompileOptions` rather than
    /// going through the `Cli`. Kept to mirror `rep`'s `Cli` surface.
    #[allow(dead_code)]
    fn is_regex(&self) -> bool {
        self.regexp || self.ignore_case || self.greedy || self.word_regexp
    }

    /// True when any transform flag (`--lower`, `--upper`, `--prepend`,
    /// `--append`, `--counter`) is set. When true AND no `<find> <replace>`
    /// positionals are provided, `<find> <replace>` is no longer required;
    /// positionals become paths instead.
    fn uses_transforms(&self) -> bool {
        self.lower
            || self.upper
            || self.append.is_some()
            || self.prepend.is_some()
            || self.counter.is_some()
    }

    fn positional_skip(&self) -> usize {
        if self.uses_expressions() {
            0
        } else if self.is_find_only() {
            1
        } else if self.uses_transforms() && self.args.len() < 2 {
            // Transforms-only mode: no `<find> <replace>` was supplied; treat
            // positionals as paths to walk.
            0
        } else {
            2
        }
    }

    fn dirs(&self) -> Vec<&str> {
        let args = &self.args[self.positional_skip()..];
        if args.is_empty() {
            vec!["."]
        } else {
            args.iter().map(|arg| arg.as_str()).collect()
        }
    }

    fn file_set(&self) -> Option<scan::FileSet> {
        let globs = scan::parse_file_globs(self.files.as_deref()?);
        if globs.is_empty() {
            return None;
        }
        Some(scan::FileSet {
            matches: globs,
            case_insensitive: true,
        })
    }

    /// Mirror of `rep`'s `paths()` for parity. `run()` uses `dirs()` directly;
    /// `paths()` exists as a typed-`PathBuf` view for tests and any future
    /// caller that needs `PathBuf`s rather than `&str`s.
    #[allow(dead_code)]
    fn paths(&self) -> Vec<PathBuf> {
        self.args
            .iter()
            .skip(self.positional_skip())
            .map(PathBuf::from)
            .collect()
    }

    fn pattern(&self) -> &str {
        &self.args[0]
    }

    fn replacement(&self) -> &str {
        &self.args[1]
    }
}

/// Preprocess argv so that `-e <find> <replace>` is compacted into a single
/// clap value joined by `EXPR_SEP` before clap parses the argument list.
/// This lets the second arg start with `-` without being treated as a flag.
pub(crate) fn preprocess_expression_args(args: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut iter = args.into_iter().peekable();
    while let Some(arg) = iter.next() {
        if arg == "-e" || arg == "--expression" {
            out.push(arg);
            let Some(find) = iter.next() else { continue };
            let Some(replace) = iter.next() else {
                out.push(find);
                continue;
            };
            out.push(format!("{find}{EXPR_SEP}{replace}"));
        } else if let Some(find) = arg.strip_prefix("-e").filter(|s| !s.is_empty()) {
            // Compact form: -efoo → find="foo", next arg is replace
            out.push("-e".to_string());
            let Some(replace) = iter.next() else {
                out.push(find.to_string());
                continue;
            };
            out.push(format!("{find}{EXPR_SEP}{replace}"));
        } else {
            out.push(arg);
        }
    }
    out
}

fn display_path(path: &std::path::Path) -> String {
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

fn render_diff_side(ops: &[DiffOp], old_side: bool, color: &str) -> String {
    let mut rendered = String::new();
    let mut colored = false;

    for op in ops {
        let (ch, changed) = match (*op, old_side) {
            (DiffOp::Same(ch), _) => (Some(ch), false),
            (DiffOp::Old(ch), true) | (DiffOp::New(ch), false) => (Some(ch), true),
            (DiffOp::Old(_), false) | (DiffOp::New(_), true) => (None, false),
        };

        let Some(ch) = ch else {
            continue;
        };

        if changed && !colored {
            rendered.push_str(color);
            colored = true;
        } else if !changed && colored {
            rendered.push_str("\x1b[m");
            colored = false;
        }
        rendered.push(ch);
    }

    if colored {
        rendered.push_str("\x1b[m");
    }

    rendered
}

fn colorized_rename_line(old: &str, new: &str) -> String {
    let ops = diff_chars(old, new);
    let old = render_diff_side(&ops, true, "\x1b[31m");
    let new = render_diff_side(&ops, false, "\x1b[32m");
    format!("{old} \x1b[2m→\x1b[m {new}",)
}

/// Translate a `Cli` into the decoupled `CompileOptions` consumed by
/// `expressions::compile_expressions`. Keeps `expressions.rs` independent
/// of the clap surface.
fn compile_options_from_cli(cli: &Cli) -> CompileOptions {
    let find_only = cli.is_find_only();
    // In transforms-only mode (transforms set, no `-e`, no `-l`, < 2
    // positionals), the surviving positionals are paths to walk - NOT a find
    // pattern. Suppress positional_find/replace so `compile_expressions`
    // produces an empty expression list and `build_plan` runs the transform
    // pipeline as the sole stage.
    let transforms_only =
        cli.uses_transforms() && !cli.uses_expressions() && !find_only && cli.args.len() < 2;

    let positional_find = if cli.uses_expressions() || cli.args.is_empty() || transforms_only {
        None
    } else {
        Some(cli.pattern().to_string())
    };
    let positional_replace =
        if cli.uses_expressions() || find_only || cli.args.len() < 2 || transforms_only {
            None
        } else {
            Some(cli.replacement().to_string())
        };

    CompileOptions {
        regex: cli.regexp,
        ignore_case: cli.ignore_case,
        greedy: cli.greedy,
        word_regexp: cli.word_regexp,
        smart: cli.smart,
        expressions: cli.expressions.clone(),
        positional_find,
        positional_replace,
        list_files_find_only: cli.list_files && !cli.uses_expressions(),
    }
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

fn has_ambiguous_digit_group_separator(separator: &str) -> bool {
    separator.chars().all(char::is_whitespace)
}

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

fn summary_message_with_formatter<F>(total_files: usize, dry: bool, format_count: F) -> String
where
    F: Fn(usize) -> String,
{
    let verb = if dry { "Would rename" } else { "Renamed" };
    format!(
        "{} {} item{}",
        verb,
        format_count(total_files),
        if total_files == 1 { "" } else { "s" },
    )
}

fn summary_message(total_files: usize, dry: bool) -> String {
    summary_message_with_formatter(total_files, dry, with_commas)
}

/// Print plan rows + summary. `dry` uses yellow "Would rename", otherwise green
/// "Renamed". Each row renders `old → new`, with the arrow dimmed when
/// stdout is a TTY.
fn print_summary(plan: &[PlanEntry], dry: bool) {
    let total_files = plan.len();
    let stdout_tty = std::io::stdout().is_terminal();

    for entry in plan {
        let old = display_path(&entry.old);
        let new = display_path(&entry.new);
        if stdout_tty {
            println!("{}", colorized_rename_line(&old, &new));
        } else {
            println!("{old} → {new}");
        }
    }

    if total_files > 0 {
        let msg = summary_message(total_files, dry);
        if stdout_tty {
            let color = if dry { "\x1b[33m" } else { "\x1b[32m" };
            println!("\n\x1b[1m{color}{msg}\x1b[m");
        } else {
            println!("\n{msg}");
        }
    }
}

fn print_error(err: &anyhow::Error) {
    if std::io::stderr().is_terminal() {
        eprintln!("\x1b[1;31merror:\x1b[m {err}");
    } else {
        eprintln!("error: {err}");
    }
}

fn main() {
    if let Err(err) = run() {
        print_error(&err);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut cli = Cli::parse_from(preprocess_expression_args(std::env::args().collect()));
    cli.apply_env_defaults();

    if let Some(shell) = cli.completions {
        clap_complete::generate(shell, &mut Cli::command(), "ren", &mut std::io::stdout());
        return Ok(());
    }

    if cli.help_long {
        print_help_long();
        std::process::exit(0);
    }

    if cli.help {
        print_help();
        std::process::exit(0);
    }

    if !cli.uses_expressions() && cli.args.is_empty() && !cli.list_files && !cli.uses_transforms() {
        print_help();
        std::process::exit(1);
    }

    if cli.positional_skip() > cli.args.len() {
        let missing = if cli.is_find_only() || cli.args.is_empty() {
            "<find>"
        } else {
            "<replace>"
        };
        print_error(&anyhow::anyhow!("missing required argument: {missing}"));
        print_help();
        std::process::exit(1);
    }

    // Validate paths exist
    for dir in &cli.dirs() {
        if !std::path::Path::new(dir).exists() {
            bail!("{dir}: no such file or directory");
        }
    }

    if cli.preview && !std::io::stdin().is_terminal() {
        bail!("--preview requires an interactive terminal");
    }

    let opts = compile_options_from_cli(&cli);
    let exprs = compile_expressions(&opts)?;

    let transforms_opts = transforms::TransformOptions {
        lower: cli.lower,
        upper: cli.upper,
        append: cli.append.clone(),
        prepend: cli.prepend.clone(),
    };
    if let Some(ref t) = cli.counter {
        transforms::validate_counter_template(t)?;
    }

    let records = scan::walk_paths(
        cli.dirs(),
        cli.file_set(),
        cli.hidden,
        cli.no_ignore,
        cli.recursive,
        cli.include_dirs,
    );

    if cli.list_files {
        // Iterate records, print each whose basename matches at least one
        // expression's regex. `walk_paths` already returns natord-sorted
        // results, so no further sorting needed.
        for record in &records {
            let Some(basename) = record.path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if exprs.iter().any(|e| e.regex.is_match(basename)) {
                println!("{}", display_path(&record.path));
            }
        }
        return Ok(());
    }

    let plan = build_plan(
        &records,
        &exprs,
        cli.no_extension,
        &transforms_opts,
        cli.counter.as_deref(),
    );
    validate_plan(&plan)?;

    if plan.is_empty() {
        println!("No matches.");
        return Ok(());
    }

    if cli.preview {
        let mut patcher = preview::PreviewPatcher::new();
        let accepted = patcher.prompt_plan(&plan)?;
        if accepted.is_empty() {
            return Ok(());
        }
        if cli.dry_run {
            print_summary(&accepted, true);
        } else {
            apply_plan(&accepted)?;
            print_summary(&accepted, false);
        }
        return Ok(());
    }

    if cli.dry_run {
        print_summary(&plan, true);
        return Ok(());
    }

    apply_plan(&plan)?;
    print_summary(&plan, false);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_cli(args: &[&str]) -> Cli {
        let processed = preprocess_expression_args(args.iter().map(|s| s.to_string()).collect());
        Cli::parse_from(processed)
    }

    // ---- delegated parse_file_globs smoke ---------------------------------

    #[test]
    fn test_parse_file_globs_delegated_to_scan() {
        // Smoke test: `parse_file_globs` lives in `scan`. Detailed cases live
        // in `scan::tests`; this just confirms the wire-up via `Cli::file_set`.
        assert_eq!(scan::parse_file_globs("rs"), vec!["*.rs"]);
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
    fn test_colorized_rename_line_handles_multiple_regex_replacements() {
        assert_eq!(
            colorized_rename_line("Cargo.lock", "Cbrgb.lbck"),
            "C\x1b[31ma\x1b[mrg\x1b[31mo\x1b[m.l\x1b[31mo\x1b[mck \x1b[2m→\x1b[m C\x1b[32mb\x1b[mrg\x1b[32mb\x1b[m.l\x1b[32mb\x1b[mck"
        );
    }

    // ---- is_regex ---------------------------------------------------------

    #[test]
    fn test_cli_is_regex_any_flag_enables_regex() {
        assert!(!Cli::parse_from(["ren", "a", "b"]).is_regex());
        assert!(Cli::parse_from(["ren", "-r", "a", "b"]).is_regex());
        assert!(Cli::parse_from(["ren", "-i", "a", "b"]).is_regex());
        assert!(Cli::parse_from(["ren", "-w", "a", "b"]).is_regex());
        assert!(Cli::parse_from(["ren", "-G", "a", "b"]).is_regex());
        // Recursion / include-dirs do not flip is_regex.
        assert!(!Cli::parse_from(["ren", "-R", "a", "b"]).is_regex());
        assert!(!Cli::parse_from(["ren", "--include-dirs", "a", "b"]).is_regex());
    }

    // ---- positional_skip --------------------------------------------------

    #[test]
    fn test_cli_positional_skip() {
        // find+replace mode: skip 2 positional args
        assert_eq!(Cli::parse_from(["ren", "a", "b"]).positional_skip(), 2);
        // expression mode: no positional find/replace
        assert_eq!(parse_cli(&["ren", "-e", "a", "b"]).positional_skip(), 0);
        // -l always consumes only the find pattern.
        assert_eq!(Cli::parse_from(["ren", "-l", "a"]).positional_skip(), 1);
        assert_eq!(
            Cli::parse_from(["ren", "-l", "a", "b"]).positional_skip(),
            1
        );
        // -R / --include-dirs are bool flags; they don't change positional layout.
        assert_eq!(
            Cli::parse_from(["ren", "-R", "a", "b"]).positional_skip(),
            2
        );
        assert_eq!(
            Cli::parse_from(["ren", "--include-dirs", "a", "b"]).positional_skip(),
            2
        );
        assert_eq!(
            Cli::parse_from(["ren", "-R", "--include-dirs", "a", "b"]).positional_skip(),
            2
        );
    }

    // ---- is_find_only -----------------------------------------------------

    #[test]
    fn test_cli_is_find_only() {
        assert!(Cli::parse_from(["ren", "-l", "a"]).is_find_only());
        assert!(Cli::parse_from(["ren", "-l", "a", "b"]).is_find_only());
        assert!(!Cli::parse_from(["ren", "a", "b"]).is_find_only());
        // -l with -e is expression mode, not find-only
        assert!(!parse_cli(&["ren", "-l", "-e", "a", "b"]).is_find_only());
    }

    #[test]
    fn test_list_files_mode_treats_trailing_positionals_as_paths() {
        let cli = Cli::parse_from(["ren", "-l", "TODO", "src", "tests"]);
        assert_eq!(cli.positional_skip(), 1);
        assert_eq!(cli.pattern(), "TODO");
        assert_eq!(
            cli.paths(),
            vec![PathBuf::from("src"), PathBuf::from("tests")]
        );
    }

    // ---- dirs / paths with new flags --------------------------------------

    #[test]
    fn test_cli_dirs_defaults_to_current_directory() {
        assert_eq!(Cli::parse_from(["ren", "a", "b"]).dirs(), vec!["."]);
    }

    #[test]
    fn test_cli_dirs_uses_trailing_positionals() {
        let cli = Cli::parse_from(["ren", "a", "b", "src", "tests"]);
        assert_eq!(cli.dirs(), vec!["src", "tests"]);
    }

    #[test]
    fn test_cli_paths_skips_find_and_replace() {
        let cli = Cli::parse_from(["ren", "a", "b", "src", "tests"]);
        assert_eq!(
            cli.paths(),
            vec![PathBuf::from("src"), PathBuf::from("tests")]
        );
    }

    #[test]
    fn test_cli_dirs_with_recursive_flag_unchanged() {
        // `-R` is a boolean - it must not consume a positional.
        let cli = Cli::parse_from(["ren", "-R", "a", "b", "src"]);
        assert_eq!(cli.dirs(), vec!["src"]);
        assert_eq!(cli.paths(), vec![PathBuf::from("src")]);
        assert!(cli.recursive);
    }

    #[test]
    fn test_cli_dirs_with_include_dirs_flag_unchanged() {
        let cli = Cli::parse_from(["ren", "--include-dirs", "a", "b", "src"]);
        assert_eq!(cli.dirs(), vec!["src"]);
        assert_eq!(cli.paths(), vec![PathBuf::from("src")]);
        assert!(cli.include_dirs);
    }

    #[test]
    fn test_transform_short_flags_parse() {
        let lower = Cli::parse_from(["ren", "-L"]);
        assert!(lower.lower);
        assert!(!lower.upper);

        let upper = Cli::parse_from(["ren", "-U"]);
        assert!(upper.upper);
        assert!(!upper.lower);
    }

    #[test]
    fn test_counter_without_value_uses_smart_default() {
        let cli = Cli::parse_from(["ren", "-c"]);
        assert_eq!(
            cli.counter.as_deref(),
            Some(transforms::SMART_COUNTER_TEMPLATE)
        );
    }

    #[test]
    fn test_expression_mode_without_paths_defaults_to_current_dir() {
        let cli = parse_cli(&["ren", "-e", "a", "b", "-e", "b", "c", "--dry-run"]);
        assert!(cli.paths().is_empty());
        assert_eq!(cli.dirs(), vec!["."]);
    }

    // ---- env defaults -----------------------------------------------------

    #[test]
    fn test_env_defaults_enable_boolean_flags() {
        let env = std::collections::HashMap::from([
            ("REN_HIDDEN", "1"),
            ("REN_NO_IGNORE", "true"),
            ("REN_RECURSIVE", "1"),
            ("REN_INCLUDE_DIRS", "true"),
            ("REN_NO_EXTENSION", "1"),
            ("REN_SMART", "TRUE"),
            ("REN_IGNORE_CASE", "1"),
            ("REN_GREEDY", "1"),
            ("REN_REGEXP", "1"),
            ("REN_PREVIEW", "1"),
        ]);
        let mut cli = Cli::parse_from(["ren", "foo", "bar"]);
        cli.apply_env_defaults_with(|k| env.get(k).map(|s| (*s).to_owned()));
        assert!(cli.hidden);
        assert!(cli.no_ignore);
        assert!(cli.recursive);
        assert!(cli.include_dirs);
        assert!(cli.no_extension);
        assert!(cli.smart);
        assert!(cli.ignore_case);
        assert!(cli.greedy);
        assert!(cli.regexp);
        assert!(cli.preview);
    }

    #[test]
    fn test_env_defaults_falsy_values_are_ignored() {
        let env = std::collections::HashMap::from([
            ("REN_HIDDEN", "0"),
            ("REN_RECURSIVE", "false"),
            ("REN_INCLUDE_DIRS", ""),
            ("REN_SMART", "false"),
        ]);
        let mut cli = Cli::parse_from(["ren", "foo", "bar"]);
        cli.apply_env_defaults_with(|k| env.get(k).map(|s| (*s).to_owned()));
        assert!(!cli.hidden);
        assert!(!cli.recursive);
        assert!(!cli.include_dirs);
        assert!(!cli.smart);
    }

    #[test]
    fn test_env_preview_composes_with_dry_run_flag() {
        // `--preview` and `--dry-run` compose: REN_PREVIEW=1 with `-n` on the
        // CLI ends up with both flags set. The runtime body uses preview to
        // collect the accepted set, then prints (rather than applies) the plan.
        let env = std::collections::HashMap::from([("REN_PREVIEW", "1")]);
        let mut cli = Cli::parse_from(["ren", "-n", "foo", "bar"]);
        cli.apply_env_defaults_with(|k| env.get(k).map(|s| (*s).to_owned()));
        assert!(cli.preview);
        assert!(cli.dry_run);
    }

    #[test]
    fn test_preview_and_dry_run_compose() {
        // `--preview --dry-run` is a legitimate user story: walk the prompts,
        // accept/reject entries, then print the plan instead of touching the
        // filesystem. Dropping `conflicts_with = "preview"` from the `dry_run`
        // clap attribute is what makes this parse.
        let cli = Cli::try_parse_from(["ren", "--preview", "--dry-run", "foo", "bar"]).unwrap();
        assert!(cli.preview);
        assert!(cli.dry_run);
    }

    #[test]
    fn test_cli_flag_wins_over_env_for_booleans() {
        // Explicit `-R` already true - env can't unset it (env is OR-only).
        let env = std::collections::HashMap::from([("REN_RECURSIVE", "0")]);
        let mut cli = Cli::parse_from(["ren", "-R", "foo", "bar"]);
        cli.apply_env_defaults_with(|k| env.get(k).map(|s| (*s).to_owned()));
        assert!(cli.recursive);
    }

    // ---- compile_options_from_cli ----------------------------------------

    #[test]
    fn test_compile_options_literal_find_replace() {
        let cli = Cli::parse_from(["ren", "foo", "bar"]);
        let opts = compile_options_from_cli(&cli);
        assert_eq!(opts.positional_find.as_deref(), Some("foo"));
        assert_eq!(opts.positional_replace.as_deref(), Some("bar"));
        assert!(!opts.list_files_find_only);
        assert!(opts.expressions.is_empty());
    }

    #[test]
    fn test_compile_options_expression_mode_drops_positionals() {
        let cli = parse_cli(&["ren", "-e", "a", "b"]);
        let opts = compile_options_from_cli(&cli);
        // Expression mode never carries positional find/replace.
        assert!(opts.positional_find.is_none());
        assert!(opts.positional_replace.is_none());
        assert!(!opts.list_files_find_only);
        assert_eq!(opts.expressions.len(), 1);
    }

    #[test]
    fn test_compile_options_list_files_find_only() {
        // `-l TODO` with no `-e`: only `<find>` is provided. Compile options
        // should mark this as find-only with no replacement.
        let cli = Cli::parse_from(["ren", "-l", "TODO"]);
        let opts = compile_options_from_cli(&cli);
        assert!(opts.list_files_find_only);
        assert_eq!(opts.positional_find.as_deref(), Some("TODO"));
        assert!(opts.positional_replace.is_none());
    }

    #[test]
    fn test_compile_options_list_files_with_expressions_is_not_find_only() {
        let cli = parse_cli(&["ren", "-l", "-e", "a", "b"]);
        let opts = compile_options_from_cli(&cli);
        // -e takes over: the find/replace come from -e, not the find-only path.
        assert!(!opts.list_files_find_only);
    }

    #[test]
    fn test_compile_options_carries_regex_flags() {
        let cli = Cli::parse_from(["ren", "-r", "-i", "-G", "-w", "foo", "bar"]);
        let opts = compile_options_from_cli(&cli);
        assert!(opts.regex);
        assert!(opts.ignore_case);
        assert!(opts.greedy);
        assert!(opts.word_regexp);
    }

    // ---- summary message -------------------------------------------------

    #[test]
    fn test_summary_message_singular_renamed() {
        assert_eq!(summary_message(1, false), "Renamed 1 item");
    }

    #[test]
    fn test_summary_message_plural_renamed() {
        assert_eq!(summary_message(5, false), "Renamed 5 items");
    }

    #[test]
    fn test_summary_message_dry_run_uses_would_rename() {
        assert_eq!(summary_message(1, true), "Would rename 1 item");
        assert_eq!(summary_message(7, true), "Would rename 7 items");
    }

    #[test]
    fn test_summary_message_large_counts_use_thousands_separators() {
        assert_eq!(
            summary_message_with_formatter(1_000, false, |n| format_count(
                n,
                &num_format::Locale::en
            )),
            "Renamed 1,000 items"
        );
        assert_eq!(
            summary_message_with_formatter(2_500_000, true, |n| format_count(
                n,
                &num_format::Locale::en
            )),
            "Would rename 2,500,000 items"
        );
    }

    #[test]
    fn test_format_count_uses_requested_locale() {
        assert_eq!(format_count(0, &num_format::Locale::en), "0");
        assert_eq!(format_count(999, &num_format::Locale::en), "999");
        assert_eq!(format_count(1_000, &num_format::Locale::en), "1,000");
        assert_eq!(format_count(648_098, &num_format::Locale::en), "648,098");
    }

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

    // ---- preprocess_expression_args --------------------------------------

    #[test]
    fn test_preprocess_expression_args_joins_pair_with_separator() {
        let args = preprocess_expression_args(
            ["ren", "-e", "foo", "bar"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        );
        assert_eq!(args, vec!["ren", "-e", &format!("foo{EXPR_SEP}bar")]);
    }

    #[test]
    fn test_preprocess_expression_args_compact_form() {
        let args = preprocess_expression_args(
            ["ren", "-efoo", "bar"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        );
        assert_eq!(args, vec!["ren", "-e", &format!("foo{EXPR_SEP}bar")]);
    }
}
