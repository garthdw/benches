use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use rune::{Tx, Waker, WindowSpec};

use crate::LogErr;
use crate::app::{State, refuse_clipboard_reads, save_layout, window_app};
use crate::config::Config;
use crate::workspace::{Project, SavedWorkspace};

pub(crate) type WindowId = u32;

pub const MAIN_WINDOW: WindowId = 0;

/// Hands projects between the windows of the app, whose workspaces each show only their own.
/// A project travels with its sessions running; the receiving window takes it on its next poll.
#[derive(Clone, Default)]
pub struct Fleet(Rc<RefCell<Windows>>);

#[derive(Default)]
struct Windows {
    opened: WindowId,
    entries: HashMap<WindowId, Entry>,
    /// What each window left for the layout as the app quit.
    saved: BTreeMap<WindowId, SavedWorkspace>,
}

#[derive(Default)]
struct Entry {
    waker: Option<Waker>,
    reveal: Option<Rc<dyn Fn(u64)>>,
    inbox: Vec<Project>,
    /// A config reloaded in another window, for this one to take on its next poll.
    config: Option<Config>,
}

impl Fleet {
    pub fn reserve_window(&self) -> WindowId {
        let mut windows = self.0.borrow_mut();
        windows.opened += 1;
        windows.opened
    }

    pub fn register(&self, window: WindowId, waker: Waker) {
        self.0.borrow_mut().entries.entry(window).or_default().waker = Some(waker);
    }

    /// `reveal` shows a session in `window` and brings the window forward, if the window has it.
    pub fn on_reveal(&self, window: WindowId, reveal: impl Fn(u64) + 'static) {
        self.0
            .borrow_mut()
            .entries
            .entry(window)
            .or_default()
            .reveal = Some(Rc::new(reveal));
    }

    /// Shows the session wherever it lives, as a clicked notification asks.
    pub fn reveal(&self, session: u64) {
        let reveals: Vec<_> = self
            .0
            .borrow()
            .entries
            .values()
            .filter_map(|entry| entry.reveal.clone())
            .collect();
        for reveal in reveals {
            reveal(session);
        }
    }

    pub fn unregister(&self, window: WindowId) {
        self.0.borrow_mut().entries.remove(&window);
    }

    pub fn send(&self, window: WindowId, project: Project) {
        let waker = {
            let mut windows = self.0.borrow_mut();
            let entry = windows.entries.entry(window).or_default();
            entry.inbox.push(project);
            entry.waker.clone()
        };
        if let Some(waker) = waker {
            waker();
        }
    }

    /// Records `window`'s layout at quit. Once the main window's is in, answers its layout with
    /// the other windows' projects after its own, so whichever window quits last saves them all.
    /// Torn-off windows are not restored: their projects come back in the main window.
    pub fn deposit(&self, window: WindowId, layout: SavedWorkspace) -> Option<SavedWorkspace> {
        let mut windows = self.0.borrow_mut();
        windows.saved.insert(window, layout);
        let mut merged = windows.saved.get(&MAIN_WINDOW)?.clone();
        let others = windows.saved.iter().filter(|(id, _)| **id != MAIN_WINDOW);
        merged
            .projects
            .extend(others.flat_map(|(_, layout)| layout.projects.clone()));
        Some(merged)
    }

    /// Hands `config` to every window but `from`, which applied it itself.
    pub fn share_config(&self, from: WindowId, config: &Config) {
        let wakers: Vec<_> = self
            .0
            .borrow_mut()
            .entries
            .iter_mut()
            .filter(|(window, _)| **window != from)
            .filter_map(|(_, entry)| {
                entry.config = Some(config.clone());
                entry.waker.clone()
            })
            .collect();
        for waker in wakers {
            waker();
        }
    }

    fn take_config(&self, window: WindowId) -> Option<Config> {
        self.0.borrow_mut().entries.get_mut(&window)?.config.take()
    }

    pub fn take(&self, window: WindowId) -> Vec<Project> {
        self.0
            .borrow_mut()
            .entries
            .get_mut(&window)
            .map(|entry| std::mem::take(&mut entry.inbox))
            .unwrap_or_default()
    }
}

