//! Command-line syntax and source precedence. Runtime receives resolved options.
use crate::expressions::{CompileOptions, EXPR_SEP};
use crate::options::{Mode, Options};
use crate::rename::ExtensionScope;
use crate::{config, scan, transforms};
use anyhow::{Context as _, Result, bail};
use clap::builder::BoolishValueParser;
use clap::parser::ValueSource;
use clap::{ArgMatches, CommandFactory as _, FromArgMatches as _, Parser};
use clap_complete::Shell;
use std::ffi::{OsStr, OsString};
use std::io::Write;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

pub(crate) enum Action {
    Run(Box<Options>),
    Exit(i32),
}

pub(crate) fn parse(
    argv: Vec<OsString>,
    environment: &config::Environment,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    colored: bool,
) -> Result<Action> {
    let config =
        config::Config::load(environment, stderr).map_err(crate::output::diagnostic_error)?;
    let (mut cli, matches, origin) = match parse_with_config(argv, environment, &config) {
        Ok(parsed) => parsed,
        Err(error) => {
            let written = if error.use_stderr() {
                write!(stderr, "{error}")
            } else {
                write!(stdout, "{error}")
            };
            // Failed diagnostics cannot turn an invalid command into success.
            if error.exit_code() == 0 {
                written?;
            }
            return Ok(Action::Exit(error.exit_code()));
        }
    };
    resolve_mutex_groups(&mut cli, &matches, &origin)?;
    if let Some(shell) = cli.completions {
        // clap_complete assumes writes succeed. Render in memory, then forward
        // through the same fallible writer as all other command output.
        let mut rendered = Vec::new();
        clap_complete::generate(shell, &mut Cli::command(), "ren", &mut rendered);
        stdout.write_all(&rendered)?;
        return Ok(Action::Exit(0));
    }
    if cli.help_long || cli.help {
        stdout.write_all(
            if cli.help_long {
                help_long(colored)
            } else {
                help(colored)
            }
            .as_bytes(),
        )?;
        return Ok(Action::Exit(0));
    }
    if !cli.uses_expressions() && cli.args.is_empty() && !cli.list_files && !cli.uses_transforms() {
        drop(stdout.write_all(help(colored).as_bytes()));
        return Ok(Action::Exit(1));
    }
    if cli.positional_skip() > cli.args.len() {
        let missing = if cli.is_find_only() || cli.args.is_empty() {
            "<find>"
        } else {
            "<replace>"
        };
        drop(writeln!(
            stderr,
            "error: missing required argument: {missing}"
        ));
        drop(stdout.write_all(help(colored).as_bytes()));
        return Ok(Action::Exit(1));
    }
    Ok(Action::Run(Box::new(cli.into_options()?)))
}

fn parse_with_config(
    argv: Vec<OsString>,
    env: &config::Environment,
    config: &config::Config,
) -> clap::error::Result<(Cli, ArgMatches, config::Origin)> {
    let values = config.values();
    let mut origin = config::Origin::default();
    let mut command = Cli::command();
    let ids: Vec<_> = command
        .get_arguments()
        .map(|arg| arg.get_id().to_string())
        .collect();
    for id in ids {
        let Some(name) = arg_env_name(&id) else {
            continue;
        };
        let value = if let Some(value) = env.get(name) {
            origin
                .sources
                .insert(id.clone(), config::Source::Environment);
            Some(value.to_os_string())
        } else if let Some(value) = values.get(name) {
            origin.sources.insert(id.clone(), config::Source::Config);
            Some(value.clone())
        } else {
            None
        };
        if let Some(value) = value {
            command = command.mut_arg(&id, |arg| arg.default_value(value));
        }
    }
    let matches = command.try_get_matches_from(preprocess_expression_args(argv))?;
    let cli = Cli::from_arg_matches(&matches)?;
    // Clap's `requires` treats default values as absent. Check the resolved
    // values so config and environment sources obey the same dependency.
    if cli.parents && cli.directory.is_none() {
        return Err(Cli::command().error(
            clap::error::ErrorKind::MissingRequiredArgument,
            "the following required arguments were not provided:\n  --directory <DIR>",
        ));
    }
    Ok((cli, matches, origin))
}

