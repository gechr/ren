mod config;
mod expressions;
mod io;
mod preview;
mod rename;
mod scan;
#[cfg(test)]
mod test_env;
mod transforms;

use std::io::IsTerminal as _;
use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use clap::builder::BoolishValueParser;
use clap::parser::ValueSource;
use clap::{ArgMatches, CommandFactory as _, FromArgMatches as _, Parser};
use clap_complete::Shell;

use crate::expressions::{CompileOptions, EXPR_SEP, compile_expressions};
use crate::rename::{ExtensionScope, PlanEntry, apply_plan, build_plan, validate_plan};

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

    /// Read NUL-separated filenames from stdin (else newline-separated).
    /// Stdin is consumed when no `<path>…` is given and stdin is piped.
    #[arg(short = '0', long = "null")]
    null: bool,

    /// Include hidden files
    #[arg(
        short = 'H',
        long = "hidden",
        env = "REN_HIDDEN",
        value_parser = BoolishValueParser::new()
    )]
    hidden: bool,

    /// Ignore .gitignore / .ignore / .git/info/exclude
    #[arg(
        long = "no-ignore",
        env = "REN_NO_IGNORE",
        value_parser = BoolishValueParser::new()
    )]
    no_ignore: bool,

    /// Recurse into subdirectories
    #[arg(
        short = 'R',
        long = "recursive",
        env = "REN_RECURSIVE",
        value_parser = BoolishValueParser::new()
    )]
    recursive: bool,

    /// Admit directories into the rename plan
    #[arg(
        short = 'd',
        long = "include-dirs",
        env = "REN_INCLUDE_DIRS",
        value_parser = BoolishValueParser::new()
    )]
    include_dirs: bool,

    /// Include file extension in matching and replacement
    ///
    /// By default the pipeline runs on the file stem only and the extension
    /// (per `Path::extension`) is reattached unchanged - this prevents
    /// accidents like `ren txt notes` rewriting `report.txt` to
    /// `report.notes`. `-x` opts back into matching the full basename.
    #[arg(
        short = 'x',
        long = "include-extension",
        env = "REN_INCLUDE_EXTENSION",
        value_parser = BoolishValueParser::new()
    )]
    include_extension: bool,

    /// Match the file extension only
    ///
    /// The pipeline runs on the extension only; the stem is preserved
    /// verbatim. Files without an extension are skipped.
    #[arg(
        short = 'X',
        long = "only-extension",
        env = "REN_ONLY_EXTENSION",
        value_parser = BoolishValueParser::new()
    )]
    only_extension: bool,

    /// Greedy matching
    #[arg(
        short = 'G',
        long = "greedy",
        env = "REN_GREEDY",
        value_parser = BoolishValueParser::new()
    )]
    greedy: bool,

    /// Case-insensitive
    #[arg(
        short = 'i',
        long = "ignore-case",
        env = "REN_IGNORE_CASE",
        value_parser = BoolishValueParser::new()
    )]
    ignore_case: bool,

    /// Use regex
    #[arg(
        short = 'r',
        long = "regex",
        alias = "regexp",
        env = "REN_REGEX",
        value_parser = BoolishValueParser::new()
    )]
    regexp: bool,

    /// Preserve-case replacement
    #[arg(
        short = 'S',
        long = "smart",
        env = "REN_SMART",
        value_parser = BoolishValueParser::new()
    )]
    smart: bool,

    /// Lowercase the name
    #[arg(
        short = 'L',
        long = "lower",
        env = "REN_LOWER",
        value_parser = BoolishValueParser::new()
    )]
    lower: bool,

    /// Uppercase the name
    #[arg(
        short = 'U',
        long = "upper",
        env = "REN_UPPER",
        value_parser = BoolishValueParser::new()
    )]
    upper: bool,

    /// Prepend a literal string or template to each name
    ///
    /// Templates: `{n}` substitutes a 1-based per-parent-directory counter;
    /// `{n:0WIDTH}` zero-pads to `WIDTH` digits; `{N}` zero-pads to a smart
    /// per-parent width. `--prepend` and `--append` share the same counter.
    #[arg(short = 'P', long = "prepend", value_name = "FMT", env = "REN_PREPEND")]
    prepend: Option<String>,

    /// Append a literal string or template to each name
    ///
    /// See `--prepend` for the templating DSL.
    #[arg(short = 'A', long = "append", value_name = "FMT", env = "REN_APPEND")]
    append: Option<String>,

    /// Find replace expression
    #[arg(short = 'e', long = "expression", value_name = "<find> <replace>")]
    expressions: Vec<String>,

    /// Whole words only
    #[arg(
        short = 'w',
        long = "word-regexp",
        env = "REN_WORD_REGEXP",
        value_parser = BoolishValueParser::new()
    )]
    word_regexp: bool,

    /// Print matching file paths
    #[arg(short = 'l', long = "list-files")]
    list_files: bool,

    /// Dry run (default)
    ///
    /// Print the planned renames without touching the filesystem. This is the
    /// default mode - pass `--write` (or `-W`) to actually rename. The flag
    /// remains for shell scripts / config that want to be explicit.
    #[arg(
        short = 'n',
        long = "dry-run",
        alias = "dry",
        env = "REN_DRY_RUN",
        value_parser = BoolishValueParser::new()
    )]
    dry_run: bool,

    /// Apply renames to disk
    ///
    /// Opts into the actual rename. Without this flag, ren prints the plan and
    /// exits. `-y` is a hidden short alias.
    #[arg(
        short = 'W',
        short_alias = 'y',
        long = "write",
        env = "REN_WRITE",
        value_parser = BoolishValueParser::new()
    )]
    write: bool,

    /// Interactive preview
    #[arg(
        short = 'p',
        long = "preview",
        env = "REN_PREVIEW",
        value_parser = BoolishValueParser::new()
    )]
    preview: bool,

    /// Create missing parent directories for rename targets.
    #[arg(
        long = "create-dirs",
        env = "REN_CREATE_DIRS",
        value_parser = BoolishValueParser::new()
    )]
    create_dirs: bool,

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
  {red}-0{reset}, {red}--null{reset}                Read NUL-separated filenames from stdin

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
  {red}-d{reset}, {red}--include-dirs{reset}        Also rename directories
  {red}-x{reset}, {red}--include-extension{reset}   Include extension in matching and replacement
  {red}-X{reset}, {red}--only-extension{reset}      Match the file extension only

