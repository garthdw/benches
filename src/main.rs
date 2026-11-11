mod config;
mod font;
mod input;
mod jump;
mod link;
mod notify;
mod render;
mod session;
mod sprite;
mod workspace;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use libghostty_vt::mouse;
use rune::tok;
use rune::{
    App, AppSpec, Chord, ClipboardContent, Color, Cursor, Cx, Drag, El, FileDialogOptions, Icon,
    Ime, Key, KeyEvent, Live, Mods, Phase, PointerButton, PointerEvent, PopupItem, PopupMenu,
    TextEdit, Tx, Waker, WheelEvent, WindowSpec, col, effect, el, icon, row, text, text_input,
};

use anyhow::Context;
use session::LogErr;
use session::{Attention, Event, Grid, PointerInput, PointerPhase, Retarget, Session, WheelInput};
use workspace::{
    Fleet, MAIN_WINDOW, Project, SavedWorkspace, Slot, Status, Tab, WindowId, Workspace,
};

const TERMINAL_MARK: u32 = 1;
const DRAWER_MARK: u32 = 2;
const OVERLAY_MARK: u32 = 3;
const TRAY_BAR_HEIGHT: f32 = 34.0;
/// Fixed, so the drawer's terminal keeps its size while the drawer slides.
const DRAWER_HEIGHT: f32 = 300.0;
const OVERLAY_INSET: f32 = 40.0;
const OVERLAY_PADDING: f32 = 6.0;
const DOT: f32 = 7.0;
/// Wider than the traffic lights, which sit over the sidebar.
const SIDEBAR_WIDTH: f32 = 200.0;
/// The sidebar's top padding too, so the traffic lights clear the first project.
const TAB_STRIP_HEIGHT: f32 = 36.0;
const TAB_MAX_WIDTH: f32 = 220.0;
const PALETTE_WIDTH: f32 = 520.0;
/// The collapsed sidebar: narrower than the traffic lights, which sit above its tabs.
const RAIL_WIDTH: f32 = 44.0;
const RAIL_TAB: f32 = 32.0;
static SIDEBAR_ICON: Icon = Icon {
    name: "sidebar",
    view: 24.0,
    body: r#"<rect x="3" y="4.5" width="18" height="15" rx="3" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M9.5 4.5v15" stroke="currentColor" stroke-width="1.6"/>"#,
};
const PALETTE_TOP: f32 = 96.0;
/// Longer than `terminate`'s grace before it kills, so stubborn processes are killed too.
const QUIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
/// Waits for typing to pause, as the lookup starts a login shell.
const JUMP_DEBOUNCE: Duration = Duration::from_millis(250);
const JUMP_TIMEOUT: Duration = Duration::from_secs(5);
/// Longer than a lookup, as `wt switch --create` runs the project's `pre-start` hooks first.
const WORKTREE_TIMEOUT: Duration = Duration::from_secs(120);
const SCENE_FIXTURE: &[u8] = b"\x1b[1;32magentd\x1b[0m fixture\r\n\x1b[31mred\x1b[0m \x1b[42;30m on green \x1b[0m \x1b[7minverse\x1b[0m\r\n$ ";
const SCENE_UNICODE: &str = concat!(
    "wide \u{4e16}\u{754c} kana \u{30ab}\u{30bf}\u{30ab}\u{30ca}|\r\n",
    "emoji \u{1F600}\u{1F680} zwj \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} vs16 \u{2764}\u{FE0F}|\r\n",
    "marks e\u{301}a\u{308} symbols \u{23FA} \u{23BF} \u{2713}|\r\n",
    "\u{250C}\u{2500}\u{252C}\u{2500}\u{2510} \u{256D}\u{2500}\u{256E} \u{2554}\u{2550}\u{2557} \u{2588}\u{2593}\u{2592}\u{2591} \u{2580}\u{2584}\u{258C}\u{2590}\r\n",
    "\u{2502} \u{253C} \u{2502} \u{2570}\u{2500}\u{256F} \u{255A}\u{2550}\u{255D} \u{E0B0}\u{E0B1}\u{E0B2}\u{E0B3}\r\n",
    "\x1b[1mbold\x1b[0m \x1b[3mitalic\x1b[0m \x1b[1;3mboth\x1b[0m \x1b[2mfaint\x1b[0m \x1b[9mstrike\x1b[0m \x1b[53mover\x1b[0m\r\n",
    "\x1b[4msingle\x1b[0m \x1b[4:2mdouble\x1b[0m \x1b[4:3;58;2;255;80;80mcurly\x1b[0m \x1b[4:4mdotted\x1b[0m \x1b[4:5mdashed\x1b[0m\r\n",
    "\x1b[41m\u{1F41F}\x1b[31;43m\u{E0C6}\x1b[30m ~ \x1b[33;42m\u{E0C6}\x1b[32;49m\u{E0C6}\x1b[0m\r\n",
    "\x1b[32m\u{F432}\x1b[0m $ "
);

/// Asks for a frame without a new view: terminal output only changes the effect slot's pixels.
#[derive(Clone)]
struct Repaint {
    live: Live<f32>,
    flip: Arc<AtomicBool>,
}

impl Default for Repaint {
    fn default() -> Self {
        Self {
            live: Live::new(0.0),
            flip: Arc::default(),
        }
    }
}

impl Repaint {
    /// Rune ignores a `Live` set that doesn't change its value, so each call flips it.
    fn waker(&self, wake: Waker) -> Waker {
        let repaint = self.clone();
        Arc::new(move || {
            let flipped = !repaint.flip.fetch_xor(true, Ordering::Relaxed);
            repaint.live.set(f32::from(flipped));
            wake();
        })
    }
}

#[derive(Default)]
pub struct State {
    waker: Option<Waker>,
    repaint: Repaint,
    fleet: Fleet,
    window: WindowId,
    workspace: Workspace,
    started: bool,
    /// Session id and scrollbar presence last built for the tab, the drawer and the overlay.
    drawn: [(u64, bool); 3],
    /// Sessions showing an attention dot, as the actors raise it without an event.
    alerting: Vec<u64>,
    /// The session that refused a paste, and the text; dismissed when keys move elsewhere.
    pending_paste: Option<(u64, String)>,
    /// The session whose close waits for confirmation; dismissed when keys move elsewhere.
    pending_close: Option<u64>,
    /// The project the pending close takes whole, rather than the focused tab.
    closing_project: Option<PathBuf>,
    hovered_project: Option<usize>,
    preedit: String,
    option_held: bool,
    caret: Option<[f32; 4]>,
    palette: TextEdit,
    palette_open: bool,
    palette_selected: usize,
    jump: JumpLookup,
    worktrees: Option<WorktreeChoice>,
    /// A `wt switch --create` running off the UI thread, answering with the new worktree's path.
    creation: Option<Creation>,
    /// Why the last worktree creation failed, shown in the command centre until the next edit.
    palette_error: Option<String>,
    sidebar_hidden: bool,
    config: config::Config,
    /// Unset in tests and scenes, which then read and write no files.
    paths: Option<config::Paths>,
}

/// The jump command's folder for the command centre's query.
#[derive(Default)]
struct JumpLookup {
    query: String,
    /// When to start the lookup, once typing pauses.
    due: Option<f64>,
    pending: Option<mpsc::Receiver<Option<PathBuf>>>,
    found: Option<PathBuf>,
}

struct Creation {
    result: mpsc::Receiver<anyhow::Result<PathBuf>>,
    cancel: jump::Cancel,
}

/// The repository worktrees the command centre offers after ⌘↵ on the jump command's folder.
#[derive(Default)]
struct WorktreeChoice {
    root: PathBuf,
    pending: Option<mpsc::Receiver<Vec<jump::Worktree>>>,
    found: Vec<jump::Worktree>,
}

impl State {
    /// The session keys go to.
    fn session(&self) -> Option<&Session> {
        self.workspace.focused_session()
    }

    fn window_waker(&self) -> Waker {
        let waker = self.waker.clone().unwrap_or_else(|| Arc::new(|| {}));
        self.repaint.waker(waker)
    }

    fn spawn_tab(&self, title: &str, command: Option<&str>, cwd: &Path) -> Tab {
        let retarget = Retarget::new(self.window_waker());
        let mut tab = match Session::spawn(command, cwd, &self.config, retarget.waker()) {
            Ok(session) => {
                let mut tab = Tab::new(title, session);
                tab.retarget = Some(retarget);
                tab
            }
            Err(error) => {
                let message = format!("failed to start {title}: {error:#}");
                let mut tab = Tab::new(title, Session::fixture(Grid::message(&message)));
                tab.status = Status::Failed(format!("{error:#}"));
                tab
            }
        };
        tab.command = command.map(str::to_owned);
        tab.cwd = Some(cwd.to_owned());
        tab
    }

    fn active_root(&self) -> Option<PathBuf> {
        self.workspace.project().map(|project| project.root.clone())
    }

    /// The worktrees whose branch and path contain the query's letters in order.
    fn visible_worktrees(&self) -> Vec<&jump::Worktree> {
        let query = self.palette.text.trim();
        self.worktrees
            .iter()
            .flat_map(|choice| &choice.found)
            .filter(|worktree| {
                let branch = worktree.branch.as_deref().unwrap_or_default();
                letters_in_order(&format!("{branch} {}", worktree.path.display()), query)
            })
            .collect()
    }

    fn jump_target(&self) -> Option<&Path> {
        self.jump
            .found
            .as_deref()
            .filter(|_| self.jump.query == self.palette.text.trim())
    }
}

fn open_tab(state: &mut State, title: &str, command: Option<&str>) {
    if let Some(root) = state.active_root() {
        let tab = state.spawn_tab(title, command, &root);
        state.workspace.add_tab(tab);
    }
}

/// Runs in the focused shell's folder, so a command typed for a subfolder works there.
fn open_tray(state: &mut State, command: &str) {
    let cwd = state.session().and_then(Session::cwd);
    if let Some(cwd) = cwd.or_else(|| state.active_root()) {
        let tab = state.spawn_tab(command, Some(command), &cwd);
        state.workspace.add_tray(tab);
    }
}

/// Stops a running tray process first and respawns it from `poll` once it exits.
fn restart_tray(state: &mut State, index: usize) {
    let Some(tab) = state.workspace.tray_mut(index) else {
        return;
    };
    if tab.status == Status::Running {
        tab.restart = true;
        tab.stop();
    } else {
        respawn(state, state.workspace.active, Slot::Tray(index));
    }
}

/// Starts the tab or tray process at `slot` afresh, with the command and folder it had.
fn respawn(state: &mut State, project: usize, slot: Slot) {
    let Some(project_ref) = state.workspace.projects.get(project) else {
        return;
    };
    let Some(old) = project_ref.tab(slot) else {
        return;
    };
    let cwd = old.cwd.clone().unwrap_or_else(|| project_ref.root.clone());
    let tab = state.spawn_tab(&old.title, old.command.as_deref(), &cwd);
    state.workspace.replace(project, slot, tab);
}

/// Starts a restored project's tabs once it is first shown; its tray waits for Restart.
fn launch_active(state: &mut State) {
    let active = state.workspace.active;
    let Some(project) = state.workspace.projects.get_mut(active) else {
        return;
    };
    if project.launched {
        return;
    }
    project.launched = true;
    for index in 0..project.tabs.len() {
        respawn(state, active, Slot::Tab(index));
    }
}

fn load_layout(state: &mut State) {
    let Some(paths) = &state.paths else {
        return;
    };
    let path = paths.state();
    let saved = config::read_optional(&path).and_then(|json| {
        json.map(|json| serde_json::from_str::<SavedWorkspace>(&json))
            .transpose()
            .with_context(|| format!("parse {}", path.display()))
    });
    if let Some(Some(saved)) = saved.log_err("restore layout") {
        state.sidebar_hidden = saved.sidebar_hidden;
        state.workspace = Workspace::restore(saved);
    }
}

