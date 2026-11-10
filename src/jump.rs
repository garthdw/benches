use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

/// Where the lookup starts, so a command that only `cd`s is told apart by the `pwd` after it.
const START: &str = "/";

/// Runs the jump command (such as `j` or `zoxide query`) with the query's words through the
/// login shell, so the user's functions and aliases resolve, and returns the folder it names.
pub fn lookup(
    shell: &Path,
    command: &str,
    query: &str,
    timeout: Duration,
) -> Result<Option<PathBuf>> {
    let mut child = Command::new(shell)
        .args(shell_args(shell, command, query))
        .current_dir(START)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("run {}", shell.display()))?;
    let mut stdout = child.stdout.take().context("no stdout")?;
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut output = Vec::new();
        let result = stdout.read_to_end(&mut output).map(|_| output);
        sender.send(result).ok();
    });
    let output = match receiver.recv_timeout(timeout) {
        Ok(output) => output.context("read output")?,
        Err(_) => {
            killpg(Pid::from_raw(child.id() as i32), Signal::SIGKILL).context("kill lookup")?;
            child.wait().context("reap lookup")?;
            bail!("“{command}” took longer than {timeout:?}");
        }
    };
    if !child.wait().context("wait for lookup")?.success() {
        return Ok(None);
    }
    Ok(parse_output(&String::from_utf8_lossy(&output)))
}

/// The query goes in as separate arguments rather than spliced into the script, so nothing in
/// it is interpreted by the shell. A command such as `j` may `cd` rather than print, so `pwd`
/// follows it.
fn shell_args(shell: &Path, command: &str, query: &str) -> Vec<String> {
    let fish = shell.file_name().is_some_and(|name| name == "fish");
    let script = if fish {
        format!("{command} $argv && pwd")
    } else {
        format!("{command} \"$@\" && pwd")
    };
    let mut args = vec!["-l".to_owned(), "-i".to_owned(), "-c".to_owned(), script];
    if !fish {
        args.push("agentd".to_owned());
    }
    args.extend(query.split_whitespace().map(str::to_owned));
    args
}

/// The last line naming a folder other than where the lookup started, as commands such as
/// zsh's `j` print the folder in colour before `cd`ing to it.
fn parse_output(output: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    strip_escapes(output)
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| *line != START)
        .map(|line| match (line.strip_prefix("~"), &home) {
            (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
                home.join(rest.trim_start_matches('/'))
            }
            _ => PathBuf::from(line),
        })
        .find(|path| path.is_absolute() && path.is_dir())
}

/// Drops CSI sequences such as colours, which is all lookup commands print.
fn strip_escapes(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(char) = chars.next() {
        if char != '\x1b' {
            plain.push(char);
        } else if chars.next() == Some('[') {
            for char in chars.by_ref() {
                if ('@'..='~').contains(&char) {
                    break;
                }
            }
        }
    }
    plain
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(10);

    #[test]
    fn the_last_folder_wins_with_colours_stripped_and_the_start_ignored() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let output = format!(
            "{}\n\x1b[31m{}\x1b[0m\nnot a folder\n/\n",
            first.display(),
            second.display()
        );
        assert_eq!(parse_output(&output), Some(second));
        assert_eq!(parse_output("autojump: not found\n.\n/\n"), None);
    }

    #[test]
    fn a_tilde_expands_to_home() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(parse_output("~\n"), Some(home.clone()));
        assert_eq!(parse_output("~/\n"), Some(home));
    }

    #[test]
    fn the_query_goes_in_as_arguments() {
        assert_eq!(
            shell_args(Path::new("/bin/zsh"), "j", " a  $(b) "),
            ["-l", "-i", "-c", "j \"$@\" && pwd", "agentd", "a", "$(b)"]
        );
        assert_eq!(
            shell_args(Path::new("/opt/homebrew/bin/fish"), "zoxide query", "a"),
            ["-l", "-i", "-c", "zoxide query $argv && pwd", "a"]
        );
    }

    /// A folder whose name would run or expand if the shell interpreted it.
    fn awkward_folder() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("a'b\"$(true)$HOME;c*");
        std::fs::create_dir(&folder).unwrap();
        (root, folder.canonicalize().unwrap())
    }

    #[test]
    fn a_command_that_cds_finds_the_folder_in_sh_style_shells() {
        let (_root, folder) = awkward_folder();
        let query = folder.to_str().unwrap();
        for shell in ["/bin/sh", "/bin/zsh"] {
            assert_eq!(
                lookup(Path::new(shell), "cd", query, TIMEOUT).unwrap(),
                Some(folder.clone()),
                "{shell}"
            );
        }
    }

    #[test]
    fn a_command_that_cds_finds_the_folder_in_fish() {
        let fish = Path::new("/opt/homebrew/bin/fish");
        if !fish.exists() {
            return;
        }
        let (_root, folder) = awkward_folder();
        let found = lookup(fish, "cd", folder.to_str().unwrap(), TIMEOUT).unwrap();
        assert_eq!(found, Some(folder));
    }

    #[test]
    fn a_failing_command_finds_nothing() {
        let root = tempfile::tempdir().unwrap();
        let script = format!("echo {}; false", root.path().display());
        assert_eq!(
            lookup(Path::new("/bin/sh"), &script, "", TIMEOUT).unwrap(),
            None
        );
    }

    #[test]
    fn a_slow_command_times_out() {
        let shell = Path::new("/bin/sh");
        assert!(lookup(shell, "sleep 10;", "", Duration::from_millis(200)).is_err());
    }
}
