use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use libghostty_vt::effect::ClipboardLocation;
use rune::{
    App, AppSpec, ClipboardContent, FileDialogOptions, Ime, Key, KeyEvent, Live, Mods, Tx, Waker,
};

use crate::LogErr;
use crate::config;
use crate::find::{Find, open_find, poll_find};
use crate::grid::{Grid, Surface};
use crate::input::caret_rect;
use crate::keymap::{Command, pressed, shortcut};
use crate::palette::{
    Action, Palette, open_palette, poll_creation, poll_jump, poll_worktrees, run_action,
};
use crate::protocol::Event;
use crate::render::TerminalSource;
use crate::secure_input::SecureInput;
use crate::session::{Attention, Retarget, Session};
use crate::view::{DRAWER_MARK, OVERLAY_MARK, view, window_title};
use crate::windows::{
    Fleet, MAIN_WINDOW, WindowId, adopt_projects, move_to_new_window, on_close, on_quit,
};
use crate::workspace::{Project, SavedWorkspace, Slot, Status, Tab, Workspace};
use anyhow::Context;

/// Asks for a frame without a new view: terminal output only changes the effect slot's pixels.
#[derive(Clone)]
pub(crate) struct Repaint {
    pub(crate) live: Live<f32>,
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
pub(crate) struct State {
    pub(crate) waker: Option<Waker>,
    pub(crate) repaint: Repaint,
    pub(crate) fleet: Fleet,
    pub(crate) window: WindowId,
    pub(crate) workspace: Workspace,
    pub(crate) started: bool,
    /// Session id and scrollbar presence last built for the tab, the drawer and the overlay.
    pub(crate) drawn: [(u64, bool); 3],
    /// Sessions showing an attention dot, as the actors raise it without an event.
    pub(crate) alerting: Vec<u64>,
    /// The project name the window title was last set for; `Some(None)` is the title with no project.
    pub(crate) titled: Option<Option<String>>,
    /// Waiting confirmations, answered newest first; dismissed when keys move elsewhere.
    pub(crate) confirms: Vec<Confirm>,
    pub(crate) hovered_project: Option<usize>,
    pub(crate) preedit: String,
    pub(crate) option_held: bool,
    pub(crate) caret: Option<[f32; 4]>,
    pub(crate) palette: Palette,
    pub(crate) find: Find,
    pub(crate) sidebar_hidden: bool,
    pub(crate) config: config::Config,
    /// Points added to the configured font size by ⌘= and ⌘-; never written to disk.
    pub(crate) font_zoom: f64,
    /// Unset in tests and scenes, which then read and write no files.
    pub(crate) paths: Option<config::Paths>,
    pub(crate) secure_input: SecureInput,
}

impl State {
    pub(crate) fn font_size(&self) -> f64 {
        (self.config.font.size + self.font_zoom).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
    }

    /// Secure input is on for the user's choice or a password prompt, only while this window has focus.
    pub(crate) fn sync_secure_input(&mut self) -> bool {
        let password_prompt = self.config.auto_secure_input
            && self
                .session()
                .is_some_and(|session| session.grid().password_input);
        let wanted = self.workspace.window_focused && (self.secure_input.manual || password_prompt);
        self.secure_input.set(wanted)
    }

    pub(crate) fn zoom(&mut self, points: f64) {
        let size = (self.font_size() + points).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        self.font_zoom = size - self.config.font.size;
    }
}

impl TerminalSource for State {
    fn font(&self) -> (&config::FontConfig, f64) {
        (&self.config.font, self.font_size())
    }

    fn resize(&self, mark: u32, surface: Surface) {
        // Hidden tabs share the visible area, as closed tray processes share the drawer's, so they are sized too.
        let workspace = &self.workspace;
        match mark {
            OVERLAY_MARK => workspace
                .overlay_sessions()
                .for_each(|session| session.resize(surface)),
            DRAWER_MARK => workspace
                .tray_sessions()
                .for_each(|session| session.resize(surface)),
            _ => workspace
                .tab_sessions()
                .for_each(|session| session.resize(surface)),
        }
    }

    fn waker(&self) -> Waker {
        self.window_waker()
    }

