// Name transforms applied after the find/replace stage in `build_plan`.
//
// Composition is fixed canonical order, NOT argv order. The pipeline runs:
//
//   find/replace  →  lower XOR upper  →  append  →  prepend
//
// `--lower` and `--upper` are mutually exclusive (clap-level conflict).
// When `-E/--no-extension` is set, the entire pipeline runs on the stem only;
// `build_plan` reattaches the extension afterward.

use std::sync::LazyLock;

use anyhow::Result;
use anyhow::bail;
use regex::Regex;

pub(crate) const SMART_COUNTER_TEMPLATE: &str = "__ren_smart_counter__";

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

/// Apply the canonical transform pipeline to a name.
pub(crate) fn apply(name: &str, opts: &TransformOptions) -> String {
    let mut s = if opts.lower {
        name.to_lowercase()
    } else if opts.upper {
        name.to_uppercase()
    } else {
        name.to_string()
    };

    if let Some(ref suffix) = opts.append {
        s.push_str(suffix);
    }

    if let Some(ref prefix) = opts.prepend {
        s = format!("{prefix}{s}");
    }

    s
}

/// Render the counter template with `n` substituted in. Supports `{n}` and
/// `{n:0WIDTH}` (zero-padded). Other `{` / `}` characters pass through unless
/// they form a recognized counter pattern.
///
/// Examples:
/// - `format_counter("{n}", 5)`        → `"5"`
/// - `format_counter("{n:03}", 5)`     → `"005"`
/// - `format_counter("{n:03}_", 5)`    → `"005_"`
/// - `format_counter("[{n:02}]-", 12)` → `"[12]-"`
pub(crate) fn format_counter(template: &str, n: usize) -> String {
    static COUNTER_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\{n(?::0(\d+))?\}").expect("counter regex compiles"));

    COUNTER_RE
        .replace_all(template, |caps: &regex::Captures| match caps.get(1) {
            Some(w) => {
                let width: usize = w.as_str().parse().unwrap_or(0);
                format!("{n:0width$}")
            }
            None => n.to_string(),
        })
        .into_owned()
}

/// Validate a counter template before the rename loop runs. Currently only
/// rejects empty templates; `format_counter` is total over all other inputs
/// (unrecognized `{...}` patterns pass through unchanged).
pub(crate) fn validate_counter_template(template: &str) -> Result<()> {
    if template.is_empty() {
        bail!("--counter template must be non-empty");
    }
    Ok(())
}

pub(crate) fn smart_counter_template(dir_entry_count: usize) -> String {
    let width = dir_entry_count.to_string().len().max(2);
    format!("{{n:0{width}}}_")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts_default() -> TransformOptions {
        TransformOptions::default()
    }

    // ---- format_counter ---------------------------------------------------

    #[test]
    fn counter_no_padding() {
        assert_eq!(format_counter("{n}", 1), "1");
        assert_eq!(format_counter("{n}", 42), "42");
        assert_eq!(format_counter("{n}", 1000), "1000");
    }

    #[test]
    fn counter_zero_padded() {
        assert_eq!(format_counter("{n:03}", 1), "001");
        assert_eq!(format_counter("{n:03}", 42), "042");
        assert_eq!(format_counter("{n:03}", 1000), "1000"); // wider than width: passes through
        assert_eq!(format_counter("{n:05}", 7), "00007");
    }

    #[test]
    fn counter_with_surrounding_text() {
        assert_eq!(format_counter("{n:03}_", 5), "005_");
        assert_eq!(format_counter("[{n:02}]-", 12), "[12]-");
        assert_eq!(format_counter("file_{n}_v2", 3), "file_3_v2");
    }

    #[test]
    fn counter_preserves_unrelated_braces() {
        // Patterns that don't look like counter slots pass through.
        assert_eq!(format_counter("{not_n}_{n:02}", 5), "{not_n}_05");
        assert_eq!(format_counter("plain text", 5), "plain text");
    }

    // ---- validate_counter_template ----------------------------------------

    #[test]
    fn validate_rejects_empty() {
        assert!(validate_counter_template("").is_err());
    }

    #[test]
    fn validate_accepts_normal_templates() {
        assert!(validate_counter_template("{n}").is_ok());
        assert!(validate_counter_template("{n:03}_").is_ok());
        assert!(validate_counter_template("plain text without slot").is_ok());
    }

    // ---- apply ------------------------------------------------------------

    #[test]
    fn apply_noop_when_all_unset() {
        assert_eq!(apply("Foo.txt", &opts_default()), "Foo.txt");
    }

    #[test]
    fn apply_lower() {
        let opts = TransformOptions {
            lower: true,
            ..opts_default()
        };
        assert_eq!(apply("Foo.TXT", &opts), "foo.txt");
    }

    #[test]
    fn apply_upper() {
        let opts = TransformOptions {
            upper: true,
            ..opts_default()
        };
        assert_eq!(apply("Foo.txt", &opts), "FOO.TXT");
    }

    #[test]
    fn apply_append() {
        let opts = TransformOptions {
            append: Some(".bak".into()),
            ..opts_default()
        };
        assert_eq!(apply("data", &opts), "data.bak");
    }

    #[test]
    fn apply_prepend() {
        let opts = TransformOptions {
            prepend: Some("draft_".into()),
            ..opts_default()
        };
        assert_eq!(apply("chapter1", &opts), "draft_chapter1");
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
        assert_eq!(apply("Foo", &opts), "draft_foo.bak");
    }

    #[test]
    fn apply_pipeline_upper_branch() {
        // upper instead of lower; same final layering.
        let opts = TransformOptions {
            upper: true,
            append: Some("-final".into()),
            prepend: Some("v_".into()),
            ..opts_default()
        };
        assert_eq!(apply("foo", &opts), "v_FOO-final");
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
        assert_eq!(apply("Foo", &opts), "foo");
    }

    #[test]
    fn smart_counter_template_uses_at_least_two_digits() {
        assert_eq!(smart_counter_template(0), "{n:02}_");
        assert_eq!(smart_counter_template(99), "{n:02}_");
    }

    #[test]
    fn smart_counter_template_grows_with_directory_size() {
        assert_eq!(smart_counter_template(100), "{n:03}_");
        assert_eq!(smart_counter_template(999), "{n:03}_");
        assert_eq!(smart_counter_template(1000), "{n:04}_");
    }
}
