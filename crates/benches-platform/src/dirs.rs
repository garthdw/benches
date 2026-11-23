//! Where Benches finds its files and Ghostty's. Lookups take the environment as `env`, such as
//! `|name| std::env::var_os(name)`, so tests can supply their own.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::os;

/// Reads an environment variable.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// `$XDG_CONFIG_HOME`, else `~/.config`, on every OS, as Ghostty reads its own config.
pub fn config_home(env: Env) -> Option<PathBuf> {
    xdg_home(env, "XDG_CONFIG_HOME", ".config")
}

/// Holds `state.json`.
pub fn state_dir(env: Env) -> Option<PathBuf> {
    Some(os::state_home(env)?.join("benches"))
}

/// The log a launch without a terminal writes to.
pub fn log_file(env: Env) -> Option<PathBuf> {
    Some(os::log_dir(env)?.join("benches.log"))
}

/// The folders Ghostty installs its themes in, searched after the user's own.
pub fn ghostty_theme_dirs(env: Env) -> Vec<PathBuf> {
    os::ghostty_theme_dirs(env)
}

/// Where a compiled terminfo directory installed with the executable `exe` may be.
pub fn terminfo_dir(exe: &Path) -> Option<PathBuf> {
    Some(resources_dir(exe)?.join("terminfo"))
}

/// Where the plugins installed with the executable `exe` may be.
pub fn bundled_plugins_dir(exe: &Path) -> Option<PathBuf> {
    Some(resources_dir(exe)?.join("plugins"))
}

fn resources_dir(exe: &Path) -> Option<PathBuf> {
    Some(exe.parent()?.parent()?.join(os::RESOURCES_DIR))
}

pub(crate) fn home(env: Env) -> Option<PathBuf> {
    absolute(env("HOME")?)
}

/// The XDG base directory `var` names, else `fallback` under home. The spec ignores a
/// relative path.
pub(crate) fn xdg_home(env: Env, var: &str, fallback: &str) -> Option<PathBuf> {
    env(var)
        .and_then(absolute)
        .or_else(|| Some(home(env)?.join(fallback)))
}

pub(crate) fn absolute(path: OsString) -> Option<PathBuf> {
    let path = PathBuf::from(path);
    path.is_absolute().then_some(path)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn lookup<'a>(vars: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.into())
        }
    }

    #[test]
    fn an_xdg_variable_wins_unless_it_is_relative() {
        let home = [("HOME", "/home/me")];
        assert_eq!(
            config_home(&lookup(&home)),
            Some(PathBuf::from("/home/me/.config"))
        );
        let set = [("HOME", "/home/me"), ("XDG_CONFIG_HOME", "/xdg")];
        assert_eq!(config_home(&lookup(&set)), Some(PathBuf::from("/xdg")));
        let relative = [("HOME", "/home/me"), ("XDG_CONFIG_HOME", "xdg")];
        assert_eq!(
            config_home(&lookup(&relative)),
            Some(PathBuf::from("/home/me/.config"))
        );
        assert_eq!(config_home(&lookup(&[])), None);
    }
}
