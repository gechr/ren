// Name transforms applied after the find/replace stage in `build_plan`.
//
// Composition is fixed canonical order, NOT argv order. The pipeline runs:
//
//   find/replace  →  lower XOR upper  →  append  →  prepend
//
// `--lower` and `--upper` are mutually exclusive (clap-level conflict).
// The pipeline runs on the file stem by default; `-x/--include-extension`
// runs it on the full basename, and `-X/--only-extension` runs it on the
// extension only. `build_plan` handles the split/reattach.
//
// `--prepend` and `--append` accept a templating DSL: `{n}` substitutes the
// 1-based per-parent-directory counter, `{n:0W}` zero-pads it to `W` digits,
// and `{N}` zero-pads to a smart per-parent width (`max(2, len(dir_count))`).
// All three forms share the same counter so both affixes can reference it.

use std::sync::LazyLock;

use regex::Regex;

/// Inputs that drive the transform pipeline. Decoupled from `Cli` for the same
/// reason `expressions::CompileOptions` is - so tests can construct values
/// directly without going through clap.
#[derive(Clone, Debug, Default)]
pub(crate) struct TransformOptions {
    pub lower: bool,
    pub upper: bool,
    pub append: Option<String>,
    pub prepend: Option<String>,
}

/// Per-record counter context resolved by `build_plan`. `n` is the 1-based
/// counter value for this entry within its parent directory; `dir_count` is the
/// total count of records in the same parent directory and drives the smart
/// `{N}` width.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CounterContext {
    pub n: usize,
    pub dir_count: usize,
}

/// Apply the canonical transform pipeline to a name. `ctx` carries the counter
/// state used to expand `{n}` / `{n:0W}` / `{N}` in `prepend` and `append`. If
/// neither affix references a counter the values are simply unused.
pub(crate) fn apply(name: &str, opts: &TransformOptions, ctx: CounterContext) -> String {
    let mut s = if opts.lower {
        name.to_lowercase()
    } else if opts.upper {
        name.to_uppercase()
    } else {
        name.to_string()
    };

    if let Some(ref suffix) = opts.append {
        s.push_str(&format_counter(suffix, ctx));
    }

    if let Some(ref prefix) = opts.prepend {
        s = format!("{}{s}", format_counter(prefix, ctx));
    }

    s
}

static COUNTER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\{(?P<smart>N)\}|\{n(?::0(?P<width>\d+))?\}").expect("counter regex compiles")
});

/// True if `s` references the counter (`{n}`, `{n:0W}`, or `{N}`). Used by
/// `build_plan` to decide whether to bother computing per-parent indices.
pub(crate) fn has_counter_placeholder(s: &str) -> bool {
    COUNTER_RE.is_match(s)
}

