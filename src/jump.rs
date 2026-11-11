use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde::Deserialize;

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
    let args = shell_args(shell, command, query);
    let output = run(
        shell,
        &args,
        Path::new(START),
        timeout,
        command,
        &Cancel::default(),
    )?;
    Ok(output
        .status
        .success()
        .then(|| parse_output(&String::from_utf8_lossy(&output.stdout)))
        .flatten())
}

/// A worktrunk worktree of a repository.
#[derive(Clone, Debug, PartialEq)]
pub struct Worktree {
    pub branch: Option<String>,
    pub path: PathBuf,
}

#[derive(Deserialize)]
struct WorktreeList {
    items: Vec<WorktreeItem>,
}

#[derive(Deserialize)]
struct WorktreeItem {
    branch: Option<String>,
    /// Absent for a branch with no worktree.
    worktree: Option<WorktreePath>,
}

#[derive(Deserialize)]
struct WorktreePath {
    path: PathBuf,
}

/// Lists the worktrees of the repository at `root` with `wt list`, through the login shell so
/// `wt` is on the `PATH`. A folder that isn't a repository has none.
pub fn worktrees(shell: &Path, root: &Path, timeout: Duration) -> Result<Vec<Worktree>> {
    let args = ["-l", "-i", "-c", "wt list --format=json"].map(str::to_owned);
    let output = run(shell, &args, root, timeout, "wt list", &Cancel::default())?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    parse_worktrees(&String::from_utf8_lossy(&output.stdout))
}

/// Creates `branch` and its worktree with `wt switch --create` in the repository at `root`, and
/// returns the worktree's path. `wt` runs the project's hooks first, so this can take a while.
pub fn create_worktree(
    shell: &Path,
    root: &Path,
    branch: &str,
    timeout: Duration,
    cancel: &Cancel,
) -> Result<PathBuf> {
    let command = "wt switch --create --no-cd --format=json";
    let args = login_args(shell, command, "", [branch].into_iter());
    let output = run(shell, &args, root, timeout, "wt switch", cancel)?;
    if !output.status.success() {
        bail!(
            "{}",
            failure_reason(&String::from_utf8_lossy(&output.stderr))
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = &stdout[stdout.find('{').unwrap_or(0)..];
    let created: Created = serde_json::Deserializer::from_str(json)
        .into_iter()
        .next()
        .context("no output")?
        .context("parse wt switch")?;
    Ok(created.path)
}

#[derive(Deserialize)]
struct Created {
    path: PathBuf,
}

/// The `✗` line `wt` prints for a refusal, else the last line.
fn failure_reason(stderr: &str) -> String {
    let plain = strip_escapes(stderr);
    let lines = || plain.lines().map(str::trim).filter(|line| !line.is_empty());
    lines()
        .find(|line| line.starts_with('✗'))
        .or_else(|| lines().next_back())
        .unwrap_or("wt switch failed")
        .to_owned()
}

/// Interactive shells may print a banner before the JSON.
fn parse_worktrees(output: &str) -> Result<Vec<Worktree>> {
    let json = &output[output.find('{').unwrap_or(0)..];
    let list: WorktreeList = serde_json::Deserializer::from_str(json)
        .into_iter()
        .next()
        .context("no output")?
        .context("parse wt list")?;
    Ok(list
        .items
        .into_iter()
        .filter_map(|item| {
            Some(Worktree {
                branch: item.branch,
                path: item.worktree?.path,
            })
        })
        .collect())
}

/// Lets another thread end a running command, which `run` watches through a clone.
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicI32>);

/// The state once cancelled or finished, so a late `cancel` signals nothing.
const DONE: i32 = -1;

impl Cancel {
    /// Kills the command's process group, or stops it from starting if it hasn't yet.
    pub fn cancel(&self) -> Result<()> {
        match self.0.swap(DONE, Ordering::SeqCst) {
            group if group > 0 => killpg(Pid::from_raw(group), Signal::SIGKILL).context("cancel"),
            _ => Ok(()),
        }
    }
}

/// Runs the login shell with `args` in `cwd` and returns what it printed, ending the whole
/// process group if it outlasts `timeout` or `cancel` fires.
fn run(
    shell: &Path,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    what: &str,
    cancel: &Cancel,
) -> Result<Output> {
    let child = Command::new(shell)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .with_context(|| format!("run {}", shell.display()))?;
    let group = Pid::from_raw(child.id() as i32);
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || sender.send(child.wait_with_output()).ok());
    let started = cancel
        .0
        .compare_exchange(0, group.as_raw(), Ordering::SeqCst, Ordering::SeqCst);
    if started.is_err() {
        killpg(group, Signal::SIGKILL).context("kill cancelled lookup")?;
    }
    let received = receiver.recv_timeout(timeout);
    cancel.0.store(DONE, Ordering::SeqCst);
    match received {
        Ok(output) => output.context("wait for lookup"),
        Err(_) => {
            killpg(group, Signal::SIGKILL).context("kill lookup")?;
            bail!("“{what}” took longer than {timeout:?}");
        }
    }
}