const TEAR_OFF_DISTANCE: f32 = 48.0;
/// Longer than `terminate`'s grace before it kills, so stubborn processes are killed too.
const QUIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Closing the main window quits, as it did when it was the only one. Closing another gives its
/// projects, still running, back to the main window.
pub(crate) fn on_close(state: &mut State, tx: &mut Tx) {
    if rune::quitting() {
        return;
    }
    if state.window == MAIN_WINDOW {
        tx.quit();
    } else {
        merge_into_main(state);
    }
}

fn merge_into_main(state: &mut State) {
    let confirms = std::mem::take(&mut state.confirms);
    refuse_clipboard_reads(state, confirms);
    while let Some(project) = state.workspace.take_project(0) {
        state.fleet.send(MAIN_WINDOW, project);
    }
    state.fleet.unregister(state.window);
}

/// Saves the layout, then ends every process so none outlives the app.
pub(crate) fn on_quit(state: &mut State, _: &mut Tx) {
    save_layout(state).log_err("save layout");
    state.workspace.shutdown(QUIT_TIMEOUT);
}

/// Hands project `index` to a new window, with its processes still running. A window's last
/// project stays, as it would leave that window empty.
pub(crate) fn move_to_new_window(state: &mut State, index: usize, position: Option<(i32, i32)>) {
    if state.workspace.projects.len() < 2 {
        return;
    }
    let Some(project) = state.workspace.take_project(index) else {
        return;
    };
    let window = state.fleet.reserve_window();
    state.fleet.send(window, project);
    let (config, paths) = (state.config.clone(), state.paths.clone());
    let app = window_app(&state.fleet, window, move |state| {
        state.config = config;
        state.paths = paths;
        state.started = true;
    });
    let mut spec = WindowSpec::from_app(format!("agentd-{window}"), app);
    spec.options.position = position;
    rune::open_window(spec);
}

/// Whether a project dragged from the sidebar was let go far enough outside it, or off the
/// window's top or left edge, to leave for a window of its own.
pub(crate) fn torn_off([x, y]: [f32; 2], sidebar_width: f32) -> bool {
    x < 0.0 || y < 0.0 || x > sidebar_width + TEAR_OFF_DISTANCE
}

/// The config at launch. A config.toml that fails falls back to the defaults, and the window says why.
pub(crate) fn load_config(state: &mut State) {
    let Some(paths) = &state.paths else { return };
    let (config, error) = Config::load(paths);
    state.config = config;
    report_config_error(state, error);
}

/// Rereads the config for every window. A config.toml that fails to parse changes nothing, and
/// this window says why.
pub(crate) fn reload_config(state: &mut State) {
    let Some(paths) = &state.paths else { return };
    match Config::reload(paths) {
        Ok(config) => {
            report_config_error(state, None);
            state.fleet.share_config(state.window, &config);
            apply_config(state, config);
        }
        Err(error) => report_config_error(state, Some(error)),
    }
}

/// Logs why config.toml was skipped and shows it in the banner, or clears the banner.
fn report_config_error(state: &mut State, error: Option<anyhow::Error>) {
    if let Some(error) = &error {
        eprintln!("agentd: config: {error:#}");
    }
    state.config_error = error.map(|error| format!("{error:#}"));
}

pub(crate) fn adopt_config(state: &mut State) -> bool {
    let config = state.fleet.take_config(state.window);
    let adopted = config.is_some();
    if let Some(config) = config {
        apply_config(state, config);
    }
    adopted
}

/// Running sessions take the theme and the like; the shell, tabs and folder apply to new tabs.
fn apply_config(state: &mut State, config: Config) {
    for tab in state.workspace.tabs_mut() {
        tab.session.configure(&config);
    }
    state.config = config;
}

