// Pattern compilation for `ren`: turns `-e <find> <replace>` (or positional)
// args into a list of `CompiledExpression` values that can be applied to a
// basename. Adapted and slimmed from `rep/src/expressions.rs` - no `regex::bytes`,
// no grep matcher, no multiline knob, no `-d`/`-x` paths. Filenames are short
// `&str` slices, so we work directly on them and skip the bulk byte machinery.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use regex::RegexBuilder;

/// Internal separator used by `preprocess_expression_args` (in `main.rs`,
/// added in Phase 4) to join the two space-separated `-e <find> <replace>`
/// args into a single clap value. Null byte is safe because Unix argv strings
/// are null-terminated C strings and can never contain one.
pub(crate) const EXPR_SEP: char = '\x00';

/// Inputs that drive expression compilation. Decoupled from `Cli` so this
/// module is independently testable and so the eventual `main.rs` can shape
/// its own flag set without churn here.
pub(crate) struct CompileOptions {
    pub regex: bool,
    pub ignore_case: bool,
    pub greedy: bool,
    pub word_regexp: bool,
    pub smart: bool,
    /// Raw `-e` entries, each a single string of the form `find\0replace`.
    pub expressions: Vec<String>,
    pub positional_find: Option<String>,
    pub positional_replace: Option<String>,
    /// When true, `--list-files` was invoked without a replacement; treat
    /// `positional_find` as a find-only matcher and use an empty replacement.
    /// `apply_to_basename` is not meaningfully called in this mode (list
    /// only matches, never rewrites), but the regex is still needed.
    pub list_files_find_only: bool,
}

impl CompileOptions {
    fn uses_expressions(&self) -> bool {
        !self.expressions.is_empty()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Expression {
    pub(crate) find: String,
    pub(crate) replace: String,
}

pub(crate) struct CompiledExpression {
    pub(crate) regex: regex::Regex,
    /// Boxed closure form of the replacer. Kept on the struct for symmetry with
    /// `rep`'s `CompiledExpression` and as a future caller hook (Phase 5
    /// preview will likely use it for inline-diff rendering); the in-crate
    /// apply path dispatches through `kind` and the specialised counting
    /// replacers, never through this field.
    #[allow(dead_code)]
    pub(crate) replacer: Box<dyn Fn(&regex::Captures) -> String + Send + Sync>,
    /// Lets the apply path short-circuit through fast specialised counting
    /// replacers rather than re-running the boxed closure per match.
    pub(crate) kind: ReplacerKind,
}

pub(crate) enum ReplacerKind {
    Literal(String),
    Regex(String),
    Smart(Arc<HashMap<String, String>>),
}

struct CountingLiteralReplacer<'a> {
    rep: &'a str,
    count: usize,
}

impl regex::Replacer for CountingLiteralReplacer<'_> {
    fn replace_append(&mut self, _: &regex::Captures<'_>, dst: &mut String) {
        self.count += 1;
        dst.push_str(self.rep);
    }
}

struct CountingRegexReplacer<'a> {
    subst: &'a str,
    count: usize,
}

impl regex::Replacer for CountingRegexReplacer<'_> {
    fn replace_append(&mut self, caps: &regex::Captures<'_>, dst: &mut String) {
        self.count += 1;
        caps.expand(self.subst, dst);
    }
}

struct CountingSmartReplacer<'a> {
    map: &'a HashMap<String, String>,
    count: usize,
}

impl regex::Replacer for CountingSmartReplacer<'_> {
    fn replace_append(&mut self, caps: &regex::Captures<'_>, dst: &mut String) {
        self.count += 1;
        let matched = caps
            .get(0)
            .expect("full regex match is always present")
            .as_str();
        dst.push_str(
            self.map
                .get(matched)
                .expect("smart replacer map must contain every regex alternative"),
        );
    }
}