#[derive(Parser)]
#[command(name = "ren", version, disable_help_flag = true)]
struct Cli {
    #[arg(value_name = "arg")]
    args: Vec<OsString>,

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
        value_parser = BoolishValueParser::new()
    )]
    hidden: bool,

    /// Ignore .gitignore / .ignore / .git/info/exclude
    #[arg(
        long = "no-ignore",
        value_parser = BoolishValueParser::new()
    )]
    no_ignore: bool,

    /// Recurse into subdirectories
    #[arg(
        short = 'R',
        long = "recursive",
        value_parser = BoolishValueParser::new()
    )]
    recursive: bool,

    /// Admit directories into the rename plan
    #[arg(
        short = 'D',
        long = "include-dirs",
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
        value_parser = BoolishValueParser::new()
    )]
    only_extension: bool,

    /// Greedy matching
    #[arg(
        short = 'G',
        long = "greedy",
        value_parser = BoolishValueParser::new()
    )]
    greedy: bool,

    /// Case-insensitive
    #[arg(
        short = 'i',
        long = "ignore-case",
        value_parser = BoolishValueParser::new()
    )]
    ignore_case: bool,

    /// Use regex
    #[arg(
        short = 'r',
        long = "regex",
        alias = "regexp",
        value_parser = BoolishValueParser::new()
    )]
    regexp: bool,

    /// Preserve-case replacement
    #[arg(
        short = 'S',
        long = "smart",
        value_parser = BoolishValueParser::new()
    )]
    smart: bool,

    /// Lowercase the name
    #[arg(
        short = 'L',
        long = "lower",
        value_parser = BoolishValueParser::new()
    )]
    lower: bool,

    /// Uppercase the name
    #[arg(
        short = 'U',
        long = "upper",
        value_parser = BoolishValueParser::new()
    )]
    upper: bool,

    /// Prepend a literal string or template to each name
    ///
    /// Templates: `{n}` substitutes a 1-based per-parent-directory counter;
    /// `{n:0WIDTH}` zero-pads to `WIDTH` digits; `{N}` zero-pads to a smart
    /// per-parent width. `--prepend` and `--append` share the same counter.
    #[arg(short = 'P', long = "prepend", value_name = "FMT")]
    prepend: Option<String>,

    /// Append a literal string or template to each name
    ///
    /// See `--prepend` for the templating DSL.
    #[arg(short = 'A', long = "append", value_name = "FMT")]
    append: Option<String>,

    /// Replace each name with a literal string or template
    ///
    /// Unlike `--prepend`/`--append`, which add to the name, `--supplant`
    /// replaces it outright. It accepts the same counter DSL (`{n}`, `{n:0W}`,
    /// `{N}`) and runs on the same segment as the rest of the pipeline - the
    /// stem by default, so the extension is preserved: `ren --supplant '{n:02}'`
    /// renames to `01.jpg`, `02.jpg`, ….
    #[arg(short = 's', long = "supplant", value_name = "FMT")]
    supplant: Option<String>,

    /// Change, add, or strip the file extension
    ///
    /// Overrides the extension that the default (stem) pipeline reattaches, so
    /// it composes with the rest of the pipeline: `ren -s '{n:02}' -E png`
    /// renames to `01.png`, `02.png`, …. A leading dot is optional (`-E png`
    /// and `-E .png` are equivalent). Files without an extension gain one
    /// (`Makefile` → `Makefile.png`); an empty value strips it (`-E ''` turns
    /// `notes.bak` into `notes`). The whole extension is replaced, including
    /// compound ones (`archive.tar.gz` → `archive.zip`). Mutually exclusive
    /// with `-x`/`-X`.
    #[arg(short = 'E', long = "change-extension", value_name = "EXT")]
    change_extension: Option<String>,

    /// Find replace expression
    #[arg(short = 'e', long = "expression", value_name = "<find> <replace>")]
    expressions: Vec<String>,

    /// Whole words only
    #[arg(
        short = 'w',
        long = "word-regexp",
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
        value_parser = BoolishValueParser::new()
    )]
    write: bool,

    /// Interactive preview
    #[arg(
        short = 'p',
        long = "preview",
        value_parser = BoolishValueParser::new()
    )]
    preview: bool,

    /// Create missing parent directories for rename targets.
    #[arg(
        long = "create-dirs",
        value_parser = BoolishValueParser::new()
    )]
    create_dirs: bool,

    /// Relocate renamed files under this base directory.
    ///
    /// By default only the renamed basename is placed in `<dir>` (flatten), so
    /// `x.jpg` with `-d out` lands at `out/x.jpg`. A trailing slash is
    /// immaterial (`-d a/b` and `-d a/b/` behave identically). Missing parent
    /// directories are created automatically (implies `--create-dirs`). Use
    /// `--parents` to preserve the source's directory structure instead.
    #[arg(short = 'd', long = "directory", alias = "dir", value_name = "DIR")]
    directory: Option<PathBuf>,

    /// Preserve the source's parent directories under `-d <dir>`.
    ///
    /// Reproduces each file's path *relative to its walk root* under `<dir>`
    /// instead of flattening: a file found at `photos/2024/x.jpg` under root
    /// `photos` lands at `out/2024/x.jpg` rather than `out/x.jpg`. Only
    /// meaningful alongside `-d`, which it requires.
    #[arg(
        long = "parents",
        value_parser = BoolishValueParser::new()
    )]
    parents: bool,

    #[arg(long = "completions", value_name = "SHELL", hide = true)]
    completions: Option<Shell>,
}

