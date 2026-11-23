use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

use crate::actor::kill_group;

/// Lets another thread end a running command, which `run` watches through a clone. Each serves
/// one `run`: once that finishes, a later `run` with it is cancelled as it starts.
#[derive(Clone, Default)]
pub struct Cancel(Arc<Mutex<Stage>>);

#[derive(Default)]
enum Stage {
    #[default]
    Starting,
    Running(Pid),
    /// Cancelled or finished, so a late `cancel` signals nothing.
    Done,
}

impl Cancel {
    /// Kills the command's process group, or stops it from starting if it hasn't yet.
    pub fn cancel(&self) -> Result<()> {
        // Signalled under the lock, which `run` takes before it reaps the group's leader and so
        // frees the group's id for reuse.
        let mut stage = self.lock();
        match std::mem::replace(&mut *stage, Stage::Done) {
            Stage::Running(group) => killpg(group, Signal::SIGKILL).context("cancel"),
            Stage::Starting | Stage::Done => Ok(()),
        }
    }

    /// Records the command's process group, unless it was cancelled before it started.
    fn start(&self, group: Pid) -> bool {
        let mut stage = self.lock();
        let starting = matches!(*stage, Stage::Starting);
        if starting {
            *stage = Stage::Running(group);
        }
        starting
    }

    /// Says whether the command was cancelled, after which `cancel` signals nothing.
    fn finish(&self) -> bool {
        !matches!(
            std::mem::replace(&mut *self.lock(), Stage::Done),
            Stage::Running(_)
        )
    }

    fn lock(&self) -> MutexGuard<'_, Stage> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Runs the login shell with `args` in `cwd` and returns what it printed, ending the whole
/// process group if it outlasts `timeout` or `cancel` fires.
pub fn run(
    shell: &Path,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
    what: &str,
    cancel: &Cancel,
) -> Result<Output> {
    run_with_input(shell, args, cwd, &[], timeout, what, cancel)
}

/// `run`, with `input` on the command's stdin.
pub fn run_with_input(
    shell: &Path,
    args: &[String],
    cwd: &Path,
    input: &[u8],
    timeout: Duration,
    what: &str,
    cancel: &Cancel,
) -> Result<Output> {
    // Read once the command exits, without waiting for every other process holding them to
    // close them, as a pipe's reader would: one the command left running, or one that another
    // thread forked meanwhile, as macOS has no `pipe2` to make a pipe close-on-exec at once.
    let mut stdout = tempfile::tempfile().context("create output file")?;
    let mut stderr = tempfile::tempfile().context("create output file")?;
    // A file for the reason the output is one: a pipe's write end could leak into a fork.
    let stdin = if input.is_empty() {
        Stdio::null()
    } else {
        let mut file = tempfile::tempfile().context("create input file")?;
        file.write_all(input)
            .and_then(|()| file.rewind())
            .context("write input file")?;
        file.into()
    };
    let mut child = Command::new(shell)
        .args(args)
        .current_dir(cwd)
        .stdin(stdin)
        .stdout(stdout.try_clone().context("share output file")?)
        .stderr(stderr.try_clone().context("share output file")?)
        .process_group(0)
        .spawn()
        .with_context(|| format!("run {}", shell.display()))?;
    let group = Pid::from_raw(child.id() as i32);
    let started = cancel.start(group);
    let (sender, exits) = mpsc::channel();
    if started {
        thread::spawn(move || sender.send(wait_for_exit(group)));
    }
    let exited = started && matches!(exits.recv_timeout(timeout), Ok(Ok(())));
    let cancelled = cancel.finish();
    // Also after a cancel, whose single `killpg` can miss a process being forked.
    let killed = if cancelled || !exited {
        kill_group(group).with_context(|| format!("kill “{what}”"))
    } else {
        Ok(())
    };
    // The waiting thread must be done before the reap frees the leader's pid for reuse.
    exits.iter().for_each(drop);
    let status = child.wait().with_context(|| format!("wait for “{what}”"))?;
    killed?;
    if !exited {
        if cancelled {
            bail!("“{what}” was cancelled");
        }
        bail!("“{what}” took longer than {timeout:?}");
    }
    Ok(Output {
        status,
        stdout: read_from_start(&mut stdout)?,
        stderr: read_from_start(&mut stderr)?,
    })
}

/// Blocks until the child `pid` exits, leaving it unreaped, so its process group's id stays
/// reserved while `run` may still signal the group.
fn wait_for_exit(pid: Pid) -> io::Result<()> {
    loop {
        // SAFETY: zeroes are a valid `siginfo_t`.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `waitid` writes one `siginfo_t`.
        let waited = unsafe {
            libc::waitid(
                libc::P_PID,
                pid.as_raw() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if waited == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn read_from_start(file: &mut File) -> Result<Vec<u8>> {
    let mut printed = Vec::new();
    file.rewind()
        .and_then(|()| file.read_to_end(&mut printed))
        .context("read output file")?;
    Ok(printed)
}

/// Arguments for a login shell to run `command` with `words` as its arguments, then `after`.
pub fn login_args<'a>(
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
        args.push("benches".to_owned());
    }
    args.extend(words.map(str::to_owned));
    args
}

/// Drops CSI sequences such as colours, which is all the commands `run` serves print.
pub fn strip_escapes(text: &str) -> String {
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
