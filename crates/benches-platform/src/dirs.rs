//! Where Benches finds its files and Ghostty's. Lookups take the environment as `env`, such as
//! `|name| std::env::var_os(name)`, so tests can supply their own.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Reads an environment variable.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

/// `$XDG_CONFIG_HOME`, else `~/.config`, on every OS, as Ghostty reads its own config.
pub fn config_home(env: Env) -> Option<PathBuf> {
    xdg_home(env, "XDG_CONFIG_HOME", ".config")
}

/// Holds `state.json`: `$XDG_STATE_HOME/benches` on Linux, and on macOS the config folder,
/// where it has always been.
pub fn state_dir(env: Env) -> Option<PathBuf> {
    let home = if cfg!(target_os = "macos") {
        config_home(env)
    } else {
        xdg_home(env, "XDG_STATE_HOME", ".local/state")
    };
    Some(home?.join("benches"))
}

/// The log a launch without a terminal writes to.
pub fn log_file(env: Env) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(home(env)?.join("Library/Logs/benches.log"))
    } else {
        Some(state_dir(env)?.join("benches.log"))
    }
}

/// The folders Ghostty installs its themes in, searched after the user's own.
pub fn ghostty_theme_dirs(env: Env) -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        vec![PathBuf::from(
            "/Applications/Ghostty.app/Contents/Resources/ghostty/themes",
        )]
    } else {
        data_dirs(env)
            .into_iter()
            .map(|dir| dir.join("ghostty/themes"))
            .collect()
    }
}

/// Where a compiled terminfo directory installed with the executable `exe` may be: the app
/// bundle's resources on macOS, and `share/benches/terminfo` beside `bin` on Linux.
pub fn terminfo_dir(exe: &Path) -> Option<PathBuf> {
    let prefix = exe.parent()?.parent()?;
    Some(if cfg!(target_os = "macos") {
        prefix.join("Resources/terminfo")
    } else {
        prefix.join("share/benches/terminfo")
    })
}

fn home(env: Env) -> Option<PathBuf> {
    absolute(env("HOME")?)
}

/// The XDG base directory `var` names, else `fallback` under home. The spec ignores a
/// relative path.
fn xdg_home(env: Env, var: &str, fallback: &str) -> Option<PathBuf> {
    env(var)
        .and_then(absolute)
        .or_else(|| Some(home(env)?.join(fallback)))
}

/// `$XDG_DATA_HOME` and then `$XDG_DATA_DIRS`, most important first.
fn data_dirs(env: Env) -> Vec<PathBuf> {
    let dirs = env("XDG_DATA_DIRS")
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    xdg_home(env, "XDG_DATA_HOME", ".local/share")
        .into_iter()
        .chain(std::env::split_paths(&dirs).filter_map(|dir| absolute(dir.into())))
        .collect()
}

fn absolute(path: OsString) -> Option<PathBuf> {
    let path = PathBuf::from(path);
    path.is_absolute().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup<'a>(vars: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
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

    #[test]
    fn data_dirs_default_to_the_spec_and_skip_relative_entries() {
        let unset = [("HOME", "/home/me")];
        assert_eq!(
            data_dirs(&lookup(&unset)),
            ["/home/me/.local/share", "/usr/local/share", "/usr/share"].map(PathBuf::from)
        );
        let set = [
            ("HOME", "/home/me"),
            ("XDG_DATA_HOME", "/data"),
            ("XDG_DATA_DIRS", "/nix/share:relative:/usr/share"),
        ];
        assert_eq!(
            data_dirs(&lookup(&set)),
            ["/data", "/nix/share", "/usr/share"].map(PathBuf::from)
        );
        let empty = [("XDG_DATA_DIRS", "")];
        assert_eq!(
            data_dirs(&lookup(&empty)),
            ["/usr/local/share", "/usr/share"].map(PathBuf::from)
        );
    }

    #[test]
    fn state_and_log_stay_where_this_os_keeps_them() {
        let vars = [("HOME", "/home/me"), ("XDG_STATE_HOME", "/state")];
        let (state, log) = (state_dir(&lookup(&vars)), log_file(&lookup(&vars)));
        if cfg!(target_os = "macos") {
            assert_eq!(state, Some(PathBuf::from("/home/me/.config/benches")));
            assert_eq!(
                log,
                Some(PathBuf::from("/home/me/Library/Logs/benches.log"))
            );
        } else {
            assert_eq!(state, Some(PathBuf::from("/state/benches")));
            assert_eq!(log, Some(PathBuf::from("/state/benches/benches.log")));
        }
    }
}