fn help(is_tty: bool) -> String {
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

  {red}-e{reset}, {red}--expression {blue}{dim}<f> <r>{reset}  Find/replace expression
  {red}-S{reset}, {red}--smart{reset}               Replace all case variants of the pattern

{yellow}{bold}Regex{reset}

  {red}-G{reset}, {red}--greedy{reset}              Use greedy matching for regular expressions
  {red}-i{reset}, {red}--ignore-case{reset}         Case-insensitive matching
  {red}-r{reset}, {red}--regex{reset}               Treat patterns as regular expressions
  {red}-w{reset}, {red}--word-regexp{reset}         Match only whole words

{yellow}{bold}Scope{reset}

  {red}-R{reset}, {red}--recursive{reset}           Recurse into subdirectories
  {red}-D{reset}, {red}--include-dirs{reset}        Also rename directories
  {red}-x{reset}, {red}--include-extension{reset}   Include extension in matching and replacement
  {red}-X{reset}, {red}--only-extension{reset}      Match the file extension only
  {red}-E{reset}, {red}--change-extension{reset}    Change, add, or strip the file extension

{yellow}{bold}Transforms{reset}

  {red}-L{reset}, {red}--lower{reset}               Lowercase names
  {red}-U{reset}, {red}--upper{reset}               Uppercase names

  {red}-P{reset}, {red}--prepend {dim}<fmt>{reset}       Prepend a string or template
  {red}-A{reset}, {red}--append {dim}<fmt>{reset}        Append a string or template
  {red}-s{reset}, {red}--supplant {dim}<fmt>{reset}      Replace names with a string or template

{yellow}{bold}Output{reset}

  {red}-d{reset}, {red}--directory {dim}<dir>{reset}     Relocate renamed files under {red}<dir>{reset} {grey}(implies --create-dirs){reset}
      {red}--parents{reset}             Keep source parent dirs under {red}-d{reset} {grey}(else flatten){reset}
      {red}--create-dirs{reset}         Create missing parent directories

{yellow}{bold}Behavior{reset}

  {red}-l{reset}, {red}--list-files{reset}          Print matching file paths {grey}(no rename){reset}

  {red}-n{reset}, {red}--dry-run{reset}             Show what would be changed without renaming {grey}(default){reset}
  {red}-W{reset}, {red}--write{reset}               Apply renames to disk
  {red}-p{reset}, {red}--preview{reset}             Preview the changes before applying them

{yellow}{bold}Miscellaneous{reset}

  {red}-V{reset}, {red}--version{reset}             Print version

  {red}-h{reset}                        Print short help
      {red}--help{reset}                Print long help with examples
"
    );
    text
}

