use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use rune::{Tx, Waker, WindowSpec};

use crate::app::{State, save_layout, window_app};
use crate::session::LogErr;
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
    while let Some(project) = state.workspace.take_project(0) {
        state.fleet.send(MAIN_WINDOW, project);
    }
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
    use crate::app::open_tab;
    use crate::session::{self, Session};
    use crate::test_support::*;
    use crate::view::SIDEBAR_WIDTH;
    use rune::Runtime;
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
        assert_eq!(shown(&runtime), Some(session::Visibility::Shown));
        assert_eq!(shown(&second), Some(session::Visibility::Shown));
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
        let mut second = second_window(&state.fleet, 1);
        second.state.waker = Some(second_waker);
        second.frame(1.0 / 60.0);
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
            Some(session::Visibility::Shown)
        );
        assert!(!adopt_projects(&mut main));
    }

    #[test]
    fn a_project_released_off_the_windows_edge_is_torn_off() {
        assert!(torn_off([-4.0, 100.0], SIDEBAR_WIDTH));
        assert!(torn_off([40.0, -4.0], SIDEBAR_WIDTH));
        assert!(!torn_off([SIDEBAR_WIDTH + 10.0, 100.0], SIDEBAR_WIDTH));
    }
}