/// Written to a temporary file and renamed into place, so a crash never leaves half a file.
fn save_layout(state: &State) -> anyhow::Result<()> {
    let Some(paths) = &state.paths else {
        return Ok(());
    };
    let path = paths.state();
    let layout = state.workspace.snapshot(state.sidebar_hidden);
    let Some(layout) = state.fleet.deposit(state.window, layout) else {
        return Ok(());
    };
    let json = serde_json::to_string_pretty(&layout)?;
    std::fs::create_dir_all(&paths.dir)
        .with_context(|| format!("create {}", paths.dir.display()))?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, json).with_context(|| format!("write {}", temporary.display()))?;
    std::fs::rename(&temporary, &path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

/// Closing the main window quits, as it did when it was the only one. Closing another gives its
/// projects, still running, back to the main window.
fn on_close(state: &mut State, tx: &mut Tx) {
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
fn on_quit(state: &mut State, _: &mut Tx) {
    save_layout(state).log_err("save layout");
    state.workspace.shutdown(QUIT_TIMEOUT);
}

fn tray_action(state: &mut State, index: usize, picked: Option<&str>) {
    match picked {
        Some("restart") => restart_tray(state, index),
        Some("stop") => {
            if let Some(tab) = state.workspace.tray_mut(index) {
                tab.stop();
            }
        }
        Some("remove") => {
            let open = state
                .workspace
                .project()
                .is_some_and(|project| project.drawer_shows(index));
            if !open {
                state.workspace.toggle_drawer(index);
            }
            state.workspace.focus_drawer(true);
            request_close(state);
        }
        _ => {}
    }
}

/// Closes the focused tab or drawer process, first asking if a program is running in it.
fn request_close(state: &mut State) {
    let Some((_, tab)) = state.workspace.focused_tab() else {
        return;
    };
    if tab.runs_program() {
        state.pending_close = Some(tab.session.id());
    } else {
        close_focused(state);
    }
}

fn close_focused(state: &mut State) {
    match state.workspace.focused_tab() {
        Some((Slot::Tab(_), _)) => state.workspace.close_tab(),
        Some((Slot::Tray(index), _)) => state.workspace.remove_tray(index),
        Some((Slot::Overlay, tab)) => {
            let session = tab.session.id();
            state.workspace.drop_overlay(session);
        }
        None => {}
    }
}

/// Closes project `index`, first asking if a program is running in any of its processes.
fn request_close_project(state: &mut State, index: usize) {
    let Some(project) = state.workspace.projects.get(index) else {
        return;
    };
    if project.runs_program()
        && let Some(session) = state.session()
    {
        state.pending_close = Some(session.id());
        state.closing_project = Some(project.root.clone());
    } else {
        state.workspace.close_project(index);
    }
}

fn resolve_close(state: &mut State, confirmed: bool) {
    let pending = state.pending_close.take();
    let project = state.closing_project.take();
    if confirmed && pending == state.session().map(Session::id) {
        match project.and_then(|root| state.workspace.project_index(&root)) {
            Some(index) => state.workspace.close_project(index),
            None => close_focused(state),
        }
    }
}

/// Starts lazygit in the project's folder the first time, and after it has ended.
fn toggle_lazygit(state: &mut State) {
    let Some(project) = state.workspace.project() else {
        return;
    };
    if project.overlay_open {
        state.workspace.hide_overlay();
        return;
    }
    let running = project
        .overlay
        .as_ref()
        .is_some_and(|tab| tab.status == Status::Running);
    let tab = (!running).then(|| state.spawn_tab("lazygit", Some("lazygit"), &project.root));
    state.workspace.show_overlay(tab);
}

fn toggle_last_drawer(state: &mut State) {
    if let Some(project) = state.workspace.project() {
        let index = project.drawer.min(project.tray.len().saturating_sub(1));
        state.workspace.toggle_drawer(index);
    }
}

fn open_project(state: &mut State, root: PathBuf) {
    if let Some(index) = state.workspace.project_index(&root) {
        state.workspace.select_project(index);
        return;
    }
    let tabs = state
        .config
        .tabs
        .iter()
        .map(|spec| state.spawn_tab(&spec.title, spec.command.as_deref(), &root))
        .collect();
    state.workspace.add_project(Project::new(root, tabs));
}

/// Hands project `index` to a new window, with its processes still running. A window's last
/// project stays, as it would leave that window empty.
fn move_to_new_window(state: &mut State, index: usize) {
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
    rune::open_window(WindowSpec::from_app(format!("agentd-{window}"), app));
}

/// Takes the projects other windows handed over, pointing their sessions at this window.
fn adopt_projects(state: &mut State) -> bool {
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

fn pick_folder(tx: &mut Tx, replace_active: bool) {
    let options = FileDialogOptions {
        title: if replace_active {
            "Change project folder"
        } else {
            "Open project"
        }
        .into(),
        ..FileDialogOptions::default()
    };
    tx.open_folder(options, move |state: &mut State, result, _| match result {
        Ok(Some(root)) if replace_active => state.workspace.change_root(root),
        Ok(Some(root)) => open_project(state, root),
        Ok(None) => {}
        Err(error) => eprintln!("agentd: open folder: {error}"),
    });
}

fn pick_new_tab(state: &mut State, picked: Option<&str>, tx: &mut Tx) {
    match picked {
        Some("shell") => run_action(state, &Action::Shell, tx),
        Some("lazygit") => run_action(state, &Action::Lazygit, tx),
        Some("palette" | "tray") => open_palette(state, tx),
        _ => {}
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Action {
    Shell,
    Lazygit,
    OpenProject,
    ChangeFolder,
    MoveToNewWindow,
    Jump(PathBuf),
    SwitchProject(usize, String),
    SwitchTab(usize, usize, String),
    RestartTray(usize, String),
    StopTray(usize, String),
    CreateWorktree(String),
    Run(String),
    Tray(String),
}

impl Action {
    fn label(&self) -> String {
        match self {
            Action::Shell => "New shell tab".to_owned(),
            Action::Lazygit => "Toggle lazygit".to_owned(),
            Action::OpenProject => "Open project…".to_owned(),
            Action::ChangeFolder => "Change project folder…".to_owned(),
            Action::MoveToNewWindow => "Move project to new window".to_owned(),
            Action::Jump(root) => format!("Open project {}", root.display()),
            Action::SwitchProject(_, name) => format!("Switch to project {name}"),
            Action::SwitchTab(_, _, name) => format!("Switch to tab {name}"),
            Action::RestartTray(_, title) => format!("Restart {title}"),
            Action::StopTray(_, title) => format!("Stop {title}"),
            Action::CreateWorktree(branch) => format!("Create worktree “{branch}”"),
            Action::Run(command) => format!("Run “{command}” in a new tab"),
            Action::Tray(command) => format!("Run “{command}” in the tray"),
        }
    }
}

fn letters_in_order(label: &str, query: &str) -> bool {
    let label = label.to_lowercase();
    let mut letters = label.chars();
    query
        .to_lowercase()
        .chars()
        .all(|wanted| letters.any(|letter| letter == wanted))
}

/// Open projects and tabs whose names contain the query's letters in order, as switch actions
/// paired with their names. The jump folder's project is left out, as that action opens it.
fn switch_actions(
    query: &str,
    jump: Option<&Path>,
    workspace: &Workspace,
) -> Vec<(String, Action)> {
    let mut found = Vec::new();
    for (index, project) in workspace.projects.iter().enumerate() {
        if Some(project.root.as_path()) != jump {
            found.push((
                project.name.clone(),
                Action::SwitchProject(index, project.name.clone()),
            ));
        }
        for (tab_index, tab) in project.tabs.iter().enumerate() {
            let name = format!("{} ({})", tab.label(), project.name);
            found.push((
                tab.label().to_owned(),
                Action::SwitchTab(index, tab_index, name),
            ));
        }
    }
    found.retain(|(name, _)| letters_in_order(name, query));
    found
}

/// The fixed actions whose labels contain the query's letters in order, then the jump command's
/// folder, then running the query, so Enter on a typed command always runs it. Below those come
/// the open projects and tabs matching the query, exact names first, then the active project's
/// tray restarts and stops. A query of `wt` and one branch name puts creating that worktree
/// first. Everything but the fixed actions appears only once something is typed.
fn palette_actions(query: &str, jump: Option<&Path>, workspace: &Workspace) -> Vec<Action> {
    let query = query.trim();
    let needle = query.to_lowercase();
    let mut actions: Vec<Action> = create_worktree_action(query, workspace)
        .into_iter()
        .collect();
    actions.extend(
        [
            Action::Shell,
            Action::Lazygit,
            Action::OpenProject,
            Action::ChangeFolder,
            Action::MoveToNewWindow,
        ]
        .into_iter()
        .filter(|action| *action != Action::ChangeFolder || workspace.project().is_some())
        .filter(|action| *action != Action::MoveToNewWindow || workspace.projects.len() > 1)
        .filter(|action| letters_in_order(&action.label(), &needle)),
    );
    actions.extend(jump.map(|root| Action::Jump(root.to_owned())));
    if query.is_empty() {
        return actions;
    }
    actions.push(Action::Run(query.to_owned()));
    actions.push(Action::Tray(query.to_owned()));
    let (exact, partial): (Vec<_>, Vec<_>) = switch_actions(&needle, jump, workspace)
        .into_iter()
        .partition(|(name, _)| name.to_lowercase() == needle);
    actions.extend(exact.into_iter().chain(partial).map(|(_, action)| action));
    let tray = workspace.project().map_or(&[][..], |project| &project.tray);
    for (index, tab) in tray.iter().enumerate() {
        let restart = Action::RestartTray(index, tab.label().to_owned());
        let stop = Action::StopTray(index, tab.label().to_owned());
        let stoppable = tab.status == Status::Running;
        actions.extend(
            [Some(restart), stoppable.then_some(stop)]
                .into_iter()
                .flatten()
                .filter(|action| letters_in_order(&action.label(), &needle)),
        );
    }
    actions
}

/// `wt <branch>` with exactly one branch name, which `wt` could not take for an option.
fn create_worktree_action(query: &str, workspace: &Workspace) -> Option<Action> {
    let branch = query.strip_prefix("wt ")?.trim();
    let is_branch =
        !branch.is_empty() && !branch.starts_with('-') && !branch.contains(char::is_whitespace);
    (is_branch && workspace.project().is_some()).then(|| Action::CreateWorktree(branch.to_owned()))
}

fn open_palette(state: &mut State, tx: &mut Tx) {
    state.palette = TextEdit::new("");
    state.palette_open = true;
    state.palette_selected = 0;
    state.worktrees = None;
    state.palette_error = None;
    tx.focus("palette-input", false);
}

/// Keeps the command centre open until `wt` answers, as its hooks can take a while.
fn create_worktree(state: &mut State, branch: &str) {
    let Some(root) = state.active_root() else {
        return;
    };
    let (sender, receiver) = mpsc::channel();
    let cancel = jump::Cancel::default();
    state.creation = Some(Creation {
        result: receiver,
        cancel: cancel.clone(),
    });
    state.palette_open = true;
    state.palette_error = None;
    let (shell, branch) = (state.config.shell.clone(), branch.to_owned());
    let waker = state.waker.clone();
    std::thread::spawn(move || {
        let created = jump::create_worktree(&shell, &root, &branch, WORKTREE_TIMEOUT, &cancel);
        if sender.send(created).is_ok()
            && let Some(waker) = waker
        {
            waker();
        }
    });
}

fn poll_creation(state: &mut State) -> bool {
    let Some(created) = state
        .creation
        .as_ref()
        .and_then(|creation| creation.result.try_recv().ok())
    else {
        return false;
    };
    state.creation = None;
    match created {
        Ok(path) => {
            state.palette_open = false;
            open_project(state, path);
        }
        Err(error) => {
            state.palette_open = true;
            state.palette_error = Some(format!("{error:#}"));
        }
    }
    true
}

fn run_palette(state: &mut State, index: usize, tx: &mut Tx) {
    let actions = palette_actions(&state.palette.text, state.jump_target(), &state.workspace);
    let Some(action) = actions.get(index.min(actions.len().saturating_sub(1))) else {
        return;
    };
    state.palette_open = false;
    run_action(state, action, tx);
}

fn run_action(state: &mut State, action: &Action, tx: &mut Tx) {
    match action {
        Action::Shell => open_tab(state, "shell", None),
        Action::Lazygit => toggle_lazygit(state),
        Action::OpenProject => pick_folder(tx, false),
        Action::ChangeFolder => pick_folder(tx, true),
        Action::MoveToNewWindow => move_to_new_window(state, state.workspace.active),
        Action::Jump(root) => open_project(state, root.clone()),
        Action::SwitchProject(project, _) => state.workspace.select_project(*project),
        Action::SwitchTab(project, tab, _) => {
            state.workspace.select_project(*project);
            state.workspace.select_tab(*tab);
        }
        Action::RestartTray(index, _) => tray_action(state, *index, Some("restart")),
        Action::StopTray(index, _) => tray_action(state, *index, Some("stop")),
        Action::CreateWorktree(branch) => create_worktree(state, branch),
        Action::Run(command) => open_tab(state, command, Some(command)),
        Action::Tray(command) => open_tray(state, command),
    }
}

fn list_worktrees(state: &mut State, root: PathBuf, tx: &mut Tx) {
    let (sender, receiver) = mpsc::channel();
    state.worktrees = Some(WorktreeChoice {
        root: root.clone(),
        pending: Some(receiver),
        found: Vec::new(),
    });
    state.palette = TextEdit::new("");
    state.palette_selected = 0;
    tx.focus("palette-input", false);
    let shell = state.config.shell.clone();
    let waker = state.waker.clone();
    std::thread::spawn(move || {
        let found = jump::worktrees(&shell, &root, JUMP_TIMEOUT)
            .log_err("wt list")
            .unwrap_or_default();
        if sender.send(found).is_ok()
            && let Some(waker) = waker
        {
            waker();
        }
    });
}

fn poll_worktrees(state: &mut State) -> bool {
    let Some(choice) = &mut state.worktrees else {
        return false;
    };
    let Some(found) = choice
        .pending
        .as_ref()
        .and_then(|pending| pending.try_recv().ok())
    else {
        return false;
    };
    choice.pending = None;
    choice.found = found;
    if choice.found.is_empty() {
        let root = choice.root.clone();
        state.palette_open = false;
        open_project(state, root);
    }
    true
}

/// Whether the key was one of the list's; typing edits the palette's input, which filters it.
fn worktree_key(state: &mut State, event: &KeyEvent) -> bool {
    let last = state.visible_worktrees().len().saturating_sub(1);
    match event.key {
        Key::Escape => state.palette_open = false,
        Key::Up => state.palette_selected = state.palette_selected.min(last).saturating_sub(1),
        Key::Down => state.palette_selected = (state.palette_selected + 1).min(last),
        Key::Enter => open_worktree(state, state.palette_selected.min(last)),
        _ => return false,
    }
    true
}

fn open_worktree(state: &mut State, index: usize) {
    let Some(worktree) = state.visible_worktrees().get(index).copied() else {
        return;
    };
    let path = worktree.path.clone();
    state.palette_open = false;
    open_project(state, path);
}

fn palette_key(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    if let Some(creation) = &state.creation {
        if event.key == Key::Escape {
            creation.cancel.cancel().log_err("cancel wt switch");
            state.creation = None;
            state.palette_open = false;
        }
        return true;
    }
    if state.worktrees.is_some() {
        return worktree_key(state, event);
    }
    let actions = palette_actions(&state.palette.text, state.jump_target(), &state.workspace);
    let last = actions.len().saturating_sub(1);
    match event.key {
        Key::Escape => state.palette_open = false,
        Key::Up => state.palette_selected = state.palette_selected.min(last).saturating_sub(1),
        Key::Down => state.palette_selected = (state.palette_selected + 1).min(last),
        Key::Enter
            if event.mods.cmd
                && let Some(Action::Jump(root)) = actions.get(state.palette_selected.min(last)) =>
        {
            list_worktrees(state, root.clone(), tx);
        }
        Key::Enter if event.mods.cmd => {
            let query = state.palette.text.trim().to_owned();
            if !query.is_empty() {
                state.palette_open = false;
                open_tray(state, &query);
            }
        }
        Key::Enter => run_palette(state, state.palette_selected, tx),
        _ => {
            // A poll after the edit lands starts the jump lookup's debounce.
            tx.wake_after(Duration::ZERO);
            state.palette_error = None;
            return false;
        }
    }
    true
}

/// Looks the command centre's query up with the jump command once typing pauses.
fn poll_jump(state: &mut State, now: f64, tx: &mut Tx) -> bool {
    let Some(command) = &state.config.jump_command else {
        return false;
    };
    let query = if state.palette_open {
        state.palette.text.trim()
    } else {
        ""
    };
    let jump = &mut state.jump;
    let mut changed = false;
    if query != jump.query {
        changed = jump.found.take().is_some();
        jump.query = query.to_owned();
        jump.pending = None;
        jump.due = (!query.is_empty()).then(|| {
            tx.wake_after(JUMP_DEBOUNCE);
            now + JUMP_DEBOUNCE.as_secs_f64()
        });
    }
    if jump.due.is_some_and(|due| now >= due) {
        jump.due = None;
        let (sender, receiver) = mpsc::channel();
        jump.pending = Some(receiver);
        let (shell, command, query) = (
            state.config.shell.clone(),
            command.clone(),
            jump.query.clone(),
        );
        let waker = state.waker.clone();
        std::thread::spawn(move || {
            let found = jump::lookup(&shell, &command, &query, JUMP_TIMEOUT)
                .log_err("jump command")
                .flatten();
            if sender.send(found).is_ok()
                && let Some(waker) = waker
            {
                waker();
            }
        });
    }
    if let Some(found) = jump
        .pending
        .as_ref()
        .and_then(|pending| pending.try_recv().ok())
    {
        jump.pending = None;
        jump.found = found;
        changed = true;
    }
    changed
}

/// Workspace shortcuts. These keys never reach the terminal.
fn shortcut(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    if !matches!(event.key, Key::Command(_)) && !event.mods.ctrl {
        return false;
    }
    let pressed = |binding: &str| pressed(binding, event, tx);
    let digit = |prefix: &str| (1..=9usize).find(|digit| pressed(&format!("{prefix}{digit}")));
    if pressed("mod+n") {
        pick_folder(tx, false);
    } else if pressed("mod+p") {
        open_palette(state, tx);
    } else if pressed("mod+b") {
        state.sidebar_hidden = !state.sidebar_hidden;
    } else if pressed("mod+j") {
        toggle_last_drawer(state);
    } else if pressed("mod+g") {
        toggle_lazygit(state);
    } else if pressed("mod+t") {
        run_action(state, &Action::Shell, tx);
    } else if pressed("mod+w") {
        request_close(state);
    } else if pressed("mod+shift+[") {
        state.workspace.cycle_tab(-1);
    } else if pressed("mod+shift+]") {
        state.workspace.cycle_tab(1);
    } else if let Some(digit) = digit("ctrl+") {
        let last = state.workspace.projects.len().saturating_sub(1);
        state.workspace.select_project((digit - 1).min(last));
    } else if let Some(digit) = digit("mod+") {
        if let Some(project) = state.workspace.project() {
            let last = project.tabs.len().saturating_sub(1);
            state.workspace.select_tab((digit - 1).min(last));
        }
    } else {
        return false;
    }
    true
}

fn pressed(binding: &str, event: &KeyEvent, tx: &Tx) -> bool {
    Chord::parse(binding).is_ok_and(|chord| chord.matches_tx(event, tx))
}

fn send_pointer(
    session: Option<&Session>,
    phase: PointerPhase,
    button: Option<mouse::Button>,
    x: f32,
    y: f32,
    mods: Mods,
) {
    if let Some(session) = session {
        session.pointer(PointerInput {
            phase,
            button,
            x,
            y,
            mods: input::vt_mods(mods),
        });
    }
}

/// The cursor cell in points within the terminal surface, or `None` before the first layout.
fn caret_rect(grid: &Grid) -> Option<[f32; 4]> {
    let (x, y) = grid.cursor?;
    let metrics = grid.metrics;
    if metrics.cell_width == 0 {
        return None;
    }
    let origin = metrics.origin([0.0; 4]);
    Some(metrics.to_points(metrics.cell_rect(origin, x, y)))
}

fn resolve_paste(state: &mut State, confirmed: bool) {
    if let Some((pending, text)) = state.pending_paste.take()
        && confirmed
        && let Some(session) = state.session()
        && session.id() == pending
    {
        session.paste(text, true);
    }
}

fn dot(color: Color) -> El {
    el().size(DOT, DOT).radius(DOT / 2.0).fill(color)
}

fn pointer_phase(phase: Phase) -> PointerPhase {
    match phase {
        Phase::Down => PointerPhase::Press,
        Phase::Move => PointerPhase::Drag,
        Phase::Up => PointerPhase::Release,
    }
}

/// Pressing the drawer's terminal gives it the keys, and pressing the tab's takes them back.
/// The overlay (`None`) always has them while shown.
fn focus_on_press(state: &mut State, drawer: Option<bool>, phase: PointerPhase, tx: &mut Tx) {
    if phase == PointerPhase::Press
        && let Some(drawer) = drawer
        && state
            .workspace
            .project()
            .is_some_and(|project| project.drawer_focused != drawer)
    {
        state.workspace.focus_drawer(drawer);
    } else {
        tx.live_only();
    }
}

/// The right padding, where the thumb is drawn, while there is history to scroll.
fn scrollbar_strip(
    key: &str,
    drawer: Option<bool>,
    pick: fn(&Workspace) -> Option<&Session>,
) -> El {
    el().key(format!("{key}-scrollbar"))
        .pin(None, Some(0.0), Some(0.0), Some(0.0))
        .w(render::PADDING_POINTS)
        .cursor(Cursor::Default)
        .on_hover_change(move |state: &mut State, hovered, tx: &mut Tx| {
            if let Some(session) = pick(&state.workspace) {
                session.hover_scrollbar(hovered);
            }
            tx.live_only();
        })
        .on_drag(move |state: &mut State, drag: &mut Drag, tx: &mut Tx| {
            let phase = pointer_phase(drag.phase);
            focus_on_press(state, drawer, phase, tx);
            if let Some(session) = pick(&state.workspace) {
                session.drag_scrollbar(phase, drag.y);
            }
        })
}

/// A terminal drawn into `mark`, taking pointer input for the session `pick` returns.
/// Pressing it sends keys to it, so the drawer and the tab each take focus by click.
fn terminal_surface(
    mark: u32,
    key: &str,
    workspace: &Workspace,
    pick: fn(&Workspace) -> Option<&Session>,
) -> El {
    let drawer = match mark {
        OVERLAY_MARK => None,
        _ => Some(mark == DRAWER_MARK),
    };
    let scrollable = pick(workspace).is_some_and(|session| session.grid().scrollbar.is_some());
    let cursor = if pick(workspace).is_some_and(Session::link_hovered) {
        Cursor::Pointer
    } else {
        Cursor::Text
    };
    effect(mark)
        .key(key)
        .cursor(cursor)
        .pointer_buttons([PointerButton::Middle])
        .children(scrollable.then(|| scrollbar_strip(key, drawer, pick)))
        .on_drag(move |state: &mut State, drag: &mut Drag, tx: &mut Tx| {
            let phase = pointer_phase(drag.phase);
            let button = match tx.button() {
                PointerButton::Middle => mouse::Button::Middle,
                _ => mouse::Button::Left,
            };
            focus_on_press(state, drawer, phase, tx);
            let session = pick(&state.workspace);
            send_pointer(session, phase, Some(button), drag.x, drag.y, tx.mods());
        })
        .on_secondary_press_local(move |state: &mut State, event: PointerEvent, tx: &mut Tx| {
            // Rune reports the right button once, with no separate release.
            for phase in [PointerPhase::Press, PointerPhase::Release] {
                let button = Some(mouse::Button::Right);
                let session = pick(&state.workspace);
                send_pointer(session, phase, button, event.x, event.y, tx.mods());
            }
        })
        .on_pointer_move_local(move |state: &mut State, event: PointerEvent, tx: &mut Tx| {
            let session = pick(&state.workspace);
            send_pointer(
                session,
                PointerPhase::Move,
                None,
                event.x,
                event.y,
                tx.mods(),
            );
            tx.live_only();
        })
        .on_wheel(move |state: &mut State, event: WheelEvent, tx: &mut Tx| {
            if let Some(session) = pick(&state.workspace) {
                session.wheel(WheelInput {
                    dy: event.dy,
                    x: event.x,
                    y: event.y,
                    mods: input::vt_mods(event.mods),
                });
            }
            tx.live_only();
            true
        })
}

fn status_text(status: &Status) -> &str {
    match status {
        Status::Running => "running",
        Status::Stopped => "stopped",
        Status::Exited => "exited",
        Status::Failed(reason) => reason,
    }
}

fn tray_menu() -> PopupMenu {
    PopupMenu::new()
        .item(PopupItem::new("restart", "Restart"))
        .item(PopupItem::new("stop", "Stop"))
        .item(PopupItem::new("remove", "Remove"))
}

fn view(state: &State, cx: &mut Cx<State>) -> El {
    let theme = tok::theme(true);
    let accent = cx.accent();
    let attention_dot = |attention: Attention| match attention {
        Attention::None => None,
        Attention::Alert => Some(dot(accent)),
    };
    let status_dot = |status: &Status| {
        dot(match status {
            Status::Running => rune::color::oklch(0.74, 0.17, 150.0),
            Status::Failed(_) => rune::color::oklch(0.66, 0.2, 25.0),
            Status::Stopped | Status::Exited => theme.text_secondary,
        })
    };
    let button = |key: String, label: &str, active: bool| {
        row()
            .key(key)
            .role("button")
            .label(label)
            .items_center()
            .gap(6.0)
            .fill(if active {
                theme.control_off
            } else {
                Color::TRANSPARENT
            })
    };
    let pill = |key: String, label: &str, active: bool, marker: Option<El>| {
        button(key, label, active)
            .px(10.0)
            .py(4.0)
            .radius(6.0)
            .children(marker)
            .child(text(label.to_owned()).color(theme.text).truncate())
    };
    let rail = state.sidebar_hidden && state.workspace.projects.len() > 1;
    let (sidebar_width, sidebar_padding) = match (state.sidebar_hidden, rail) {
        (false, _) => (SIDEBAR_WIDTH, 8.0),
        (true, true) => (RAIL_WIDTH, 6.0),
        (true, false) => (0.0, 0.0),
    };
    let projects = state
        .workspace
        .projects
        .iter()
        .enumerate()
        .filter(|_| sidebar_width > 0.0)
        .map(|(index, project)| {
            let key = format!("project-{index}");
            let menu_key = key.clone();
            let active = index == state.workspace.active;
            let shows_close = active || state.hovered_project == Some(index);
            let project_tab = if rail {
                let mut initials = project.name.chars();
                let initials: String = initials
                    .next()
                    .into_iter()
                    .flat_map(char::to_uppercase)
                    .chain(initials.next())
                    .collect();
                let marker = attention_dot(project.attention())
                    .map(|marker| marker.absolute(RAIL_TAB - DOT - 3.0, 3.0));
                button(key, &project.name, active)
                    .size(RAIL_TAB, RAIL_TAB)
                    .center()
                    .radius(8.0)
                    .child(text(initials).color(theme.text))
                    .children(marker)
            } else {
                pill(
                    key,
                    &project.name,
                    active,
                    attention_dot(project.attention()),
                )
                .w_full()
                .children(shows_close.then(|| el().grow(1.0)))
                .children(shows_close.then(|| {
                    el().key(format!("project-close-{index}"))
                        .role("button")
                        .label("Close project")
                        .px(4.0)
                        .radius(4.0)
                        .child(text("×").color(theme.text_secondary))
                        .on_press(move |state: &mut State| request_close_project(state, index))
                }))
            };
            project_tab
                .on_hover_change(move |state: &mut State, hovered, _: &mut Tx| {
                    if hovered {
                        state.hovered_project = Some(index);
                    } else if state.hovered_project == Some(index) {
                        state.hovered_project = None;
                    }
                })
                .on_press(move |state: &mut State| state.workspace.select_project(index))
                .on_secondary_press(move |_: &mut State, _, tx: &mut Tx| {
                    let menu = PopupMenu::new().item(PopupItem::new("move", "Move to new window"));
                    tx.popup_menu(menu, &menu_key, move |state: &mut State, picked, _| {
                        if picked == Some("move") {
                            move_to_new_window(state, index);
                        }
                    });
                })
        });
    // Stays in the tree so its width springs between expanded, rail and gone.
    let sidebar = col()
        .key("sidebar")
        .w(sidebar_width)
        .h_full()
        .gap(2.0)
        .pt(TAB_STRIP_HEIGHT)
        .px(sidebar_padding)
        .clip_rounded(0.0)
        .fill(theme.card)
        .children(projects);
    let tabs = state.workspace.project().map_or_else(Vec::new, |project| {
        project
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                pill(
                    format!("tab-{index}"),
                    tab.label(),
                    index == project.active_tab,
                    attention_dot(tab.session.attention()),
                )
                .max_w(TAB_MAX_WIDTH)
                .on_press(move |state: &mut State| state.workspace.select_tab(index))
            })
            .collect()
    });
    let sidebar_toggle = el()
        .key("sidebar-toggle")
        .role("button")
        .label("Toggle sidebar")
        .padding(6.0)
        .radius(6.0)
        .child(icon(&SIDEBAR_ICON, 16.0).color(theme.text_secondary))
        .on_press(|state: &mut State| state.sidebar_hidden = !state.sidebar_hidden);
    let new_tab = state.workspace.project().map(|_| {
        pill("new-tab".to_owned(), "+", false, None).on_press_tx(|_: &mut State, tx: &mut Tx| {
            let menu = PopupMenu::new()
                .item(PopupItem::new("shell", "Shell"))
                .item(PopupItem::new("lazygit", "lazygit"))
                .item(PopupItem::new("tray", "Background command…"))
                .item(PopupItem::new("palette", "Command centre…"));
            tx.popup_menu(menu, "new-tab", pick_new_tab);
        })
    });
    let mut overlays = Vec::new();
    if state.workspace.projects.is_empty() {
        overlays.push(
            col()
                .key("empty-hint")
                .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .center()
                .child(text("Open a folder with ⌘N, or drop one here").color(theme.text_secondary)),
        );
    }
    // The caret, composition and paste bar belong to whichever terminal takes keys.
    let mut focus_overlays = Vec::new();
    if let Some([x, y, width, height]) = state.caret {
        // winit routes Option through the input method while one is active, which eats Alt chords.
        if !state.option_held || !state.preedit.is_empty() {
            focus_overlays.push(
                el().key("caret")
                    .absolute(x, y)
                    .size(width, height)
                    .ime_cursor(),
            );
        }
        if !state.preedit.is_empty() {
            focus_overlays.push(
                el().key("preedit")
                    .absolute(x, y)
                    .fill(theme.bg)
                    .child(text(state.preedit.clone()).color(theme.text)),
            );
        }
    }
    let confirm_bar = |key: &'static str,
                       message: String,
                       action: &'static str,
                       resolve: fn(&mut State, bool)| {
        let button = |key: String, label: &'static str| {
            el().key(key)
                .role("button")
                .label(label)
                .px(10.0)
                .py(4.0)
                .radius(6.0)
                .fill(theme.bg)
                .child(text(label).color(theme.text))
        };
        row()
            .key(key)
            .absolute(12.0, 12.0)
            .gap(8.0)
            .items_center()
            .px(10.0)
            .py(6.0)
            .radius(6.0)
            .fill(theme.control_off)
            .child(text(message).color(theme.text))
            .child(
                button(format!("{key}-yes"), action)
                    .on_press(move |state: &mut State| resolve(state, true)),
            )
            .child(
                button(format!("{key}-no"), "Cancel")
                    .on_press(move |state: &mut State| resolve(state, false)),
            )
    };
    if state.pending_paste.is_some() {
        focus_overlays.push(confirm_bar(
            "paste-confirm",
            "This paste could run commands".to_owned(),
            "Paste",
            resolve_paste,
        ));
    }
    if state.pending_close.is_some() {
        let place = state
            .closing_project
            .as_ref()
            .and_then(|root| state.workspace.project_index(root))
            .and_then(|index| state.workspace.projects.get(index))
            .map_or("here".to_owned(), |project| format!("in {}", project.name));
        focus_overlays.push(confirm_bar(
            "close-confirm",
            format!("A program is still running {place}"),
            "Close",
            resolve_close,
        ));
    }
    if let Some((_, tab)) = state.workspace.focused_tab()
        && tab.status != Status::Running
    {
        focus_overlays.push(
            row()
                .key("restart-hint")
                .absolute(12.0, 12.0)
                .px(10.0)
                .py(6.0)
                .radius(6.0)
                .fill(theme.control_off)
                .child(
                    text(format!("{} · Enter restarts", status_text(&tab.status)))
                        .color(theme.text),
                ),
        );
    }
    let project = state.workspace.project();
    let drawer_focused = project.is_some_and(|project| project.drawer_focused);
    let mut drawer_overlays = Vec::new();
    let mut overlay_overlays = Vec::new();
    if project.is_some_and(|project| project.overlay_open) {
        overlay_overlays = focus_overlays;
    } else if drawer_focused {
        drawer_overlays = focus_overlays;
    } else {
        overlays.extend(focus_overlays);
    }
    let tray_bar = project
        .filter(|project| !project.tray.is_empty())
        .map(|project| {
            let chips = project.tray.iter().enumerate().map(|(index, tab)| {
                let key = format!("chip-{index}");
                let open = project.drawer_shows(index);
                pill(
                    key.clone(),
                    tab.label(),
                    open,
                    Some(status_dot(&tab.status)),
                )
                .max_w(TAB_MAX_WIDTH)
                .on_press(move |state: &mut State| state.workspace.toggle_drawer(index))
                .on_secondary_press(move |_: &mut State, _, tx: &mut Tx| {
                    tx.popup_menu(tray_menu(), &key, move |state: &mut State, picked, _| {
                        tray_action(state, index, picked);
                    });
                })
            });
            row()
                .key("tray-bar")
                .w_full()
                .h(TRAY_BAR_HEIGHT)
                .gap(4.0)
                .px(8.0)
                .items_center()
                .fill(theme.card)
                .children(chips)
                .child(
                    pill("tray-new".to_owned(), "+", false, None)
                        .on_press_tx(|state: &mut State, tx: &mut Tx| open_palette(state, tx)),
                )
        });
    // Stays in the tree with the last tray process it showed, so it can slide out as well as in.
    let drawer = project.map(|project| {
        let open = project.drawer_tab().is_some();
        let header = project.tray.get(project.drawer).map(|tab| {
            let index = project.drawer;
            let action = |key: &str, label: &str, picked: &'static str| {
                pill(key.to_owned(), label, false, None).on_press(move |state: &mut State| {
                    tray_action(state, index, Some(picked));
                })
            };
            row()
                .w_full()
                .h(TRAY_BAR_HEIGHT)
                .gap(8.0)
                .px(8.0)
                .items_center()
                .child(status_dot(&tab.status))
                .child(text(tab.label().to_owned()).color(theme.text).truncate())
                .child(text(status_text(&tab.status).to_owned()).color(theme.text_secondary))
                .child(el().grow(1.0))
                .child(action("drawer-restart", "Restart", "restart"))
                .child(action("drawer-stop", "Stop", "stop"))
                .child(
                    pill("drawer-close".to_owned(), "Close", false, None)
                        .on_press(move |state: &mut State| state.workspace.toggle_drawer(index)),
                )
        });
        col()
            .key("drawer")
            .pin(
                Some(0.0),
                None,
                Some(0.0),
                Some(if tray_bar.is_some() {
                    TRAY_BAR_HEIGHT
                } else {
                    0.0
                }),
            )
            .h(if open { DRAWER_HEIGHT } else { 0.0 })
            .justify_end()
            .clip_rounded(0.0)
            .child(
                col()
                    .key("drawer-panel")
                    .w_full()
                    .h(DRAWER_HEIGHT)
                    .fill(theme.card)
                    .border(1.0, theme.card_border)
                    .children(header)
                    .child(
                        terminal_surface(
                            DRAWER_MARK,
                            "drawer-terminal",
                            &state.workspace,
                            Workspace::drawer_session,
                        )
                        .w_full()
                        .grow(1.0)
                        .min_h(0.0)
                        .children(drawer_overlays),
                    ),
            )
    });
    let overlay = project.and_then(Project::overlay_tab).map(|_| {
        col()
            .key("overlay-layer")
            .pin(Some(0.0), Some(TAB_STRIP_HEIGHT), Some(0.0), Some(0.0))
            .padding(OVERLAY_INSET)
            .fill(theme.shadow_soft)
            .on_press(|state: &mut State| state.workspace.hide_overlay())
            .child(
                col()
                    .key("overlay-panel")
                    .w_full()
                    .grow(1.0)
                    .min_h(0.0)
                    .padding(OVERLAY_PADDING)
                    .radius(10.0)
                    .clip_rounded(10.0)
                    .fill(theme.card)
                    .border(1.0, theme.card_border)
                    .shadow(12.0, 40.0, theme.shadow)
                    // Keeps clicks inside the panel from reaching the layer, which hides it.
                    .on_press(|_: &mut State| {})
                    .child(
                        terminal_surface(
                            OVERLAY_MARK,
                            "overlay-terminal",
                            &state.workspace,
                            Workspace::overlay_session,
                        )
                        .w_full()
                        .grow(1.0)
                        .min_h(0.0)
                        .children(overlay_overlays),
                    ),
            )
    });
    let terminal = col()
        .h_full()
        .grow(1.0)
        .min_w(0.0)
        .child(
            row()
                .key("tab-strip")
                .w_full()
                .h(TAB_STRIP_HEIGHT)
                .gap(4.0)
                // Clears the traffic lights once the sidebar is narrower than them.
                .pl((cx.titlebar_inset() + 8.0 - sidebar_width).max(8.0))
                .pr(8.0)
                .items_center()
                .child(sidebar_toggle)
                .children(tabs)
                .children(new_tab),
        )
        .child(
            terminal_surface(
                TERMINAL_MARK,
                "terminal",
                &state.workspace,
                Workspace::active_session,
            )
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .children(overlays),
        )
        .children(tray_bar)
        .children(drawer)
        .children(overlay);
    let palette = state.palette_open.then(|| {
        let rows: Vec<El> = if state.creation.is_some() {
            vec![pill("palette-0".to_owned(), "Creating worktree…", false, None).w_full()]
        } else if let Some(choice) = &state.worktrees {
            let visible = state.visible_worktrees();
            let selected = state.palette_selected.min(visible.len().saturating_sub(1));
            let placeholder = choice
                .pending
                .is_some()
                .then(|| pill("palette-0".to_owned(), "Loading worktrees…", false, None).w_full());
            let worktrees = visible.iter().enumerate().map(|(index, worktree)| {
                let branch = worktree.branch.as_deref().unwrap_or("(detached)");
                let label = format!("{branch}  {}", jump::abbreviate_home(&worktree.path));
                pill(format!("palette-{index}"), &label, index == selected, None)
                    .w_full()
                    .on_press(move |state: &mut State| open_worktree(state, index))
            });
            placeholder.into_iter().chain(worktrees).collect()
        } else {
            let actions =
                palette_actions(&state.palette.text, state.jump_target(), &state.workspace);
            let selected = state.palette_selected.min(actions.len().saturating_sub(1));
            actions
                .iter()
                .enumerate()
                .map(|(index, action)| {
                    let shortcut = match action {
                        Action::Tray(_) => Some("⌘↵"),
                        Action::Jump(_) => Some("⌘↵ worktrees"),
                        _ => None,
                    }
                    .map(|hint| {
                        text(hint)
                            .key("palette-shortcut")
                            .color(theme.text_secondary)
                    });
                    pill(
                        format!("palette-{index}"),
                        &action.label(),
                        index == selected,
                        None,
                    )
                    .children(shortcut)
                    .justify_between()
                    .w_full()
                    .on_press_tx(move |state: &mut State, tx: &mut Tx| {
                        run_palette(state, index, tx)
                    })
                })
                .collect()
        };
        col()
            .key("palette-layer")
            .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
            .items_center()
            .pt(PALETTE_TOP)
            .on_press(|state: &mut State| state.palette_open = false)
            .child(
                col()
                    .key("palette")
                    .w(PALETTE_WIDTH)
                    .padding(8.0)
                    .gap(4.0)
                    .radius(10.0)
                    .fill(theme.card)
                    .border(1.0, theme.card_border)
                    // Keeps clicks between rows from reaching the layer, which closes it.
                    .on_press(|_: &mut State| {})
                    .child(
                        text_input(&state.palette, tok::type_::BODY)
                            .key("palette-input")
                            .label("Command")
                            .single_line()
                            .w_full()
                            .px(10.0)
                            .py(6.0)
                            .radius(6.0)
                            .fill(theme.control_track)
                            .color(theme.text)
                            .on_edit(|state: &mut State| &mut state.palette)
                            .on_input_key(palette_key),
                    )
                    .children(state.palette_error.as_deref().map(|error| {
                        text(error)
                            .key("palette-error")
                            .color(rune::color::oklch(0.66, 0.2, 25.0))
                            .px(10.0)
                            .py(4.0)
                    }))
                    .children(rows),
            )
    });
    row()
        .w_full()
        .h_full()
        .fill(theme.bg)
        .child(
            el().key("repaint")
                .absolute(0.0, 0.0)
                .size(0.0, 0.0)
                .live_opacity(&state.repaint.live),
        )
        .on_drop_files(|state: &mut State, paths: &[PathBuf], _: &mut Tx| {
            for path in paths.iter().filter(|path| path.is_dir()) {
                open_project(state, path.clone());
            }
        })
        .child(sidebar)
        .child(terminal)
        .children(palette)
}

fn poll(state: &mut State, now: f64, tx: &mut Tx) -> bool {
    if !state.started {
        state.started = true;
        load_layout(state);
        if state.workspace.projects.is_empty() {
            let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
            let tab = state.spawn_tab("shell", None, &home);
            state.workspace.add_project(Project::new(home, vec![tab]));
        }
    }
    let mut changed = adopt_projects(state);
    launch_active(state);
    let active = state.session().map(Session::id);
    changed |= poll_jump(state, now, tx);
    changed |= poll_worktrees(state);
    changed |= poll_creation(state);
    if state
        .pending_paste
        .as_ref()
        .is_some_and(|(pending, _)| Some(*pending) != active)
    {
        state.pending_paste = None;
        changed = true;
    }
    if state.pending_close.is_some() && state.pending_close != active {
        state.pending_close = None;
        state.closing_project = None;
        changed = true;
    }
    let mut restarts = Vec::new();
    let mut exits = Vec::new();
    let mut notices = Vec::new();
    for tab in state.workspace.tabs_mut() {
        while let Some(event) = tab.session.next_event() {
            match event {
                Event::Copied(text) => tx.copy(text),
                Event::PasteRejected(text) if Some(tab.session.id()) == active => {
                    state.pending_paste = Some((tab.session.id(), text));
                    changed = true;
                }
                Event::PasteRejected(_) => {}
                Event::Title(title) => {
                    tab.program_title = Some(title).filter(|title| !title.is_empty());
                    changed = true;
                }
                Event::Exited(status) => {
                    tab.exited(status);
                    if tab.status == Status::Exited {
                        exits.push(tab.session.id());
                    }
                    if tab.restart {
                        restarts.push(tab.session.id());
                    }
                    changed = true;
                }
                Event::LinkHover => changed = true,
                Event::Notify { title, body } => notices.push((tab.session.id(), title, body)),
            }
        }
    }
    for (session, title, body) in notices {
        if let Some((project, slot)) = state.workspace.locate(session)
            && let Some(project) = state.workspace.projects.get(project)
            && let Some(tab) = project.tab(slot)
        {
            let notification = notify::Notification::new(&project.name, tab.label(), title, body);
            notify::post(session, &notification);
        }
    }
    for session in exits {
        if let Some((_, Slot::Overlay)) = state.workspace.locate(session) {
            state.workspace.drop_overlay(session);
        }
    }
    for session in restarts {
        if let Some((project, slot @ Slot::Tray(_))) = state.workspace.locate(session) {
            respawn(state, project, slot);
        }
    }
    // Kept while programs hide the cursor, as dropping the caret cancels a composition.
    let grid = state.session().map_or_else(Arc::default, Session::grid);
    if let Some(caret) = caret_rect(&grid)
        && state.caret != Some(caret)
    {
        state.caret = Some(caret);
        changed = true;
    }
    let drawn = |session: Option<&Session>| {
        session.map_or((0, false), |session| {
            (session.id(), session.grid().scrollbar.is_some())
        })
    };
    let drawn = [
        drawn(state.workspace.active_session()),
        drawn(state.workspace.drawer_session()),
        drawn(state.workspace.overlay_session()),
    ];
    changed |= drawn != state.drawn;
    state.drawn = drawn;
    let alerting: Vec<u64> = state
        .workspace
        .sessions()
        .filter(|session| session.attention() == Attention::Alert)
        .map(Session::id)
        .collect();
    changed |= alerting != state.alerting;
    state.alerting = alerting;
    changed
}

fn on_key(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    if state.pending_paste.is_some() && matches!(event.key, Key::Enter | Key::Escape) {
        resolve_paste(state, event.key == Key::Enter);
        return true;
    }
    if state.pending_close.is_some() && matches!(event.key, Key::Enter | Key::Escape) {
        resolve_close(state, event.key == Key::Enter);
        return true;
    }
    if shortcut(state, event, tx) {
        return true;
    }
    if event.key == Key::Enter
        && event.mods == Mods::default()
        && tx.focused().is_none()
        && let Some((slot, tab)) = state.workspace.focused_tab()
        && tab.status != Status::Running
    {
        respawn(state, state.workspace.active, slot);
        return true;
    }
    // A focused field, such as the command centre's, keeps the keys its editor didn't take.
    if tx.focused().is_some() {
        return false;
    }
    if pressed("mod+c", event, tx) {
        if let Some(session) = state.session() {
            session.copy();
        }
        return true;
    }
    if pressed("mod+v", event, tx) {
        tx.read_clipboard(
            |state: &mut State, content: &ClipboardContent, _: &mut Tx| {
                if let Some(session) = state.session()
                    && !content.text().is_empty()
                {
                    session.paste(content.text().to_owned(), false);
                }
            },
        );
        return true;
    }
    match (
        state.session(),
        input::map_key(event, tx.physical_key(), tx.key_base()),
    ) {
        (Some(session), Some(key)) => {
            session.key(key);
            true
        }
        _ => false,
    }
}

fn on_ime(state: &mut State, ime: &Ime, _: &mut Tx) -> bool {
    match ime {
        Ime::Preedit(preedit, _) => {
            state.preedit.clone_from(preedit);
            true
        }
        Ime::Commit(committed) => {
            state.preedit.clear();
            if let Some(session) = state.session() {
                session.key(input::map_text(committed, Mods::default(), None, None));
            }
            true
        }
        _ => false,
    }
}

fn fixture_grid(bytes: &[u8]) -> Grid {
    Grid::from_vt(bytes, 60, 12)
        .unwrap_or_else(|error| Grid::message(&format!("failed to build fixture: {error:#}")))
}

fn fixture_tab(title: &str, bytes: &[u8]) -> Tab {
    Tab::new(title, Session::fixture(fixture_grid(bytes)))
}

/// Rune applies a scene over the state `init` built, so drop the paths that would load and save the user's layout.
fn isolate_scene(state: &mut State) {
    state.started = true;
    state.paths = None;
}

fn show_tab(state: &mut State, tab: Tab) {
    isolate_scene(state);
    state
        .workspace
        .add_project(Project::new("/fixture".into(), vec![tab]));
}

fn show_scene(state: &mut State, bytes: &[u8]) {
    show_tab(state, fixture_tab("shell", bytes));
}

/// History with the scrollbar as it shows just after scrolling, or while `hovered`.
fn scrollback_tab(hovered: bool) -> Tab {
    let lines: String = (1..=40).map(|line| format!("{line}\r\n")).collect();
    let mut grid = fixture_grid(lines.as_bytes());
    if let Some(scrollbar) = &mut grid.scrollbar {
        (scrollbar.opacity, scrollbar.wide) = (1.0, hovered);
    }
    Tab::new("shell", Session::fixture(grid))
}

fn show_workspace_scene(state: &mut State) {
    isolate_scene(state);
    let mut titled = fixture_tab("shell", b"$ ");
    titled.program_title = Some("vim ~/dev/agentd/src/workspace.rs and a long tail".to_owned());
    let tabs = vec![
        fixture_tab("claude", SCENE_FIXTURE),
        titled,
        fixture_tab("lazygit", b""),
    ];
    state
        .workspace
        .add_project(Project::new("/dev/agentd".into(), tabs));
    let tabs = vec![fixture_tab("claude", b""), fixture_tab("shell", b"$ ")];
    state
        .workspace
        .add_project(Project::new("/dev/site".into(), tabs));
    state.workspace.select_project(0);
}

const SCENE_DEV_SERVER: &[u8] = b"\x1b[1;32m  VITE\x1b[0m ready in 412 ms\r\n\r\n  \x1b[32m\xe2\x9e\x9c\x1b[0m  Local:   \x1b[36mhttp://localhost:5173/\x1b[0m\r\n";

fn add_scene_tray(state: &mut State) {
    let mut failed = fixture_tab("test:watch", b"FAIL src/app.test.ts\r\n");
    failed.status = Status::Failed("exit 1".to_owned());
    let mut stopped = fixture_tab("storybook", b"");
    stopped.status = Status::Stopped;
    for tab in [fixture_tab("pnpm dev", SCENE_DEV_SERVER), failed, stopped] {
        state.workspace.add_tray(tab);
    }
}

fn show_tray_scene(state: &mut State) {
    show_workspace_scene(state);
    add_scene_tray(state);
    state.workspace.projects[0].tabs[1].session.alert();
    state.workspace.toggle_drawer(0);
}

const SCENE_LAZYGIT: &[u8] = b"\x1b[1mStatus\x1b[0m\r\n\x1b[32m\xe2\x9c\x93\x1b[0m agentd -> lazygit-overlay\r\n\r\n\x1b[1mFiles\x1b[0m\r\n\x1b[32m M\x1b[0m src/main.rs\r\n\x1b[32m M\x1b[0m src/workspace.rs\r\n\x1b[31m??\x1b[0m notes.txt\r\n";

fn show_lazygit_scene(state: &mut State) {
    show_workspace_scene(state);
    state
        .workspace
        .show_overlay(Some(fixture_tab("lazygit", SCENE_LAZYGIT)));
}

/// A bundle launched from Finder has its stderr discarded, so send it to a file instead.
fn log_to_file_in_bundle() -> anyhow::Result<()> {
    use std::os::fd::AsRawFd;

    let exe = std::env::current_exe()?;
    if !exe.to_string_lossy().contains(".app/Contents/MacOS/") {
        return Ok(());
    }
    let home = std::env::var_os("HOME").context("HOME is unset")?;
    let path = PathBuf::from(home).join("Library/Logs/agentd.log");
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
        "agentd {} started at unix time {}, pid {}",
        env!("CARGO_PKG_VERSION"),
        started.as_secs(),
        std::process::id()
    );
    Ok(())
}