fn help_long(is_tty: bool) -> String {
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

  {grey}# Renumber and change extension at once (photo_*.jpg → 01.webp, …){reset}
  {green}${reset} ren -s '{{n:02}}' -E webp

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

  {grey}# Relocate matches into a base directory, flattening by default{reset}
  {grey}#  (a/b/c.txt → out/c.txt; -d implies --create-dirs){reset}
  {green}${reset} ren -R -d out foo bar

  {grey}# ...or keep the source's directory structure with --parents{reset}
  {grey}#  (a/b/c.txt → out/a/b/c.txt){reset}
  {green}${reset} ren -R -d out --parents foo bar

  {grey}# Number all files: 01_foo.txt, 02_bar.txt, … (smart per-dir width){reset}
  {green}${reset} ren --prepend '{{N}}_'

  {grey}# Custom counter format with explicit zero-padding{reset}
  {green}${reset} ren --prepend='{{n:03}}_'

  {grey}# Counter as a suffix on the stem (foo.txt → foo-1.txt, …){reset}
  {green}${reset} ren --append '-{{n}}'

  {grey}# Replace the whole name with a counter: 01.jpg, 02.jpg, …{reset}
  {green}${reset} ren --supplant '{{n:02}}'

  {grey}# Lowercase every name in cwd{reset}
  {green}${reset} ren --lower

  {grey}# Compose: find/replace → lower → append → prepend (fixed order){reset}
  {grey}# (use -e for find/replace when combining with a transform){reset}
  {green}${reset} ren --prepend '{{N}}_' --lower -e foo bar
"
    );
    format!("{}{text}", help(is_tty))
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

    /// True when any transform flag (`--lower`, `--upper`, `--prepend`,
    /// `--append`, `--supplant`, `--change-extension`) is set.
    fn uses_transforms(&self) -> bool {
        self.lower
            || self.upper
            || self.append.is_some()
            || self.prepend.is_some()
            || self.supplant.is_some()
            || self.change_extension.is_some()
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

    #[cfg(test)]
    fn dirs(&self) -> Vec<&Path> {
        let args = &self.args[self.positional_skip()..];
        if args.is_empty() {
            vec![Path::new(".")]
        } else {
            args.iter().map(Path::new).collect()
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

    fn paths(&self) -> Vec<PathBuf> {
        self.args
            .iter()
            .skip(self.positional_skip())
            .map(PathBuf::from)
            .collect()
    }

    fn pattern(&self) -> &OsStr {
        &self.args[0]
    }

    fn replacement(&self) -> &OsStr {
        &self.args[1]
    }
}

/// Enforce the `config < env < CLI` precedence policy across mutually
/// exclusive flags. The winner of each group is the highest-priority "true"
/// flag (CLI > shell env > config); the losers are cleared in
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

    // Extension scope: `-x`, `-X`, and `-E` all redefine how the extension is
    // handled, so at most one may win. `-E` carries a value rather than being a
    // bool flag, so its membership is decided by a predicate: an explicit CLI
    // `-E` (even `-E ''`, which strips) is active, but an *empty*
    // `REN_CHANGE_EXTENSION` from the environment is inert - a stray empty env
    // var must not silently rewrite every extension.
    let ext = resolve_group_with(
        matches,
        origin,
        &["include_extension", "only_extension", "change_extension"],
        |id| {
            if id == "change_extension" {
                match matches.value_source(id) {
                    Some(ValueSource::CommandLine) => true,
                    Some(ValueSource::DefaultValue) if origin.sources.contains_key(id) => {
                        matches!(origin.sources.get(id), Some(config::Source::Config))
                            || matches.get_one::<String>(id).is_some_and(|v| !v.is_empty())
                    }
                    _ => false,
                }
            } else {
                matches.get_flag(id)
            }
        },
    )?;
    cli.include_extension = ext == Some("include_extension");
    cli.only_extension = ext == Some("only_extension");
    if ext != Some("change_extension") {
        // Either `-x`/`-X` outranked it, or it was an inert empty env var.
        cli.change_extension = None;
    }

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
    if cli.supplant.as_deref() == Some("") {
        cli.supplant = None;
    }

    Ok(())
}

/// Pick the winner of an "at most one is true" group of boolean flags. Higher
/// tier wins; same-tier conflicts are errors with wording specific to the
/// source.
fn resolve_group<'a>(
    matches: &ArgMatches,
    origin: &config::Origin,
    ids: &[&'a str],
) -> Result<Option<&'a str>> {
    resolve_group_with(matches, origin, ids, |id| matches.get_flag(id))
}

/// As [`resolve_group`], but membership is decided by `is_active` rather than
/// `ArgMatches::get_flag`. This admits value-bearing options (e.g.
/// `--change-extension`) into a mutex group, where "active" means more than
/// "the flag is true" - the predicate can inspect the value and its source.
fn resolve_group_with<'a>(
    matches: &ArgMatches,
    origin: &config::Origin,
    ids: &[&'a str],
    is_active: impl Fn(&str) -> bool,
) -> Result<Option<&'a str>> {
    let mut by_tier: [Vec<&'a str>; Tier::COUNT] = std::array::from_fn(|_| Vec::new());
    for id in ids {
        if !is_active(id) {
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
        Some(ValueSource::DefaultValue) => origin.sources.get(id).map(|source| match source {
            config::Source::Config => Tier::Config,
            config::Source::Environment => Tier::ShellEnv,
        }),
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
        "supplant" => "REN_SUPPLANT",
        "change_extension" => "REN_CHANGE_EXTENSION",
        "dry_run" => "REN_DRY_RUN",
        "write" => "REN_WRITE",
        "preview" => "REN_PREVIEW",
        "create_dirs" => "REN_CREATE_DIRS",
        "directory" => "REN_DIRECTORY",
        "parents" => "REN_PARENTS",
        _ => return None,
    })
}

/// Preprocess argv so that `-e <find> <replace>` is compacted into a single
/// clap value joined by `EXPR_SEP` before clap parses the argument list.
/// This lets the second arg start with `-` without being treated as a flag.
pub(crate) fn preprocess_expression_args(args: Vec<OsString>) -> Vec<OsString> {
    let mut out = Vec::with_capacity(args.len());
    let mut iter = args.into_iter().peekable();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            out.push(arg);
            out.extend(iter);
            break;
        }
        if arg == "-e" || arg == "--expression" {
            out.push(arg);
            let Some(find) = iter.next() else { continue };
            let Some(replace) = iter.next() else {
                out.push(find);
                continue;
            };
            let mut pair = find;
            pair.push(EXPR_SEP.to_string());
            pair.push(replace);
            out.push(pair);
        } else if let Some(find) = arg
            .to_str()
            .and_then(|s| s.strip_prefix("-e"))
            .filter(|s| !s.is_empty())
        {
            // Compact form: -efoo → find="foo", next arg is replace
            out.push(OsString::from("-e"));
            let Some(replace) = iter.next() else {
                out.push(OsString::from(find));
                continue;
            };
            let mut pair = OsString::from(find);
            pair.push(EXPR_SEP.to_string());
            pair.push(replace);
            out.push(pair);
        } else {
            out.push(arg);
        }
    }
    out
}