/// The query goes in as separate arguments rather than spliced into the script, so nothing in
/// it is interpreted by the shell. A command such as `j` may `cd` rather than print, so `pwd`
/// follows it.
fn shell_args(shell: &Path, command: &str, query: &str) -> Vec<String> {
    login_args(shell, command, " && pwd", query.split_whitespace())
}

/// Arguments for a login shell to run `command` with `words` as its arguments, then `after`.
fn login_args<'a>(
    shell: &Path,
    command: &str,
    after: &str,
    words: impl Iterator<Item = &'a str>,
) -> Vec<String> {
    let fish = shell.file_name().is_some_and(|name| name == "fish");
    let script = if fish {
        format!("{command} $argv{after}")
    } else {
        format!("{command} \"$@\"{after}")
    };
    let mut args = vec!["-l".to_owned(), "-i".to_owned(), "-c".to_owned(), script];
    if !fish {
        args.push("agentd".to_owned());
    }
    args.extend(words.map(str::to_owned));
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

/// The path with `~` for the home folder, as `parse_output` expands it.
pub fn abbreviate_home(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
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
    fn wt_list_output_parses_into_worktrees() {
        let output = concat!(
            "welcome back\n",
            r#"{"schema":2,"repo":{},"items":["#,
            r#"{"branch":"main","head":{"sha":"a"},"worktree":{"path":"/dev/site","main":true}},"#,
            r#"{"branch":"feature","worktree":{"path":"/dev/site.feature"}},"#,
            r#"{"branch":null,"worktree":{"path":"/dev/site.detached"}},"#,
            r#"{"branch":"remote-only"}]}"#,
        );
        assert_eq!(
            parse_worktrees(output).unwrap(),
            [
                Worktree {
                    branch: Some("main".to_owned()),
                    path: PathBuf::from("/dev/site")
                },
                Worktree {
                    branch: Some("feature".to_owned()),
                    path: PathBuf::from("/dev/site.feature")
                },
                Worktree {
                    branch: None,
                    path: PathBuf::from("/dev/site.detached")
                },
            ]
        );
        assert!(parse_worktrees("not json").is_err());
    }

    #[test]
    fn the_home_folder_is_abbreviated_to_a_tilde() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(abbreviate_home(&home), "~");
        assert_eq!(abbreviate_home(&home.join("dev/site")), "~/dev/site");
        assert_eq!(abbreviate_home(Path::new("/dev/site")), "/dev/site");
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

    /// A shell that records its arguments instead of running the script, then runs `body`.
    fn stub_shell(body: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let record = root.path().join("args");
        let shell = root.path().join("stub-shell");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n{body}\n",
            record.display()
        );
        std::fs::write(&shell, script).unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        (root, shell, record)
    }

    #[test]
    fn creating_a_worktree_runs_wt_switch_create_and_returns_its_path() {
        let (_root, shell, record) = stub_shell(
            r#"echo 'welcome'; echo '{"action":"created","path":"/dev/site.new-branch"}'"#,
        );
        let path = create_worktree(
            &shell,
            Path::new("/"),
            "new-branch",
            TIMEOUT,
            &Cancel::default(),
        )
        .unwrap();
        assert_eq!(path, Path::new("/dev/site.new-branch"));
        assert_eq!(
            std::fs::read_to_string(record).unwrap(),
            "-l\n-i\n-c\nwt switch --create --no-cd --format=json \"$@\"\nagentd\nnew-branch\n"
        );
    }

    #[test]
    fn a_refused_worktree_reports_wts_reason() {
        let (_root, shell, _) = stub_shell(
            "printf '\\033[31m✗ Branch x already exists\\033[0m\\n↳ run without --create\\n' >&2; exit 1",
        );
        let error =
            create_worktree(&shell, Path::new("/"), "x", TIMEOUT, &Cancel::default()).unwrap_err();
        assert_eq!(error.to_string(), "✗ Branch x already exists");
    }

    #[test]
    fn a_slow_command_times_out() {
        let shell = Path::new("/bin/sh");
        assert!(lookup(shell, "sleep 10;", "", Duration::from_millis(200)).is_err());
    }
}