/// Render the counter template with `ctx.n` substituted in.
///
/// - `{n}`        → the bare 1-based index
/// - `{n:0WIDTH}` → zero-padded to `WIDTH` digits
/// - `{N}`        → zero-padded to `max(2, len(ctx.dir_count))` digits
///
/// Other `{` / `}` characters pass through unless they form a recognized
/// counter pattern.
pub(crate) fn format_counter(template: &str, ctx: CounterContext) -> String {
    COUNTER_RE
        .replace_all(template, |caps: &regex::Captures| {
            if caps.name("smart").is_some() {
                let width = ctx.dir_count.to_string().len().max(2);
                format!("{n:0width$}", n = ctx.n)
            } else if let Some(w) = caps.name("width") {
                let width: usize = w.as_str().parse().unwrap_or(0);
                format!("{n:0width$}", n = ctx.n)
            } else {
                ctx.n.to_string()
            }
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts_default() -> TransformOptions {
        TransformOptions::default()
    }

    fn ctx(n: usize, dir_count: usize) -> CounterContext {
        CounterContext { n, dir_count }
    }

    // ---- format_counter ---------------------------------------------------

    #[test]
    fn counter_no_padding() {
        assert_eq!(format_counter("{n}", ctx(1, 0)), "1");
        assert_eq!(format_counter("{n}", ctx(42, 0)), "42");
        assert_eq!(format_counter("{n}", ctx(1000, 0)), "1000");
    }

    #[test]
    fn counter_zero_padded() {
        assert_eq!(format_counter("{n:03}", ctx(1, 0)), "001");
        assert_eq!(format_counter("{n:03}", ctx(42, 0)), "042");
        assert_eq!(format_counter("{n:03}", ctx(1000, 0)), "1000");
        assert_eq!(format_counter("{n:05}", ctx(7, 0)), "00007");
    }

    #[test]
    fn counter_with_surrounding_text() {
        assert_eq!(format_counter("{n:03}_", ctx(5, 0)), "005_");
        assert_eq!(format_counter("[{n:02}]-", ctx(12, 0)), "[12]-");
        assert_eq!(format_counter("file_{n}_v2", ctx(3, 0)), "file_3_v2");
    }

    #[test]
    fn counter_preserves_unrelated_braces() {
        assert_eq!(format_counter("{not_n}_{n:02}", ctx(5, 0)), "{not_n}_05");
        assert_eq!(format_counter("plain text", ctx(5, 0)), "plain text");
    }

    // ---- {N} smart width ---------------------------------------------------

    #[test]
    fn smart_marker_uses_at_least_two_digits() {
        assert_eq!(format_counter("{N}", ctx(1, 0)), "01");
        assert_eq!(format_counter("{N}", ctx(99, 99)), "99");
    }

    #[test]
    fn smart_marker_grows_with_dir_count() {
        assert_eq!(format_counter("{N}_", ctx(1, 100)), "001_");
        assert_eq!(format_counter("{N}_", ctx(99, 999)), "099_");
        assert_eq!(format_counter("{N}_", ctx(1, 1000)), "0001_");
    }

    #[test]
    fn smart_marker_composes_with_literal_n() {
        // Two slots in the same template, one smart, one literal-padded.
        assert_eq!(format_counter("{N}-{n:02}", ctx(3, 100)), "003-03");
    }

    // ---- has_counter_placeholder -----------------------------------------

    #[test]
    fn has_placeholder_detects_all_forms() {
        assert!(has_counter_placeholder("{n}"));
        assert!(has_counter_placeholder("{n:03}"));
        assert!(has_counter_placeholder("{N}"));
        assert!(has_counter_placeholder("prefix-{n}-suffix"));
        assert!(!has_counter_placeholder("plain"));
        assert!(!has_counter_placeholder("{not_n}"));
        // `{N:03}` is NOT recognized - smart marker is bare `{N}` only.
        assert!(!has_counter_placeholder("{N:03}"));
    }

    // ---- apply ------------------------------------------------------------

    #[test]
    fn apply_noop_when_all_unset() {
        assert_eq!(apply("Foo.txt", &opts_default(), ctx(1, 1)), "Foo.txt");
    }

    #[test]
    fn apply_lower() {
        let opts = TransformOptions {
            lower: true,
            ..opts_default()
        };
        assert_eq!(apply("Foo.TXT", &opts, ctx(1, 1)), "foo.txt");
    }

    #[test]
    fn apply_upper() {
        let opts = TransformOptions {
            upper: true,
            ..opts_default()
        };
        assert_eq!(apply("Foo.txt", &opts, ctx(1, 1)), "FOO.TXT");
    }

    #[test]
    fn apply_append_literal() {
        let opts = TransformOptions {
            append: Some(".bak".into()),
            ..opts_default()
        };
        assert_eq!(apply("data", &opts, ctx(1, 1)), "data.bak");
    }

    #[test]
    fn apply_prepend_literal() {
        let opts = TransformOptions {
            prepend: Some("draft_".into()),
            ..opts_default()
        };
        assert_eq!(apply("chapter1", &opts, ctx(1, 1)), "draft_chapter1");
    }

    #[test]
    fn apply_prepend_with_counter_template() {
        let opts = TransformOptions {
            prepend: Some("{n:02}_".into()),
            ..opts_default()
        };
        assert_eq!(apply("foo", &opts, ctx(7, 0)), "07_foo");
    }

    #[test]
    fn apply_append_with_counter_template() {
        let opts = TransformOptions {
            append: Some("-{n}".into()),
            ..opts_default()
        };
        assert_eq!(apply("foo", &opts, ctx(3, 0)), "foo-3");
    }

    #[test]
    fn apply_prepend_and_append_share_counter() {
        let opts = TransformOptions {
            prepend: Some("{n}-".into()),
            append: Some("-{n}".into()),
            ..opts_default()
        };
        // Same n on both sides: prepend uses 4, append uses 4.
        assert_eq!(apply("foo", &opts, ctx(4, 0)), "4-foo-4");
    }

    #[test]
    fn apply_pipeline_order_lower_then_append_then_prepend() {
        // Pipeline: lower → append → prepend. For `Foo` with all on:
        //   1. lower:    "foo"
        //   2. append:   "foo.bak"
        //   3. prepend:  "draft_foo.bak"
        let opts = TransformOptions {
            lower: true,
            upper: false,
            append: Some(".bak".into()),
            prepend: Some("draft_".into()),
        };
        assert_eq!(apply("Foo", &opts, ctx(1, 1)), "draft_foo.bak");
    }

    #[test]
    fn apply_pipeline_upper_branch() {
        let opts = TransformOptions {
            upper: true,
            append: Some("-final".into()),
            prepend: Some("v_".into()),
            ..opts_default()
        };
        assert_eq!(apply("foo", &opts, ctx(1, 1)), "v_FOO-final");
    }

    /// `lower` and `upper` are mutually exclusive at the clap layer; if both
    /// somehow get set on the struct (only possible from a hand-built
    /// `TransformOptions` in tests), `lower` wins to keep the function total.
    #[test]
    fn apply_lower_wins_when_both_set() {
        let opts = TransformOptions {
            lower: true,
            upper: true,
            ..opts_default()
        };
        assert_eq!(apply("Foo", &opts, ctx(1, 1)), "foo");
    }
}