fn compile_options_from_cli(cli: &Cli) -> Result<CompileOptions> {
    let find_only = cli.is_find_only();
    // In transforms-only mode, every surviving positional is a path to walk -
    // NOT a find pattern. Suppress positional_find/replace so
    // `compile_expressions` produces an empty expression list and `build_plan`
    // runs the transform pipeline as the sole stage.
    let transforms_only = cli.is_transforms_only();

    let positional_find = if cli.uses_expressions() || cli.args.is_empty() || transforms_only {
        None
    } else {
        Some(
            cli.pattern()
                .to_str()
                .context("find pattern must be valid UTF-8")?
                .to_owned(),
        )
    };
    let positional_replace =
        if cli.uses_expressions() || find_only || cli.args.len() < 2 || transforms_only {
            None
        } else {
            Some(
                cli.replacement()
                    .to_str()
                    .context("replacement must be valid UTF-8")?
                    .to_owned(),
            )
        };

    Ok(CompileOptions {
        regex: cli.regexp,
        ignore_case: cli.ignore_case,
        greedy: cli.greedy,
        word_regexp: cli.word_regexp,
        smart: cli.smart,
        expressions: cli.expressions.clone(),
        positional_find,
        positional_replace,
        list_files_find_only: cli.list_files && !cli.uses_expressions(),
    })
}

