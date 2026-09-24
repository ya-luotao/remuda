//! Path handling for account homes and remuda's own directories (SPEC R2, R3).

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use unicode_normalization::is_nfc;

use crate::Env;

/// `$REMUDA_HOME`, else `$HOME/.remuda`.
pub fn remuda_home(env: &Env) -> Result<PathBuf> {
    if let Some(dir) = non_empty(env, "REMUDA_HOME") {
        return Ok(PathBuf::from(dir));
    }
    match non_empty(env, "HOME") {
        Some(home) => Ok(Path::new(home).join(".remuda")),
        None => bail!("neither REMUDA_HOME nor HOME is set"),
    }
}

/// Expands a leading `~` or `~/` once, using `HOME` from the snapshot. Anything else
/// (including `~user`) is returned unchanged.
pub fn expand_tilde(raw: &str, env: &Env) -> Result<String> {
    let rest = match raw.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => rest,
        _ => return Ok(raw.to_string()),
    };
    let Some(home) = non_empty(env, "HOME") else {
        bail!("cannot expand `~` in {raw:?}: HOME is not set");
    };
    if rest.is_empty() {
        return Ok(home.to_string());
    }
    Ok(format!("{}{rest}", home.strip_suffix('/').unwrap_or(home)))
}

/// Checks the string-level invariants of a home path: absolute and NFC (R2).
pub fn check_home_string(home: &str) -> Result<()> {
    if !home.starts_with('/') {
        bail!("home path must be absolute: {home:?}");
    }
    if !is_nfc(home) {
        bail!(
            "home path must be Unicode NFC (the Keychain entry is keyed by the NFC string): {home:?}"
        );
    }
    Ok(())
}

/// `$HOME/.claude`: the native login's directory, i.e. the implicit `default` account (R14).
pub fn native_claude_home(env: &Env) -> Option<PathBuf> {
    user_home(env).map(|home| home.join(".claude"))
}

/// `$HOME` from the snapshot, if set and non-empty.
pub fn user_home(env: &Env) -> Option<&Path> {
    non_empty(env, "HOME").map(Path::new)
}

fn non_empty<'a>(env: &'a Env, key: &str) -> Option<&'a str> {
    env.get(key).map(String::as_str).filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn remuda_home_prefers_remuda_home_var() {
        let e = env(&[("HOME", "/h"), ("REMUDA_HOME", "/r")]);
        assert_eq!(remuda_home(&e).unwrap(), PathBuf::from("/r"));
    }

    #[test]
    fn remuda_home_defaults_under_home() {
        let e = env(&[("HOME", "/h")]);
        assert_eq!(remuda_home(&e).unwrap(), PathBuf::from("/h/.remuda"));
        let e = env(&[("HOME", "/h"), ("REMUDA_HOME", "")]);
        assert_eq!(remuda_home(&e).unwrap(), PathBuf::from("/h/.remuda"));
    }

    #[test]
    fn remuda_home_errors_without_home() {
        assert!(remuda_home(&env(&[])).is_err());
    }

    #[test]
    fn expand_tilde_cases() {
        let e = env(&[("HOME", "/Users/you")]);
        let cases = [
            ("~", "/Users/you"),
            ("~/", "/Users/you/"),
            ("~/p/max", "/Users/you/p/max"),
            ("~/p/max/", "/Users/you/p/max/"),
            ("/abs/~/x", "/abs/~/x"),
            ("~other/x", "~other/x"),
            ("rel/x", "rel/x"),
            ("/a//b/", "/a//b/"),
        ];
        for (raw, want) in cases {
            assert_eq!(expand_tilde(raw, &e).unwrap(), want, "input {raw:?}");
        }
    }

    #[test]
    fn expand_tilde_home_with_trailing_slash_does_not_double_it() {
        let e = env(&[("HOME", "/Users/you/")]);
        assert_eq!(expand_tilde("~/p", &e).unwrap(), "/Users/you/p");
        assert_eq!(expand_tilde("~", &e).unwrap(), "/Users/you/");
    }

    #[test]
    fn expand_tilde_needs_home() {
        assert!(expand_tilde("~/p", &env(&[])).is_err());
        assert_eq!(expand_tilde("/p", &env(&[])).unwrap(), "/p");
    }

    #[test]
    fn home_string_must_be_absolute() {
        assert!(check_home_string("/a/b").is_ok());
        assert!(check_home_string("/a/b/").is_ok());
        assert!(check_home_string("a/b").is_err());
        assert!(check_home_string("").is_err());
        assert!(check_home_string("default").is_err());
    }

    #[test]
    fn home_string_must_be_nfc() {
        assert!(check_home_string("/p/caf\u{e9}").is_ok());
        let err = check_home_string("/p/cafe\u{301}").unwrap_err();
        assert!(err.to_string().contains("NFC"), "{err}");
    }
}
