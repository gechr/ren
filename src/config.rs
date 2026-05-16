//! TOML configuration loader for `~/.config/ren/config.toml`.
//!
//! Each `Some` field is projected onto a matching `REN_*` env var (only when
//! the env var is not already set, so a user's shell env always beats the
//! config file). Clap reads those env vars natively via `#[arg(env = ...)]`.
//!
//! `load_into_env` returns an [`Origin`] recording which `REN_*` keys were
//! synthesized from config (vs already provided by the user's shell). The
//! resolver in `main` uses that record to enforce the full
//! `config < env < CLI` precedence: `ValueSource::EnvVariable` alone cannot
//! tell config-derived values apart from real shell env.
//!
//! Read and parse failures print to stderr and continue; they never abort.

use serde::Deserialize;
use std::collections::HashSet;
use std::path::PathBuf;

const PATH_ENV: &str = "REN_CONFIG_PATH";

/// Records which `REN_*` env var names were synthesized by the config loader.
/// An entry being present means "this env var carries a config-derived value";
/// absent means "anything in the env for this key came from outside ren" (the
/// user's shell, a parent process, a wrapper script).
#[derive(Default)]
pub struct Origin {
    keys: HashSet<&'static str>,
}

impl Origin {
    pub fn is_config_derived(&self, env_name: &str) -> bool {
        self.keys.contains(env_name)
    }

    /// Remove every `REN_*` env var that this loader synthesized, so spawned
    /// subprocesses inherit only the user's shell env. Startup-only:
    /// must be called before any worker threads spawn.
    pub fn unset_synthesized(&self) {
        for key in &self.keys {
            // Single-threaded startup path.
            unsafe {
                std::env::remove_var(key);
            }
        }
    }

    /// Mark `env_name` as config-derived without actually setting the env
    /// var. Intended for tests that simulate config projection by setting
    /// env vars directly.
    #[cfg(test)]
    pub fn mark_as_config_derived(&mut self, env_name: &'static str) {
        self.keys.insert(env_name);
    }
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
    pub greedy: Option<bool>,
    pub ignore_case: Option<bool>,
    pub regex: Option<bool>,
    pub word_regexp: Option<bool>,
    pub smart: Option<bool>,
    pub lower: Option<bool>,
    pub upper: Option<bool>,
    pub prepend: Option<String>,
    pub append: Option<String>,
    pub dry_run: Option<bool>,
    pub preview: Option<bool>,
    pub create_dirs: Option<bool>,
}

/// Resolve the config path, parse it if present, project each set field
/// onto the matching `REN_*` env var, and return an [`Origin`] recording
/// which env vars were synthesized. The env layer is set-once via
/// `set_if_unset`: any value already in the environment (from the user's
/// shell) wins over the config file, and only the keys we successfully
/// set get recorded in `Origin`.
pub fn load_into_env() -> Origin {
    let mut origin = Origin::default();
    let Some(path) = resolve_path() else {
        return origin;
    };
    if !path.exists() {
        return origin;
    }
    let body = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("ren: failed to read {}: {err}", path.display());
            return origin;
        }
    };
    let cfg: Config = match toml::from_str(&body) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("ren: {}: {err}", path.display());
            return origin;
        }
    };
    apply_to_env(&cfg, &mut origin);
    origin
}

fn apply_to_env(cfg: &Config, origin: &mut Origin) {
    set_bool(cfg.hidden, "REN_HIDDEN", origin);
    set_bool(cfg.no_ignore, "REN_NO_IGNORE", origin);
    set_bool(cfg.recursive, "REN_RECURSIVE", origin);
    set_bool(cfg.include_dirs, "REN_INCLUDE_DIRS", origin);
    set_bool(cfg.include_extension, "REN_INCLUDE_EXTENSION", origin);
    set_bool(cfg.only_extension, "REN_ONLY_EXTENSION", origin);
    set_bool(cfg.greedy, "REN_GREEDY", origin);
    set_bool(cfg.ignore_case, "REN_IGNORE_CASE", origin);
    set_bool(cfg.regex, "REN_REGEX", origin);
    set_bool(cfg.word_regexp, "REN_WORD_REGEXP", origin);
    set_bool(cfg.smart, "REN_SMART", origin);
    set_bool(cfg.lower, "REN_LOWER", origin);
    set_bool(cfg.upper, "REN_UPPER", origin);
    set_str(cfg.prepend.as_deref(), "REN_PREPEND", origin);
    set_str(cfg.append.as_deref(), "REN_APPEND", origin);
    set_bool(cfg.dry_run, "REN_DRY_RUN", origin);
    set_bool(cfg.preview, "REN_PREVIEW", origin);
    set_bool(cfg.create_dirs, "REN_CREATE_DIRS", origin);
}

fn set_bool(value: Option<bool>, key: &'static str, origin: &mut Origin) {
    if let Some(v) = value {
        set_if_unset(key, if v { "true" } else { "false" }, origin);
    }
}

fn set_str(value: Option<&str>, key: &'static str, origin: &mut Origin) {
    if let Some(v) = value {
        set_if_unset(key, v, origin);
    }
}

fn set_if_unset(key: &'static str, value: &str, origin: &mut Origin) {
    if std::env::var_os(key).is_some() {
        return;
    }
    // Single-threaded startup path.
    unsafe {
        std::env::set_var(key, value);
    }
    origin.keys.insert(key);
}

fn resolve_path() -> Option<PathBuf> {
    // An explicitly-set `REN_CONFIG_PATH` always wins, even if it points
    // somewhere that doesn't exist - that's how the user disables the
    // default lookup. An empty value means "no config".
    if let Some(value) = std::env::var_os(PATH_ENV) {
        if value.is_empty() {
            return None;
        }
        return Some(PathBuf::from(value));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("ren/config.toml"));
    }
    #[allow(deprecated)]
    let home = std::env::home_dir()?;
    Some(home.join(".config/ren/config.toml"))
}

#[cfg(test)]
mod tests {
    use super::Config;

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