/// Build the 7 case variant pairs for preserve-case replacement.
/// Returns (variant_map, regex_pattern).
pub(crate) fn build_case_variants(
    pattern: &str,
    replacement: &str,
) -> (HashMap<String, String>, String) {
    use inflector::cases::{
        camelcase::to_camel_case, kebabcase::to_kebab_case, pascalcase::to_pascal_case,
        screamingsnakecase::to_screaming_snake_case, snakecase::to_snake_case,
        traincase::to_train_case,
    };

    fn to_ada_case(input: &str) -> String {
        to_train_case(input).replace('-', "_")
    }

    let converters: &[fn(&str) -> String] = &[
        to_ada_case,
        to_camel_case,
        to_kebab_case,
        to_pascal_case,
        to_screaming_snake_case,
        to_snake_case,
        to_train_case,
    ];

    fn normalize_separators(input: &str) -> String {
        input.replace(['_', '-'], " ")
    }

    let mut map = HashMap::new();
    let mut alt_parts = Vec::new();
    let seeds = [
        (pattern.to_string(), replacement.to_string()),
        (
            normalize_separators(pattern),
            normalize_separators(replacement),
        ),
    ];

    for (pattern_seed, replacement_seed) in seeds {
        for convert in converters {
            let from = convert(&pattern_seed);
            let to = convert(&replacement_seed);
            if !from.is_empty() && !map.contains_key(&from) {
                alt_parts.push(regex::escape(&from));
                map.insert(from, to);
            }
        }
    }

    // Sort longest first so regex alternation matches greedily
    alt_parts.sort_by_key(|a| std::cmp::Reverse(a.len()));
    let regex_pattern = alt_parts.join("|");

    (map, regex_pattern)
}

pub(crate) fn build_pattern_for(opts: &CompileOptions, pattern: &str) -> String {
    let base = if opts.regex {
        pattern.to_string()
    } else {
        regex::escape(pattern)
    };

    let wrapped = if opts.word_regexp {
        format!(r"\b(?:{base})\b")
    } else {
        base
    };

    if opts.regex && !opts.greedy {
        format!("(?U){wrapped}")
    } else {
        wrapped
    }
}

pub(crate) fn build_subst_for(opts: &CompileOptions, replacement: &str) -> String {
    if opts.regex {
        replacement.to_string()
    } else {
        replacement.replace('$', "$$")
    }
}

fn parse_expression(input: &str) -> Result<Expression> {
    let Some((find, replace)) = input.split_once(EXPR_SEP) else {
        bail!("Invalid expression: expected `-e <find> <replace>`");
    };
    Ok(Expression {
        find: find.to_string(),
        replace: replace.to_string(),
    })
}

fn parse_expressions(opts: &CompileOptions) -> Result<Vec<Expression>> {
    opts.expressions
        .iter()
        .map(|expr| parse_expression(expr))
        .collect()
}

fn compile_expression(opts: &CompileOptions, expr: &Expression) -> Result<CompiledExpression> {
    if opts.smart {
        let (variant_map, variant_pattern) = build_case_variants(&expr.find, &expr.replace);
        let regex = RegexBuilder::new(&variant_pattern)
            .dot_matches_new_line(false)
            .build()
            .with_context(|| format!("Invalid smart pattern: {}", expr.find))?;
        let variant_map = Arc::new(variant_map);
        let closure_map = Arc::clone(&variant_map);
        let replacer = move |caps: &regex::Captures| -> String {
            let matched = caps
                .get(0)
                .expect("full regex match is always present")
                .as_str();
            closure_map
                .get(matched)
                .cloned()
                .expect("smart replacer map must contain every regex alternative")
        };
        Ok(CompiledExpression {
            regex,
            replacer: Box::new(replacer),
            kind: ReplacerKind::Smart(variant_map),
        })
    } else {
        let pattern = build_pattern_for(opts, &expr.find);
        let subst = build_subst_for(opts, &expr.replace);
        // Multiline mode is irrelevant for filenames (no embedded newlines),
        // and `dot_matches_new_line` stays off so `.` keeps its default behaviour.
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(opts.ignore_case)
            .dot_matches_new_line(false)
            .build()
            .with_context(|| format!("Invalid regex: {}", expr.find))?;
        let kind = if opts.regex {
            ReplacerKind::Regex(subst.clone())
        } else {
            ReplacerKind::Literal(expr.replace.clone())
        };
        let replacer = move |caps: &regex::Captures| -> String {
            let mut out = String::with_capacity(subst.len());
            caps.expand(&subst, &mut out);
            out
        };
        Ok(CompiledExpression {
            regex,
            replacer: Box::new(replacer),
            kind,
        })
    }
}

pub(crate) fn compile_expressions(opts: &CompileOptions) -> Result<Vec<CompiledExpression>> {
    let expressions = if opts.list_files_find_only {
        // `--list-files` invoked without a replacement: only the find half
        // matters. The "replacer" is never used because `--list-files` exits
        // before any rename; we still build a regex for `is_match` filtering.
        vec![Expression {
            find: opts.positional_find.clone().unwrap_or_default(),
            replace: String::new(),
        }]
    } else if opts.uses_expressions() {
        parse_expressions(opts)?
    } else {
        vec![Expression {
            find: opts.positional_find.clone().unwrap_or_default(),
            replace: opts.positional_replace.clone().unwrap_or_default(),
        }]
    };

    expressions
        .iter()
        .filter(|expr| opts.regex || opts.smart || expr.find != expr.replace)
        .map(|expr| compile_expression(opts, expr))
        .collect()
}