type Scene = (&'static str, fn(&mut State));

const SCENES: &[Scene] = &[
    ("fixture", |state| show_scene(state, SCENE_FIXTURE)),
    ("unicode", |state| {
        show_scene(state, SCENE_UNICODE.as_bytes())
    }),
    ("scrollback", |state| show_tab(state, scrollback_tab(false))),
    ("scrollback-hover", |state| {
        show_tab(state, scrollback_tab(true))
    }),
    ("workspace", show_workspace_scene),
    ("palette", |state| {
        show_workspace_scene(state);
        state.palette = TextEdit::new("lz");
        state.palette_open = true;
    }),
    ("palette-error", |state| {
        show_workspace_scene(state);
        add_scene_tray(state);
        state.palette = TextEdit::new("site");
        state.palette_open = true;
        state.palette_error = Some("✗ Branch site already exists".to_owned());
    }),
    ("worktrees", |state| {
        show_workspace_scene(state);
        state.palette = TextEdit::new("site");
        state.palette_open = true;
        state.worktrees = Some(WorktreeChoice {
            root: PathBuf::from("/Users/me/dev/site"),
            pending: None,
            found: vec![
                jump::Worktree {
                    branch: Some("main".to_owned()),
                    path: PathBuf::from("/Users/me/dev/site"),
                },
                jump::Worktree {
                    branch: Some("redesign".to_owned()),
                    path: PathBuf::from("/Users/me/dev/site.redesign"),
                },
            ],
        });
    }),
    ("tray", show_tray_scene),
    ("lazygit", show_lazygit_scene),
    ("empty", isolate_scene),
    ("collapsed", |state| {
        show_workspace_scene(state);
        state.sidebar_hidden = true;
    }),
];

/// One window's app, with its state made by `init`.
fn window_app(
    fleet: &Fleet,
    window: WindowId,
    configure: impl FnOnce(&mut State) + 'static,
) -> App<State> {
    let spec = AppSpec::<State> {
        scenes: SCENES,
        setup: Some(|state, waker| {
            state.fleet.register(state.window, waker.clone());
            state.waker = Some(waker);
        }),
        poll: Some(poll),
        on_key: Some(on_key),
        ..AppSpec::new("agentd", (960.0, 640.0), State::default, view)
    };
    let init_fleet = fleet.clone();
    let app = App::new(spec)
        .init(move || {
            let mut state = State {
                fleet: init_fleet,
                window,
                ..State::default()
            };
            configure(&mut state);
            state
        })
        .compositor(render::TerminalCompositor::default())
        .on_ime(on_ime)
        .on_modifiers(|state, mods, _| {
            state.option_held = mods.alt;
            for tab in state.workspace.tabs_mut() {
                tab.session.command_held(mods.logo);
            }
        })
        .on_quit(on_quit)
        .on_close(on_close)
        .on_focus(|state, focused, _| state.workspace.set_window_focused(focused));
    let handle = app.handle();
    fleet.on_reveal(window, move |session| {
        let opener = handle.clone();
        handle.update(move |state, _| {
            if state.workspace.locate(session).is_some() {
                state.workspace.reveal(session);
                opener.open();
            }
        });
    });
    app
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some((_, config_args)) = args.split_first().filter(|(name, _)| *name == "config") {
        let result = config::Paths::from_home()
            .context("HOME is not set")
            .and_then(|paths| config::command(config_args, &paths, &mut std::io::stdout()));
        if let Err(error) = result {
            eprintln!("agentd: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    log_to_file_in_bundle().log_err("log file");
    let fleet = Fleet::default();
    let app = window_app(&fleet, MAIN_WINDOW, |state| {
        state.paths = config::Paths::from_home();
        state.config = state
            .paths
            .as_ref()
            .map(config::Config::load)
            .unwrap_or_default();
    });
    let handle = app.handle();
    notify::start(move |session| match session {
        Some(session) => fleet.reveal(session),
        None => handle.open(),
    });
    app.run();
}

#[cfg(test)]
mod tests {
    use super::*;
    use rune::{Env, Runtime};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static WINDOW_KEYS: AtomicUsize = AtomicUsize::new(0);
    static KEYS_AFTER_TERMINAL_CLICK: AtomicUsize = AtomicUsize::new(0);

    fn runtime_with(state: State) -> Runtime<State> {
        let env = Env {
            system_dark: true,
            scale: 2.0,
        };
        let mut runtime = Runtime::new(state, view, (960.0, 640.0), env);
        runtime.frame(0.0);
        runtime
    }

    fn output(bytes: &[u8], generation: u64) -> Grid {
        Grid {
            generation,
            ..fixture_grid(bytes)
        }
    }

    #[test]
    fn output_alone_repaints_without_rebuilding_the_view() {
        let mut state = State::default();
        show_scene(&mut state, b"$ ");
        let mut runtime = runtime_with(state);
        runtime.poll = Some(poll);
        runtime.frame(1.0 / 60.0);
        let commits = runtime.stats.commits;
        let session = runtime.state.workspace.active_session().unwrap();
        session.publish_fixture(output(b"$ ls", 2));
        runtime.frame(2.0 / 60.0);
        assert_eq!(runtime.stats.commits, commits);
        assert!(!runtime.live_pending());
        runtime.state.repaint.waker(Arc::new(|| {}))();
        assert!(runtime.live_pending());
        let session = runtime.state.workspace.active_session().unwrap();
        let history: String = (1..=40).map(|line| format!("{line}\r\n")).collect();
        session.publish_fixture(output(history.as_bytes(), 3));
        runtime.frame(3.0 / 60.0);
        assert!(runtime.stats.commits > commits);
        assert!(runtime.find("terminal-scrollbar").is_some());
    }

    #[test]
    fn the_scrollbar_strip_shows_an_arrow_only_with_history() {
        let mut state = State::default();
        show_scene(&mut state, b"$ ");
        assert_eq!(runtime_with(state).find("terminal-scrollbar"), None);
        let mut state = State::default();
        show_tab(&mut state, scrollback_tab(false));
        let mut runtime = runtime_with(state);
        let (x, y) = runtime.find("terminal-scrollbar").unwrap();
        runtime.pointer_move(x, y);
        runtime.frame(1.0 / 60.0);
        assert_eq!(runtime.cursor(), Cursor::Default);
        let (x, y) = runtime.find("terminal").unwrap();
        runtime.pointer_move(x, y);
        runtime.frame(2.0 / 60.0);
        assert_eq!(runtime.cursor(), Cursor::Text);
    }

    #[test]
    fn the_terminal_shows_a_hand_while_command_hovers_a_link() {
        let mut state = State::default();
        show_scene(&mut state, b"https://a.test");
        let mut runtime = runtime_with(state);
        let (x, y) = runtime.find("terminal").unwrap();
        runtime.pointer_move(x, y);
        runtime.frame(1.0 / 60.0);
        assert_eq!(runtime.cursor(), Cursor::Text);
        runtime.update(|state, _| {
            if let Some(session) = state.workspace.active_session() {
                session.set_link_hovered(true);
            }
        });
        runtime.frame(2.0 / 60.0);
        assert_eq!(runtime.cursor(), Cursor::Pointer);
    }

    #[test]
    fn pressing_a_scrollbar_moves_keys_to_its_terminal() {
        let mut state = State::default();
        show_tab(&mut state, scrollback_tab(false));
        state.workspace.add_tray(scrollback_tab(false));
        state.workspace.toggle_drawer(0);
        let mut runtime = runtime_with(state);
        runtime.frame(1.0);
        let drawer_focused =
            |runtime: &Runtime<State>| runtime.state.workspace.projects[0].drawer_focused;
        assert!(drawer_focused(&runtime));
        for (strip, focused) in [
            ("terminal-scrollbar", false),
            ("drawer-terminal-scrollbar", true),
        ] {
            let (x, y) = runtime.find(strip).unwrap();
            runtime.pointer_down(x, y);
            runtime.pointer_up(x, y);
            runtime.frame(2.0);
            assert_eq!(drawer_focused(&runtime), focused, "{strip}");
        }
    }

    #[test]
    fn clicking_the_terminal_leaves_keys_with_the_window() {
        let mut runtime = runtime_with(State::default());
        runtime.on_key = Some(|_, _, _| {
            KEYS_AFTER_TERMINAL_CLICK.fetch_add(1, Ordering::SeqCst);
            true
        });
        let (x, y) = runtime.find("terminal").unwrap();
        runtime.pointer_down(x, y);
        runtime.pointer_up(x, y);
        runtime.frame(1.0 / 60.0);
        runtime.key(KeyEvent {
            key: Key::Tab,
            mods: Mods::default(),
        });
        runtime.frame(1.0 / 60.0);
        assert_eq!(KEYS_AFTER_TERMINAL_CLICK.load(Ordering::SeqCst), 1);
    }

    fn with_pending_paste() -> Runtime<State> {
        runtime_with(State {
            pending_paste: Some((0, "rm -rf x\n".to_owned())),
            ..State::default()
        })
    }

    #[test]
    fn cancel_dismisses_the_paste_bar() {
        let mut runtime = with_pending_paste();
        let (x, y) = runtime.find("paste-confirm-no").unwrap();
        runtime.pointer_down(x, y);
        runtime.pointer_up(x, y);
        runtime.frame(1.0 / 60.0);
        runtime.frame(2.0 / 60.0);
        assert!(runtime.find("paste-confirm").is_none());
    }

    #[test]
    fn escape_dismisses_the_paste_bar() {
        let mut runtime = with_pending_paste();
        runtime.on_key = Some(on_key);
        runtime.key(KeyEvent {
            key: Key::Escape,
            mods: Mods::default(),
        });
        runtime.frame(1.0 / 60.0);
        runtime.frame(2.0 / 60.0);
        assert!(runtime.find("paste-confirm").is_none());
    }

    #[test]
    fn holding_option_removes_the_ime_caret() {
        let caret = Some([1.0, 1.0, 8.0, 16.0]);
        let shown = runtime_with(State {
            caret,
            ..State::default()
        });
        assert!(shown.find("caret").is_some());
        let held = runtime_with(State {
            caret,
            option_held: true,
            ..State::default()
        });
        assert!(held.find("caret").is_none());
    }

    #[test]
    fn holding_option_keeps_the_caret_during_a_composition() {
        let composing = runtime_with(State {
            caret: Some([1.0, 1.0, 8.0, 16.0]),
            option_held: true,
            preedit: "に".to_owned(),
            ..State::default()
        });
        assert!(composing.find("caret").is_some());
    }

    #[test]
    fn the_caret_follows_the_cursor_in_points() {
        let mut grid = Grid::from_vt(b"ab", 10, 2).unwrap();
        grid.metrics = session::Metrics {
            cell_width: 16,
            cell_height: 32,
            padding: 12.0,
            scale: 2.0,
        };
        assert_eq!(caret_rect(&grid), Some([22.0, 6.0, 8.0, 16.0]));
    }

    #[test]
    fn the_caret_is_the_drawn_cell_in_points_at_fractional_scales() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let mut grid = Grid::from_vt(b"ab", 10, 2).unwrap();
            grid.metrics = session::Metrics {
                cell_width: 9,
                cell_height: 19,
                padding: 12.0 * scale,
                scale,
            };
            let metrics = grid.metrics;
            let (x, y) = grid.cursor.unwrap();
            let drawn = metrics.cell_rect(metrics.origin([0.0; 4]), x, y);
            assert_eq!(caret_rect(&grid), Some(metrics.to_points(drawn)), "{scale}");
        }
    }

    fn workspace_state() -> State {
        let mut state = State::default();
        show_workspace_scene(&mut state);
        state
    }

    fn click(runtime: &mut Runtime<State>, key: &str) {
        let (x, y) = runtime.find(key).unwrap();
        runtime.pointer_down(x, y);
        runtime.pointer_up(x, y);
        runtime.frame(1.0 / 60.0);
    }

    #[test]
    fn clicking_a_project_and_a_tab_selects_them() {
        let mut runtime = runtime_with(workspace_state());
        click(&mut runtime, "project-1");
        click(&mut runtime, "tab-1");
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.active, 1);
        assert_eq!(workspace.projects[1].active_tab, 1);
    }

    fn press(runtime: &mut Runtime<State>, key: Key, mods: Mods) {
        runtime.key(KeyEvent { key, mods });
        runtime.frame(1.0 / 60.0);
    }

    fn command() -> Mods {
        Mods {
            cmd: true,
            logo: true,
            ..Mods::default()
        }
    }

    fn command_shift() -> Mods {
        Mods {
            shift: true,
            ..command()
        }
    }

    fn control() -> Mods {
        Mods {
            ctrl: true,
            ..Mods::default()
        }
    }

    fn workspace_runtime() -> Runtime<State> {
        let mut runtime = runtime_with(workspace_state());
        runtime.on_key = Some(on_key);
        runtime
    }

    #[test]
    fn control_digits_pick_projects_and_command_digits_pick_tabs() {
        let mut runtime = workspace_runtime();
        press(&mut runtime, Key::Text("2".into()), control());
        press(&mut runtime, Key::Command('2'), command());
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.active, 1);
        assert_eq!(workspace.projects[1].active_tab, 1);
    }

    #[test]
    fn a_command_digit_past_the_last_tab_selects_the_last_tab() {
        let mut runtime = workspace_runtime();
        press(&mut runtime, Key::Command('9'), command());
        assert_eq!(runtime.state.workspace.projects[0].active_tab, 2);
    }

    #[test]
    fn a_control_digit_past_the_last_project_selects_the_last_project() {
        let mut runtime = workspace_runtime();
        press(&mut runtime, Key::Text("9".into()), control());
        assert_eq!(runtime.state.workspace.active, 1);
    }

    #[test]
    fn command_shift_digits_do_not_switch_tabs() {
        let mut runtime = workspace_runtime();
        press(&mut runtime, Key::Command('2'), command_shift());
        press(&mut runtime, Key::Command('@'), command_shift());
        let workspace = &runtime.state.workspace;
        assert_eq!((workspace.active, workspace.projects[0].active_tab), (0, 0));
    }

    fn settle(runtime: &mut Runtime<State>) {
        for frame in 2..120 {
            runtime.frame(frame as f64 / 60.0);
        }
    }

    fn sidebar_width(runtime: &Runtime<State>) -> f32 {
        runtime
            .presented("sidebar")
            .map_or(-1.0, |(rect, _)| rect[2])
    }

    #[test]
    fn collapsing_with_several_projects_leaves_a_rail_of_project_tabs() {
        let mut runtime = workspace_runtime();
        press(&mut runtime, Key::Command('b'), command());
        settle(&mut runtime);
        assert_eq!(sidebar_width(&runtime), RAIL_WIDTH);
        click(&mut runtime, "project-1");
        assert_eq!(runtime.state.workspace.active, 1);
    }

    #[test]
    fn collapsing_with_one_project_slides_the_sidebar_away_and_the_toggle_restores_it() {
        let mut state = State::default();
        show_scene(&mut state, SCENE_FIXTURE);
        let mut runtime = runtime_with(state);
        runtime.on_key = Some(on_key);
        press(&mut runtime, Key::Command('b'), command());
        runtime.frame(2.0 / 60.0);
        let sliding = sidebar_width(&runtime);
        assert!(0.0 < sliding && sliding < SIDEBAR_WIDTH, "{sliding}");
        settle(&mut runtime);
        assert_eq!(sidebar_width(&runtime), 0.0);
        assert!(runtime.find("project-0").is_none());
        click(&mut runtime, "sidebar-toggle");
        settle(&mut runtime);
        assert_eq!(sidebar_width(&runtime), SIDEBAR_WIDTH);
    }

    #[test]
    fn command_shift_brackets_cycle_tabs() {
        let mut runtime = workspace_runtime();
        press(&mut runtime, Key::Command('{'), command_shift());
        assert_eq!(runtime.state.workspace.projects[0].active_tab, 2);
        press(&mut runtime, Key::Command(']'), command_shift());
        assert_eq!(runtime.state.workspace.projects[0].active_tab, 0);
    }

    #[test]
    fn command_w_closes_the_active_tab() {
        let mut runtime = workspace_runtime();
        press(&mut runtime, Key::Command('w'), command());
        let titles: Vec<_> = runtime.state.workspace.projects[0]
            .tabs
            .iter()
            .map(|tab| tab.title.as_str())
            .collect();
        assert_eq!(titles, ["shell", "lazygit"]);
    }

    fn hover(runtime: &mut Runtime<State>, key: &str) {
        let (x, y) = runtime.find(key).unwrap();
        runtime.pointer_move(x, y);
        runtime.frame(1.0 / 60.0);
    }

    #[test]
    fn the_active_project_always_shows_its_close_button() {
        let mut runtime = runtime_with(workspace_state());
        assert!(runtime.find("project-close-0").is_some());
        assert!(runtime.find("project-close-1").is_none());
        click(&mut runtime, "project-close-0");
        assert_eq!(runtime.state.workspace.projects.len(), 1);
    }

    #[test]
    fn hovering_an_inactive_project_reveals_its_close_button_and_clicking_closes_it() {
        let mut runtime = runtime_with(workspace_state());
        hover(&mut runtime, "project-1");
        assert!(runtime.find("project-close-1").is_some());
        click(&mut runtime, "project-close-1");
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.projects.len(), 1);
        assert_eq!(workspace.active, 0);
    }

    #[test]
    fn closing_a_running_inactive_project_asks_without_selecting_it() {
        let mut runtime = tray_runtime();
        runtime.state.workspace.projects[1].tabs[0].command = Some("claude".to_owned());
        hover(&mut runtime, "project-1");
        click(&mut runtime, "project-close-1");
        assert!(runtime.find("close-confirm").is_some());
        assert_eq!(runtime.state.workspace.active, 0);
        press(&mut runtime, Key::Escape, Mods::default());
        assert_eq!(runtime.state.workspace.projects.len(), 2);
        hover(&mut runtime, "project-1");
        click(&mut runtime, "project-close-1");
        press(&mut runtime, Key::Enter, Mods::default());
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.projects.len(), 1);
        assert_eq!(workspace.projects[0].name, "agentd");
    }

    fn command_tab_runtime() -> Runtime<State> {
        let mut state = workspace_state();
        state.workspace.projects[0].tabs[0].command = Some("claude".to_owned());
        let mut runtime = runtime_with(state);
        runtime.on_key = Some(on_key);
        runtime
    }

    fn tab_titles(runtime: &Runtime<State>) -> Vec<&str> {
        let tabs = &runtime.state.workspace.projects[0].tabs;
        tabs.iter().map(|tab| tab.title.as_str()).collect()
    }

    #[test]
    fn command_w_on_a_running_command_asks_and_escape_keeps_it() {
        let mut runtime = command_tab_runtime();
        press(&mut runtime, Key::Command('w'), command());
        assert!(runtime.find("close-confirm").is_some());
        press(&mut runtime, Key::Escape, Mods::default());
        assert!(runtime.find("close-confirm").is_none());
        assert_eq!(tab_titles(&runtime), ["claude", "shell", "lazygit"]);
    }

    #[test]
    fn enter_confirms_closing_a_running_command() {
        let mut runtime = command_tab_runtime();
        press(&mut runtime, Key::Command('w'), command());
        press(&mut runtime, Key::Enter, Mods::default());
        assert_eq!(tab_titles(&runtime), ["shell", "lazygit"]);
    }

    #[test]
    fn removing_a_running_tray_process_opens_its_drawer_and_asks() {
        let mut runtime = tray_runtime();
        runtime.state.workspace.projects[0].tray[0].command = Some("pnpm dev".to_owned());
        runtime.update(|state, _| tray_action(state, 0, Some("remove")));
        runtime.frame(1.0);
        assert!(runtime.find("close-confirm").is_some());
        assert_eq!(runtime.state.workspace.projects[0].tray.len(), 3);
        press(&mut runtime, Key::Enter, Mods::default());
        assert_eq!(runtime.state.workspace.projects[0].tray.len(), 2);
    }

    #[test]
    fn command_w_in_the_drawer_removes_its_process_and_keeps_the_tabs() {
        let mut runtime = tray_runtime();
        click(&mut runtime, "chip-1");
        press(&mut runtime, Key::Command('w'), command());
        let project = &runtime.state.workspace.projects[0];
        let tray: Vec<_> = project.tray.iter().map(|tab| tab.title.as_str()).collect();
        assert_eq!(tray, ["pnpm dev", "storybook"]);
        assert_eq!(project.tabs.len(), 3);
        assert_eq!(drawer_id(&runtime), None);
        click(&mut runtime, "chip-0");
        click(&mut runtime, "terminal");
        press(&mut runtime, Key::Command('w'), command());
        let project = &runtime.state.workspace.projects[0];
        assert_eq!(project.tray.len(), 2);
        assert_eq!(project.tabs.len(), 2);
    }

    #[test]
    fn command_n_opening_a_known_folder_selects_its_project() {
        let mut runtime = workspace_runtime();
        press(&mut runtime, Key::Command('n'), command());
        runtime
            .resolve_file_dialog(Some(PathBuf::from("/dev/site")))
            .unwrap();
        runtime.frame(2.0 / 60.0);
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.projects.len(), 2);
        assert_eq!(workspace.active, 1);
    }

    #[test]
    fn change_project_folder_replaces_the_active_projects_root() {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Text("change".into()), Mods::default());
        press(&mut runtime, Key::Enter, Mods::default());
        runtime
            .resolve_file_dialog(Some(PathBuf::from("/dev/moved")))
            .unwrap();
        runtime.frame(2.0 / 60.0);
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.projects.len(), 2);
        assert_eq!(
            workspace.projects[workspace.active].root,
            Path::new("/dev/moved")
        );
        assert_eq!(workspace.projects[workspace.active].name, "moved");
    }

    fn palette_runtime() -> Runtime<State> {
        let mut runtime = workspace_runtime();
        press(&mut runtime, Key::Command('p'), command());
        runtime.frame(2.0 / 60.0);
        runtime
    }

    #[test]
    fn command_p_opens_the_command_centre_and_escape_closes_it() {
        let mut runtime = palette_runtime();
        assert!(runtime.find("palette").is_some());
        press(&mut runtime, Key::Escape, Mods::default());
        runtime.frame(3.0 / 60.0);
        assert!(runtime.find("palette").is_none());
    }

    #[test]
    fn clicking_outside_the_command_centre_closes_it() {
        let mut runtime = palette_runtime();
        runtime.pointer_down(4.0, 600.0);
        runtime.pointer_up(4.0, 600.0);
        runtime.frame(3.0 / 60.0);
        runtime.frame(4.0 / 60.0);
        assert!(!runtime.state.palette_open);
    }

    #[test]
    fn command_enter_runs_the_query_in_the_tray() {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Text("x".into()), Mods::default());
        press(&mut runtime, Key::Enter, command());
        let project = &runtime.state.workspace.projects[0];
        let tray: Vec<_> = project.tray.iter().map(|tab| tab.title.as_str()).collect();
        assert_eq!(tray, ["x"]);
        assert_eq!(project.tabs.len(), 3);
        assert!(!runtime.state.palette_open);
    }

    #[test]
    fn the_tray_row_shows_its_shortcut() {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Text("x".into()), Mods::default());
        runtime.frame(0.0);
        assert!(runtime.find("palette-shortcut").is_some());
    }

    #[test]
    fn command_enter_with_no_query_does_nothing() {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Enter, command());
        let project = &runtime.state.workspace.projects[0];
        assert!(project.tray.is_empty());
        assert_eq!(project.tabs.len(), 3);
    }

    #[test]
    fn typing_in_the_command_centre_stays_out_of_the_terminal() {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Text("x".into()), Mods::default());
        assert_eq!(runtime.state.palette.text, "x");
    }

    fn one_project() -> Workspace {
        let mut workspace = Workspace::default();
        workspace.add_project(Project::new("/dev/agentd".into(), Vec::new()));
        workspace
    }

    #[test]
    fn the_command_centre_filters_by_letters_in_order() {
        assert_eq!(
            palette_actions("lzg", None, &one_project()),
            [
                Action::Lazygit,
                Action::Run("lzg".to_owned()),
                Action::Tray("lzg".to_owned()),
            ]
        );
        assert_eq!(
            palette_actions("", None, &one_project()),
            [
                Action::Shell,
                Action::Lazygit,
                Action::OpenProject,
                Action::ChangeFolder
            ]
        );
        assert!(!palette_actions("", None, &Workspace::default()).contains(&Action::ChangeFolder));
    }

    #[test]
    fn the_jump_commands_folder_follows_the_fixed_actions() {
        let root = Path::new("/dev/site");
        assert_eq!(
            palette_actions("site", Some(root), &one_project()),
            [
                Action::Jump(root.to_owned()),
                Action::Run("site".to_owned()),
                Action::Tray("site".to_owned()),
            ]
        );
    }

    #[test]
    fn enter_on_the_jump_commands_folder_opens_it_as_a_project() {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Text("site".into()), Mods::default());
        runtime.state.jump.query = "site".to_owned();
        runtime.state.jump.found = Some(PathBuf::from("/dev/site"));
        press(&mut runtime, Key::Enter, Mods::default());
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.projects.len(), 2);
        assert_eq!(workspace.projects[1].root, Path::new("/dev/site"));
        assert!(!runtime.state.palette_open);
    }

    #[test]
    fn typing_a_project_or_tab_name_offers_switching_to_it() {
        let state = workspace_state();
        let actions = palette_actions("sit", None, &state.workspace);
        assert_eq!(
            actions[..3],
            [
                Action::Run("sit".to_owned()),
                Action::Tray("sit".to_owned()),
                Action::SwitchProject(1, "site".to_owned()),
            ]
        );
        let actions = palette_actions("lazy", None, &state.workspace);
        assert!(actions.contains(&Action::SwitchTab(0, 2, "lazygit (agentd)".to_owned())));
        assert!(
            !palette_actions("", None, &state.workspace)
                .iter()
                .any(|action| matches!(action, Action::SwitchProject(..) | Action::SwitchTab(..)))
        );
    }

    #[test]
    fn switching_comes_after_running_the_query_and_exact_names_lead_the_switches() {
        let state = workspace_state();
        let actions = palette_actions("shell", None, &state.workspace);
        assert_eq!(actions[0], Action::Shell);
        assert_eq!(actions[1], Action::Run("shell".to_owned()));
        assert_eq!(actions[2], Action::Tray("shell".to_owned()));
        assert_eq!(
            actions[3],
            Action::SwitchTab(1, 1, "shell (site)".to_owned())
        );
    }

    #[test]
    fn the_jump_folder_replaces_the_switch_to_its_open_project() {
        let state = workspace_state();
        let root = Path::new("/dev/site");
        let actions = palette_actions("site", Some(root), &state.workspace);
        assert_eq!(actions[0], Action::Jump(root.to_owned()));
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, Action::SwitchProject(..)))
        );
    }

    fn enter_on_switch_row(runtime: &mut Runtime<State>, query: &str, row: Action) {
        press(runtime, Key::Text(query.into()), Mods::default());
        let actions = palette_actions(query, None, &runtime.state.workspace);
        runtime.state.palette_selected = actions.iter().position(|action| *action == row).unwrap();
        press(runtime, Key::Enter, Mods::default());
    }

    #[test]
    fn enter_on_a_project_row_makes_it_active() {
        let mut runtime = palette_runtime();
        enter_on_switch_row(
            &mut runtime,
            "site",
            Action::SwitchProject(1, "site".to_owned()),
        );
        assert_eq!(runtime.state.workspace.active, 1);
        assert!(!runtime.state.palette_open);
    }

    #[test]
    fn enter_on_a_tab_row_activates_its_project_and_tab() {
        let mut runtime = palette_runtime();
        enter_on_switch_row(
            &mut runtime,
            "lazygit",
            Action::SwitchTab(0, 2, "lazygit (agentd)".to_owned()),
        );
        assert_eq!(runtime.state.workspace.active, 0);
        assert_eq!(runtime.state.workspace.projects[0].active_tab, 2);
        let mut runtime = palette_runtime();
        enter_on_switch_row(
            &mut runtime,
            "shell",
            Action::SwitchTab(1, 1, "shell (site)".to_owned()),
        );
        assert_eq!(runtime.state.workspace.active, 1);
        assert_eq!(runtime.state.workspace.projects[1].active_tab, 1);
    }

    #[test]
    fn enter_on_a_name_that_matches_a_project_still_runs_the_command() {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Text("site".into()), Mods::default());
        press(&mut runtime, Key::Enter, Mods::default());
        assert_eq!(runtime.state.workspace.active, 0);
        assert_eq!(runtime.state.workspace.projects[0].tabs.len(), 4);
    }

    fn worktree(branch: &str, path: &str) -> jump::Worktree {
        jump::Worktree {
            branch: Some(branch.to_owned()),
            path: PathBuf::from(path),
        }
    }

    fn loaded(found: Vec<jump::Worktree>) -> WorktreeChoice {
        WorktreeChoice {
            root: PathBuf::from("/dev/site"),
            pending: None,
            found,
        }
    }

    fn jump_runtime() -> Runtime<State> {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Text("site".into()), Mods::default());
        runtime.state.jump.query = "site".to_owned();
        runtime.state.jump.found = Some(PathBuf::from("/dev/site"));
        runtime
    }

    #[test]
    fn the_jump_row_hints_at_worktrees() {
        let mut runtime = jump_runtime();
        runtime.frame(0.0);
        assert!(runtime.find("palette-shortcut").is_some());
    }

    #[test]
    fn command_enter_on_the_jump_row_lists_worktrees_and_enter_opens_one() {
        let mut runtime = jump_runtime();
        press(&mut runtime, Key::Enter, command());
        assert!(runtime.state.palette_open);
        assert!(runtime.state.worktrees.is_some());
        let found = vec![
            worktree("main", "/dev/site"),
            worktree("feature", "/dev/site.feature"),
        ];
        runtime.state.worktrees = Some(loaded(found));
        press(&mut runtime, Key::Down, Mods::default());
        press(&mut runtime, Key::Enter, Mods::default());
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.projects.len(), 3);
        assert_eq!(workspace.projects[2].root, Path::new("/dev/site.feature"));
        assert!(!runtime.state.palette_open);
    }

    #[test]
    fn an_open_worktree_is_selected_rather_than_reopened() {
        let mut runtime = jump_runtime();
        let root = runtime.state.workspace.projects[1].root.clone();
        runtime.state.worktrees = Some(loaded(vec![jump::Worktree {
            branch: None,
            path: root,
        }]));
        press(&mut runtime, Key::Enter, Mods::default());
        assert_eq!(runtime.state.workspace.projects.len(), 2);
        assert_eq!(runtime.state.workspace.active, 1);
        assert!(!runtime.state.palette_open);
    }

    #[test]
    fn entering_worktree_mode_clears_the_query_and_typing_filters_the_list() {
        let mut runtime = jump_runtime();
        press(&mut runtime, Key::Enter, command());
        assert_eq!(runtime.state.palette.text, "");
        runtime.state.worktrees = Some(loaded(vec![
            worktree("main", "/dev/site"),
            worktree("feature", "/dev/site.feature"),
            worktree("redesign", "/dev/site.redesign"),
        ]));
        press(&mut runtime, Key::Text("rdsn".into()), Mods::default());
        let branches: Vec<_> = runtime
            .state
            .visible_worktrees()
            .iter()
            .map(|worktree| worktree.branch.clone().unwrap())
            .collect();
        assert_eq!(branches, ["redesign"]);
        for _ in 0..3 {
            press(&mut runtime, Key::Down, Mods::default());
        }
        press(&mut runtime, Key::Enter, Mods::default());
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.projects[2].root, Path::new("/dev/site.redesign"));
    }

    #[test]
    fn a_filter_matching_nothing_leaves_enter_with_nothing_to_open() {
        let mut runtime = jump_runtime();
        runtime.state.worktrees = Some(loaded(vec![worktree("main", "/dev/site")]));
        press(&mut runtime, Key::Text("zzz".into()), Mods::default());
        press(&mut runtime, Key::Enter, Mods::default());
        assert!(runtime.state.palette_open);
        assert_eq!(runtime.state.workspace.projects.len(), 2);
    }

    #[test]
    fn no_worktrees_falls_back_to_opening_the_jump_target() {
        let mut runtime = jump_runtime();
        let (sender, receiver) = mpsc::channel();
        runtime.state.worktrees = Some(WorktreeChoice {
            root: PathBuf::from("/dev/elsewhere"),
            pending: Some(receiver),
            ..loaded(Vec::new())
        });
        assert!(!poll_worktrees(&mut runtime.state));
        assert!(runtime.state.palette_open);
        sender.send(Vec::new()).unwrap();
        assert!(poll_worktrees(&mut runtime.state));
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.projects[2].root, Path::new("/dev/elsewhere"));
        assert!(!runtime.state.palette_open);
    }

    #[test]
    fn escape_closes_the_worktree_list_and_reopening_resets_it() {
        let mut runtime = jump_runtime();
        press(&mut runtime, Key::Enter, command());
        press(&mut runtime, Key::Escape, Mods::default());
        assert!(!runtime.state.palette_open);
        press(&mut runtime, Key::Command('p'), command());
        assert!(runtime.state.worktrees.is_none());
    }

    #[test]
    fn command_enter_off_the_jump_row_still_runs_in_the_tray() {
        let mut runtime = jump_runtime();
        press(&mut runtime, Key::Down, Mods::default());
        press(&mut runtime, Key::Enter, command());
        assert!(runtime.state.worktrees.is_none());
        assert_eq!(runtime.state.workspace.projects[0].tray.len(), 1);
    }

    #[test]
    fn a_stale_jump_folder_is_not_offered() {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Text("sit".into()), Mods::default());
        runtime.state.jump.query = "si".to_owned();
        runtime.state.jump.found = Some(PathBuf::from("/dev/site"));
        assert_eq!(runtime.state.jump_target(), None);
    }

    #[test]
    fn arrows_move_the_command_centre_selection_within_bounds() {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Up, Mods::default());
        assert_eq!(runtime.state.palette_selected, 0);
        for _ in 0..5 {
            press(&mut runtime, Key::Down, Mods::default());
        }
        assert_eq!(runtime.state.palette_selected, 4);
    }

    fn tray_runtime() -> Runtime<State> {
        let mut state = workspace_state();
        add_scene_tray(&mut state);
        let mut runtime = runtime_with(state);
        runtime.on_key = Some(on_key);
        runtime
    }

    fn drawer_height(runtime: &Runtime<State>) -> f32 {
        runtime
            .presented("drawer")
            .map_or(-1.0, |(rect, _)| rect[3])
    }

    fn drawer_id(runtime: &Runtime<State>) -> Option<u64> {
        runtime.state.workspace.drawer_session().map(Session::id)
    }

    fn tray_id(runtime: &Runtime<State>, index: usize) -> Option<u64> {
        Some(runtime.state.workspace.projects[0].tray[index].session.id())
    }

    #[test]
    fn clicking_a_chip_slides_its_drawer_open_and_again_closes_it() {
        let mut runtime = tray_runtime();
        assert_eq!(drawer_height(&runtime), 0.0);
        click(&mut runtime, "chip-1");
        assert_eq!(drawer_id(&runtime), tray_id(&runtime, 1));
        runtime.frame(2.0 / 60.0);
        let sliding = drawer_height(&runtime);
        assert!(0.0 < sliding && sliding < DRAWER_HEIGHT, "{sliding}");
        let panel = runtime.presented("drawer-panel").map(|(rect, _)| rect[3]);
        assert_eq!(panel, Some(DRAWER_HEIGHT));
        settle(&mut runtime);
        assert_eq!(drawer_height(&runtime), DRAWER_HEIGHT);
        click(&mut runtime, "chip-1");
        settle(&mut runtime);
        assert_eq!(drawer_id(&runtime), None);
        assert_eq!(drawer_height(&runtime), 0.0);
    }

    #[test]
    fn clicking_a_terminal_sends_keys_to_it() {
        let mut runtime = tray_runtime();
        click(&mut runtime, "chip-0");
        settle(&mut runtime);
        let focused = |runtime: &Runtime<State>| runtime.state.session().map(Session::id);
        assert_eq!(focused(&runtime), tray_id(&runtime, 0));
        click(&mut runtime, "terminal");
        let tab = runtime.state.workspace.active_session().map(Session::id);
        assert_eq!(focused(&runtime), tab);
        click(&mut runtime, "drawer-terminal");
        assert_eq!(focused(&runtime), tray_id(&runtime, 0));
    }

    #[test]
    fn command_j_toggles_the_last_drawer() {
        let mut runtime = tray_runtime();
        click(&mut runtime, "chip-2");
        click(&mut runtime, "chip-2");
        press(&mut runtime, Key::Command('j'), command());
        assert_eq!(drawer_id(&runtime), tray_id(&runtime, 2));
        press(&mut runtime, Key::Command('j'), command());
        assert_eq!(drawer_id(&runtime), None);
    }

    fn overlay_id(runtime: &Runtime<State>) -> Option<u64> {
        runtime.state.workspace.overlay_session().map(Session::id)
    }

    fn overlay_runtime() -> Runtime<State> {
        let mut state = workspace_state();
        state
            .workspace
            .show_overlay(Some(fixture_tab("lazygit", b"")));
        state.workspace.hide_overlay();
        let mut runtime = runtime_with(state);
        runtime.on_key = Some(on_key);
        runtime
    }

    #[test]
    fn command_g_toggles_the_same_overlay_process() {
        let mut runtime = overlay_runtime();
        assert_eq!(overlay_id(&runtime), None);
        assert_eq!(runtime.find("overlay-panel"), None);
        let kept = runtime
            .state
            .workspace
            .overlay_sessions()
            .next()
            .map(Session::id);
        press(&mut runtime, Key::Command('g'), command());
        assert_eq!(overlay_id(&runtime), kept);
        assert!(runtime.find("overlay-panel").is_some());
        press(&mut runtime, Key::Command('g'), command());
        assert_eq!(overlay_id(&runtime), None);
        press(&mut runtime, Key::Command('g'), command());
        assert_eq!(overlay_id(&runtime), kept);
    }

    #[test]
    fn the_first_command_g_starts_an_overlay_for_the_project() {
        let mut runtime = runtime_with(workspace_state());
        runtime.on_key = Some(on_key);
        press(&mut runtime, Key::Command('g'), command());
        assert!(overlay_id(&runtime).is_some());
        let titles: Vec<_> = runtime.state.workspace.projects[0]
            .tabs
            .iter()
            .map(|tab| tab.title.as_str())
            .collect();
        assert_eq!(titles, ["claude", "shell", "lazygit"]);
    }

    #[test]
    fn the_overlay_takes_keys_and_escape_reaches_it() {
        let mut runtime = overlay_runtime();
        press(&mut runtime, Key::Command('g'), command());
        assert_eq!(
            runtime.state.session().map(Session::id),
            overlay_id(&runtime)
        );
        press(&mut runtime, Key::Escape, Mods::default());
        assert_eq!(
            overlay_id(&runtime),
            runtime.state.session().map(Session::id)
        );
        assert!(overlay_id(&runtime).is_some());
    }

    #[test]
    fn clicking_outside_the_overlay_hides_it_and_inside_does_not() {
        let mut runtime = overlay_runtime();
        press(&mut runtime, Key::Command('g'), command());
        click(&mut runtime, "overlay-terminal");
        assert!(overlay_id(&runtime).is_some());
        let (rect, _) = runtime.presented("overlay-layer").unwrap();
        let (x, y) = (rect[0] + 4.0, rect[1] + 4.0);
        runtime.pointer_down(x, y);
        runtime.pointer_up(x, y);
        runtime.frame(2.0 / 60.0);
        assert_eq!(overlay_id(&runtime), None);
    }

    #[test]
    fn a_failing_overlay_process_stays_shown_with_its_status() {
        let mut state = workspace_state();
        let tab = state.spawn_tab(
            "lazygit",
            Some("sh -c 'sleep 0.2; exit 3'"),
            Path::new("/tmp"),
        );
        state.workspace.show_overlay(Some(tab));
        let mut runtime = runtime_with(state);
        runtime.poll = Some(poll);
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut frame = 1.0;
        while runtime
            .state
            .workspace
            .focused_tab()
            .is_some_and(|(_, tab)| tab.status == Status::Running)
        {
            assert!(std::time::Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(20));
            frame += 1.0;
            runtime.frame(frame / 60.0);
        }
        let status = runtime.state.workspace.overlay_session().map(|_| {
            runtime
                .state
                .workspace
                .focused_tab()
                .map(|(_, tab)| tab.status.clone())
        });
        assert_eq!(status, Some(Some(Status::Failed("exit 3".to_owned()))));
        assert!(runtime.find("restart-hint").is_some());
    }

    #[test]
    fn an_exiting_overlay_process_is_dropped_so_the_next_toggle_starts_afresh() {
        let mut state = workspace_state();
        let tab = state.spawn_tab("lazygit", Some("sleep 0.2"), Path::new("/tmp"));
        state.workspace.show_overlay(Some(tab));
        let mut runtime = runtime_with(state);
        runtime.poll = Some(poll);
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut frame = 1.0;
        while runtime.state.workspace.overlay_sessions().count() > 0 {
            assert!(std::time::Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(20));
            frame += 1.0;
            runtime.frame(frame / 60.0);
        }
        assert_eq!(overlay_id(&runtime), None);
    }

    #[test]
    fn a_chip_menu_removes_its_process() {
        let mut runtime = tray_runtime();
        let (x, y) = runtime.find("chip-1").unwrap();
        runtime.secondary_down(x, y);
        runtime.secondary_up(x, y);
        runtime.frame(1.0 / 60.0);
        runtime.resolve_popup(Some("remove")).unwrap();
        runtime.frame(2.0 / 60.0);
        let titles: Vec<_> = runtime.state.workspace.projects[0]
            .tray
            .iter()
            .map(|tab| tab.title.as_str())
            .collect();
        assert_eq!(titles, ["pnpm dev", "storybook"]);
    }

    fn live_tray(command: &str) -> Runtime<State> {
        let mut state = State::default();
        state
            .workspace
            .add_project(Project::new("/tmp".into(), vec![]));
        open_tray(&mut state, command);
        let mut runtime = runtime_with(state);
        runtime.poll = Some(poll);
        runtime
    }

    fn poll_until(runtime: &mut Runtime<State>, done: impl Fn(&Tab) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut frame = 1.0;
        while !done(&runtime.state.workspace.projects[0].tray[0]) {
            assert!(std::time::Instant::now() < deadline, "timed out");
            std::thread::sleep(std::time::Duration::from_millis(20));
            frame += 1.0;
            runtime.frame(frame / 60.0);
        }
    }

    #[test]
    fn a_crashing_tray_process_shows_as_failed() {
        let mut runtime = live_tray("sh -c 'sleep 0.2; exit 4'");
        poll_until(&mut runtime, |tab| tab.status != Status::Running);
        let tray = &runtime.state.workspace.projects[0].tray[0];
        assert_eq!(tray.status, Status::Failed("exit 4".to_owned()));
        assert_eq!(
            runtime.state.workspace.projects[0].attention(),
            Attention::Alert
        );
    }

    #[test]
    fn restarting_waits_for_the_old_process_then_runs_the_command_again() {
        let mut runtime = live_tray("sleep 100");
        let old = runtime.state.workspace.projects[0].tray[0].session.id();
        restart_tray(&mut runtime.state, 0);
        assert_eq!(
            runtime.state.workspace.projects[0].tray[0].session.id(),
            old
        );
        poll_until(&mut runtime, |tab| tab.session.id() != old);
        let tray = &runtime.state.workspace.projects[0].tray[0];
        assert_eq!(tray.status, Status::Running);
        assert_eq!(tray.command.as_deref(), Some("sleep 100"));
    }

    #[test]
    fn the_wt_prefix_offers_creating_a_worktree_first() {
        let workspace = one_project();
        let create = Action::CreateWorktree("feature-x".to_owned());
        let actions = palette_actions("wt feature-x", None, &workspace);
        assert_eq!(actions[0], create);
        assert!(actions.contains(&Action::Run("wt feature-x".to_owned())));
        for query in [
            "feature-x",
            "wt",
            "wt ",
            "wt -x",
            "wt two words",
            "wtf x",
            "",
        ] {
            assert!(
                !palette_actions(query, None, &workspace)
                    .iter()
                    .any(|action| matches!(action, Action::CreateWorktree(_))),
                "{query:?}"
            );
        }
        let none = palette_actions("wt feature-x", None, &Workspace::default());
        assert!(!none.contains(&create));
    }

    /// Stands in for the login shell, so no real `wt` runs. It records its arguments and runs `body`.
    fn stub_shell(body: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let record = directory.path().join("args");
        let shell = directory.path().join("stub-shell");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n{body}\n",
            record.display()
        );
        std::fs::write(&shell, script).unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        (directory, shell, record)
    }

    fn create_worktree_from_palette(shell: PathBuf, branch: &str) -> Runtime<State> {
        let mut state = workspace_state();
        state.config.shell = shell;
        state.workspace.projects[0].root = std::env::temp_dir();
        let mut runtime = runtime_with(state);
        runtime.on_key = Some(on_key);
        runtime.poll = Some(poll);
        press(&mut runtime, Key::Command('p'), command());
        press(
            &mut runtime,
            Key::Text(format!("wt {branch}")),
            Mods::default(),
        );
        press(&mut runtime, Key::Enter, Mods::default());
        runtime
    }

    fn poll_until_created(runtime: &mut Runtime<State>) {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut frame = 1.0;
        while runtime.state.creation.is_some() {
            assert!(std::time::Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(20));
            frame += 1.0;
            runtime.frame(frame / 60.0);
        }
    }

    #[test]
    fn creating_a_worktree_runs_wt_switch_and_opens_the_result_as_a_project() {
        let created = tempfile::tempdir().unwrap();
        let (_directory, shell, record) = stub_shell(&format!(
            "echo '{{\"path\":\"{}\"}}'",
            created.path().display()
        ));
        let mut runtime = create_worktree_from_palette(shell, "feature-x");
        assert!(runtime.state.palette_open);
        assert!(runtime.find("palette-0").is_some());
        poll_until_created(&mut runtime);
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.projects.len(), 3);
        assert_eq!(workspace.active, 2);
        assert_eq!(workspace.projects[2].root, created.path());
        assert!(!runtime.state.palette_open);
        let args = std::fs::read_to_string(record).unwrap();
        assert!(
            args.contains("wt switch --create --no-cd --format=json \"$@\""),
            "{args}"
        );
        assert!(args.contains("agentd\nfeature-x\n"));
    }

    #[test]
    fn a_refused_worktree_keeps_the_command_centre_open_with_the_reason() {
        let (_directory, shell, _) =
            stub_shell("echo '✗ Branch feature-x already exists' >&2; exit 1");
        let mut runtime = create_worktree_from_palette(shell, "feature-x");
        poll_until_created(&mut runtime);
        runtime.frame(1000.0);
        assert_eq!(runtime.state.workspace.projects.len(), 2);
        assert!(runtime.state.palette_open);
        assert_eq!(
            runtime.state.palette_error.as_deref(),
            Some("✗ Branch feature-x already exists")
        );
        assert!(runtime.find("palette-error").is_some());
    }

    #[test]
    fn escape_cancels_a_running_worktree_creation_without_opening_a_project() {
        let pids = tempfile::tempdir().unwrap();
        let pid_file = pids.path().join("pid");
        let (_directory, shell, _) = stub_shell(&format!(
            "echo $$ > '{}'; echo '{{\"path\":\"/tmp\"}}'; sleep 30",
            pid_file.display()
        ));
        let mut runtime = create_worktree_from_palette(shell, "feature-x");
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
            {
                break pid;
            }
            assert!(std::time::Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(20));
        };
        press(&mut runtime, Key::Escape, Mods::default());
        assert!(runtime.state.creation.is_none());
        assert!(!runtime.state.palette_open);
        while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok() {
            assert!(std::time::Instant::now() < deadline, "wt was not killed");
            std::thread::sleep(Duration::from_millis(20));
        }
        runtime.frame(1000.0);
        assert_eq!(runtime.state.workspace.projects.len(), 2);
    }

    #[test]
    fn tray_restarts_and_stops_are_offered_for_a_matching_query() {
        let mut state = State::default();
        let mut workspace = one_project();
        workspace.add_tray(fixture_tab("pnpm dev", b""));
        let mut stopped = fixture_tab("storybook", b"");
        stopped.status = Status::Stopped;
        workspace.add_tray(stopped);
        state.workspace = workspace;
        let actions = palette_actions("stop", None, &state.workspace);
        assert_eq!(actions[2], Action::StopTray(0, "pnpm dev".to_owned()));
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, Action::StopTray(1, _)))
        );
        let actions = palette_actions("restart", None, &state.workspace);
        assert_eq!(
            actions[2..4],
            [
                Action::RestartTray(0, "pnpm dev".to_owned()),
                Action::RestartTray(1, "storybook".to_owned()),
            ]
        );
        let actions = palette_actions("restart sto", None, &state.workspace);
        assert_eq!(actions[2], Action::RestartTray(1, "storybook".to_owned()));
        assert!(
            palette_actions("", None, &state.workspace)
                .iter()
                .all(|action| {
                    !matches!(action, Action::RestartTray(..) | Action::StopTray(..))
                })
        );
    }

    fn run_tray_action_from_palette(runtime: &mut Runtime<State>, query: &str) {
        runtime.on_key = Some(on_key);
        press(runtime, Key::Command('p'), command());
        press(runtime, Key::Text(query.into()), Mods::default());
        runtime.state.palette_selected = 2;
        press(runtime, Key::Enter, Mods::default());
    }

    #[test]
    fn the_palette_restarts_a_tray_process() {
        let mut runtime = live_tray("sleep 100");
        let old = runtime.state.workspace.projects[0].tray[0].session.id();
        run_tray_action_from_palette(&mut runtime, "restart sleep");
        poll_until(&mut runtime, |tab| tab.session.id() != old);
        let tray = &runtime.state.workspace.projects[0].tray[0];
        assert_eq!(tray.status, Status::Running);
    }

    #[test]
    fn the_palette_stops_a_tray_process() {
        let mut runtime = live_tray("sleep 100");
        run_tray_action_from_palette(&mut runtime, "stop sleep");
        poll_until(&mut runtime, |tab| tab.status == Status::Stopped);
    }

    #[test]
    fn a_tray_process_starts_and_restarts_in_the_shells_folder() {
        let mut state = State::default();
        state
            .workspace
            .add_project(Project::new("/tmp".into(), vec![]));
        open_tab(
            &mut state,
            "shell",
            Some("sh -c 'cd /usr/bin && exec sleep 100'"),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.session().and_then(Session::cwd) != Some("/usr/bin".into()) {
            assert!(
                std::time::Instant::now() < deadline,
                "shell never left /tmp"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        open_tray(&mut state, "sleep 100");
        let mut runtime = runtime_with(state);
        runtime.poll = Some(poll);
        let in_usr_bin = |tab: &Tab| tab.session.cwd() == Some("/usr/bin".into());
        poll_until(&mut runtime, in_usr_bin);
        let old = runtime.state.workspace.projects[0].tray[0].session.id();
        restart_tray(&mut runtime.state, 0);
        poll_until(&mut runtime, |tab| {
            tab.session.id() != old && in_usr_bin(tab)
        });
    }

    #[test]
    fn the_tray_bar_shows_only_with_tray_processes() {
        assert!(runtime_with(workspace_state()).find("tray-bar").is_none());
        assert!(tray_runtime().find("tray-bar").is_some());
    }

    #[test]
    fn an_empty_workspace_shows_a_hint() {
        let runtime = runtime_with(State::default());
        assert!(runtime.find("empty-hint").is_some());
    }

    #[test]
    fn switching_tabs_dismisses_a_pending_paste() {
        let mut state = workspace_state();
        let session = state.session().map(Session::id).unwrap();
        state.pending_paste = Some((session, "rm -rf x\n".to_owned()));
        let mut runtime = runtime_with(state);
        runtime.poll = Some(poll);
        runtime.frame(1.0 / 60.0);
        assert!(runtime.find("paste-confirm").is_some());
        click(&mut runtime, "tab-1");
        runtime.frame(2.0 / 60.0);
        assert!(runtime.state.pending_paste.is_none());
        assert!(runtime.find("paste-confirm").is_none());
    }

    #[test]
    fn tab_and_escape_reach_the_window_after_clicking_a_tab() {
        let mut runtime = runtime_with(workspace_state());
        runtime.on_key = Some(|_, _, _| {
            WINDOW_KEYS.fetch_add(1, Ordering::SeqCst);
            true
        });
        click(&mut runtime, "tab-0");
        runtime.key(KeyEvent {
            key: Key::Tab,
            mods: Mods::default(),
        });
        runtime.key(KeyEvent {
            key: Key::Escape,
            mods: Mods::default(),
        });
        runtime.key(KeyEvent {
            key: Key::Text(" ".into()),
            mods: Mods::default(),
        });
        runtime.frame(1.0 / 60.0);
        assert_eq!(WINDOW_KEYS.load(Ordering::SeqCst), 3);
    }

    fn saved_project(root: &str) -> workspace::SavedProject {
        workspace::SavedProject {
            root: root.into(),
            active_tab: 0,
            tabs: vec![workspace::SavedTab {
                title: "sleep".to_owned(),
                command: Some("sleep 100".to_owned()),
                cwd: None,
            }],
            tray: vec![],
        }
    }

    #[test]
    fn a_restored_project_starts_its_tabs_when_first_shown() {
        let tmp = std::env::temp_dir().canonicalize().unwrap();
        let saved = SavedWorkspace {
            active: 0,
            sidebar_hidden: false,
            projects: vec![saved_project("/"), saved_project(tmp.to_str().unwrap())],
        };
        let mut runtime = runtime_with(State {
            workspace: Workspace::restore(saved),
            started: true,
            ..State::default()
        });
        runtime.poll = Some(poll);
        runtime.frame(1.0 / 60.0);
        let launched = |runtime: &Runtime<State>| -> Vec<bool> {
            let projects = &runtime.state.workspace.projects;
            projects.iter().map(|project| project.launched).collect()
        };
        assert_eq!(launched(&runtime), [true, false]);
        let tab = &runtime.state.workspace.projects[0].tabs[0];
        assert_eq!(tab.command.as_deref(), Some("sleep 100"));
        assert_eq!(tab.cwd, Some("/".into()));
        assert!(tab.session.cwd().is_some(), "a real process runs");
        runtime.state.workspace.select_project(1);
        runtime.frame(2.0 / 60.0);
        assert_eq!(launched(&runtime), [true, true]);
    }

    #[test]
    fn scenes_read_and_write_no_files() {
        for (name, scene) in SCENES {
            let mut state = State {
                paths: config::Paths::from_home(),
                ..State::default()
            };
            scene(&mut state);
            assert!(state.started && state.paths.is_none(), "{name}");
        }
    }

    #[test]
    fn the_layout_saves_and_loads_back() {
        let directory = tempfile::tempdir().unwrap();
        let paths = || config::Paths {
            dir: directory.path().join("agentd"),
            ghostty_dir: directory.path().join("ghostty"),
        };
        let mut state = State::default();
        for root in ["/", "/usr"] {
            let tabs = vec![fixture_tab("shell", b"$ "), fixture_tab("claude", b"")];
            state.workspace.add_project(Project::new(root.into(), tabs));
        }
        state.workspace.select_tab(1);
        state.workspace.add_tray(fixture_tab("pnpm dev", b""));
        state.sidebar_hidden = true;
        state.paths = Some(paths());
        save_layout(&state).unwrap();
        let mut loaded = State {
            paths: Some(paths()),
            ..State::default()
        };
        load_layout(&mut loaded);
        assert!(loaded.sidebar_hidden);
        assert_eq!(
            loaded.workspace.snapshot(true),
            state.workspace.snapshot(true)
        );
    }

    #[test]
    fn enter_restarts_an_exited_tab() {
        let mut state = State::default();
        state
            .workspace
            .add_project(Project::new("/tmp".into(), vec![]));
        open_tab(&mut state, "quick", Some("sh -c 'exit 0'"));
        let mut runtime = runtime_with(state);
        runtime.poll = Some(poll);
        runtime.on_key = Some(on_key);
        let old = runtime.state.workspace.active_session().map(Session::id);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut frame = 1.0;
        while runtime.find("restart-hint").is_none() {
            assert!(std::time::Instant::now() < deadline, "timed out");
            std::thread::sleep(std::time::Duration::from_millis(20));
            frame += 1.0;
            runtime.frame(frame / 60.0);
        }
        runtime.key(KeyEvent {
            key: Key::Enter,
            mods: Mods::default(),
        });
        runtime.frame((frame + 1.0) / 60.0);
        let tab = runtime.state.workspace.active_tab().unwrap();
        assert_ne!(Some(tab.session.id()), old);
        assert_eq!(tab.status, Status::Running);
        assert_eq!(tab.command.as_deref(), Some("sh -c 'exit 0'"));
    }
    fn counting_waker() -> (Waker, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let waker: Waker = {
            let count = count.clone();
            Arc::new(move || {
                count.fetch_add(1, Ordering::Relaxed);
            })
        };
        (waker, count)
    }

    fn second_window(fleet: &Fleet, window: WindowId) -> Runtime<State> {
        let state = State {
            fleet: fleet.clone(),
            window,
            started: true,
            ..State::default()
        };
        state.fleet.register(window, Arc::new(|| {}));
        let mut runtime = runtime_with(state);
        runtime.poll = Some(poll);
        runtime
    }

    #[test]
    fn moving_a_project_is_offered_only_beside_another_project() {
        let moves = |workspace: &Workspace| {
            palette_actions("", None, workspace).contains(&Action::MoveToNewWindow)
        };
        assert!(!moves(&one_project()));
        assert!(moves(&workspace_state().workspace));
        assert_eq!(
            Action::MoveToNewWindow.label(),
            "Move project to new window"
        );
    }

    #[test]
    fn the_palette_hands_the_active_project_to_a_new_window() {
        let mut runtime = workspace_runtime();
        runtime.state.workspace.select_project(1);
        press(&mut runtime, Key::Command('p'), command());
        press(
            &mut runtime,
            Key::Text("move project".into()),
            Mods::default(),
        );
        runtime.state.palette_selected = 0;
        press(&mut runtime, Key::Enter, Mods::default());
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.projects.len(), 1);
        assert_eq!(
            workspace.project().map(|project| project.name.as_str()),
            Some("agentd")
        );

        let mut second = second_window(&runtime.state.fleet, 1);
        second.frame(3.0 / 60.0);
        let adopted = &second.state.workspace;
        assert_eq!(adopted.projects.len(), 1);
        assert_eq!(
            adopted.project().map(|project| project.name.as_str()),
            Some("site")
        );
        assert_eq!(
            adopted.active_tab().map(|tab| tab.title.as_str()),
            Some("claude")
        );
    }

    #[test]
    fn a_window_keeps_its_last_project() {
        let mut state = State::default();
        state
            .workspace
            .add_project(Project::new("/tmp".into(), vec![]));
        move_to_new_window(&mut state, 0);
        assert_eq!(state.workspace.projects.len(), 1);
        assert!(state.fleet.take(1).is_empty());
    }

    #[test]
    fn each_window_shows_only_its_own_active_session() {
        let mut runtime = workspace_runtime();
        move_to_new_window(&mut runtime.state, 1);
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
        move_to_new_window(&mut state, 0);

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
    fn a_project_menu_moves_it_to_a_new_window() {
        let mut runtime = workspace_runtime();
        let (x, y) = runtime.find("project-1").unwrap();
        runtime.secondary_down(x, y);
        runtime.secondary_up(x, y);
        runtime.frame(1.0 / 60.0);
        runtime.resolve_popup(Some("move")).unwrap();
        runtime.frame(2.0 / 60.0);
        assert_eq!(runtime.state.workspace.projects.len(), 1);
        assert_eq!(runtime.state.fleet.take(1).len(), 1);
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
    fn the_last_window_to_quit_saves_every_windows_projects() {
        let directory = tempfile::tempdir().unwrap();
        let paths = config::Paths {
            dir: directory.path().join("agentd"),
            ghostty_dir: directory.path().join("ghostty"),
        };
        let fleet = Fleet::default();
        let window = |window, root: &str| {
            let mut state = State {
                fleet: fleet.clone(),
                window,
                paths: Some(paths.clone()),
                ..State::default()
            };
            let tabs = vec![fixture_tab("shell", b"$ ")];
            state.workspace.add_project(Project::new(root.into(), tabs));
            state
        };
        let (main, second) = (window(MAIN_WINDOW, "/"), window(1, "/usr"));
        save_layout(&second).unwrap();
        assert!(!paths.state().exists());
        save_layout(&main).unwrap();
        let mut loaded = State {
            paths: Some(paths),
            ..State::default()
        };
        load_layout(&mut loaded);
        let roots: Vec<_> = loaded
            .workspace
            .projects
            .iter()
            .map(|project| project.root.clone())
            .collect();
        assert_eq!(roots, [PathBuf::from("/"), PathBuf::from("/usr")]);
    }
}
