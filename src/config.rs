//! Configuration sources are captured and resolved without changing process state.

use serde::Deserialize;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

#[derive(Clone, Default)]
pub(crate) struct Environment {
    values: HashMap<OsString, OsString>,
    home: Option<PathBuf>,
}

impl Environment {
    pub(crate) fn capture() -> Self {
        Self {
            values: std::env::vars_os()
                .filter(|(key, _)| {
                    key.to_string_lossy().starts_with("REN_")
                        || matches!(
                            key.to_str(),
                            Some("HOME" | "USERPROFILE" | "XDG_CONFIG_HOME" | "NO_COLOR")
                        )
                })
                .collect(),
            // Preserve the standard library's passwd/profile fallback without
            // consulting the user's environment again while resolving config.
            home: std::env::home_dir(),
        }
    }

    pub(crate) fn get(&self, key: &str) -> Option<&OsStr> {
        self.values.get(OsStr::new(key)).map(OsString::as_os_str)
    }

    #[cfg(test)]
    pub(crate) fn from_pairs(values: &[(&str, &str)]) -> Self {
        Self::from_pairs_with_home(values, None)
    }

    #[cfg(test)]
    fn from_pairs_with_home(values: &[(&str, &str)], fallback: Option<PathBuf>) -> Self {
        // Injected snapshots must never fall back to the real user's home.
        let mut snapshot = Self {
            values: values
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                .collect(),
            home: None,
        };
        let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        snapshot.home = snapshot
            .get(key)
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .or(fallback);
        snapshot
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Source {
    Config,
    Environment,
}

#[derive(Default)]
pub(crate) struct Origin {
    pub(crate) sources: HashMap<String, Source>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct Config {
    pub hidden: Option<bool>,
    pub no_ignore: Option<bool>,
    pub recursive: Option<bool>,
    pub include_dirs: Option<bool>,
    pub include_extension: Option<bool>,
    pub only_extension: Option<bool>,
    pub change_extension: Option<String>,
    pub greedy: Option<bool>,
    pub ignore_case: Option<bool>,
    pub regex: Option<bool>,
    pub word_regexp: Option<bool>,
    pub smart: Option<bool>,
    pub lower: Option<bool>,
    pub upper: Option<bool>,
    pub prepend: Option<String>,
    pub append: Option<String>,
    pub supplant: Option<String>,
    pub dry_run: Option<bool>,
    pub write: Option<bool>,
    pub preview: Option<bool>,
    pub create_dirs: Option<bool>,
    pub directory: Option<String>,
    pub parents: Option<bool>,
}

impl Config {
    pub(crate) fn load(env: &Environment) -> Self {
        let Some(path) = resolve_path(env) else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(body) => match toml::from_str(&body) {
                Ok(config) => config,
                Err(err) => {
                    eprintln!("ren: {}: {err}", path.display());
                    Self::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                eprintln!("ren: failed to read {}: {err}", path.display());
                Self::default()
            }
        }
    }

    pub(crate) fn values(&self) -> HashMap<&'static str, OsString> {
        let cfg = self;
        let mut values = HashMap::new();
        set_bool(cfg.hidden, "REN_HIDDEN", &mut values);
        set_bool(cfg.no_ignore, "REN_NO_IGNORE", &mut values);
        set_bool(cfg.recursive, "REN_RECURSIVE", &mut values);
        set_bool(cfg.include_dirs, "REN_INCLUDE_DIRS", &mut values);
        set_bool(cfg.include_extension, "REN_INCLUDE_EXTENSION", &mut values);
        set_bool(cfg.only_extension, "REN_ONLY_EXTENSION", &mut values);
        set_bool(cfg.greedy, "REN_GREEDY", &mut values);
        set_bool(cfg.ignore_case, "REN_IGNORE_CASE", &mut values);
        set_bool(cfg.regex, "REN_REGEX", &mut values);
        set_bool(cfg.word_regexp, "REN_WORD_REGEXP", &mut values);
        set_bool(cfg.smart, "REN_SMART", &mut values);
        set_bool(cfg.lower, "REN_LOWER", &mut values);
        set_bool(cfg.upper, "REN_UPPER", &mut values);
        set_str(cfg.prepend.as_deref(), "REN_PREPEND", &mut values);
        set_str(cfg.append.as_deref(), "REN_APPEND", &mut values);
        set_str(cfg.supplant.as_deref(), "REN_SUPPLANT", &mut values);
        set_bool(cfg.dry_run, "REN_DRY_RUN", &mut values);
        set_bool(cfg.write, "REN_WRITE", &mut values);
        set_bool(cfg.preview, "REN_PREVIEW", &mut values);
        set_bool(cfg.create_dirs, "REN_CREATE_DIRS", &mut values);
        set_str(cfg.directory.as_deref(), "REN_DIRECTORY", &mut values);
        set_bool(cfg.parents, "REN_PARENTS", &mut values);
        set_str(
            cfg.change_extension.as_deref(),
            "REN_CHANGE_EXTENSION",
            &mut values,
        );
        values
    }
}

fn set_bool(value: Option<bool>, key: &'static str, values: &mut HashMap<&'static str, OsString>) {
    if let Some(v) = value {
        values.insert(key, OsString::from(if v { "true" } else { "false" }));
    }
}
fn set_str(value: Option<&str>, key: &'static str, values: &mut HashMap<&'static str, OsString>) {
    if let Some(v) = value {
        values.insert(key, OsString::from(v));
    }
}
fn resolve_path(env: &Environment) -> Option<PathBuf> {
    if let Some(path) = env.get("REN_CONFIG_PATH") {
        return (!path.is_empty()).then(|| PathBuf::from(path));
    }
    if let Some(path) = env.get("XDG_CONFIG_HOME").filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path).join("ren/config.toml"));
    }
    env.home
        .as_ref()
        .map(|home| home.join(".config/ren/config.toml"))
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn injected_snapshot_does_not_fall_back_to_process_home() {
        assert!(super::resolve_path(&super::Environment::default()).is_none());
        assert!(
            super::resolve_path(&super::Environment::from_pairs(&[("REN_CONFIG_PATH", "")]))
                .is_none()
        );
    }

    fn parse(input: &str) -> Config {
        toml::from_str(input).expect("valid config")
    }

    #[test]
    fn empty_file_yields_default_config() {
        let cfg = parse("");
        assert!(cfg.hidden.is_none());
        assert!(cfg.prepend.is_none());
        assert!(cfg.smart.is_none());
    }

    #[test]
    fn kebab_case_keys_map_to_snake_fields() {
        let cfg = parse(
            "\
hidden = true
ignore-case = true
include-dirs = true
only-extension = true
regex = true
word-regexp = true
prepend = \"{N}_\"
append = \"-{n}\"
supplant = \"{n:02}\"
",
        );
        assert_eq!(cfg.hidden, Some(true));
        assert_eq!(cfg.ignore_case, Some(true));
        assert_eq!(cfg.include_dirs, Some(true));
        assert_eq!(cfg.only_extension, Some(true));
        // The TOML key is `regex` (matching the canonical --regex CLI flag and
        // rep's config) even though the Rust field is `regexp`.
        assert_eq!(cfg.regex, Some(true));
        assert_eq!(cfg.word_regexp, Some(true));
        assert_eq!(cfg.prepend.as_deref(), Some("{N}_"));
        assert_eq!(cfg.append.as_deref(), Some("-{n}"));
        assert_eq!(cfg.supplant.as_deref(), Some("{n:02}"));
    }

    #[test]
    fn legacy_regexp_key_is_rejected() {
        // The CLI alias is `--regexp` but the TOML key is `regex`. Catch typos
        // up-front rather than silently ignoring the value (deny_unknown_fields).
        let err = toml::from_str::<Config>("regexp = true")
            .err()
            .expect("expected unknown-field error for `regexp`");
        assert!(
            err.to_string().contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = toml::from_str::<Config>("not-a-real-flag = true")
            .err()
            .expect("expected unknown-field error");
        assert!(
            err.to_string().contains("unknown field"),
            "expected unknown-field error, got: {err}"
        );
    }
}
