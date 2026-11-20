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

use std::path::PathBuf;

use anyhow::Context;
use app::window_app;
use benches_core::{LogErr, config};
use windows::{Fleet, MAIN_WINDOW};

/// A bundle launched from Finder has its stderr discarded, so send it to a file instead.
fn log_to_file_in_bundle() -> anyhow::Result<()> {
    use std::os::fd::AsRawFd;

    if !app::in_bundle() {
        return Ok(());
    }
    let home = std::env::var_os("HOME").context("HOME is unset")?;
    let path = PathBuf::from(home).join("Library/Logs/benches.log");
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
        let result = config::Paths::from_home()
            .context("HOME is not set")
            .and_then(|paths| config::command(config_args, &paths, &mut std::io::stdout()));
        if let Err(error) = result {
            eprintln!("benches: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    log_to_file_in_bundle().log_err("log file");
    let fleet = Fleet::default();
    let app = window_app(&fleet, MAIN_WINDOW, |state| {
        state.paths = config::Paths::from_home();
        windows::load_config(state);
    });
    let handle = app.handle();
    benches_platform::notify::start(move |session| match session {
        Some(session) => handle.update(move |state, _| state.fleet.reveal(session)),
        None => handle.open(),
    });
    app.run();
}