impl Cli {
    fn into_options(self) -> Result<Options> {
        let matching = compile_options_from_cli(&self)?;
        let paths = self.paths();
        let file_set = self.file_set();
        Ok(Options {
            mode: if self.list_files {
                Mode::List
            } else if self.preview {
                Mode::Preview
            } else if self.write {
                Mode::Write
            } else {
                Mode::DryRun
            },
            paths,
            matching,
            file_set,
            null: self.null,
            hidden: self.hidden,
            no_ignore: self.no_ignore,
            recursive: self.recursive,
            include_dirs: self.include_dirs,
            scope: if self.only_extension {
                ExtensionScope::Only
            } else if self.include_extension {
                ExtensionScope::Include
            } else {
                ExtensionScope::Exclude
            },
            transforms: transforms::TransformOptions {
                lower: self.lower,
                upper: self.upper,
                append: self.append,
                prepend: self.prepend,
                supplant: self.supplant,
            },
            change_extension: self.change_extension,
            create_dirs: self.create_dirs || self.directory.is_some(),
            directory: self.directory,
            parents: self.parents,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_cli(args: &[&str]) -> Cli {
        let argv = args.iter().map(OsString::from).collect();
        parse_with_config(
            argv,
            &config::Environment::default(),
            &config::Config::default(),
        )
        .unwrap()
        .0
    }

    fn resolve_with_origin(
        args: &[&str],
        env: &config::Environment,
        overrides: &config::Origin,
    ) -> Result<Cli> {
        let argv = args.iter().map(OsString::from).collect();
        let (mut cli, matches, mut origin) =
            parse_with_config(argv, env, &config::Config::default())?;
        origin.sources.extend(
            overrides
                .sources
                .iter()
                .map(|(key, source)| (key.clone(), *source)),
        );
        resolve_mutex_groups(&mut cli, &matches, &origin)?;
        Ok(cli)
    }

    fn parse_and_resolve(args: &[&str]) -> Result<Cli> {
        resolve_with_origin(
            args,
            &config::Environment::default(),
            &config::Origin::default(),
        )
    }

    fn fake_config_origin(keys: &[&str]) -> config::Origin {
        let mut origin = config::Origin::default();
        for arg in Cli::command().get_arguments() {
            if arg_env_name(arg.get_id().as_str()).is_some_and(|name| keys.contains(&name)) {
                origin
                    .sources
                    .insert(arg.get_id().to_string(), config::Source::Config);
            }
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
        // --supplant is a transform too: positionals are paths, never a
        // phantom find/replace pair.
        assert_eq!(
            parse_cli(&["ren", "-s", "{n:02}", "a", "b"]).positional_skip(),
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
    fn test_supplant_short_flag_parses() {
        let cli = parse_cli(&["ren", "-s", "{n:02}"]);
        assert_eq!(cli.supplant.as_deref(), Some("{n:02}"));
        assert!(cli.uses_transforms());
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
    fn test_directory_flag_parses_short_long_and_alias() {
        for argv in [
            ["ren", "-d", "foo/bar"],
            ["ren", "--directory", "foo/bar"],
            ["ren", "--dir", "foo/bar"],
        ] {
            let cli = parse_cli(&argv);
            assert_eq!(
                cli.directory.as_deref(),
                Some(std::path::Path::new("foo/bar"))
            );
        }
    }

    #[test]
    fn test_directory_short_flag_freed_from_include_dirs() {
        // `-d` now selects `--directory`, not `--include-dirs` (which moved to
        // `-D`). Pin both halves of the rebind.
        let cli = parse_cli(&["ren", "-d", "out", "a", "b"]);
        assert_eq!(cli.directory.as_deref(), Some(std::path::Path::new("out")));
        assert!(!cli.include_dirs);

        let cli = parse_cli(&["ren", "-D", "a", "b"]);
        assert!(cli.include_dirs);
        assert!(cli.directory.is_none());
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
        let env = config::Environment::from_pairs(&[
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
        let cli =
            resolve_with_origin(&["ren", "foo", "bar"], &env, &config::Origin::default()).unwrap();
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
        let env = config::Environment::from_pairs(&[("REN_ONLY_EXTENSION", "1")]);
        let cli =
            resolve_with_origin(&["ren", "rs", "txt"], &env, &config::Origin::default()).unwrap();
        assert!(cli.only_extension);
        assert!(!cli.include_extension);
    }

    #[test]
    fn test_env_prepend_and_append_string_values() {
        let env =
            config::Environment::from_pairs(&[("REN_PREPEND", "{N}_"), ("REN_APPEND", "-{n}")]);
        let cli = resolve_with_origin(&["ren"], &env, &config::Origin::default()).unwrap();
        assert_eq!(cli.prepend.as_deref(), Some("{N}_"));
        assert_eq!(cli.append.as_deref(), Some("-{n}"));
    }

    #[test]
    fn test_env_directory_string_value() {
        let env = config::Environment::from_pairs(&[("REN_DIRECTORY", "foo/bar")]);
        let cli =
            resolve_with_origin(&["ren", "foo", "bar"], &env, &config::Origin::default()).unwrap();
        assert_eq!(
            cli.directory.as_deref(),
            Some(std::path::Path::new("foo/bar"))
        );
    }

    #[test]
    fn test_env_empty_string_for_optional_strings_is_unset() {
        // `Option<String>` env vars must collapse `Some("")` to `None`, else
        // `uses_transforms()` flips ren into transforms-only mode and the next
        // positional gets misparsed as a path. Pin the behavior here.
        let env = config::Environment::from_pairs(&[("REN_PREPEND", ""), ("REN_APPEND", "")]);
        let cli =
            resolve_with_origin(&["ren", "foo", "bar"], &env, &config::Origin::default()).unwrap();
        assert!(cli.prepend.is_none());
        assert!(cli.append.is_none());
        assert!(!cli.uses_transforms());
    }

    #[test]
    fn test_env_defaults_falsy_values_are_ignored() {
        // `BoolishValueParser` accepts "0"/"false" (case-insensitive) as
        // falsy. Setting these via env is equivalent to leaving the flag
        // unset on the CLI - the boolean stays `false`.
        let env = config::Environment::from_pairs(&[
            ("REN_HIDDEN", "0"),
            ("REN_RECURSIVE", "false"),
            ("REN_INCLUDE_DIRS", "FALSE"),
            ("REN_SMART", "false"),
        ]);
        let cli =
            resolve_with_origin(&["ren", "foo", "bar"], &env, &config::Origin::default()).unwrap();
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
        let env = config::Environment::from_pairs(&[("REN_INCLUDE_DIRS", "")]);
        let err = resolve_with_origin(&["ren", "foo", "bar"], &env, &config::Origin::default())
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
        let env = config::Environment::from_pairs(&[("REN_DRY_RUN", "1")]);
        let cli = resolve_with_origin(
            &["ren", "-W", "foo", "bar"],
            &env,
            &config::Origin::default(),
        )
        .unwrap();
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
        let env = config::Environment::from_pairs(&[("REN_UPPER", "true")]);
        let cli = resolve_with_origin(
            &["ren", "--lower", "foo", "bar"],
            &env,
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
        let env = config::Environment::from_pairs(&[("REN_LOWER", "true"), ("REN_UPPER", "true")]);
        let origin = fake_config_origin(&["REN_LOWER"]);
        let cli = resolve_with_origin(&["ren", "foo", "bar"], &env, &origin).unwrap();
        assert!(
            cli.upper,
            "shell REN_UPPER must beat config-derived REN_LOWER"
        );
        assert!(!cli.lower);
    }

    #[test]
    fn test_two_shell_env_vars_in_one_group_errors() {
        let env = config::Environment::from_pairs(&[("REN_LOWER", "true"), ("REN_UPPER", "true")]);
        let err = resolve_with_origin(&["ren", "foo", "bar"], &env, &config::Origin::default())
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
        let env = config::Environment::from_pairs(&[("REN_LOWER", "true"), ("REN_UPPER", "true")]);
        let origin = fake_config_origin(&["REN_LOWER", "REN_UPPER"]);
        let err = resolve_with_origin(&["ren", "foo", "bar"], &env, &origin)
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
        let env = config::Environment::from_pairs(&[("REN_INCLUDE_EXTENSION", "true")]);
        let cli = resolve_with_origin(
            &["ren", "--only-extension", "rs", "txt"],
            &env,
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
        let env = config::Environment::from_pairs(&[
            ("REN_INCLUDE_EXTENSION", "true"),
            ("REN_ONLY_EXTENSION", "true"),
        ]);
        let origin = fake_config_origin(&["REN_INCLUDE_EXTENSION"]);
        let cli = resolve_with_origin(&["ren", "rs", "txt"], &env, &origin).unwrap();
        assert!(
            cli.only_extension,
            "shell REN_ONLY_EXTENSION must beat config-derived REN_INCLUDE_EXTENSION"
        );
        assert!(!cli.include_extension);
    }

    #[test]
    fn test_two_shell_env_vars_in_extension_group_errors() {
        let env = config::Environment::from_pairs(&[
            ("REN_INCLUDE_EXTENSION", "true"),
            ("REN_ONLY_EXTENSION", "true"),
        ]);
        let err = resolve_with_origin(&["ren", "rs", "txt"], &env, &config::Origin::default())
            .err()
            .expect("expected resolver error")
            .to_string();
        assert!(
            err.contains("environment variables"),
            "expected env-conflict wording, got: {err}"
        );
    }

    #[test]
    fn test_change_extension_conflicts_with_scope_flags_on_cli() {
        for scope_flag in ["--only-extension", "--include-extension"] {
            let err = parse_and_resolve(&["ren", "-E", "png", scope_flag, "foo", "bar"])
                .err()
                .unwrap_or_else(|| panic!("CLI conflict expected for {scope_flag}"));
            assert!(
                err.to_string().contains("--change-extension")
                    && err.to_string().contains(&scope_flag[2..]),
                "expected CLI mutex error for {scope_flag}, got: {err}"
            );
        }
    }

    #[test]
    fn test_cli_change_extension_beats_shell_env_only_extension() {
        let env = config::Environment::from_pairs(&[("REN_ONLY_EXTENSION", "true")]);
        let cli = resolve_with_origin(
            &["ren", "-E", "png", "foo", "bar"],
            &env,
            &config::Origin::default(),
        )
        .unwrap();
        assert_eq!(cli.change_extension.as_deref(), Some("png"));
        assert!(!cli.only_extension);
    }

    #[test]
    fn test_empty_change_extension_env_is_ignored() {
        // A stray empty REN_CHANGE_EXTENSION must not be treated as a request to
        // strip every file's extension; it resolves to "unset".
        let env = config::Environment::from_pairs(&[("REN_CHANGE_EXTENSION", "")]);
        let cli =
            resolve_with_origin(&["ren", "foo", "bar"], &env, &config::Origin::default()).unwrap();
        assert_eq!(cli.change_extension, None);
    }

    #[test]
    fn test_cli_empty_change_extension_is_retained_for_stripping() {
        // An *explicit* `-E ''` on the command line is a real request to strip,
        // so it survives resolution (unlike the empty env var above).
        let cli = parse_and_resolve(&["ren", "-E", "", "foo", "bar"]).unwrap();
        assert_eq!(cli.change_extension.as_deref(), Some(""));
    }

    #[test]
    fn configured_keys_have_cli_mappings() {
        let config = config::Config {
            change_extension: Some("png".into()),
            hidden: Some(true),
            ..config::Config::default()
        };
        let command = Cli::command();
        for key in config.values().keys() {
            assert!(
                command
                    .get_arguments()
                    .any(|arg| arg_env_name(arg.get_id().as_str()) == Some(*key))
            );
        }
    }

    #[test]
    fn snapshot_config_environment_and_cli_precedence() {
        let cfg = config::Config {
            hidden: Some(true),
            change_extension: Some("png".into()),
            ..config::Config::default()
        };
        let env = config::Environment::from_pairs(&[
            ("REN_HIDDEN", "false"),
            ("REN_CHANGE_EXTENSION", "jpg"),
        ]);
        let argv = ["ren", "--hidden", "-E", "webp"]
            .map(OsString::from)
            .to_vec();
        let (mut cli, matches, origin) = parse_with_config(argv, &env, &cfg).unwrap();
        resolve_mutex_groups(&mut cli, &matches, &origin).unwrap();
        assert!(cli.hidden);
        assert_eq!(cli.change_extension.as_deref(), Some("webp"));
        let (cli, _, _) = parse_with_config(vec!["ren".into()], &env, &cfg).unwrap();
        assert!(!cli.hidden);
        assert_eq!(cli.change_extension.as_deref(), Some("jpg"));
    }

    #[test]
    fn empty_config_extension_strips_without_mutating_process_environment() {
        let before = std::env::var_os("REN_CHANGE_EXTENSION").is_some();
        let cfg: config::Config = toml::from_str("change-extension = \"\"").unwrap();
        let (mut cli, matches, origin) =
            parse_with_config(vec!["ren".into()], &config::Environment::default(), &cfg).unwrap();
        resolve_mutex_groups(&mut cli, &matches, &origin).unwrap();
        assert_eq!(cli.change_extension.as_deref(), Some(""));
        assert_eq!(std::env::var_os("REN_CHANGE_EXTENSION").is_some(), before);
    }

    #[test]
    fn parents_dependency_uses_all_resolved_sources() {
        let empty = config::Environment::default();
        let parents = config::Environment::from_pairs(&[("REN_PARENTS", "true")]);
        let directory = config::Environment::from_pairs(&[("REN_DIRECTORY", "out")]);
        let disabled = config::Environment::from_pairs(&[("REN_PARENTS", "false")]);
        let cfg: config::Config = toml::from_str("parents = true").unwrap();
        let cfg_dir: config::Config = toml::from_str("directory = \"out\"").unwrap();
        for (args, env, config, valid) in [
            (
                vec!["ren", "--parents"],
                &empty,
                &config::Config::default(),
                false,
            ),
            (vec!["ren"], &parents, &config::Config::default(), false),
            (vec!["ren"], &empty, &cfg, false),
            (
                vec!["ren", "--parents"],
                &directory,
                &config::Config::default(),
                true,
            ),
            (vec!["ren", "--parents"], &empty, &cfg_dir, true),
            (vec!["ren", "-d", "out"], &empty, &cfg, true),
            (vec!["ren"], &disabled, &cfg, true),
        ] {
            let result = parse_with_config(args.iter().map(OsString::from).collect(), env, config);
            assert_eq!(result.is_ok(), valid);
            if let Err(error) = result {
                assert_eq!(
                    error.kind(),
                    clap::error::ErrorKind::MissingRequiredArgument
                );
                assert_eq!(error.exit_code(), 2);
            }
        }
    }

    #[test]
    fn test_cli_flag_wins_over_env_for_booleans() {
        // Explicit `-R` set on CLI beats `REN_RECURSIVE=0` from env (clap's
        // ValueSource::CommandLine wins over the injected default).
        let env = config::Environment::from_pairs(&[("REN_RECURSIVE", "0")]);
        let cli = resolve_with_origin(
            &["ren", "-R", "foo", "bar"],
            &env,
            &config::Origin::default(),
        )
        .unwrap();
        assert!(cli.recursive);
    }

    // ---- compile_options_from_cli ----------------------------------------

    #[test]
    fn test_compile_options_literal_find_replace() {
        let cli = parse_cli(&["ren", "foo", "bar"]);
        let opts = compile_options_from_cli(&cli).unwrap();
        assert_eq!(opts.positional_find.as_deref(), Some("foo"));
        assert_eq!(opts.positional_replace.as_deref(), Some("bar"));
        assert!(!opts.list_files_find_only);
        assert!(opts.expressions.is_empty());
    }

    #[test]
    fn test_compile_options_expression_mode_drops_positionals() {
        let cli = parse_cli(&["ren", "-e", "a", "b"]);
        let opts = compile_options_from_cli(&cli).unwrap();
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
        let cli = parse_cli(&["ren", "-l", "TODO"]);
        let opts = compile_options_from_cli(&cli).unwrap();
        assert!(opts.list_files_find_only);
        assert_eq!(opts.positional_find.as_deref(), Some("TODO"));
        assert!(opts.positional_replace.is_none());
    }

    #[test]
    fn test_compile_options_list_files_with_expressions_is_not_find_only() {
        let cli = parse_cli(&["ren", "-l", "-e", "a", "b"]);
        let opts = compile_options_from_cli(&cli).unwrap();
        // -e takes over: the find/replace come from -e, not the find-only path.
        assert!(!opts.list_files_find_only);
    }

    #[test]
    fn test_compile_options_carries_regex_flags() {
        let cli = parse_cli(&["ren", "-r", "-i", "-G", "-w", "foo", "bar"]);
        let opts = compile_options_from_cli(&cli).unwrap();
        assert!(opts.regex);
        assert!(opts.ignore_case);
        assert!(opts.greedy);
        assert!(opts.word_regexp);
    }

    // ---- preprocess_expression_args --------------------------------------

    #[test]
    fn test_preprocess_expression_args_joins_pair_with_separator() {
        let args = preprocess_expression_args(
            ["ren", "-e", "foo", "bar"]
                .iter()
                .map(|s| OsString::from(*s))
                .collect(),
        );
        assert_eq!(args, vec!["ren", "-e", &format!("foo{EXPR_SEP}bar")]);
    }

    #[test]
    fn test_preprocess_expression_args_compact_form() {
        let args = preprocess_expression_args(
            ["ren", "-efoo", "bar"]
                .iter()
                .map(|s| OsString::from(*s))
                .collect(),
        );
        assert_eq!(args, vec!["ren", "-e", &format!("foo{EXPR_SEP}bar")]);
    }
}