/// Run every compiled expression in order against `name`, returning the
/// rewritten string and the total count of substitutions performed.
pub(crate) fn apply_to_basename(name: &str, exprs: &[CompiledExpression]) -> (String, usize) {
    use std::borrow::Cow;

    use regex::Replacer as _;

    let mut current: Cow<'_, str> = Cow::Borrowed(name);
    let mut total = 0;

    for expr in exprs {
        let (replaced, count) = match &expr.kind {
            ReplacerKind::Literal(rep) => {
                let mut counter = CountingLiteralReplacer { rep, count: 0 };
                let out = expr.regex.replace_all(&current, counter.by_ref());
                (out.into_owned(), counter.count)
            }
            ReplacerKind::Regex(subst) => {
                let mut counter = CountingRegexReplacer { subst, count: 0 };
                let out = expr.regex.replace_all(&current, counter.by_ref());
                (out.into_owned(), counter.count)
            }
            ReplacerKind::Smart(map) => {
                let mut counter = CountingSmartReplacer { map, count: 0 };
                let out = expr.regex.replace_all(&current, counter.by_ref());
                (out.into_owned(), counter.count)
            }
        };
        if count > 0 {
            total += count;
            current = Cow::Owned(replaced);
        }
    }

    (current.into_owned(), total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default `CompileOptions` - no flags set, no expressions. Tests adjust
    /// fields they care about; the rest stay at their natural defaults.
    fn opts() -> CompileOptions {
        CompileOptions {
            regex: false,
            ignore_case: false,
            greedy: false,
            word_regexp: false,
            smart: false,
            expressions: Vec::new(),
            positional_find: None,
            positional_replace: None,
            list_files_find_only: false,
        }
    }

    /// `-e <find> <replace>` is shaped at the CLI layer by joining the two
    /// args with a `\0`. Tests construct that joined form directly.
    fn expr_arg(find: &str, replace: &str) -> String {
        format!("{find}{EXPR_SEP}{replace}")
    }

    #[test]
    fn literal_find_replace_applies() {
        let mut o = opts();
        o.positional_find = Some("foo".into());
        o.positional_replace = Some("bar".into());
        let exprs = compile_expressions(&o).unwrap();

        let (out, n) = apply_to_basename("foo_x_foo.txt", &exprs);
        assert_eq!(out, "bar_x_bar.txt");
        assert_eq!(n, 2);
    }

    #[test]
    fn multiple_expressions_chain_in_order() {
        // -e a b -e b c on "a b" → b applies first ("b b"), then c ("c c").
        let mut o = opts();
        o.expressions = vec![expr_arg("a", "b"), expr_arg("b", "c")];
        let exprs = compile_expressions(&o).unwrap();

        let (out, n) = apply_to_basename("a b", &exprs);
        assert_eq!(out, "c c");
        assert_eq!(n, 3);
    }

    #[test]
    fn regex_capture_groups_swap() {
        let mut o = opts();
        o.regex = true;
        o.expressions = vec![expr_arg(r"(foo)\.(bar)", "$2.$1")];
        let exprs = compile_expressions(&o).unwrap();

        let (out, n) = apply_to_basename("foo.bar.txt", &exprs);
        assert_eq!(out, "bar.foo.txt");
        assert_eq!(n, 1);
    }

    #[test]
    fn word_regexp_matches_only_whole_words() {
        let mut o = opts();
        o.word_regexp = true;
        o.expressions = vec![expr_arg("foo", "bar")];
        let exprs = compile_expressions(&o).unwrap();

        let (out, n) = apply_to_basename("foo foobar food", &exprs);
        assert_eq!(out, "bar foobar food");
        assert_eq!(n, 1);
    }

    #[test]
    fn ignore_case_matches_all_cases() {
        let mut o = opts();
        o.ignore_case = true;
        o.expressions = vec![expr_arg("foo", "bar")];
        let exprs = compile_expressions(&o).unwrap();

        let (out, n) = apply_to_basename("Foo FOO foo", &exprs);
        assert_eq!(out, "bar bar bar");
        assert_eq!(n, 3);
    }

    #[test]
    fn regex_default_is_non_greedy() {
        let mut o = opts();
        o.regex = true;
        let pat = build_pattern_for(&o, "a.*b");
        assert_eq!(pat, "(?U)a.*b");
    }

    #[test]
    fn regex_greedy_flag_drops_inverted_marker() {
        let mut o = opts();
        o.regex = true;
        o.greedy = true;
        let pat = build_pattern_for(&o, "a.*b");
        assert_eq!(pat, "a.*b");
    }

    #[test]
    fn smart_case_variants_pin_all_seven() {
        let (map, _) = build_case_variants("foo_bar", "hello_world");
        // Pin the seven canonical variants from the converter list (with
        // separator-normalised seeds collapsing onto these same keys).
        assert_eq!(map.get("foo_bar"), Some(&"hello_world".to_string())); // snake
        assert_eq!(map.get("FooBar"), Some(&"HelloWorld".to_string())); // pascal
        assert_eq!(map.get("FOO_BAR"), Some(&"HELLO_WORLD".to_string())); // screaming-snake
        assert_eq!(map.get("foo-bar"), Some(&"hello-world".to_string())); // kebab
        assert_eq!(map.get("fooBar"), Some(&"helloWorld".to_string())); // camel
        assert_eq!(map.get("Foo-Bar"), Some(&"Hello-World".to_string())); // train
        assert_eq!(map.get("Foo_Bar"), Some(&"Hello_World".to_string())); // ada
        assert_eq!(map.len(), 7);
    }

    #[test]
    fn smart_replaces_in_basename() {
        let mut o = opts();
        o.smart = true;
        o.positional_find = Some("foo_bar".into());
        o.positional_replace = Some("hello_world".into());
        let exprs = compile_expressions(&o).unwrap();

        let (out, _) = apply_to_basename("FooBar_foo_bar_FOO_BAR.rs", &exprs);
        assert_eq!(out, "HelloWorld_hello_world_HELLO_WORLD.rs");
    }

    #[test]
    fn empty_replacement_deletes_match() {
        let mut o = opts();
        o.expressions = vec![expr_arg("foo", "")];
        let exprs = compile_expressions(&o).unwrap();

        let (out, n) = apply_to_basename("foobarfoo", &exprs);
        assert_eq!(out, "bar");
        assert_eq!(n, 2);
    }

    #[test]
    fn noop_find_eq_replace_is_filtered_out() {
        let mut o = opts();
        o.expressions = vec![expr_arg("foo", "foo")];
        let exprs = compile_expressions(&o).unwrap();
        assert!(exprs.is_empty());

        let (out, n) = apply_to_basename("foo bar", &exprs);
        assert_eq!(out, "foo bar");
        assert_eq!(n, 0);
    }

    #[test]
    fn list_files_find_only_compiles_a_matcher() {
        let mut o = opts();
        o.list_files_find_only = true;
        o.positional_find = Some("readme".into());
        let exprs = compile_expressions(&o).unwrap();
        assert_eq!(exprs.len(), 1);
        assert!(exprs[0].regex.is_match("readme.md"));
        assert!(!exprs[0].regex.is_match("CHANGELOG.md"));
    }

    #[test]
    fn parse_expression_requires_null_separator() {
        assert!(parse_expression("missing-separator").is_err());
        let parsed = parse_expression(&format!("a{EXPR_SEP}b=c")).unwrap();
        assert_eq!(
            parsed,
            Expression {
                find: "a".into(),
                replace: "b=c".into(),
            }
        );
    }

    #[test]
    fn build_pattern_escapes_metacharacters_in_literal_mode() {
        let o = opts();
        let pat = build_pattern_for(&o, "1.2.3");
        assert_eq!(pat, r"1\.2\.3");
    }

    #[test]
    fn build_subst_escapes_dollar_in_literal_mode() {
        let o = opts();
        assert_eq!(build_subst_for(&o, "$1"), "$$1");
    }

    #[test]
    fn build_subst_preserves_dollar_in_regex_mode() {
        let mut o = opts();
        o.regex = true;
        assert_eq!(build_subst_for(&o, "$1"), "$1");
    }

    #[test]
    fn literal_mode_emits_dollar_references_verbatim() {
        // Regression guard: `$1` must reach the output untouched in literal mode.
        let mut o = opts();
        o.positional_find = Some("foo".into());
        o.positional_replace = Some("$1bar".into());
        let exprs = compile_expressions(&o).unwrap();
        let (out, n) = apply_to_basename("foo baz", &exprs);
        assert_eq!(out, "$1bar baz");
        assert_eq!(n, 1);
    }
}
