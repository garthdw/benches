mod app;
mod find;
mod input;
mod jump;
mod keymap;
mod palette;
mod render;
mod scenes;
mod secure_input;
mod session;
#[cfg(test)]
mod test_support;
mod view;
mod windows;
mod workspace;

use anyhow::Context;
use app::window_app;
use benches_core::{LogErr, config};
use windows::{Fleet, MAIN_WINDOW};

/// An app launched from Finder or a desktop launcher has its stderr discarded or buried in the
/// system log, so send it to a file instead.
fn log_to_file_without_terminal() -> anyhow::Result<()> {
    use std::os::fd::AsRawFd;

    if !benches_platform::launched_without_terminal() {
        return Ok(());
    }
    let path = benches_platform::dirs::log_file(&|name| std::env::var_os(name))
        .context("HOME is unset")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    // SAFETY: both descriptors are open; dup2 leaves `file` free to close afterwards.
    if unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
        return Err(std::io::Error::last_os_error()).context("redirect stderr");
    }
    let started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
    eprintln!(
        "benches {} started at unix time {}, pid {}",
        env!("CARGO_PKG_VERSION"),
        started.as_secs(),
        std::process::id()
    );
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some((_, config_args)) = args.split_first().filter(|(name, _)| *name == "config") {
        let result = config::Paths::from_env()
            .context("HOME is not set")
            .and_then(|paths| config::command(config_args, &paths, &mut std::io::stdout()));
        if let Err(error) = result {
            eprintln!("benches: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    log_to_file_without_terminal().log_err("log file");
    let fleet = Fleet::default();
    let app = window_app(&fleet, MAIN_WINDOW, |state| {
        state.paths = config::Paths::from_env();
        windows::load_config(state);
    });
    let handle = app.handle();
    benches_platform::notify::start(move |session| match session {
        Some(session) => handle.update(move |state, _| state.fleet.reveal(session)),
        None => handle.open(),
    });
    app.run();
}