{yellow}{bold}Transforms{reset}

  {red}-L{reset}, {red}--lower{reset}               Lowercase names
  {red}-U{reset}, {red}--upper{reset}               Uppercase names

  {red}-P{reset}, {red}--prepend {dim}<fmt>{reset}       Prepend a string or template
  {red}-A{reset}, {red}--append {dim}<fmt>{reset}        Append a string or template

{yellow}{bold}Behavior{reset}

      {red}--create-dirs{reset}         Create missing parent directories

  {red}-l{reset}, {red}--list-files{reset}          Print matching file paths (no rename)

  {red}-n{reset}, {red}--dry-run{reset}             Show what would be changed without renaming {grey}(default){reset}
  {red}-W{reset}, {red}--write{reset}               Apply renames to disk
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

  {grey}# List every .go file (no rename){reset}
  {green}${reset} ren -f go -l

  {grey}# Stem-only matching is the default (api_v1.json → api_v2.json){reset}
  {green}${reset} ren v1 v2

  {grey}# Include extensions in matching and replacement{reset}
  {green}${reset} ren -x rs txt

  {grey}# Match only the extension; preserve the stem (foo.rs → foo.txt){reset}
  {green}${reset} ren -X rs txt

  {grey}# Smart-rename across case variants:{reset}
  {grey}#  \"foo_bar\" → \"hello_world\", \"FooBar\" → \"HelloWorld\", etc.{reset}
  {green}${reset} ren --smart foo_bar hello_world

  {grey}# Regex rename: replace test_ prefix with spec_{reset}
  {green}${reset} ren --regex '^test_' 'spec_'

  {grey}# Plan is shown by default; pass -W to actually rename{reset}
  {green}${reset} ren -W foo bar

  {grey}# Interactive preview, applying only the accepted entries{reset}
  {green}${reset} ren --preview foo bar

  {grey}# Apply multiple replacements in a single pass{reset}
  {green}${reset} ren -e foo bar -e baz qux

  {grey}# Read filenames from a pipeline (auto-detected when no path is given){reset}
  {green}${reset} fd -t f '\\.tmp$' | ren .tmp .bak

  {grey}# NUL-separated for paths with newlines or odd characters{reset}
  {green}${reset} fd -t f -0 | ren -0 foo bar

  {grey}# Move into new subdirectories, creating parents as needed{reset}
  {green}${reset} ren --create-dirs --regex '^(.*)\\.log$' 'logs/$1.log'

  {grey}# Number all files: 01_foo.txt, 02_bar.txt, ... (smart per-dir width){reset}
  {green}${reset} ren --prepend '{{N}}_'

  {grey}# Custom counter format with explicit zero-padding{reset}
  {green}${reset} ren --prepend='{{n:03}}_'

  {grey}# Counter as a suffix on the stem (foo.txt → foo-1.txt, ...){reset}
  {green}${reset} ren --append '-{{n}}'

  {grey}# Lowercase every name in cwd{reset}
  {green}${reset} ren --lower

  {grey}# Compose: find/replace → lower → append → prepend (fixed order){reset}
  {grey}# (use -e for find/replace when combining with a transform){reset}
  {green}${reset} ren --prepend '{{N}}_' --lower -e foo bar
"
    );
    print!("{text}");
}

impl Cli {
    fn uses_expressions(&self) -> bool {
        !self.expressions.is_empty()
    }

    /// True when the CLI takes only `<find>` (no `<replace>`).
    ///
    /// `-l`/`--list-files` without `-e` consumes at most `<find>`; the find
    /// pattern is optional, and all remaining positionals are search roots.
    /// When no `<find>` is supplied, `-l` lists every file the other filters
    /// admit.
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
    /// `--append`) is set.
    fn uses_transforms(&self) -> bool {
        self.lower || self.upper || self.append.is_some() || self.prepend.is_some()
    }

    /// True when a transform is present and no other mode (expressions,
    /// list-files) claims the positionals. In this mode every positional is a
    /// path; `<find> <replace>` requires `-e` instead, because the positional
    /// shape would be ambiguous with the path list.
    fn is_transforms_only(&self) -> bool {
        self.uses_transforms() && !self.uses_expressions() && !self.is_find_only()
    }