    fn drawn_session(&self, mark: u32) -> Option<&Session> {
        let workspace = &self.workspace;
        match mark {
            OVERLAY_MARK => workspace.overlay_session(),
            DRAWER_MARK => workspace
                .project()
                // Not `drawer_session`, which is gone once closing starts, so the drawer slides out showing its process.
                .and_then(|project| project.tray.get(project.drawer))
                .map(|tab| &tab.session),
            _ => workspace.active_session(),
        }
    }
}

const MIN_FONT_SIZE: f64 = 6.0;
const MAX_FONT_SIZE: f64 = 72.0;

impl State {
    /// The session keys go to.
    pub(crate) fn session(&self) -> Option<&Session> {
        self.workspace.focused_session()
    }

    pub(crate) fn window_waker(&self) -> Waker {
        let waker = self.waker.clone().unwrap_or_else(|| Arc::new(|| {}));
        self.repaint.waker(waker)
    }

    pub(crate) fn spawn_tab(&self, title: &str, command: Option<&str>, cwd: &Path) -> Tab {
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

    pub(crate) fn active_root(&self) -> Option<PathBuf> {
        self.workspace.project().map(|project| project.root.clone())
    }
}

pub(crate) fn open_tab(state: &mut State, title: &str, command: Option<&str>) {
    if let Some(root) = state.active_root() {
        let tab = state.spawn_tab(title, command, &root);
        state.workspace.add_tab(tab);
    }
}

/// Runs in the focused shell's folder, so a command typed for a subfolder works there.
pub(crate) fn open_tray(state: &mut State, command: &str) {
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
pub(crate) fn save_layout(state: &State) -> anyhow::Result<()> {
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

pub(crate) fn tray_action(state: &mut State, index: usize, picked: Option<&str>) {
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

pub(crate) enum Confirm {
    /// The session that refused a paste, and the text.
    Paste { session: u64, text: String },
    /// A program's OSC 52 write, held until the user agrees to replace the clipboard.
    ClipboardWrite { session: u64, text: String },
    /// A program's OSC 52 read; it stays blocked until this is answered, even when refused.
    ClipboardRead {
        session: u64,
        location: ClipboardLocation,
    },
    /// The session whose close waits, and the project it takes whole rather than the focused tab.
    Close {
        session: u64,
        project: Option<PathBuf>,
    },
}

impl Confirm {
    fn session(&self) -> u64 {
        match self {
            Self::Paste { session, .. }
            | Self::ClipboardWrite { session, .. }
            | Self::ClipboardRead { session, .. }
            | Self::Close { session, .. } => *session,
        }
    }
}

/// A repeated request replaces the pending one of its kind and becomes the newest.
fn push_confirm(confirms: &mut Vec<Confirm>, confirm: Confirm) {
    confirms.retain(|pending| {
        pending.session() != confirm.session()
            || std::mem::discriminant(pending) != std::mem::discriminant(&confirm)
    });
    confirms.push(confirm);
}

/// Closes the focused tab or drawer process, first asking if a program is running in it.
fn request_close(state: &mut State) {
    let Some((_, tab)) = state.workspace.focused_tab() else {
        return;
    };
    if tab.runs_program() {
        let session = tab.session.id();
        push_confirm(
            &mut state.confirms,
            Confirm::Close {
                session,
                project: None,
            },
        );
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
pub(crate) fn request_close_project(state: &mut State, index: usize) {
    let Some(project) = state.workspace.projects.get(index) else {
        return;
    };
    if project.runs_program()
        && let Some(session) = state.session()
    {
        let confirm = Confirm::Close {
            session: session.id(),
            project: Some(project.root.clone()),
        };
        push_confirm(&mut state.confirms, confirm);
    } else {
        state.workspace.close_project(index);
    }
}

/// A second read while one waits is refused, so the program is never left with two to answer.
fn asks_to_read(confirms: &[Confirm], session: u64) -> bool {
    confirms
        .iter()
        .any(|confirm| matches!(confirm, Confirm::ClipboardRead { session: asking, .. } if *asking == session))
}

/// Hands the program its clipboard once the UI has read it.
fn read_clipboard_for(tx: &mut Tx, session: u64, location: ClipboardLocation) {
    tx.read_clipboard(
        move |state: &mut State, content: &ClipboardContent, _: &mut Tx| {
            answer_clipboard_read(state, session, location, Some(content.text().to_owned()));
        },
    );
}

fn answer_clipboard_read(
    state: &State,
    session: u64,
    location: ClipboardLocation,
    text: Option<String>,
) {
    if let Some(session) = state
        .workspace
        .sessions()
        .find(|other| other.id() == session)
    {
        session.clipboard_reply(location, text);
    }
}

pub(crate) fn resolve_confirm(state: &mut State, index: usize, confirmed: bool, tx: &mut Tx) {
    if index >= state.confirms.len() {
        return;
    }
    let confirm = state.confirms.remove(index);
    let active = Some(confirm.session()) == state.session().map(Session::id);
    if let Confirm::ClipboardRead { session, location } = confirm {
        if confirmed && active {
            read_clipboard_for(tx, session, location);
        } else {
            answer_clipboard_read(state, session, location, None);
        }
        return;
    }
    if !confirmed || !active {
        return;
    }
    match confirm {
        Confirm::ClipboardWrite { text, .. } => tx.copy(text),
        Confirm::ClipboardRead { .. } => {}
        Confirm::Paste { text, .. } => {
            if let Some(session) = state.session() {
                session.paste(text, true);
            }
        }
        Confirm::Close { project, .. } => {
            match project.and_then(|root| state.workspace.project_index(&root)) {
                Some(index) => state.workspace.close_project(index),
                None => close_focused(state),
            }
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

pub(crate) fn open_project(state: &mut State, root: PathBuf) {
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

pub(crate) fn pick_new_tab(state: &mut State, picked: Option<&str>, tx: &mut Tx) {
    match picked {
        Some("shell") => run_action(state, &Action::Command(Command::NewShell), tx),
        Some("lazygit") => run_action(state, &Action::Command(Command::ToggleLazygit), tx),
        Some("palette" | "tray") => open_palette(state, tx),
        _ => {}
    }
}

pub(crate) fn run_command(state: &mut State, command: Command, tx: &mut Tx) {
    match command {
        Command::NewShell => open_tab(state, "shell", None),
        Command::ToggleLazygit => toggle_lazygit(state),
        Command::OpenProject => pick_folder(tx, false),
        Command::ChangeFolder => pick_folder(tx, true),
        Command::MoveToNewWindow => move_to_new_window(state, state.workspace.active, None),
        Command::Palette => open_palette(state, tx),
        Command::Find => open_find(state, tx),
        Command::ToggleSidebar => state.sidebar_hidden = !state.sidebar_hidden,
        Command::ToggleDrawer => toggle_last_drawer(state),
        Command::CloseTab => request_close(state),
        Command::ToggleSecureInput => {
            state.secure_input.manual = !state.secure_input.manual;
            state.sync_secure_input();
        }
        Command::ZoomIn => state.zoom(1.0),
        Command::ZoomOut => state.zoom(-1.0),
        Command::ZoomReset => state.font_zoom = 0.0,
        Command::PrevTab => state.workspace.cycle_tab(-1),
        Command::NextTab => state.workspace.cycle_tab(1),
    }
}

pub(crate) fn poll(state: &mut State, now: f64, tx: &mut Tx) -> bool {
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
    let project_name = state.workspace.project().map(|project| &project.name);
    if state.titled.as_ref().map(Option::as_ref) != Some(project_name) {
        tx.set_title(window_title(&state.workspace));
        state.titled = Some(project_name.cloned());
    }
    if state.window != MAIN_WINDOW && state.workspace.projects.is_empty() {
        tx.close_window();
    }
    launch_active(state);
    let active = state.session().map(Session::id);
    changed |= poll_jump(state, now, tx);
    changed |= poll_find(state, active);
    changed |= poll_worktrees(state);
    changed |= poll_creation(state);
    let (kept, dismissed): (Vec<_>, Vec<_>) = std::mem::take(&mut state.confirms)
        .into_iter()
        .partition(|confirm| Some(confirm.session()) == active);
    state.confirms = kept;
    changed |= !dismissed.is_empty();
    for confirm in dismissed {
        if let Confirm::ClipboardRead { session, location } = confirm {
            answer_clipboard_read(state, session, location, None);
        }
    }
    let mut restarts = Vec::new();
    let mut exits = Vec::new();
    let mut notices = Vec::new();
    for tab in state.workspace.tabs_mut() {
        while let Some(event) = tab.session.next_event() {
            match event {
                Event::Copied(text) => tx.copy(text),
                Event::PasteRejected(text) if Some(tab.session.id()) == active => {
                    let session = tab.session.id();
                    push_confirm(&mut state.confirms, Confirm::Paste { session, text });
                    changed = true;
                }
                Event::PasteRejected(_) => {}
                Event::ClipboardWrite { text, ask: false } => tx.copy(text),
                Event::ClipboardWrite { text, ask: true } if Some(tab.session.id()) == active => {
                    let session = tab.session.id();
                    push_confirm(
                        &mut state.confirms,
                        Confirm::ClipboardWrite { session, text },
                    );
                    changed = true;
                }
                Event::ClipboardWrite { .. } => {}
                Event::ClipboardRead {
                    location,
                    ask: false,
                } => {
                    read_clipboard_for(tx, tab.session.id(), location);
                }
                Event::ClipboardRead {
                    location,
                    ask: true,
                } if Some(tab.session.id()) == active
                    && !asks_to_read(&state.confirms, tab.session.id()) =>
                {
                    let session = tab.session.id();
                    state
                        .confirms
                        .push(Confirm::ClipboardRead { session, location });
                    changed = true;
                }
                Event::ClipboardRead { location, .. } => {
                    tab.session.clipboard_reply(location, None)
                }
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
            let notification =
                crate::notify::Notification::new(&project.name, tab.label(), title, body);
            crate::notify::post(session, &notification);
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
    let alerting = || {
        state
            .workspace
            .sessions()
            .filter(|session| session.attention() == Attention::Alert)
            .map(Session::id)
    };
    if !alerting().eq(state.alerting.iter().copied()) {
        state.alerting = alerting().collect();
        changed = true;
    }
    changed | state.sync_secure_input()
}

pub(crate) fn on_key(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    if !state.confirms.is_empty() && matches!(event.key, Key::Enter | Key::Escape) {
        resolve_confirm(state, state.confirms.len() - 1, event.key == Key::Enter, tx);
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
        crate::input::map_key(event, tx.physical_key(), tx.key_base()),
    ) {
        (Some(session), Some(key)) => {
            session.key(key);
            true
        }
        _ => false,
    }
}

pub(crate) fn on_ime(state: &mut State, ime: &Ime, _: &mut Tx) -> bool {
    match ime {
        Ime::Preedit(preedit, _) => {
            state.preedit.clone_from(preedit);
            true
        }
        Ime::Commit(committed) => {
            state.preedit.clear();
            // Rune commits an open composition when the window goes inactive; in a terminal
            // that would type unfinished text into the program, so drop it.
            if !state.workspace.window_focused {
                return true;
            }
            if let Some(session) = state.session() {
                session.key(crate::input::map_text(
                    committed,
                    Mods::default(),
                    None,
                    None,
                ));
            }
            true
        }
        _ => false,
    }
}

/// One window's app, with its state made by `init`.
pub(crate) fn window_app(
    fleet: &Fleet,
    window: WindowId,
    configure: impl FnOnce(&mut State) + 'static,
) -> App<State> {
    let spec = AppSpec::<State> {
        scenes: crate::scenes::SCENES,
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
        .compositor(crate::render::TerminalCompositor::default())
        .on_ime(on_ime)
        .on_modifiers(|state, mods, _| {
            state.option_held = mods.alt;
            for tab in state.workspace.tabs_mut() {
                tab.session.command_held(mods.logo);
            }
        })
        .on_quit(on_quit)
        .on_close(on_close)
        .on_focus(|state, focused, _| {
            state.workspace.set_window_focused(focused);
            state.sync_secure_input();
        });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Metrics;
    use crate::scenes::*;
    use crate::test_support::*;
    use crate::view::{DRAWER_HEIGHT, RAIL_WIDTH, SIDEBAR_WIDTH};
    use crate::workspace;
    use rune::{Cursor, Runtime};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    static WINDOW_KEYS: AtomicUsize = AtomicUsize::new(0);
    static KEYS_AFTER_TERMINAL_CLICK: AtomicUsize = AtomicUsize::new(0);

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
            confirms: vec![Confirm::Paste {
                session: 0,
                text: "rm -rf x\n".to_owned(),
            }],
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

    fn secure_input_state() -> State {
        let mut state = State::default();
        crate::scenes::show_scene(&mut state, b"");
        state.workspace.set_window_focused(true);
        state
    }

    #[test]
    fn a_password_prompt_turns_secure_input_on_until_it_ends() {
        let mut state = secure_input_state();
        let password = Grid {
            password_input: true,
            ..Grid::default()
        };
        state.session().unwrap().publish_fixture(password);
        assert!(state.sync_secure_input());
        assert!(state.secure_input.enabled);
        state.session().unwrap().publish_fixture(Grid::default());
        assert!(state.sync_secure_input());
        assert!(!state.secure_input.enabled);
    }

    #[test]
    fn auto_secure_input_can_be_turned_off() {
        let mut state = secure_input_state();
        state.config.auto_secure_input = false;
        let password = Grid {
            password_input: true,
            ..Grid::default()
        };
        state.session().unwrap().publish_fixture(password);
        assert!(!state.sync_secure_input());
        assert!(!state.secure_input.enabled);
    }

    #[test]
    fn the_manual_toggle_survives_tabs_but_not_losing_focus() {
        let mut state = secure_input_state();
        let mut tx = Tx::default();
        run_command(&mut state, Command::ToggleSecureInput, &mut tx);
        assert!(state.secure_input.enabled);
        open_tab(&mut state, "other", Some("cat"));
        assert!(!state.sync_secure_input());
        assert!(state.secure_input.enabled);
        state.workspace.set_window_focused(false);
        assert!(state.sync_secure_input());
        assert!(!state.secure_input.enabled);
        state.workspace.set_window_focused(true);
        assert!(state.sync_secure_input());
        run_command(&mut state, Command::ToggleSecureInput, &mut tx);
        assert!(!state.secure_input.enabled);
    }

    #[test]
    fn a_composition_finished_by_leaving_the_window_stays_out_of_the_terminal() {
        let mut state = State::default();
        state
            .workspace
            .add_project(Project::new("/tmp".into(), vec![]));
        open_tab(&mut state, "shell", Some("cat"));
        let mut tx = Tx::default();
        state.workspace.set_window_focused(true);
        on_ime(
            &mut state,
            &Ime::Preedit("leaked".to_owned(), None),
            &mut tx,
        );
        state.workspace.set_window_focused(false);
        on_ime(&mut state, &Ime::Commit("leaked".to_owned()), &mut tx);
        assert_eq!(state.preedit, "");
        state.workspace.set_window_focused(true);
        on_ime(&mut state, &Ime::Commit("ok".to_owned()), &mut tx);
        let session = state.session().unwrap();
        let enter = KeyEvent {
            key: Key::Enter,
            mods: Mods::default(),
        };
        session.key(crate::input::map_key(&enter, None, None).unwrap());
        wait_until(
            Duration::from_secs(15),
            || format!("{:#?}", session.grid().lines()),
            || {
                let lines = session.grid().lines();
                lines
                    .iter()
                    .any(|line| line.trim_end() == "ok")
                    .then_some(())
            },
        );
    }

    #[test]
    fn the_caret_follows_the_cursor_in_points() {
        let mut grid = Grid::from_vt(b"ab", 10, 2).unwrap();
        grid.metrics = Metrics {
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
            grid.metrics = Metrics {
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

    #[test]
    fn clicking_a_project_and_a_tab_selects_them() {
        let mut runtime = runtime_with(workspace_state());
        click(&mut runtime, "project-1");
        click(&mut runtime, "tab-1");
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.active, 1);
        assert_eq!(workspace.projects[1].active_tab, 1);
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

    fn sidebar_width(runtime: &Runtime<State>) -> f32 {
        runtime
            .presented("sidebar")
            .map_or(-1.0, |(rect, _)| rect[2])
    }

    fn zoomed(chords: &[char]) -> Runtime<State> {
        let mut state = State::default();
        show_scene(&mut state, SCENE_FIXTURE);
        let mut runtime = runtime_with(state);
        runtime.on_key = Some(on_key);
        for chord in chords {
            press(&mut runtime, Key::Command(*chord), command());
        }
        runtime
    }

    #[test]
    fn command_equals_and_minus_zoom_the_font_by_one_point() {
        let base = State::default().font_size();
        assert_eq!(zoomed(&['=']).state.font_size(), base + 1.0);
        assert_eq!(zoomed(&['=', '=', '-']).state.font_size(), base + 1.0);
        assert_eq!(zoomed(&['-']).state.font_size(), base - 1.0);
    }

    #[test]
    fn command_zero_restores_the_configured_size() {
        let base = State::default().font_size();
        assert_eq!(zoomed(&['=', '=', '0']).state.font_size(), base);
        assert_eq!(zoomed(&['-', '0']).state.font_size(), base);
    }

    #[test]
    fn zoom_stops_at_the_limits() {
        assert_eq!(zoomed(&['='; 80]).state.font_size(), MAX_FONT_SIZE);
        assert_eq!(zoomed(&['-'; 80]).state.font_size(), MIN_FONT_SIZE);
        let mut runtime = zoomed(&['-'; 80]);
        press(&mut runtime, Key::Command('='), command());
        assert_eq!(runtime.state.font_size(), MIN_FONT_SIZE + 1.0);
    }

    #[test]
    fn zoom_leaves_the_configured_size_alone() {
        let base = State::default().config.font.size;
        assert_eq!(zoomed(&['=']).state.config.font.size, base);
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

    fn reject_paste(runtime: &mut Runtime<State>) {
        let session = runtime.state.session().map(Session::id).unwrap();
        runtime.state.confirms.push(Confirm::Paste {
            session,
            text: "rm -rf x\n".to_owned(),
        });
    }

    #[test]
    fn enter_answers_the_newest_confirmation_first() {
        let mut runtime = command_tab_runtime();
        reject_paste(&mut runtime);
        press(&mut runtime, Key::Command('w'), command());
        press(&mut runtime, Key::Enter, Mods::default());
        assert_eq!(tab_titles(&runtime), ["shell", "lazygit"]);
        assert!(matches!(
            runtime.state.confirms[..],
            [Confirm::Paste { .. }]
        ));
    }

    #[test]
    fn escape_dismisses_a_newer_paste_before_the_older_close() {
        let mut runtime = command_tab_runtime();
        press(&mut runtime, Key::Command('w'), command());
        reject_paste(&mut runtime);
        press(&mut runtime, Key::Escape, Mods::default());
        assert!(matches!(
            runtime.state.confirms[..],
            [Confirm::Close { .. }]
        ));
        press(&mut runtime, Key::Escape, Mods::default());
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
        let mut frame = 1.0;
        wait_until(
            std::time::Duration::from_secs(15),
            || "timed out".to_owned(),
            || {
                if done(&runtime.state.workspace.projects[0].tray[0]) {
                    return Some(());
                }
                frame += 1.0;
                runtime.frame(frame / 60.0);
                None
            },
        );
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

    fn run_tray_action_from_palette(runtime: &mut Runtime<State>, query: &str) {
        runtime.on_key = Some(on_key);
        press(runtime, Key::Command('p'), command());
        press(runtime, Key::Text(query.into()), Mods::default());
        runtime.state.palette.selected = 2;
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

    fn pending_clipboard(confirm: impl FnOnce(u64) -> Confirm) -> Runtime<State> {
        let mut state = workspace_state();
        let session = state.session().map(Session::id).unwrap();
        state.confirms.push(confirm(session));
        let mut runtime = runtime_with(state);
        runtime.on_key = Some(on_key);
        runtime.poll = Some(poll);
        runtime.frame(1.0 / 60.0);
        runtime
    }

    fn pending_read(session: u64) -> Confirm {
        Confirm::ClipboardRead {
            session,
            location: ClipboardLocation::Standard,
        }
    }

    #[test]
    fn a_clipboard_read_asks_before_answering() {
        let runtime = pending_clipboard(pending_read);
        assert!(runtime.find("clipboard-read-confirm").is_some());
        assert!(runtime.find("clipboard-read-confirm-yes").is_some());
    }

    #[test]
    fn escape_refuses_a_clipboard_read() {
        let mut runtime = pending_clipboard(pending_read);
        press(&mut runtime, Key::Escape, Mods::default());
        assert!(runtime.state.confirms.is_empty());
        assert!(runtime.find("clipboard-read-confirm").is_none());
    }

    #[test]
    fn enter_allows_a_clipboard_read() {
        let mut runtime = pending_clipboard(pending_read);
        press(&mut runtime, Key::Enter, Mods::default());
        assert!(runtime.state.confirms.is_empty());
    }

    #[test]
    fn a_clipboard_write_asks_before_replacing_the_clipboard() {
        let mut runtime = pending_clipboard(|session| Confirm::ClipboardWrite {
            session,
            text: "hi".to_owned(),
        });
        assert!(runtime.find("clipboard-write-confirm").is_some());
        press(&mut runtime, Key::Enter, Mods::default());
        assert!(runtime.state.confirms.is_empty());
    }

    #[test]
    fn switching_tabs_dismisses_a_pending_clipboard_read() {
        let mut runtime = pending_clipboard(pending_read);
        click(&mut runtime, "tab-1");
        runtime.frame(2.0 / 60.0);
        assert!(runtime.state.confirms.is_empty());
    }

    #[test]
    fn switching_tabs_dismisses_a_pending_paste() {
        let mut state = workspace_state();
        let session = state.session().map(Session::id).unwrap();
        state.confirms.push(Confirm::Paste {
            session,
            text: "rm -rf x\n".to_owned(),
        });
        let mut runtime = runtime_with(state);
        runtime.poll = Some(poll);
        runtime.frame(1.0 / 60.0);
        assert!(runtime.find("paste-confirm").is_some());
        click(&mut runtime, "tab-1");
        runtime.frame(2.0 / 60.0);
        assert!(runtime.state.confirms.is_empty());
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
        runtime.state.palette.selected = 0;
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

    #[test]
    fn a_torn_off_window_closes_when_its_last_project_goes() {
        let fleet = Fleet::default();
        let mut second = second_window(&fleet, 1);
        second.frame(2.0 / 60.0);
        assert_eq!(second.shell_request, Some(rune::Shell::Close));
    }

    #[test]
    fn a_torn_off_window_with_a_project_stays_open() {
        let fleet = Fleet::default();
        let mut second = second_window(&fleet, 1);
        second
            .state
            .workspace
            .add_project(Project::new("/tmp".into(), vec![]));
        second.frame(2.0 / 60.0);
        assert_eq!(second.shell_request, None);
    }

    #[test]
    fn the_main_window_stays_open_with_no_projects() {
        let mut main = second_window(&Fleet::default(), MAIN_WINDOW);
        main.frame(2.0 / 60.0);
        assert_eq!(main.shell_request, None);
    }

    fn drag_project(runtime: &mut Runtime<State>, key: &str, to: (f32, f32)) {
        let (x, y) = runtime.find(key).unwrap();
        runtime.pointer_down(x, y);
        runtime.pointer_move((x + to.0) / 2.0, (y + to.1) / 2.0);
        runtime.pointer_move(to.0, to.1);
        runtime.pointer_up(to.0, to.1);
        runtime.frame(2.0 / 60.0);
    }

    #[test]
    fn dragging_a_project_out_of_the_sidebar_tears_it_off() {
        let mut runtime = workspace_runtime();
        drag_project(&mut runtime, "project-1", (SIDEBAR_WIDTH + 200.0, 300.0));
        assert_eq!(runtime.state.workspace.projects.len(), 1);
        assert_eq!(runtime.state.fleet.take(1).len(), 1);
    }

    #[test]
    fn dragging_a_project_within_the_sidebar_keeps_it() {
        let mut runtime = workspace_runtime();
        drag_project(&mut runtime, "project-1", (SIDEBAR_WIDTH - 20.0, 300.0));
        assert_eq!(runtime.state.workspace.projects.len(), 2);
        assert!(runtime.state.fleet.take(1).is_empty());
    }
}
