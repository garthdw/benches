use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use benches_core::process::{Cancel, login_args, run, strip_escapes};
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
    cancel: &Cancel,
) -> Result<Option<PathBuf>> {
    let args = shell_args(shell, command, query);
    let output = run(shell, &args, Path::new(START), timeout, command, cancel)?;
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

/// The query goes in as separate arguments rather than spliced into the script, so nothing in
/// it is interpreted by the shell. A command such as `j` may `cd` rather than print, so `pwd`
/// follows it.
fn shell_args(shell: &Path, command: &str, query: &str) -> Vec<String> {
    login_args(shell, command, " && pwd", query.split_whitespace())
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

/// `path` below `base`, or with home abbreviated when it's `base` itself or outside it.
pub fn relative_to(path: &Path, base: &Path) -> String {
    match path.strip_prefix(base) {
        Ok(rest) if !rest.as_os_str().is_empty() => rest.display().to_string(),
        _ => abbreviate_home(path),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use nix::errno::Errno;
    use nix::sys::signal::{Signal, kill, killpg};
    use nix::unistd::Pid;

    use super::*;
    use crate::test_support::{installed, stub_shell, wait_for_pid, wait_for_pid_exit, wait_until};

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
    fn a_folder_inside_the_working_directory_is_shown_relative_to_it() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let base = home.join("dev");
        assert_eq!(relative_to(&base.join("own/site"), &base), "own/site");
        assert_eq!(relative_to(&base, &base), "~/dev");
        assert_eq!(relative_to(&home.join("notes"), &base), "~/notes");
        assert_eq!(relative_to(Path::new("/dev/site"), &base), "/dev/site");
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
            ["-l", "-i", "-c", "j \"$@\" && pwd", "benches", "a", "$(b)"]
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
        for shell in std::iter::once(PathBuf::from("/bin/sh")).chain(installed("zsh")) {
            assert_eq!(
                lookup(&shell, "cd", query, TIMEOUT, &Cancel::default()).unwrap(),
                Some(folder.clone()),
                "{}",
                shell.display()
            );
        }
    }

    #[test]
    fn a_command_that_cds_finds_the_folder_in_fish() {
        let Some(fish) = installed("fish") else {
            return;
        };
        let (_root, folder) = awkward_folder();
        let found = lookup(
            &fish,
            "cd",
            folder.to_str().unwrap(),
            TIMEOUT,
            &Cancel::default(),
        )
        .unwrap();
        assert_eq!(found, Some(folder));
    }

    #[test]
    fn a_failing_command_finds_nothing() {
        let root = tempfile::tempdir().unwrap();
        let script = format!("echo {}; false", root.path().display());
        assert_eq!(
            lookup(
                Path::new("/bin/sh"),
                &script,
                "",
                TIMEOUT,
                &Cancel::default()
            )
            .unwrap(),
            None
        );
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
            "-l\n-i\n-c\nwt switch --create --no-cd --format=json \"$@\"\nbenches\nnew-branch\n"
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

    /// Creates a worktree on another thread, so the test can cancel it meanwhile.
    fn create_in_background(shell: &Path, cancel: &Cancel) -> mpsc::Receiver<Result<PathBuf>> {
        let (sender, receiver) = mpsc::channel();
        let (shell, cancel) = (shell.to_owned(), cancel.clone());
        thread::spawn(move || {
            let created = create_worktree(&shell, Path::new("/"), "x", TIMEOUT, &cancel);
            sender.send(created)
        });
        receiver
    }

    #[test]
    fn a_cancel_ends_processes_the_command_was_starting() {
        let pids = tempfile::tempdir().unwrap();
        let pid_file = pids.path().join("pid");
        let (_directory, shell, _) = stub_shell(&format!(
            "echo $$ > '{}'; while :; do sleep 30 & sleep 0.002; done",
            pid_file.display()
        ));
        // macOS's `killpg` misses a child being forked as it runs, so try a few times to catch one.
        for _ in 0..10 {
            std::fs::remove_file(&pid_file).ok();
            let cancel = Cancel::default();
            let receiver = create_in_background(&shell, &cancel);
            let group = wait_for_pid(&pid_file);
            thread::sleep(Duration::from_millis(20));
            cancel.cancel().unwrap();
            assert!(receiver.recv_timeout(TIMEOUT).unwrap().is_err());
            // Killed processes linger as zombies until their new parent reaps them.
            wait_until(
                Duration::from_secs(2),
                || "a process survived the cancel".to_owned(),
                || (killpg(Pid::from_raw(group), None) == Err(Errno::ESRCH)).then_some(()),
            );
        }
    }

    #[test]
    fn a_cancelled_command_ends_while_another_process_holds_its_output_open() {
        let pids = tempfile::tempdir().unwrap();
        let (holder_file, pid_file) = (pids.path().join("holder"), pids.path().join("pid"));
        // `set -m` puts the holder in a process group of its own, out of reach of the cancel, as
        // it would each later job, so the last command replaces the shell instead.
        let (_directory, shell, _) = stub_shell(&format!(
            "set -m; sleep 30 & echo $! > '{}'; echo $$ > '{}'; exec sleep 30",
            holder_file.display(),
            pid_file.display()
        ));
        let cancel = Cancel::default();
        let receiver = create_in_background(&shell, &cancel);
        let (holder, pid) = (wait_for_pid(&holder_file), wait_for_pid(&pid_file));
        cancel.cancel().unwrap();
        let ended = receiver.recv_timeout(Duration::from_secs(5));
        kill(Pid::from_raw(holder), Signal::SIGKILL).unwrap();
        assert!(ended.unwrap().is_err());
        wait_for_pid_exit(pid, Duration::from_secs(5), "the cancelled command");
    }

    #[test]
    fn a_command_ends_while_another_process_holds_its_output_open() {
        let pids = tempfile::tempdir().unwrap();
        let holder_file = pids.path().join("holder");
        let (_directory, shell, _) = stub_shell(&format!(
            "set -m; sleep 30 & echo $! > '{}'; echo '{{\"path\":\"/dev/site.x\"}}'",
            holder_file.display()
        ));
        let receiver = create_in_background(&shell, &Cancel::default());
        let holder = wait_for_pid(&holder_file);
        let ended = receiver.recv_timeout(Duration::from_secs(5));
        kill(Pid::from_raw(holder), Signal::SIGKILL).unwrap();
        assert_eq!(ended.unwrap().unwrap(), Path::new("/dev/site.x"));
    }

    #[test]
    fn a_slow_command_times_out() {
        let shell = Path::new("/bin/sh");
        assert!(
            lookup(
                shell,
                "sleep 10;",
                "",
                Duration::from_millis(200),
                &Cancel::default()
            )
            .is_err()
        );
    }
}