    fn positional_skip(&self) -> usize {
        if self.uses_expressions() {
            0
        } else if self.is_find_only() {
            // `-l` consumes the first positional as `<find>` when present;
            // with no positionals it lists every admitted file.
            self.args.len().min(1)
        } else if self.is_transforms_only() {
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

/// Enforce the `config < env < CLI` precedence policy across mutually
/// exclusive flags. The winner of each group is the highest-priority "true"
/// flag (CLI > shell env > config-derived env); the losers are cleared in
/// the resolved `Cli` so dispatch logic only sees one active flag per group.
/// Returns an error when two flags in the same group both come from the
/// same source tier (two CLI flags, two shell env vars, or two config
/// entries) - the genuine ambiguity cases.
fn resolve_mutex_groups(
    cli: &mut Cli,
    matches: &ArgMatches,
    origin: &config::Origin,
) -> Result<()> {
    // Mode group: exactly one of dry-run / write / preview / list-files wins.
    // Same-tier collisions (two CLI flags, two env vars, etc.) become errors;
    // cross-tier resolves by precedence so a CLI flag always beats env/config.
    let mode = resolve_group(
        matches,
        origin,
        &["dry_run", "write", "preview", "list_files"],
    )?;
    cli.dry_run = mode == Some("dry_run");
    cli.write = mode == Some("write");
    cli.preview = mode == Some("preview");
    cli.list_files = mode == Some("list_files");

    let case = resolve_group(matches, origin, &["lower", "upper"])?;
    cli.lower = case == Some("lower");
    cli.upper = case == Some("upper");

    let ext = resolve_group(matches, origin, &["include_extension", "only_extension"])?;
    cli.include_extension = ext == Some("include_extension");
    cli.only_extension = ext == Some("only_extension");

    // Empty-string values for `Option<String>` env vars come through as
    // `Some("")`, which would trip `uses_transforms()` and silently switch
    // ren into transforms-only mode. Treat them as "unset" instead - matches
    // the prior `apply_env_defaults` behavior and the principle that a
    // missing template is not a request for a zero-length template.
    if cli.prepend.as_deref() == Some("") {
        cli.prepend = None;
    }
    if cli.append.as_deref() == Some("") {
        cli.append = None;
    }

    Ok(())
}

/// Pick the winner of an "at most one is true" group. Higher tier wins;
/// same-tier conflicts are errors with wording specific to the source.
fn resolve_group<'a>(
    matches: &ArgMatches,
    origin: &config::Origin,
    ids: &[&'a str],
) -> Result<Option<&'a str>> {
    let mut by_tier: [Vec<&'a str>; Tier::COUNT] = std::array::from_fn(|_| Vec::new());
    for id in ids {
        if !matches.get_flag(id) {
            continue;
        }
        if let Some(tier) = tier_of(id, matches, origin) {
            by_tier[tier.index()].push(*id);
        }
    }
    // Walk tiers high-to-low: the highest-priority tier with any active
    // flag determines the winner. Same-tier ties become source-aware errors.
    for tier in [Tier::Cli, Tier::ShellEnv, Tier::Config] {
        let ids_in_tier = &by_tier[tier.index()];
        if ids_in_tier.len() > 1 {
            bail!(same_tier_error(tier, ids_in_tier));
        }
        if let Some(id) = ids_in_tier.first() {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// Source tier for precedence resolution. Higher discriminant = higher
/// priority. The explicit `index` method (rather than `as usize` casts at
/// call sites) gives the compiler a chance to enforce exhaustiveness if a
/// variant is added.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    Config,
    ShellEnv,
    Cli,
}

impl Tier {
    const COUNT: usize = 3;

    const fn index(self) -> usize {
        match self {
            Self::Config => 0,
            Self::ShellEnv => 1,
            Self::Cli => 2,
        }
    }
}

fn tier_of(id: &str, matches: &ArgMatches, origin: &config::Origin) -> Option<Tier> {
    match matches.value_source(id) {
        Some(ValueSource::CommandLine) => Some(Tier::Cli),
        Some(ValueSource::EnvVariable) => {
            let env_name = arg_env_name(id)?;
            if origin.is_config_derived(env_name) {
                Some(Tier::Config)
            } else {
                Some(Tier::ShellEnv)
            }
        }
        _ => None,
    }
}

fn same_tier_error(tier: Tier, ids: &[&str]) -> String {
    let names: Vec<String> = ids.iter().map(|id| (*id).replace('_', "-")).collect();
    match tier {
        Tier::Cli => format!(
            "the following flags cannot be used together: {}",
            names
                .iter()
                .map(|n| format!("--{n}"))
                .collect::<Vec<_>>()
                .join(" / ")
        ),
        Tier::ShellEnv => format!(
            "conflicting environment variables: {}",
            ids.iter()
                .map(|id| arg_env_name(id)
                    .map_or_else(|| format!("REN_{}", id.to_ascii_uppercase()), str::to_owned))
                .collect::<Vec<_>>()
                .join(" / ")
        ),
        Tier::Config => format!(
            "config sets conflicting keys: {}",
            names
                .iter()
                .map(|n| format!("`{n}`"))
                .collect::<Vec<_>>()
                .join(" / ")
        ),
    }
}

/// Map a clap arg id to its declared `REN_*` env var name. Returns `None`
/// for ids without an `env = ...` attribute - those can't be config-derived.
fn arg_env_name(id: &str) -> Option<&'static str> {
    Some(match id {
        "hidden" => "REN_HIDDEN",
        "no_ignore" => "REN_NO_IGNORE",
        "recursive" => "REN_RECURSIVE",
        "include_dirs" => "REN_INCLUDE_DIRS",
        "include_extension" => "REN_INCLUDE_EXTENSION",
        "only_extension" => "REN_ONLY_EXTENSION",
        "greedy" => "REN_GREEDY",
        "ignore_case" => "REN_IGNORE_CASE",
        "regexp" => "REN_REGEX",
        "word_regexp" => "REN_WORD_REGEXP",
        "smart" => "REN_SMART",
        "lower" => "REN_LOWER",
        "upper" => "REN_UPPER",
        "prepend" => "REN_PREPEND",
        "append" => "REN_APPEND",
        "dry_run" => "REN_DRY_RUN",
        "write" => "REN_WRITE",
        "preview" => "REN_PREVIEW",
        "create_dirs" => "REN_CREATE_DIRS",
        _ => return None,
    })
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
    // In transforms-only mode, every surviving positional is a path to walk -
    // NOT a find pattern. Suppress positional_find/replace so
    // `compile_expressions` produces an empty expression list and `build_plan`
    // runs the transform pipeline as the sole stage.
    let transforms_only = cli.is_transforms_only();

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

/// Classify each plan entry as file or directory by stat'ing whichever side of
/// the rename currently exists. `print_summary` runs both before apply (dry
/// run, `old` exists) and after (`new` exists); rename preserves file type, so
/// either side answers the question. Symlinks count as files.
fn count_files_and_dirs(plan: &[PlanEntry]) -> (usize, usize) {
    let mut files = 0;
    let mut dirs = 0;
    for entry in plan {
        let is_dir = entry
            .new
            .symlink_metadata()
            .or_else(|_| entry.old.symlink_metadata())
            .map(|m| m.file_type().is_dir())
            .unwrap_or(false);
        if is_dir {
            dirs += 1;
        } else {
            files += 1;
        }
    }
    (files, dirs)
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
        let (files, dirs) = count_files_and_dirs(plan);
        let msg = summary_message(files, dirs, dry);
        if stdout_tty {
            let color = if dry { "\x1b[33m" } else { "\x1b[32m" };
            println!("\n\x1b[1m{color}{msg}\x1b[m");
        } else {
            println!("\n{msg}");
        }
    }
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
    for entry in plan {
        let Some(parent) = entry.new.parent() else {
            continue;
        };
        if parent.as_os_str().is_empty() {
            continue;
        }
        // `metadata` follows symlinks, so a symlink-to-dir counts as a dir
        // (correct) and a symlink-to-file is reported as the non-dir it
        // resolves to (also correct). A broken symlink or missing path falls
        // through to `create_dir_all`, which produces its own error.
        match std::fs::metadata(parent) {
            Ok(m) if m.is_dir() => continue,
            Ok(_) => {
                bail!(
                    "cannot create parent directory {}: a non-directory file already exists at that path (rename target: {})",
                    parent.display(),
                    entry.new.display(),
                );
            }
            Err(_) => {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create parent directory: {}", parent.display()))?;
            }
        }
    }
    Ok(())
}

fn print_error(err: &anyhow::Error) {
    // `{err:#}` walks the anyhow context chain, joining layers with `: ` so the
    // underlying io::Error (or other source) reaches the user instead of being
    // swallowed by the outermost context. Without this the user sees only the
    // top frame, which is often just a generic "X failed" wrapper.
    if std::io::stderr().is_terminal() {
        eprintln!("\x1b[1;31merror:\x1b[m {err:#}");
    } else {
        eprintln!("error: {err:#}");
    }
}

fn main() {
    if let Err(err) = run() {
        print_error(&err);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let argv: Vec<_> = std::env::args().collect();
    let cfg_origin = config::load_into_env();
    let matches = Cli::command().get_matches_from(preprocess_expression_args(argv));
    let mut cli = Cli::from_arg_matches(&matches).map_err(|e| anyhow::anyhow!(e))?;
    resolve_mutex_groups(&mut cli, &matches, &cfg_origin)?;
    // Clear config-synthesized env so spawned subprocesses inherit only the
    // user's real shell env.
    cfg_origin.unset_synthesized();

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

    let no_positional_paths = cli.args.len() <= cli.positional_skip();
    let from_stdin = no_positional_paths && io::stdin_has_input();

    if cli.null && !from_stdin {
        bail!("--null requires reading filenames from stdin");
    }

    // Validate paths exist (only when walking the filesystem).
    if !from_stdin {
        for dir in &cli.dirs() {
            if !std::path::Path::new(dir).exists() {
                bail!("{dir}: no such file or directory");
            }
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

    let records = if from_stdin {
        io::records_from_paths(io::read_paths_from_stdin(cli.null)?)
    } else {
        scan::walk_paths(
            cli.dirs(),
            cli.file_set(),
            cli.hidden,
            cli.no_ignore,
            cli.recursive,
            cli.include_dirs,
        )
    };

    if cli.list_files {
        // Iterate records, print each whose basename matches at least one
        // expression's regex. With no expressions (no `<find>` and no `-e`),
        // every admitted record is printed - the other filters (`-f`, paths,
        // `-R`, ...) already narrowed the set. `walk_paths` returns
        // natord-sorted results, so no further sorting needed.
        for record in &records {
            let Some(basename) = record.path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if exprs.is_empty() || exprs.iter().any(|e| e.regex.is_match(basename)) {
                println!("{}", display_path(&record.path));
            }
        }
        return Ok(());
    }

    let scope = if cli.only_extension {
        ExtensionScope::Only
    } else if cli.include_extension {
        ExtensionScope::Include
    } else {
        ExtensionScope::Exclude
    };
    let plan = build_plan(&records, &exprs, scope, &transforms_opts);
    validate_plan(&plan)?;

    if plan.is_empty() {
        return Ok(());
    }

    if cli.preview {
        let mut patcher = preview::PreviewPatcher::new();
        let accepted = patcher.prompt_plan(&plan)?;
        if accepted.is_empty() {
            return Ok(());
        }
        if cli.create_dirs {
            create_missing_parents(&accepted)?;
        }
        apply_plan(&accepted)?;
        print_summary(&accepted, false);
        return Ok(());
    }

    if !cli.write {
        print_summary(&plan, true);
        return Ok(());
    }

    if cli.create_dirs {
        create_missing_parents(&plan)?;
    }
    apply_plan(&plan)?;
    print_summary(&plan, false);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_env::{EnvGuard, lock_for_parse};

    fn parse_cli(args: &[&str]) -> Cli {
        let _lock = lock_for_parse();
        let processed = preprocess_expression_args(args.iter().map(|s| s.to_string()).collect());
        Cli::parse_from(processed)
    }

    /// Resolver helper for env-aware tests. The caller must hold an
    /// [`EnvGuard`] for the duration of this call - the matches read env
    /// state and would race with a concurrent mutator.
    fn resolve_with_origin(args: &[&str], origin: &config::Origin) -> Result<Cli> {
        let processed = preprocess_expression_args(args.iter().map(|s| (*s).to_string()).collect());
        let matches = Cli::command().try_get_matches_from(processed)?;
        let mut cli = Cli::from_arg_matches(&matches).map_err(|e| anyhow::anyhow!(e))?;
        resolve_mutex_groups(&mut cli, &matches, origin)?;
        Ok(cli)
    }

    /// Resolver helper for tests that DON'T mutate env. Acquires
    /// [`ENV_MUTEX`] and scrubs ambient `REN_*` vars internally so that
    /// concurrent env-mutating tests (which take the same lock via
    /// `EnvGuard`) can't leak state into clap's parse. The mutex is held
    /// only for the parse + resolve; reads of the returned `Cli` are safe.
    fn parse_and_resolve(args: &[&str]) -> Result<Cli> {
        let _lock = lock_for_parse();
        resolve_with_origin(args, &config::Origin::default())
    }

    /// Build a config `Origin` claiming the given env var names are
    /// config-derived. Used to simulate `apply_to_env` having projected
    /// values onto the environment without actually loading a config file.
    fn fake_config_origin(keys: &'static [&'static str]) -> config::Origin {
        let mut origin = config::Origin::default();
        for k in keys {
            origin.mark_as_config_derived(k);
        }
        origin
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
        assert!(!parse_cli(&["ren", "a", "b"]).is_regex());
        assert!(parse_cli(&["ren", "-r", "a", "b"]).is_regex());
        assert!(parse_cli(&["ren", "-i", "a", "b"]).is_regex());
        assert!(parse_cli(&["ren", "-w", "a", "b"]).is_regex());
        assert!(parse_cli(&["ren", "-G", "a", "b"]).is_regex());
        // Recursion / include-dirs do not flip is_regex.
        assert!(!parse_cli(&["ren", "-R", "a", "b"]).is_regex());
        assert!(!parse_cli(&["ren", "--include-dirs", "a", "b"]).is_regex());
    }

    // ---- positional_skip --------------------------------------------------

    #[test]
    fn test_cli_positional_skip() {
        // find+replace mode: skip 2 positional args
        assert_eq!(parse_cli(&["ren", "a", "b"]).positional_skip(), 2);
        // expression mode: no positional find/replace
        assert_eq!(parse_cli(&["ren", "-e", "a", "b"]).positional_skip(), 0);
        // -l consumes the first positional as <find> when present, 0 otherwise.
        assert_eq!(parse_cli(&["ren", "-l"]).positional_skip(), 0);
        assert_eq!(parse_cli(&["ren", "-l", "a"]).positional_skip(), 1);
        assert_eq!(parse_cli(&["ren", "-l", "a", "b"]).positional_skip(), 1);
        // -R / --include-dirs are bool flags; they don't change positional layout.
        assert_eq!(parse_cli(&["ren", "-R", "a", "b"]).positional_skip(), 2);
        assert_eq!(
            parse_cli(&["ren", "--include-dirs", "a", "b"]).positional_skip(),
            2
        );
        assert_eq!(
            parse_cli(&["ren", "-R", "--include-dirs", "a", "b"]).positional_skip(),
            2
        );
        // Transforms-only mode: every positional is a path, regardless of
        // count. Without this, `ren -U a b c` silently stole `a b` as
        // find/replace and only operated on `c`.
        assert_eq!(parse_cli(&["ren", "-U"]).positional_skip(), 0);
        assert_eq!(parse_cli(&["ren", "-U", "a"]).positional_skip(), 0);
        assert_eq!(parse_cli(&["ren", "-U", "a", "b"]).positional_skip(), 0);
        assert_eq!(
            parse_cli(&["ren", "-U", "a", "b", "c"]).positional_skip(),
            0
        );
        assert_eq!(parse_cli(&["ren", "-L", "a", "b"]).positional_skip(), 0);
        assert_eq!(
            parse_cli(&["ren", "-A", "_suffix", "a", "b"]).positional_skip(),
            0
        );
        assert_eq!(
            parse_cli(&["ren", "-P", "pfx_", "a", "b"]).positional_skip(),
            0
        );
        // Expression mode wins over transforms: `-e` already implies
        // positionals are paths.
        assert_eq!(
            parse_cli(&["ren", "-e", "a", "b", "-U", "x", "y"]).positional_skip(),
            0
        );
    }

    #[test]
    fn test_transforms_only_treats_all_positionals_as_paths() {
        // Behavior-level: `dirs()` returns every positional, not a subset
        // sliced past a phantom `<find> <replace>` pair.
        let cli = parse_cli(&["ren", "-U", "a", "b", "c"]);
        assert_eq!(cli.dirs(), vec!["a", "b", "c"]);
    }

    // ---- is_find_only -----------------------------------------------------

    #[test]
    fn test_cli_is_find_only() {
        assert!(parse_cli(&["ren", "-l", "a"]).is_find_only());
        assert!(parse_cli(&["ren", "-l", "a", "b"]).is_find_only());
        assert!(!parse_cli(&["ren", "a", "b"]).is_find_only());
        // -l with -e is expression mode, not find-only
        assert!(!parse_cli(&["ren", "-l", "-e", "a", "b"]).is_find_only());
    }

    #[test]
    fn test_list_files_mode_treats_trailing_positionals_as_paths() {
        let cli = parse_cli(&["ren", "-l", "TODO", "src", "tests"]);
        assert_eq!(cli.positional_skip(), 1);
        assert_eq!(cli.pattern(), "TODO");
        assert_eq!(
            cli.paths(),
            vec![PathBuf::from("src"), PathBuf::from("tests")]
        );
    }

    #[test]
    fn test_list_files_without_find_pattern() {
        // `ren -l` and `ren -f <glob> -l` are list-only modes with no <find>:
        // every walked record passes through.
        let bare = parse_cli(&["ren", "-l"]);
        assert_eq!(bare.positional_skip(), 0);
        assert!(bare.paths().is_empty());
        assert_eq!(bare.dirs(), vec!["."]);

        let filtered = parse_cli(&["ren", "-f", "go", "-l"]);
        assert_eq!(filtered.positional_skip(), 0);
        assert!(filtered.paths().is_empty());
        assert_eq!(filtered.files.as_deref(), Some("go"));
    }

    // ---- dirs / paths with new flags --------------------------------------

    #[test]
    fn test_cli_dirs_defaults_to_current_directory() {
        assert_eq!(parse_cli(&["ren", "a", "b"]).dirs(), vec!["."]);
    }

    #[test]
    fn test_cli_dirs_uses_trailing_positionals() {
        let cli = parse_cli(&["ren", "a", "b", "src", "tests"]);
        assert_eq!(cli.dirs(), vec!["src", "tests"]);
    }

    #[test]
    fn test_cli_paths_skips_find_and_replace() {
        let cli = parse_cli(&["ren", "a", "b", "src", "tests"]);
        assert_eq!(
            cli.paths(),
            vec![PathBuf::from("src"), PathBuf::from("tests")]
        );
    }

    #[test]
    fn test_cli_dirs_with_recursive_flag_unchanged() {
        // `-R` is a boolean - it must not consume a positional.
        let cli = parse_cli(&["ren", "-R", "a", "b", "src"]);
        assert_eq!(cli.dirs(), vec!["src"]);
        assert_eq!(cli.paths(), vec![PathBuf::from("src")]);
        assert!(cli.recursive);
    }

    #[test]
    fn test_cli_dirs_with_include_dirs_flag_unchanged() {
        let cli = parse_cli(&["ren", "--include-dirs", "a", "b", "src"]);
        assert_eq!(cli.dirs(), vec!["src"]);
        assert_eq!(cli.paths(), vec![PathBuf::from("src")]);
        assert!(cli.include_dirs);
    }

    #[test]
    fn test_transform_short_flags_parse() {
        let lower = parse_cli(&["ren", "-L"]);
        assert!(lower.lower);
        assert!(!lower.upper);

        let upper = parse_cli(&["ren", "-U"]);
        assert!(upper.upper);
        assert!(!upper.lower);
    }

    #[test]
    fn test_prepend_and_append_short_flags_parse() {
        // The append value here intentionally starts with `-` to lock in
        // that the `=`-attached form is the way to pass dashed templates;
        // the bare `-A -{n}` form trips clap's flag-vs-value heuristic.
        let cli = parse_cli(&["ren", "-P", "{N}_", "-A=-{n}"]);
        assert_eq!(cli.prepend.as_deref(), Some("{N}_"));
        assert_eq!(cli.append.as_deref(), Some("-{n}"));
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
        let _g = EnvGuard::set(&[
            ("REN_HIDDEN", "1"),
            ("REN_NO_IGNORE", "true"),
            ("REN_RECURSIVE", "1"),
            ("REN_INCLUDE_DIRS", "true"),
            ("REN_INCLUDE_EXTENSION", "1"),
            ("REN_SMART", "TRUE"),
            ("REN_IGNORE_CASE", "1"),
            ("REN_GREEDY", "1"),
            ("REN_REGEX", "1"),
            ("REN_WORD_REGEXP", "1"),
            ("REN_DRY_RUN", "1"),
            ("REN_CREATE_DIRS", "true"),
        ]);
        let cli = resolve_with_origin(&["ren", "foo", "bar"], &config::Origin::default()).unwrap();
        assert!(cli.hidden);
        assert!(cli.no_ignore);
        assert!(cli.recursive);
        assert!(cli.include_dirs);
        assert!(cli.include_extension);
        // `include_extension` and `only_extension` are mutex: setting both via
        // env would error. This run sets only `include_extension`; the dedicated
        // mutex test covers the opposite.
        assert!(!cli.only_extension);
        assert!(cli.smart);
        assert!(cli.ignore_case);
        assert!(cli.greedy);
        assert!(cli.regexp);
        assert!(cli.word_regexp);
        // Only one of the mode flags (dry-run/write/preview/list-files) can be
        // set at a time. REN_PREVIEW would collide with REN_DRY_RUN at the
        // same env tier - the dedicated mode-mutex tests cover that case.
        assert!(cli.dry_run);
        assert!(cli.create_dirs);
    }

    #[test]
    fn test_env_only_extension_branch_of_mutex_group() {
        // Counterpart of `test_env_defaults_enable_boolean_flags`, which
        // exercises the `include_extension` side of the same mutex pair.
        let _g = EnvGuard::set(&[("REN_ONLY_EXTENSION", "1")]);
        let cli = resolve_with_origin(&["ren", "rs", "txt"], &config::Origin::default()).unwrap();
        assert!(cli.only_extension);
        assert!(!cli.include_extension);
    }

    #[test]
    fn test_env_prepend_and_append_string_values() {
        let _g = EnvGuard::set(&[("REN_PREPEND", "{N}_"), ("REN_APPEND", "-{n}")]);
        let cli = resolve_with_origin(&["ren"], &config::Origin::default()).unwrap();
        assert_eq!(cli.prepend.as_deref(), Some("{N}_"));
        assert_eq!(cli.append.as_deref(), Some("-{n}"));
    }

    #[test]
    fn test_env_empty_string_for_optional_strings_is_unset() {
        // `Option<String>` env vars must collapse `Some("")` to `None`, else
        // `uses_transforms()` flips ren into transforms-only mode and the next
        // positional gets misparsed as a path. Pin the behavior here.
        let _g = EnvGuard::set(&[("REN_PREPEND", ""), ("REN_APPEND", "")]);
        let cli = resolve_with_origin(&["ren", "foo", "bar"], &config::Origin::default()).unwrap();
        assert!(cli.prepend.is_none());
        assert!(cli.append.is_none());
        assert!(!cli.uses_transforms());
    }

    #[test]
    fn test_env_defaults_falsy_values_are_ignored() {
        // `BoolishValueParser` accepts "0"/"false" (case-insensitive) as
        // falsy. Setting these via env is equivalent to leaving the flag
        // unset on the CLI - the boolean stays `false`.
        let _g = EnvGuard::set(&[
            ("REN_HIDDEN", "0"),
            ("REN_RECURSIVE", "false"),
            ("REN_INCLUDE_DIRS", "FALSE"),
            ("REN_SMART", "false"),
        ]);
        let cli = resolve_with_origin(&["ren", "foo", "bar"], &config::Origin::default()).unwrap();
        assert!(!cli.hidden);
        assert!(!cli.recursive);
        assert!(!cli.include_dirs);
        assert!(!cli.smart);
    }

    #[test]
    fn test_env_empty_string_for_bool_is_rejected() {
        // `BoolishValueParser` only accepts truthy/falsy tokens; an empty
        // string is neither and parses fail. This catches typos in `.env`
        // files where a variable is mentioned without a value.
        let _g = EnvGuard::set(&[("REN_INCLUDE_DIRS", "")]);
        let err = resolve_with_origin(&["ren", "foo", "bar"], &config::Origin::default())
            .err()
            .expect("empty bool should fail to parse");
        assert!(
            err.to_string()
                .to_ascii_lowercase()
                .contains("include-dirs"),
            "error should name the offending flag, got: {err}"
        );
    }

    #[test]
    fn test_cli_write_beats_shell_env_dry_run() {
        // Mode-group precedence: CLI `--write` beats a shell-env REN_DRY_RUN.
        let _g = EnvGuard::set(&[("REN_DRY_RUN", "1")]);
        let cli =
            resolve_with_origin(&["ren", "-W", "foo", "bar"], &config::Origin::default()).unwrap();
        assert!(cli.write);
        assert!(!cli.dry_run);
    }

    #[test]
    fn test_write_and_dry_run_cli_conflict_errors() {
        // The mode group makes write/dry-run/preview/list-files exclusive on
        // the CLI tier; same-tier collisions error with both flag names.
        let err = parse_and_resolve(&["ren", "--write", "--dry-run", "foo", "bar"])
            .err()
            .expect("CLI write/dry-run conflict expected");
        let msg = err.to_string();
        assert!(
            msg.contains("--write") && msg.contains("--dry-run"),
            "expected CLI mutex error naming both flags, got: {msg}"
        );
    }

    #[test]
    fn test_write_and_preview_cli_conflict_errors() {
        let err = parse_and_resolve(&["ren", "--write", "--preview", "foo", "bar"])
            .err()
            .expect("CLI write/preview conflict expected");
        let msg = err.to_string();
        assert!(
            msg.contains("--write") && msg.contains("--preview"),
            "expected CLI mutex error naming both flags, got: {msg}"
        );
    }

    #[test]
    fn test_write_short_alias_y_parses() {
        // `-y` is a hidden short alias for `-W` (kept off the help screen).
        let cli = parse_cli(&["ren", "-y", "foo", "bar"]);
        assert!(cli.write);
    }

    // ---- mutex resolver --------------------------------------------------

    #[test]
    fn test_lower_and_upper_are_mutex_on_cli() {
        let err = parse_and_resolve(&["ren", "--lower", "--upper", "foo", "bar"])
            .err()
            .expect("CLI lower/upper conflict expected");
        assert!(
            err.to_string().contains("--lower") && err.to_string().contains("--upper"),
            "expected CLI mutex error, got: {err}"
        );
    }

    #[test]
    fn test_include_and_only_extension_are_mutex_on_cli() {
        let err = parse_and_resolve(&[
            "ren",
            "--include-extension",
            "--only-extension",
            "foo",
            "bar",
        ])
        .err()
        .expect("CLI ext conflict expected");
        assert!(
            err.to_string().contains("--include-extension")
                && err.to_string().contains("--only-extension"),
            "expected CLI mutex error, got: {err}"
        );
    }

    #[test]
    fn test_cli_lower_beats_shell_env_upper() {
        let _g = EnvGuard::set(&[("REN_UPPER", "true")]);
        let cli = resolve_with_origin(
            &["ren", "--lower", "foo", "bar"],
            &config::Origin::default(),
        )
        .unwrap();
        assert!(cli.lower);
        assert!(!cli.upper);
    }

    #[test]
    fn test_shell_env_beats_config_in_mutex_group() {
        // Config sets lower=true (synthesized REN_LOWER), shell sets
        // REN_UPPER=true. Shell wins over config.
        let _g = EnvGuard::set(&[("REN_LOWER", "true"), ("REN_UPPER", "true")]);
        let origin = fake_config_origin(&["REN_LOWER"]);
        let cli = resolve_with_origin(&["ren", "foo", "bar"], &origin).unwrap();
        assert!(
            cli.upper,
            "shell REN_UPPER must beat config-derived REN_LOWER"
        );
        assert!(!cli.lower);
    }

    #[test]
    fn test_two_shell_env_vars_in_one_group_errors() {
        let _g = EnvGuard::set(&[("REN_LOWER", "true"), ("REN_UPPER", "true")]);
        let err = resolve_with_origin(&["ren", "foo", "bar"], &config::Origin::default())
            .err()
            .expect("expected resolver error")
            .to_string();
        assert!(
            err.contains("environment variables"),
            "expected env-conflict wording, got: {err}"
        );
    }

    #[test]
    fn test_two_config_keys_in_one_group_errors() {
        let _g = EnvGuard::set(&[("REN_LOWER", "true"), ("REN_UPPER", "true")]);
        let origin = fake_config_origin(&["REN_LOWER", "REN_UPPER"]);
        let err = resolve_with_origin(&["ren", "foo", "bar"], &origin)
            .err()
            .expect("expected resolver error")
            .to_string();
        assert!(
            err.contains("config sets"),
            "expected config-conflict wording, got: {err}"
        );
    }

    #[test]
    fn test_cli_only_extension_beats_shell_env_include_extension() {
        let _g = EnvGuard::set(&[("REN_INCLUDE_EXTENSION", "true")]);
        let cli = resolve_with_origin(
            &["ren", "--only-extension", "rs", "txt"],
            &config::Origin::default(),
        )
        .unwrap();
        assert!(cli.only_extension);
        assert!(!cli.include_extension);
    }

    #[test]
    fn test_shell_env_beats_config_in_extension_group() {
        // Config sets include-extension; shell sets REN_ONLY_EXTENSION. Shell
        // wins. Same precedence as lower/upper, this just pins the second
        // mutex group's wiring.
        let _g = EnvGuard::set(&[
            ("REN_INCLUDE_EXTENSION", "true"),
            ("REN_ONLY_EXTENSION", "true"),
        ]);
        let origin = fake_config_origin(&["REN_INCLUDE_EXTENSION"]);
        let cli = resolve_with_origin(&["ren", "rs", "txt"], &origin).unwrap();
        assert!(
            cli.only_extension,
            "shell REN_ONLY_EXTENSION must beat config-derived REN_INCLUDE_EXTENSION"
        );
        assert!(!cli.include_extension);
    }

    #[test]
    fn test_two_shell_env_vars_in_extension_group_errors() {
        let _g = EnvGuard::set(&[
            ("REN_INCLUDE_EXTENSION", "true"),
            ("REN_ONLY_EXTENSION", "true"),
        ]);
        let err = resolve_with_origin(&["ren", "rs", "txt"], &config::Origin::default())
            .err()
            .expect("expected resolver error")
            .to_string();
        assert!(
            err.contains("environment variables"),
            "expected env-conflict wording, got: {err}"
        );
    }

    #[test]
    fn test_arg_env_name_matches_clap_spec() {
        // The hardcoded `arg_env_name` map mirrors the `env = ...` attributes
        // on the `Cli` struct. If a new env-backed flag is added without
        // updating the map, source-aware resolution and config-tier error
        // wording will silently misclassify it.
        let _lock = lock_for_parse();
        let cmd = Cli::command();
        for arg in cmd.get_arguments() {
            if let Some(declared) = arg.get_env() {
                let id = arg.get_id().as_str();
                let mapped = arg_env_name(id);
                assert_eq!(
                    mapped,
                    Some(declared.to_str().expect("env name is UTF-8")),
                    "arg_env_name({id}) does not match clap's env attribute",
                );
            }
        }
    }

    #[test]
    fn test_cli_flag_wins_over_env_for_booleans() {
        // Explicit `-R` set on CLI beats `REN_RECURSIVE=0` from env (clap's
        // ValueSource::CommandLine wins over ValueSource::EnvVariable).
        let _g = EnvGuard::set(&[("REN_RECURSIVE", "0")]);
        let cli =
            resolve_with_origin(&["ren", "-R", "foo", "bar"], &config::Origin::default()).unwrap();
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

    // ---- create_missing_parents ------------------------------------------

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