/// Takes the projects other windows handed over, pointing their sessions at this window.
pub(crate) fn adopt_projects(state: &mut State) -> bool {
    let projects = state.fleet.take(state.window);
    let adopted = !projects.is_empty();
    for mut project in projects {
        let waker = state.window_waker();
        for retarget in project.tabs_mut().filter_map(|tab| tab.retarget.as_ref()) {
            retarget.set(waker.clone());
        }
        state.workspace.add_project(project);
    }
    adopted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Confirm, open_tab};
    use crate::config::Paths;
    use crate::protocol::Visibility;
    use crate::session::Session;
    use crate::test_support::*;
    use crate::view::SIDEBAR_WIDTH;
    use libghostty_vt::effect::ClipboardLocation;
    use rune::Runtime;
    use std::cell::Cell;
    use std::sync::atomic::Ordering;

    #[test]
    fn a_window_keeps_its_last_project() {
        let mut state = State::default();
        state
            .workspace
            .add_project(Project::new("/tmp".into(), vec![]));
        move_to_new_window(&mut state, 0, None);
        assert_eq!(state.workspace.projects.len(), 1);
        assert!(state.fleet.take(1).is_empty());
    }

    #[test]
    fn each_window_shows_only_its_own_active_session() {
        let mut runtime = workspace_runtime();
        move_to_new_window(&mut runtime.state, 1, None);
        let mut second = second_window(&runtime.state.fleet, 1);
        second.frame(3.0 / 60.0);
        let shown = |runtime: &Runtime<State>| {
            runtime
                .state
                .workspace
                .active_session()
                .map(|session| session.visibility())
        };
        assert_eq!(shown(&runtime), Some(Visibility::Shown));
        assert_eq!(shown(&second), Some(Visibility::Shown));
    }

    #[test]
    fn a_reloaded_config_reaches_every_window() {
        let mut runtime = workspace_runtime();
        move_to_new_window(&mut runtime.state, 1, None);
        let mut second = second_window(&runtime.state.fleet, 1);
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            dir: root.path().join("agentd"),
            ghostty_dir: root.path().join("ghostty"),
        };
        std::fs::create_dir_all(&paths.dir).unwrap();
        std::fs::write(paths.dir.join("config.toml"), "jump-command = \"j\"\n").unwrap();
        runtime.state.paths = Some(paths);
        press(&mut runtime, rune::Key::Command(','), command_shift());
        second.frame(2.0 / 60.0);
        let jump = |state: &State| state.config.jump_command.clone();
        assert_eq!(jump(&runtime.state).as_deref(), Some("j"));
        assert_eq!(jump(&second.state).as_deref(), Some("j"));
    }

    #[test]
    fn a_broken_config_at_launch_falls_back_and_shows_the_banner() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            dir: root.path().join("agentd"),
            ghostty_dir: root.path().join("ghostty"),
        };
        std::fs::create_dir_all(&paths.dir).unwrap();
        std::fs::write(paths.dir.join("config.toml"), "shel = 1\n").unwrap();
        let mut state = State {
            paths: Some(paths),
            ..State::default()
        };
        load_config(&mut state);
        assert_eq!(state.config, Config::default());
        assert!(
            state
                .config_error
                .as_deref()
                .is_some_and(|error| error.contains("shel"))
        );
    }

    #[test]
    fn a_broken_config_shows_a_banner_until_a_reload_succeeds_or_it_is_dismissed() {
        let mut runtime = workspace_runtime();
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            dir: root.path().join("agentd"),
            ghostty_dir: root.path().join("ghostty"),
        };
        std::fs::create_dir_all(&paths.dir).unwrap();
        let write = |text: &str| std::fs::write(paths.dir.join("config.toml"), text).unwrap();
        runtime.state.paths = Some(paths.clone());
        let reload = |runtime: &mut Runtime<State>| {
            press(runtime, rune::Key::Command(','), command_shift());
        };

        write("shel = 1\n");
        reload(&mut runtime);
        assert!(runtime.find("config-error").is_some());
        assert_eq!(runtime.state.config, Config::default());
        write("shell = \"/bin/zsh\"\n");
        reload(&mut runtime);
        assert!(runtime.find("config-error").is_none());

        write("shel = 1\n");
        reload(&mut runtime);
        click(&mut runtime, "config-error-dismiss");
        assert!(runtime.find("config-error").is_none());
    }

    #[test]
    fn a_moved_session_keeps_running_and_wakes_its_new_window() {
        let (first_waker, first_wakes) = counting_waker();
        let mut state = State {
            waker: Some(first_waker),
            ..State::default()
        };
        state
            .workspace
            .add_project(Project::new("/tmp".into(), vec![]));
        open_tab(&mut state, "slow", Some("sh -c 'sleep 0.5; echo moved'"));
        state
            .workspace
            .add_project(Project::new("/dev/site".into(), vec![]));
        state.workspace.select_project(0);
        move_to_new_window(&mut state, 0, None);

        let (second_waker, second_wakes) = counting_waker();
        let second_state = State {
            fleet: state.fleet.clone(),
            window: 1,
            waker: Some(second_waker),
            ..State::default()
        };
        second_state.fleet.register(1, std::sync::Arc::new(|| {}));
        let mut second = runtime_with(second_state);
        assert_eq!(second.state.workspace.projects.len(), 1);
        let before = first_wakes.load(Ordering::Relaxed);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut frame = 1.0;
        while !second
            .state
            .workspace
            .active_session()
            .is_some_and(|session| {
                let grid = session.grid();
                let text: String = grid.cells.iter().map(|cell| cell.character).collect();
                text.contains("moved")
            })
        {
            assert!(std::time::Instant::now() < deadline, "timed out");
            std::thread::sleep(std::time::Duration::from_millis(20));
            frame += 1.0;
            second.frame(frame / 60.0);
        }
        assert!(second_wakes.load(Ordering::Relaxed) > 0);
        assert_eq!(first_wakes.load(Ordering::Relaxed), before);
    }

    #[test]
    fn closing_a_window_gives_its_running_projects_back_to_the_main_window() {
        let (main_waker, _) = counting_waker();
        let mut main = State {
            waker: Some(main_waker),
            ..State::default()
        };
        main.workspace
            .add_project(Project::new("/dev/site".into(), vec![]));
        let mut second = State {
            fleet: main.fleet.clone(),
            window: 1,
            started: true,
            ..State::default()
        };
        second
            .workspace
            .add_project(Project::new("/tmp".into(), vec![]));
        open_tab(&mut second, "slow", Some("sh -c 'sleep 0.5; echo back'"));
        let session = second.workspace.active_session().map(Session::id);
        merge_into_main(&mut second);
        assert!(second.workspace.projects.is_empty());

        assert!(adopt_projects(&mut main));
        assert_eq!(main.workspace.projects.len(), 2);
        assert_eq!(main.workspace.active_session().map(Session::id), session);
        assert_eq!(
            main.workspace.active_session().map(Session::visibility),
            Some(Visibility::Shown)
        );
        assert!(!adopt_projects(&mut main));
    }

    #[test]
    fn closing_a_window_refuses_its_pending_clipboard_read() {
        let (main_waker, _) = counting_waker();
        let mut main = State {
            waker: Some(main_waker),
            ..State::default()
        };
        let mut second = State {
            fleet: main.fleet.clone(),
            window: 1,
            started: true,
            ..State::default()
        };
        second
            .workspace
            .add_project(Project::new("/tmp".into(), vec![]));
        open_tab(
            &mut second,
            "ask",
            Some(r#"sh -c 'printf "\\033]52;c;?\\007"; cat'"#),
        );
        let session = second.workspace.active_session().map(Session::id).unwrap();
        second.confirms.push(Confirm::ClipboardRead {
            session,
            location: ClipboardLocation::Standard,
        });
        merge_into_main(&mut second);

        assert!(adopt_projects(&mut main));
        let session = main.workspace.active_session().unwrap();
        wait_until(
            std::time::Duration::from_secs(5),
            || "the read was never refused".to_owned(),
            || {
                (session.grid().lines().iter())
                    .any(|line| line.contains("]52;c;"))
                    .then_some(())
            },
        );
    }

    #[test]
    fn a_closed_window_is_no_longer_revealed() {
        let mut closed = State {
            window: 1,
            ..State::default()
        };
        let revealed = Rc::new(Cell::new(false));
        closed.fleet.on_reveal(1, {
            let revealed = revealed.clone();
            move |_| revealed.set(true)
        });
        merge_into_main(&mut closed);

        closed.fleet.reveal(0);
        assert!(!revealed.get());
    }

    #[test]
    fn a_project_released_off_the_windows_edge_is_torn_off() {
        assert!(torn_off([-4.0, 100.0], SIDEBAR_WIDTH));
        assert!(torn_off([40.0, -4.0], SIDEBAR_WIDTH));
        assert!(!torn_off([SIDEBAR_WIDTH + 10.0, 100.0], SIDEBAR_WIDTH));
    }
}
