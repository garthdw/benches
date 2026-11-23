use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use rune::color::Color;
use rune::tok;
use rune::{
    El, Key, KeyEvent, Mods, PointerPress, Span, TextEdit, Tx, Waker, col, el, row, text,
    text_input,
};

use crate::app::{State, open_project, open_tab, open_tray, run_command, tray_action};
use crate::jump;
use crate::keymap::{COMMANDS, Command, CommandSpec, chord_hint, field_shortcut};
use crate::workspace::{Status, Workspace};
use benches_core::LogErr;
use benches_core::config::Config;
use benches_core::history;
use benches_core::process::Cancel;

const PALETTE_WIDTH: f32 = 520.0;
const PALETTE_TOP: f32 = 96.0;
/// Waits for typing to pause, as the lookup starts a login shell.
const JUMP_DEBOUNCE: Duration = Duration::from_millis(250);
const JUMP_TIMEOUT: Duration = Duration::from_secs(5);
/// Longer than a lookup, as `wt switch --create` runs the project's `pre-start` hooks first.
const WORKTREE_TIMEOUT: Duration = Duration::from_secs(120);
const HISTORY_ROWS: usize = 8;
const PALETTE_PADDING: f32 = 8.0;
const INPUT_PADDING_X: f32 = 10.0;
const INPUT_PADDING_Y: f32 = 6.0;
const INPUT_TEXT_WIDTH: f32 = PALETTE_WIDTH - 2.0 * (PALETTE_PADDING + INPUT_PADDING_X);

#[derive(Default)]
pub(crate) struct Palette {
    pub(crate) edit: TextEdit,
    pub(crate) open: bool,
    pub(crate) selected: usize,
    /// The query `selected` was chosen for.
    selected_query: String,
    pub(crate) jump: JumpLookup,
    pub(crate) worktrees: Option<WorktreeChoice>,
    /// A `wt switch --create` running off the UI thread, answering with the new worktree's path.
    pub(crate) creation: Option<Creation>,
    /// Why the last worktree creation failed, shown in the command centre until the next edit.
    pub(crate) error: Option<String>,
    /// The login shell's history, newest first, suggested for `!` commands.
    pub(crate) history: Vec<String>,
    history_stamp: history::Stamp,
    history_pending: Option<mpsc::Receiver<Option<history::Loaded>>>,
}

/// What the command centre's query asks for.
enum Query<'a> {
    /// The command after a leading `!`, which every row runs. A trailing space stays, as it
    /// narrows the suggestions.
    Shell(&'a str),
    Search(&'a str),
}

impl<'a> Query<'a> {
    fn of(text: &'a str) -> Self {
        match text.trim_start().strip_prefix('!') {
            Some(command) => Query::Shell(command.trim_start()),
            None => Query::Search(text.trim()),
        }
    }
}

pub(crate) struct Rows {
    pub(crate) actions: Vec<Action>,
    /// For a `!` command, the input's text completed with the newest history entry it starts,
    /// as fish suggests it.
    ghost: Option<String>,
}

/// The jump command's folder for the command centre's query.
#[derive(Default)]
pub(crate) struct JumpLookup {
    pub(crate) query: String,
    /// When to start the lookup, once typing pauses.
    due: Option<f64>,
    pending: Option<mpsc::Receiver<Option<PathBuf>>>,
    cancel: Cancel,
    pub(crate) found: Option<PathBuf>,
}

pub(crate) struct Creation {
    result: mpsc::Receiver<anyhow::Result<PathBuf>>,
    cancel: Cancel,
}

/// The repository worktrees the command centre offers after ⌘↵ on the jump command's folder, or
/// for the active project.
#[derive(Default)]
pub(crate) struct WorktreeChoice {
    pub(crate) root: PathBuf,
    /// Opens `root` when it has no worktrees, as a jump folder needn't be a repository.
    pub(crate) fallback_to_root: bool,
    pub(crate) pending: Option<mpsc::Receiver<Vec<jump::Worktree>>>,
    pub(crate) found: Vec<jump::Worktree>,
}

impl State {
    /// The worktrees whose branch and path contain the query's letters in order.
    fn visible_worktrees(&self) -> Vec<&jump::Worktree> {
        let query = self.palette.edit.text.trim();
        self.palette
            .worktrees
            .iter()
            .flat_map(|choice| &choice.found)
            .filter(|worktree| {
                let branch = worktree.branch.as_deref().unwrap_or_default();
                letters_in_order(&format!("{branch} {}", worktree.path.display()), query)
            })
            .collect()
    }

    fn jump_target(&self) -> Option<&Path> {
        self.palette
            .jump
            .found
            .as_deref()
            .filter(|_| self.palette.jump.query == self.palette.edit.text.trim())
    }

    /// `tray` makes run rows run in the tray, as ⌘↵ would.
    pub(crate) fn palette_rows(&self, tray: bool) -> Rows {
        let Rows { actions, ghost } = match Query::of(&self.palette.edit.text) {
            Query::Shell(typed) => self.shell_rows(typed),
            Query::Search(query) => Rows {
                actions: palette_actions(
                    query,
                    self.jump_target(),
                    &self.workspace,
                    &self.config,
                    self.is_torn_off(),
                ),
                ghost: None,
            },
        };
        let actions = actions
            .into_iter()
            .map(|action| match action {
                Action::Run(command) if tray => Action::Tray(command),
                action => action,
            })
            .collect();
        Rows { actions, ghost }
    }

    /// The typed `!` command, then the history entries containing it, as run rows.
    fn shell_rows(&self, typed: &str) -> Rows {
        let command = typed.trim_end();
        let history = &self.palette.history;
        let prefixed = history.iter().filter(|entry| entry.starts_with(typed));
        let contained = history
            .iter()
            .filter(|entry| !entry.starts_with(typed) && entry.contains(typed));
        let mut suggested = prefixed
            .chain(contained)
            .map(String::as_str)
            .filter(|entry| *entry != command)
            .peekable();
        let edit = &self.palette.edit;
        // The suggestion follows neither a composition nor the input scrolling sideways.
        let ghost = suggested
            .peek()
            .filter(|_| !typed.is_empty() && !edit.is_composing())
            .and_then(|entry| history::single_line(entry))
            .and_then(|line| Some(format!("{}{}", edit.text, line.strip_prefix(typed)?)))
            .filter(|line| {
                line.len() > edit.text.len()
                    && rune::text::advance(line, tok::type_::BODY) <= INPUT_TEXT_WIDTH
            });
        let actions = (!command.is_empty())
            .then_some(command)
            .into_iter()
            .chain(suggested.take(HISTORY_ROWS))
            .map(|command| Action::Run(command.to_owned()))
            .collect();
        Rows { actions, ghost }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Action {
    Command(Command),
    Jump(PathBuf),
    SwitchProject(usize, String),
    SwitchTab(usize, usize, String),
    RestartTray(usize, String),
    StopTray(usize, String),
    CreateWorktree(String),
    /// Lists the worktrees of the named active project, filtered.
    ListWorktrees(String, String),
    Run(String),
    Tray(String),
}

impl Action {
    /// Whether ⌘↵ does something on this row, where `palette_rows` was asked for tray rows.
    fn takes_command_enter(&self) -> bool {
        matches!(self, Action::Tray(_) | Action::Jump(_))
    }

    fn label(&self, working_directory: &Path) -> String {
        match self {
            Action::Command(command) => command.label().to_owned(),
            Action::Jump(root) => format!(
                "Open project {}",
                jump::relative_to(root, working_directory)
            ),
            Action::SwitchProject(_, name) => format!("Switch to project {name}"),
            Action::SwitchTab(_, _, name) => format!("Switch to tab {name}"),
            Action::RestartTray(_, title) => format!("Restart {title}"),
            Action::StopTray(_, title) => format!("Stop {title}"),
            Action::CreateWorktree(branch) => format!("Create worktree “{branch}”"),
            Action::ListWorktrees(project, filter) if filter.is_empty() => {
                format!("Open a worktree of {project}")
            }
            Action::ListWorktrees(project, filter) => {
                format!("Open a worktree of {project} matching “{filter}”")
            }
            Action::Run(command) => format!("Run “{}” in a new tab", history::flattened(command)),
            Action::Tray(command) => {
                format!("Run “{}” in the tray", history::flattened(command))
            }
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

/// How well a command label matches a query. Only the query's letters in order are required.
struct Match {
    score: u32,
    /// A contiguous substring starting a word, so safe to rank above `Run`.
    strong: bool,
}

fn fuzzy_match(label: &str, query: &str) -> Option<Match> {
    let label: Vec<char> = label.to_lowercase().chars().collect();
    let query: Vec<char> = query.to_lowercase().chars().collect();
    let starts_word = |index: usize| index == 0 || !label[index - 1].is_alphanumeric();
    let found: Vec<usize> = (0..label.len())
        .filter(|&start| label[start..].starts_with(&query))
        .collect();
    if let Some(&start) = found
        .iter()
        .find(|&&start| starts_word(start))
        .or(found.first())
    {
        let prefix = if start == 0 { 200 } else { 0 };
        let word = if starts_word(start) { 100 } else { 0 };
        return Some(Match {
            score: 1000 + prefix + word - start as u32,
            strong: starts_word(start),
        });
    }
    let word_starts: Vec<usize> = (0..label.len())
        .filter(|&index| starts_word(index))
        .collect();
    let mut starts = word_starts.iter();
    if query
        .iter()
        .all(|wanted| starts.any(|&index| label[index] == *wanted))
    {
        return Some(Match {
            score: 500,
            strong: false,
        });
    }
    let mut score = 0;
    let mut next = 0;
    let mut previous = None;
    for wanted in &query {
        let index = (next..label.len()).find(|&index| label[index] == *wanted)?;
        score += 1;
        if previous.is_some_and(|previous| previous + 1 == index) {
            score += 3;
        }
        if starts_word(index) {
            score += 2;
        }
        previous = Some(index);
        next = index + 1;
    }
    Some(Match {
        score,
        strong: false,
    })
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

/// The jump command's folder first, unless a command label or project or tab name equals the
/// query, which goes just above it. Then the commands strongly matching the query, best first,
/// then running the query, so Enter on a typed command always runs it. Below those come the commands
/// matching only loosely, then the open projects and tabs matching the query, exact names first, then the active project's
/// tray restarts and stops. A query of `wt` and one branch name puts creating that worktree
/// first, and `wts` with an optional filter puts listing the active project's worktrees first.
/// Everything but the fixed actions appears only once something is typed.
pub(crate) fn palette_actions(
    query: &str,
    jump: Option<&Path>,
    workspace: &Workspace,
    config: &Config,
    torn_off: bool,
) -> Vec<Action> {
    let query = query.trim();
    let needle = query.to_lowercase();
    let mut actions: Vec<Action> = create_worktree_action(query, workspace)
        .into_iter()
        .chain(list_worktrees_action(query, workspace))
        .collect();
    if query.is_empty() {
        actions.extend(
            COMMANDS
                .iter()
                .filter(|spec| spec.default && (spec.enabled)(workspace, config, torn_off))
                .map(|spec| Action::Command(spec.command)),
        );
        actions.extend(jump.map(|root| Action::Jump(root.to_owned())));
        return actions;
    }
    let mut matched: Vec<(&CommandSpec, Match)> = COMMANDS
        .iter()
        .filter(|spec| (spec.enabled)(workspace, config, torn_off))
        .filter_map(|spec| Some((spec, fuzzy_match(spec.label, query)?)))
        .collect();
    matched.sort_by_key(|(_, found)| Reverse(found.score));
    let (strong, weak): (Vec<_>, Vec<_>) = matched.into_iter().partition(|(_, found)| found.strong);
    let command = |(spec, _): (&CommandSpec, Match)| Action::Command(spec.command);
    let (exact_switches, partial_switches): (Vec<_>, Vec<_>) =
        switch_actions(&needle, jump, workspace)
            .into_iter()
            .partition(|(name, _)| name.to_lowercase() == needle);
    // With a jump result, exact matches go just above it and everything else below.
    let (exact_commands, strong) = if jump.is_some() {
        strong
            .into_iter()
            .partition(|(spec, _)| spec.label.to_lowercase() == needle)
    } else {
        (Vec::new(), strong)
    };
    let (leading_switches, exact_switches) = if jump.is_some() {
        (exact_switches, Vec::new())
    } else {
        (Vec::new(), exact_switches)
    };
    actions.extend(exact_commands.into_iter().map(command));
    actions.extend(leading_switches.into_iter().map(|(_, action)| action));
    actions.extend(jump.map(|root| Action::Jump(root.to_owned())));
    actions.extend(strong.into_iter().map(command));
    actions.push(Action::Run(query.to_owned()));
    actions.extend(weak.into_iter().map(command));
    actions.extend(
        exact_switches
            .into_iter()
            .chain(partial_switches)
            .map(|(_, action)| action),
    );
    let tray = workspace.project().map_or(&[][..], |project| &project.tray);
    for (index, tab) in tray.iter().enumerate() {
        let restart = Action::RestartTray(index, tab.label().to_owned());
        let stop = Action::StopTray(index, tab.label().to_owned());
        let stoppable = tab.status == Status::Running;
        actions.extend(
            [Some(restart), stoppable.then_some(stop)]
                .into_iter()
                .flatten()
                .filter(|action| {
                    letters_in_order(&action.label(&config.working_directory), &needle)
                }),
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

fn list_worktrees_action(query: &str, workspace: &Workspace) -> Option<Action> {
    let filter = match query.strip_prefix("wts")? {
        "" => "",
        rest => rest.strip_prefix(' ')?,
    };
    let project = workspace.project()?;
    Some(Action::ListWorktrees(
        project.name.clone(),
        filter.trim().to_owned(),
    ))
}

pub(crate) fn open_palette(state: &mut State, tx: &mut Tx) {
    open_palette_with(state, "", tx);
}

pub(crate) fn open_palette_with(state: &mut State, text: &str, tx: &mut Tx) {
    state.palette.edit = TextEdit::new(text);
    state.palette.open = true;
    state.palette.selected = 0;
    state.palette.worktrees = None;
    state.palette.error = None;
    tx.focus("palette-input", false);
    if state.paths.is_some() {
        let shell = state.config.shell.clone();
        let known = state.palette.history_stamp.clone();
        state.palette.history_pending = Some(spawn_waking(state.waker.clone(), move || {
            history::load(&shell, &known)
                .log_err("shell history")
                .flatten()
        }));
    }
}

/// Runs `work` off the UI thread, then wakes the window to take its answer.
fn spawn_waking<T: Send + 'static>(
    waker: Option<Waker>,
    work: impl FnOnce() -> T + Send + 'static,
) -> mpsc::Receiver<T> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        if sender.send(work()).is_ok()
            && let Some(waker) = waker
        {
            waker();
        }
    });
    receiver
}

pub(crate) fn poll_history(state: &mut State) -> bool {
    let palette = &mut state.palette;
    let Some(pending) = &palette.history_pending else {
        return false;
    };
    let found = match pending.try_recv() {
        Err(TryRecvError::Empty) => return false,
        found => found.ok().flatten(),
    };
    palette.history_pending = None;
    let Some((stamp, found)) = found else {
        return false;
    };
    palette.history_stamp = stamp;
    palette.history = found;
    // The rows change under the selection, so it goes back to the top one rather than pointing
    // at a different command.
    palette.selected = 0;
    true
}

/// Keeps the command centre open until `wt` answers, as its hooks can take a while.
fn create_worktree(state: &mut State, branch: &str) {
    let Some(root) = state.active_root() else {
        return;
    };
    let cancel = Cancel::default();
    let (shell, branch) = (state.config.shell.clone(), branch.to_owned());
    let result = spawn_waking(state.waker.clone(), {
        let cancel = cancel.clone();
        move || jump::create_worktree(&shell, &root, &branch, WORKTREE_TIMEOUT, &cancel)
    });
    state.palette.creation = Some(Creation { result, cancel });
    state.palette.open = true;
    state.palette.error = None;
}

pub(crate) fn poll_creation(state: &mut State) -> bool {
    let Some(created) = state
        .palette
        .creation
        .as_ref()
        .and_then(|creation| creation.result.try_recv().ok())
    else {
        return false;
    };
    state.palette.creation = None;
    match created {
        Ok(path) => {
            state.palette.open = false;
            open_project(state, path);
        }
        Err(error) => {
            state.palette.open = true;
            state.palette.error = Some(format!("{error:#}"));
        }
    }
    true
}

fn run_palette(state: &mut State, index: usize, tray: bool, tx: &mut Tx) {
    let actions = state.palette_rows(tray).actions;
    let Some(action) = actions.get(index.min(actions.len().saturating_sub(1))) else {
        return;
    };
    match action {
        Action::Jump(root) if tray => list_worktrees(state, root.clone(), true, "", tx),
        _ if tray && !action.takes_command_enter() => {}
        _ => {
            state.palette.open = false;
            run_action(state, action, tx);
        }
    }
}

pub(crate) fn run_action(state: &mut State, action: &Action, tx: &mut Tx) {
    match action {
        Action::Command(command) => run_command(state, *command, tx),
        Action::Jump(root) => open_project(state, root.clone()),
        Action::SwitchProject(project, _) => state.workspace.select_project(*project),
        Action::SwitchTab(project, tab, _) => {
            state.workspace.select_project(*project);
            state.workspace.select_tab(*tab);
        }
        Action::RestartTray(index, _) => tray_action(state, *index, Some("restart")),
        Action::StopTray(index, _) => tray_action(state, *index, Some("stop")),
        Action::CreateWorktree(branch) => create_worktree(state, branch),
        Action::ListWorktrees(_, filter) => list_project_worktrees(state, filter, tx),
        Action::Run(command) => open_tab(state, &history::flattened(command), Some(command)),
        Action::Tray(command) => open_tray(state, command),
    }
}

/// Opens the command centre on the active project's worktrees, filtered by `filter`.
pub(crate) fn list_project_worktrees(state: &mut State, filter: &str, tx: &mut Tx) {
    if let Some(root) = state.active_root() {
        state.palette.open = true;
        list_worktrees(state, root, false, filter, tx);
    }
}

fn list_worktrees(
    state: &mut State,
    root: PathBuf,
    fallback_to_root: bool,
    filter: &str,
    tx: &mut Tx,
) {
    let pending = spawn_waking(state.waker.clone(), {
        let (shell, root) = (state.config.shell.clone(), root.clone());
        move || {
            jump::worktrees(&shell, &root, JUMP_TIMEOUT)
                .log_err("wt list")
                .unwrap_or_default()
        }
    });
    state.palette.worktrees = Some(WorktreeChoice {
        root,
        fallback_to_root,
        pending: Some(pending),
        found: Vec::new(),
    });
    state.palette.edit = TextEdit::new(filter);
    state.palette.selected = 0;
    state.palette.error = None;
    tx.focus("palette-input", false);
}

pub(crate) fn poll_worktrees(state: &mut State) -> bool {
    if !state.palette.open {
        state.palette.worktrees = None;
        return false;
    }
    let Some(choice) = &mut state.palette.worktrees else {
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
        if choice.fallback_to_root {
            state.palette.open = false;
            open_project(state, root);
        } else {
            state.palette.error = Some(format!(
                "No worktrees found in {}",
                jump::abbreviate_home(&root)
            ));
        }
    }
    true
}

/// Whether the key was one of the list's; typing edits the palette's input, which filters it.
fn worktree_key(state: &mut State, event: &KeyEvent) -> bool {
    let last = state.visible_worktrees().len().saturating_sub(1);
    match event.key {
        Key::Escape => state.palette.open = false,
        Key::Up => state.palette.selected = state.palette.selected.min(last).saturating_sub(1),
        Key::Down => state.palette.selected = (state.palette.selected + 1).min(last),
        Key::Enter => open_worktree(state, state.palette.selected.min(last)),
        _ => return false,
    }
    true
}

fn open_worktree(state: &mut State, index: usize) {
    let Some(worktree) = state.visible_worktrees().get(index).copied() else {
        return;
    };
    let path = worktree.path.clone();
    state.palette.open = false;
    open_project(state, path);
}

fn palette_key(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    // A key's own modifiers win should `on_modifiers` miss a change, which would leave every
    // run row in the tray.
    state.command_held = event.mods.cmd;
    if let Some(creation) = &state.palette.creation {
        if event.key == Key::Escape {
            creation.cancel.cancel().log_err("cancel wt switch");
            state.palette.creation = None;
            state.palette.open = false;
        }
        return true;
    }
    if field_shortcut(state, event, tx) {
        return true;
    }
    if state.palette.worktrees.is_some() {
        return worktree_key(state, event);
    }
    let rows = state.palette_rows(event.mods.cmd);
    let last = rows.actions.len().saturating_sub(1);
    match (&event.key, Query::of(&state.palette.edit.text)) {
        (Key::Escape, _) => state.palette.open = false,
        (Key::Up, _) => state.palette.selected = state.palette.selected.min(last).saturating_sub(1),
        (Key::Down, _) => state.palette.selected = (state.palette.selected + 1).min(last),
        (Key::Tab | Key::Right, Query::Shell(_))
            if event.mods == Mods::default()
                && state.palette.edit.range()
                    == (state.palette.edit.text.len(), state.palette.edit.text.len())
                && let Some(line) = rows.ghost.as_deref() =>
        {
            state
                .palette
                .edit
                .insert(&line[state.palette.edit.text.len()..]);
            state.palette.error = None;
        }
        (Key::Enter, _) => run_palette(state, state.palette.selected, event.mods.cmd, tx),
        _ => {
            // A poll after the edit lands starts the jump lookup's debounce.
            tx.wake_after(Duration::ZERO);
            state.palette.error = None;
            return false;
        }
    }
    true
}

/// An edit reorders the rows, so the selection goes back to the top one. Checked after the edit
/// lands, as the input applies it only after `palette_key` passes the key on.
pub(crate) fn poll_selection(state: &mut State) -> bool {
    let palette = &mut state.palette;
    if palette.edit.text == palette.selected_query {
        return false;
    }
    palette.selected_query.clone_from(&palette.edit.text);
    palette.selected = 0;
    true
}

/// Looks the command centre's query up with the jump command once typing pauses.
pub(crate) fn poll_jump(state: &mut State, now: f64, tx: &mut Tx) -> bool {
    let Some(command) = &state.config.jump_command else {
        return false;
    };
    let query = match Query::of(&state.palette.edit.text) {
        Query::Search(query) if state.palette.open => query,
        _ => "",
    };
    let jump = &mut state.palette.jump;
    let mut changed = false;
    if query != jump.query {
        changed = jump.found.take().is_some();
        jump.query = query.to_owned();
        jump.pending = None;
        jump.cancel.cancel().log_err("cancel jump command");
        jump.due = (!query.is_empty()).then(|| {
            tx.wake_after(JUMP_DEBOUNCE);
            now + JUMP_DEBOUNCE.as_secs_f64()
        });
    }
    if jump.due.is_some_and(|due| now >= due) {
        jump.due = None;
        jump.cancel = Cancel::default();
        let cancel = jump.cancel.clone();
        let (shell, command, query) = (
            state.config.shell.clone(),
            command.clone(),
            jump.query.clone(),
        );
        jump.pending = Some(spawn_waking(state.waker.clone(), move || {
            jump::lookup(&shell, &command, &query, JUMP_TIMEOUT, &cancel)
                .log_err("jump command")
                .flatten()
        }));
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

fn enter_hint(theme: &tok::Theme, hint: Option<&str>) -> Option<El> {
    hint.map(|hint| text(hint).key("palette-enter").color(theme.text_secondary))
}

/// Selects on moving, not on entering, so rows that arrive under a still pointer don't take it.
fn select_on_pointer_move(index: usize) -> impl Fn(&mut State, PointerPress) {
    move |state, _| state.palette.selected = index
}

pub(crate) fn view(
    state: &State,
    theme: &tok::Theme,
    pill: &impl Fn(String, &str, bool, Option<El>) -> El,
) -> Option<El> {
    state.palette.open.then(|| {
        let rows = state.palette_rows(state.command_held);
        let (return_key, command_return) = (chord_hint("enter"), chord_hint("mod+enter"));
        let elements: Vec<El> = if state.palette.creation.is_some() {
            vec![pill("palette-0".to_owned(), "Creating worktree…", false, None).w_full()]
        } else if let Some(choice) = &state.palette.worktrees {
            let visible = state.visible_worktrees();
            let selected = state.palette.selected.min(visible.len().saturating_sub(1));
            let placeholder = choice
                .pending
                .is_some()
                .then(|| pill("palette-0".to_owned(), "Loading worktrees…", false, None).w_full());
            let worktrees = visible.iter().enumerate().map(|(index, worktree)| {
                let branch = worktree.branch.as_deref().unwrap_or("(detached)");
                let label = format!("{branch}  {}", jump::abbreviate_home(&worktree.path));
                pill(format!("palette-{index}"), &label, index == selected, None)
                    .children(enter_hint(
                        theme,
                        (index == selected).then_some(return_key.as_str()),
                    ))
                    .justify_between()
                    .w_full()
                    .on_pointer_move(select_on_pointer_move(index))
                    .on_press(move |state: &mut State| open_worktree(state, index))
            });
            placeholder.into_iter().chain(worktrees).collect()
        } else {
            let selected = state
                .palette
                .selected
                .min(rows.actions.len().saturating_sub(1));
            rows.actions
                .iter()
                .enumerate()
                .map(|(index, action)| {
                    let is_selected = index == selected;
                    let hint = match action {
                        Action::Command(command) => command.hint(),
                        _ => None,
                    };
                    let alternative = match action {
                        Action::Run(_) => Some(format!("{command_return} in the tray")),
                        Action::Jump(_) => Some(format!("{command_return} worktrees")),
                        _ => None,
                    }
                    .filter(|_| is_selected && !state.command_held);
                    let enter = match (is_selected, state.command_held) {
                        (false, _) => None,
                        (true, false) => Some(return_key.as_str()),
                        (true, true) => action
                            .takes_command_enter()
                            .then_some(command_return.as_str()),
                    };
                    let shortcuts = row()
                        .gap(8.0)
                        .children(hint.map(|hint| {
                            text(hint)
                                .key("palette-shortcut")
                                .color(theme.text_secondary)
                        }))
                        .children(enter_hint(theme, enter));
                    let label = action.label(&state.config.working_directory);
                    col()
                        .key(format!("palette-{index}"))
                        .w_full()
                        .radius(6.0)
                        .fill(if is_selected {
                            theme.control_off
                        } else {
                            Color::TRANSPARENT
                        })
                        .child(
                            pill(format!("palette-{index}-label"), &label, false, None)
                                .child(shortcuts)
                                .justify_between()
                                .w_full(),
                        )
                        .children(alternative.map(|alternative| {
                            text(alternative)
                                .key("palette-alternative")
                                .font(tok::type_::LABEL)
                                .color(theme.text_secondary)
                                .px(10.0)
                                .pb(4.0)
                        }))
                        .on_pointer_move(select_on_pointer_move(index))
                        .on_press_tx(move |state: &mut State, tx: &mut Tx| {
                            run_palette(state, index, tx.mods().cmd, tx)
                        })
                })
                .collect()
        };
        col()
            .key("palette-layer")
            .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
            .items_center()
            .pt(PALETTE_TOP)
            .on_press(|state: &mut State| state.palette.open = false)
            .child(
                col()
                    .key("palette")
                    .w(PALETTE_WIDTH)
                    .padding(PALETTE_PADDING)
                    .gap(4.0)
                    .radius(10.0)
                    .fill(theme.card)
                    .border(1.0, theme.card_border)
                    // Keeps clicks between rows from reaching the layer, which closes it.
                    .on_press(|_: &mut State| {})
                    .child(
                        el().w_full()
                            .child(
                                text_input(&state.palette.edit, tok::type_::BODY)
                                    .key("palette-input")
                                    .label("Command")
                                    .single_line()
                                    .w_full()
                                    .px(INPUT_PADDING_X)
                                    .py(INPUT_PADDING_Y)
                                    .radius(6.0)
                                    .fill(theme.control_track)
                                    .color(theme.text)
                                    .on_edit(|state: &mut State| &mut state.palette.edit)
                                    .on_input_key(palette_key),
                            )
                            .children(rows.ghost.map(|line| {
                                // The typed text is drawn clear, so the rest lines up after it.
                                let span = Span::new(state.palette.edit.text.len(), line.len())
                                    .color(theme.text_secondary);
                                text(line)
                                    .key("palette-ghost")
                                    .font(tok::type_::BODY)
                                    .color(Color::TRANSPARENT)
                                    .spans([span])
                                    .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                                    .px(INPUT_PADDING_X)
                                    .py(INPUT_PADDING_Y)
                                    .inert()
                            })),
                    )
                    .children(state.palette.error.as_deref().map(|error| {
                        text(error)
                            .key("palette-error")
                            .color(rune::color::oklch(0.66, 0.2, 25.0))
                            .px(10.0)
                            .py(4.0)
                    }))
                    .children(elements),
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenes::*;
    use crate::test_support::*;
    use rune::{Mods, Runtime};

    /// The main window's actions, which most tests want.
    fn main_window_actions(
        query: &str,
        jump: Option<&Path>,
        workspace: &Workspace,
        config: &Config,
    ) -> Vec<Action> {
        palette_actions(query, jump, workspace, config, false)
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
    fn a_tab_digit_switches_tabs_rather_than_typing() {
        let mut runtime = palette_runtime();
        press_chord(
            &mut runtime,
            &format!("{}2", crate::keymap::TAB_DIGIT.here()),
        );
        assert_eq!(runtime.state.workspace.projects[0].active_tab, 1);
        assert_eq!(runtime.state.palette.edit.text, "");
    }

    #[test]
    fn the_next_prompt_chord_moves_the_selection_rather_than_the_terminal() {
        let mut runtime = palette_runtime();
        press_command(&mut runtime, Command::NextPrompt);
        assert_eq!(runtime.state.palette.selected, 1);
    }

    #[test]
    fn clicking_outside_the_command_centre_closes_it() {
        let mut runtime = palette_runtime();
        runtime.pointer_down(4.0, 600.0);
        runtime.pointer_up(4.0, 600.0);
        runtime.frame(3.0 / 60.0);
        runtime.frame(4.0 / 60.0);
        assert!(!runtime.state.palette.open);
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
        assert!(!runtime.state.palette.open);
    }

    #[test]
    fn the_run_row_hints_at_the_tray_until_command_is_held() {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Text("x".into()), Mods::default());
        runtime.frame(0.0);
        assert!(runtime.find("palette-alternative").is_some());
        runtime.state.command_held = true;
        runtime.invalidate();
        runtime.frame(1.0 / 60.0);
        assert!(runtime.find("palette-alternative").is_none());
    }

    #[test]
    fn only_the_selected_row_shows_enter() {
        let mut runtime = palette_runtime();
        runtime.frame(0.0);
        let row_y = |runtime: &Runtime<State>, key: &str| runtime.find(key).map(|(_, y)| y);
        assert_eq!(
            row_y(&runtime, "palette-enter"),
            row_y(&runtime, "palette-0")
        );
        press(&mut runtime, Key::Down, Mods::default());
        runtime.frame(1.0 / 60.0);
        assert_eq!(
            row_y(&runtime, "palette-enter"),
            row_y(&runtime, "palette-1")
        );
    }

    #[test]
    fn hovering_a_row_selects_it() {
        let mut runtime = palette_runtime();
        runtime.frame(0.0);
        hover(&mut runtime, "palette-2");
        assert_eq!(runtime.state.palette.selected, 2);
    }

    #[test]
    fn command_enter_on_a_command_row_does_nothing() {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Text("sidebar".into()), Mods::default());
        press(&mut runtime, Key::Enter, command());
        assert!(!runtime.state.sidebar_hidden);
        assert!(runtime.state.workspace.projects[0].tray.is_empty());
        assert!(runtime.state.palette.open);
    }

    #[test]
    fn holding_command_hints_enter_only_where_command_enter_applies() {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Text("sidebar".into()), Mods::default());
        runtime.state.command_held = true;
        runtime.invalidate();
        runtime.frame(2.0 / 60.0);
        assert!(runtime.find("palette-enter").is_none());
        runtime.state.palette.edit = TextEdit::new("x");
        runtime.invalidate();
        runtime.frame(3.0 / 60.0);
        assert!(runtime.find("palette-enter").is_some());
    }

    #[test]
    fn rows_arriving_under_a_still_pointer_keep_the_selection() {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Text("x".into()), Mods::default());
        assert!(rows(&runtime).len() < 4);
        let (x, y) = runtime.find("palette-0").unwrap();
        runtime.pointer_move(x, y + 85.0);
        runtime.frame(3.0 / 60.0);
        press(&mut runtime, Key::Backspace, Mods::default());
        settle(&mut runtime);
        assert!(rows(&runtime).len() > 4);
        assert_eq!(runtime.state.palette.selected, 0);
    }

    #[test]
    fn a_key_without_command_clears_a_command_release_that_went_unheard() {
        let mut runtime = history_runtime();
        runtime.state.command_held = true;
        press(&mut runtime, Key::Text("!ls".into()), Mods::default());
        assert!(!runtime.state.command_held);
    }

    #[test]
    fn a_click_without_command_runs_the_row_despite_an_unheard_command_release() {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Text("sidebar".into()), Mods::default());
        runtime.state.command_held = true;
        runtime.invalidate();
        runtime.frame(2.0 / 60.0);
        click(&mut runtime, "palette-0");
        assert!(runtime.state.sidebar_hidden);
    }

    #[test]
    fn holding_command_alone_swaps_the_run_row_for_the_tray() {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Text("x".into()), Mods::default());
        runtime.update(|state, _| crate::app::modifiers_changed(state, command()));
        runtime.frame(3.0 / 60.0);
        assert!(runtime.state.command_held);
        assert!(runtime.find("palette-alternative").is_none());
        runtime.update(|state, _| crate::app::modifiers_changed(state, Mods::default()));
        runtime.frame(4.0 / 60.0);
        assert!(!runtime.state.command_held);
        assert!(runtime.find("palette-alternative").is_some());
    }

    #[test]
    fn leaving_the_window_lets_go_of_command() {
        let mut runtime = palette_runtime();
        runtime.state.command_held = true;
        runtime.update(|state, tx| crate::app::on_window_focus(state, false, tx));
        assert!(!runtime.state.command_held);
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
        assert_eq!(runtime.state.palette.edit.text, "x");
    }

    #[test]
    fn the_command_centre_filters_by_letters_in_order() {
        assert_eq!(
            main_window_actions("tlgt", None, &one_project(), &Config::default()),
            [
                Action::Run("tlgt".to_owned()),
                Action::Command(Command::ToggleGit),
            ]
        );
        assert_eq!(
            main_window_actions("", None, &one_project(), &Config::default()),
            [
                Action::Command(Command::NewShell),
                Action::Command(Command::ToggleGit),
                Action::Command(Command::OpenProject),
                Action::Command(Command::ChangeFolder)
            ]
        );
        assert!(
            !main_window_actions("", None, &Workspace::default(), &Config::default())
                .contains(&Action::Command(Command::ChangeFolder))
        );
    }

    #[test]
    fn a_blank_git_command_hides_toggle_git() {
        let config = Config {
            git_command: Some(" ".to_owned()),
            ..Config::default()
        };
        for query in ["", "git"] {
            assert!(
                !main_window_actions(query, None, &one_project(), &config)
                    .contains(&Action::Command(Command::ToggleGit))
            );
        }
    }

    fn first_action(query: &str) -> Action {
        main_window_actions(query, None, &one_project(), &Config::default()).remove(0)
    }

    #[test]
    fn typed_shell_commands_stay_first_despite_loose_matches() {
        for query in [
            "ls",
            "htop",
            "cat",
            "git status",
            "make",
            "ct",
            "nt",
            "cp",
            "zi",
        ] {
            assert_eq!(
                first_action(query),
                Action::Run(query.to_owned()),
                "{query}"
            );
        }
    }

    #[test]
    fn strong_matches_lead_the_commands() {
        let zoom_in = Action::Command(Command::ZoomIn);
        assert_eq!(first_action("zoom"), zoom_in);
        assert_eq!(
            first_action("sidebar"),
            Action::Command(Command::ToggleSidebar)
        );
    }

    #[test]
    fn loose_matches_follow_the_run_row() {
        let actions = main_window_actions("ls", None, &one_project(), &Config::default());
        assert_eq!(actions[1], Action::Command(Command::ToggleSidebar));
        let actions = main_window_actions("zi", None, &one_project(), &Config::default());
        assert_eq!(actions[1], Action::Command(Command::ZoomIn));
    }

    #[test]
    fn every_command_is_searchable_but_the_palette_itself() {
        let actions = main_window_actions("tab", None, &one_project(), &Config::default());
        assert!(actions.contains(&Action::Command(Command::CloseTab)));
        assert!(actions.contains(&Action::Command(Command::NextTab)));
        assert!(
            !main_window_actions("palette", None, &one_project(), &Config::default())
                .contains(&Action::Command(Command::Palette))
        );
    }

    #[test]
    fn command_rows_show_their_chord() {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Text("zoom".into()), Mods::default());
        runtime.frame(0.0);
        assert!(runtime.find("palette-shortcut").is_some());
    }

    #[test]
    fn the_jump_commands_folder_follows_the_fixed_actions() {
        let root = Path::new("/dev/site");
        assert_eq!(
            main_window_actions("site", Some(root), &one_project(), &Config::default()),
            [
                Action::Jump(root.to_owned()),
                Action::Run("site".to_owned()),
            ]
        );
    }

    #[test]
    fn enter_on_the_jump_commands_folder_opens_it_as_a_project() {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Text("site".into()), Mods::default());
        runtime.state.palette.jump.query = "site".to_owned();
        runtime.state.palette.jump.found = Some(PathBuf::from("/dev/site"));
        press(&mut runtime, Key::Enter, Mods::default());
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.projects.len(), 2);
        assert_eq!(workspace.projects[1].root, Path::new("/dev/site"));
        assert!(!runtime.state.palette.open);
    }

    #[test]
    fn typing_a_project_or_tab_name_offers_switching_to_it() {
        let state = workspace_state();
        let actions = main_window_actions("sit", None, &state.workspace, &Config::default());
        assert_eq!(
            actions[..2],
            [
                Action::Run("sit".to_owned()),
                Action::SwitchProject(1, "site".to_owned()),
            ]
        );
        let actions = main_window_actions("lazy", None, &state.workspace, &Config::default());
        assert!(actions.contains(&Action::SwitchTab(0, 2, "lazygit (benches)".to_owned())));
        assert!(
            !main_window_actions("", None, &state.workspace, &Config::default())
                .iter()
                .any(|action| matches!(action, Action::SwitchProject(..) | Action::SwitchTab(..)))
        );
    }

    #[test]
    fn switching_comes_after_running_the_query_and_exact_names_lead_the_switches() {
        let state = workspace_state();
        let actions = main_window_actions("shell", None, &state.workspace, &Config::default());
        assert_eq!(actions[0], Action::Command(Command::NewShell));
        assert_eq!(actions[1], Action::Run("shell".to_owned()));
        assert_eq!(
            actions[2],
            Action::SwitchTab(1, 1, "shell (site)".to_owned())
        );
    }

    #[test]
    fn the_jump_folder_replaces_the_switch_to_its_open_project() {
        let state = workspace_state();
        let root = Path::new("/dev/site");
        let actions = main_window_actions("site", Some(root), &state.workspace, &Config::default());
        assert_eq!(actions[0], Action::Jump(root.to_owned()));
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, Action::SwitchProject(..)))
        );
    }

    #[test]
    fn the_jump_folder_is_named_relative_to_the_working_directory() {
        let jump = Action::Jump(PathBuf::from("/dev/own/site"));
        assert_eq!(jump.label(Path::new("/dev")), "Open project own/site");
        assert_eq!(jump.label(Path::new("/srv")), "Open project /dev/own/site");
    }

    #[test]
    fn the_jump_folder_leads_the_strong_command_matches() {
        let root = Path::new("/dev/pr");
        let actions = main_window_actions("pr", Some(root), &one_project(), &Config::default());
        assert_eq!(actions[0], Action::Jump(root.to_owned()));
        assert!(actions.contains(&Action::Command(Command::PrevTab)));
    }

    #[test]
    fn an_exact_command_label_leads_the_jump_folder() {
        let root = Path::new("/dev/find");
        let actions = main_window_actions(
            " Find In Scrollback ",
            Some(root),
            &one_project(),
            &Config::default(),
        );
        assert_eq!(actions[0], Action::Command(Command::Find));
        assert_eq!(actions[1], Action::Jump(root.to_owned()));
    }

    #[test]
    fn an_exact_switch_name_leads_the_jump_folder_once() {
        let state = workspace_state();
        let root = Path::new("/dev/else");
        let actions =
            main_window_actions("Shell", Some(root), &state.workspace, &Config::default());
        let tab = Action::SwitchTab(1, 1, "shell (site)".to_owned());
        assert_eq!(actions[..2], [tab.clone(), Action::Jump(root.to_owned())]);
        assert_eq!(actions[2], Action::Command(Command::NewShell));
        assert_eq!(actions.iter().filter(|action| **action == tab).count(), 1);
        let actions = main_window_actions("site", Some(root), &state.workspace, &Config::default());
        let project = Action::SwitchProject(1, "site".to_owned());
        assert_eq!(
            actions[..2],
            [project.clone(), Action::Jump(root.to_owned())]
        );
        assert_eq!(
            actions.iter().filter(|action| **action == project).count(),
            1
        );
    }

    fn enter_on_switch_row(runtime: &mut Runtime<State>, query: &str, row: Action) {
        press(runtime, Key::Text(query.into()), Mods::default());
        runtime.frame(0.0);
        let actions =
            main_window_actions(query, None, &runtime.state.workspace, &Config::default());
        runtime.state.palette.selected = actions.iter().position(|action| *action == row).unwrap();
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
        assert!(!runtime.state.palette.open);
    }

    #[test]
    fn enter_on_a_tab_row_activates_its_project_and_tab() {
        let mut runtime = palette_runtime();
        enter_on_switch_row(
            &mut runtime,
            "lazygit",
            Action::SwitchTab(0, 2, "lazygit (benches)".to_owned()),
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
            fallback_to_root: true,
            pending: None,
            found,
        }
    }

    fn jump_runtime() -> Runtime<State> {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Text("site".into()), Mods::default());
        runtime.state.palette.jump.query = "site".to_owned();
        runtime.state.palette.jump.found = Some(PathBuf::from("/dev/site"));
        runtime
    }

    #[test]
    fn the_jump_row_hints_at_worktrees() {
        let mut runtime = jump_runtime();
        runtime.frame(0.0);
        assert!(runtime.find("palette-alternative").is_some());
    }

    #[test]
    fn command_enter_on_the_jump_row_lists_worktrees_and_enter_opens_one() {
        let mut runtime = jump_runtime();
        press(&mut runtime, Key::Enter, command());
        assert!(runtime.state.palette.open);
        assert!(runtime.state.palette.worktrees.is_some());
        let found = vec![
            worktree("main", "/dev/site"),
            worktree("feature", "/dev/site.feature"),
        ];
        runtime.state.palette.worktrees = Some(loaded(found));
        press(&mut runtime, Key::Down, Mods::default());
        press(&mut runtime, Key::Enter, Mods::default());
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.projects.len(), 3);
        assert_eq!(workspace.projects[2].root, Path::new("/dev/site.feature"));
        assert!(!runtime.state.palette.open);
    }

    #[test]
    fn an_open_worktree_is_selected_rather_than_reopened() {
        let mut runtime = jump_runtime();
        let root = runtime.state.workspace.projects[1].root.clone();
        runtime.state.palette.worktrees = Some(loaded(vec![jump::Worktree {
            branch: None,
            path: root,
        }]));
        press(&mut runtime, Key::Enter, Mods::default());
        assert_eq!(runtime.state.workspace.projects.len(), 2);
        assert_eq!(runtime.state.workspace.active, 1);
        assert!(!runtime.state.palette.open);
    }

    #[test]
    fn entering_worktree_mode_clears_the_query_and_typing_filters_the_list() {
        let mut runtime = jump_runtime();
        press(&mut runtime, Key::Enter, command());
        assert_eq!(runtime.state.palette.edit.text, "");
        runtime.state.palette.worktrees = Some(loaded(vec![
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
        runtime.state.palette.worktrees = Some(loaded(vec![worktree("main", "/dev/site")]));
        press(&mut runtime, Key::Text("zzz".into()), Mods::default());
        press(&mut runtime, Key::Enter, Mods::default());
        assert!(runtime.state.palette.open);
        assert_eq!(runtime.state.workspace.projects.len(), 2);
    }

    #[test]
    fn no_worktrees_falls_back_to_opening_the_jump_target() {
        let mut runtime = jump_runtime();
        let (sender, receiver) = mpsc::channel();
        runtime.state.palette.worktrees = Some(WorktreeChoice {
            root: PathBuf::from("/dev/elsewhere"),
            pending: Some(receiver),
            ..loaded(Vec::new())
        });
        assert!(!poll_worktrees(&mut runtime.state));
        assert!(runtime.state.palette.open);
        sender.send(Vec::new()).unwrap();
        assert!(poll_worktrees(&mut runtime.state));
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.projects[2].root, Path::new("/dev/elsewhere"));
        assert!(!runtime.state.palette.open);
    }

    #[test]
    fn closing_the_palette_while_worktrees_load_does_not_open_a_project() {
        let mut runtime = jump_runtime();
        let (sender, receiver) = mpsc::channel();
        press(&mut runtime, Key::Enter, command());
        if let Some(choice) = &mut runtime.state.palette.worktrees {
            choice.root = PathBuf::from("/dev/elsewhere");
            choice.pending = Some(receiver);
        }
        press(&mut runtime, Key::Escape, Mods::default());
        sender.send(Vec::new()).unwrap();
        poll_worktrees(&mut runtime.state);
        assert_eq!(runtime.state.workspace.projects.len(), 2);
    }

    #[test]
    fn escape_closes_the_worktree_list_and_reopening_resets_it() {
        let mut runtime = jump_runtime();
        press(&mut runtime, Key::Enter, command());
        press(&mut runtime, Key::Escape, Mods::default());
        assert!(!runtime.state.palette.open);
        press_command(&mut runtime, Command::Palette);
        assert!(runtime.state.palette.worktrees.is_none());
    }

    #[test]
    fn command_enter_off_the_jump_row_still_runs_in_the_tray() {
        let mut runtime = jump_runtime();
        press(&mut runtime, Key::Down, Mods::default());
        press(&mut runtime, Key::Enter, command());
        assert!(runtime.state.palette.worktrees.is_none());
        assert_eq!(runtime.state.workspace.projects[0].tray.len(), 1);
    }

    #[test]
    fn a_stale_jump_folder_is_not_offered() {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Text("sit".into()), Mods::default());
        runtime.state.palette.jump.query = "si".to_owned();
        runtime.state.palette.jump.found = Some(PathBuf::from("/dev/site"));
        assert_eq!(runtime.state.jump_target(), None);
    }

    #[test]
    fn arrows_move_the_command_centre_selection_within_bounds() {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Up, Mods::default());
        assert_eq!(runtime.state.palette.selected, 0);
        for _ in 0..5 {
            press(&mut runtime, Key::Down, Mods::default());
        }
        assert_eq!(runtime.state.palette.selected, 4);
    }

    #[test]
    fn typing_selects_the_top_row_again() {
        let mut runtime = palette_runtime();
        press(&mut runtime, Key::Down, Mods::default());
        press(&mut runtime, Key::Down, Mods::default());
        assert_eq!(runtime.state.palette.selected, 2);
        press(&mut runtime, Key::Text("s".into()), Mods::default());
        runtime.frame(0.0);
        assert_eq!(runtime.state.palette.selected, 0);
        press(&mut runtime, Key::Down, Mods::default());
        runtime.frame(0.0);
        assert_eq!(runtime.state.palette.selected, 1);
    }

    #[test]
    fn the_wt_prefix_offers_creating_a_worktree_first() {
        let workspace = one_project();
        let create = Action::CreateWorktree("feature-x".to_owned());
        let actions = main_window_actions("wt feature-x", None, &workspace, &Config::default());
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
                !main_window_actions(query, None, &workspace, &Config::default())
                    .iter()
                    .any(|action| matches!(action, Action::CreateWorktree(_))),
                "{query:?}"
            );
        }
        let none = main_window_actions(
            "wt feature-x",
            None,
            &Workspace::default(),
            &Config::default(),
        );
        assert!(!none.contains(&create));
    }

    #[test]
    fn the_wts_query_offers_listing_the_current_projects_worktrees_first() {
        let workspace = one_project();
        let list = |filter: &str| Action::ListWorktrees("benches".to_owned(), filter.to_owned());
        for (query, filter) in [
            ("wts", ""),
            (" wts ", ""),
            ("wts fea", "fea"),
            ("wts  a b", "a b"),
        ] {
            let actions = main_window_actions(query, None, &workspace, &Config::default());
            assert_eq!(actions[0], list(filter), "{query:?}");
        }
        for query in ["wt", "wtsx", "wtsx a", ""] {
            assert!(
                !main_window_actions(query, None, &workspace, &Config::default())
                    .iter()
                    .any(|action| matches!(action, Action::ListWorktrees(..))),
                "{query:?}"
            );
        }
        let none = main_window_actions("wts", None, &Workspace::default(), &Config::default());
        assert!(!none.contains(&list("")));
        assert_eq!(
            list("fea").label(Path::new("/")),
            "Open a worktree of benches matching “fea”"
        );
    }

    #[test]
    fn enter_on_wts_lists_the_current_projects_worktrees_filtered_and_enter_opens_one() {
        let mut runtime = palette_runtime();
        let root = runtime.state.active_root().unwrap();
        press(&mut runtime, Key::Text("wts feat".into()), Mods::default());
        press(&mut runtime, Key::Enter, Mods::default());
        assert!(runtime.state.palette.open);
        assert_eq!(runtime.state.palette.edit.text, "feat");
        let choice = runtime.state.palette.worktrees.as_ref().unwrap();
        assert_eq!(choice.root, root);
        assert!(!choice.fallback_to_root);
        runtime.state.palette.worktrees = Some(loaded(vec![
            worktree("main", "/dev/site"),
            worktree("feature", "/dev/site.feature"),
        ]));
        let branches: Vec<_> = runtime
            .state
            .visible_worktrees()
            .iter()
            .map(|worktree| worktree.branch.clone().unwrap())
            .collect();
        assert_eq!(branches, ["feature"]);
        press(&mut runtime, Key::Enter, Mods::default());
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.projects.len(), 3);
        assert_eq!(workspace.projects[2].root, Path::new("/dev/site.feature"));
        assert!(!runtime.state.palette.open);
    }

    #[test]
    fn no_worktrees_for_the_current_project_says_so_instead_of_closing() {
        let mut runtime = palette_runtime();
        let (sender, receiver) = mpsc::channel();
        press(&mut runtime, Key::Text("wts".into()), Mods::default());
        press(&mut runtime, Key::Enter, Mods::default());
        if let Some(choice) = &mut runtime.state.palette.worktrees {
            choice.pending = Some(receiver);
        }
        sender.send(Vec::new()).unwrap();
        assert!(poll_worktrees(&mut runtime.state));
        runtime.frame(1.0);
        assert!(runtime.state.palette.open);
        assert_eq!(runtime.state.workspace.projects.len(), 2);
        assert_eq!(runtime.state.workspace.active, 0);
        let error = runtime.state.palette.error.as_deref().unwrap();
        assert!(error.starts_with("No worktrees found in "), "{error}");
        assert!(runtime.find("palette-error").is_some());
    }

    #[test]
    fn worktree_commands_are_searchable_only_with_a_project() {
        for (query, command) in [
            ("open worktree", Command::OpenWorktree),
            ("create worktree", Command::CreateWorktree),
        ] {
            let action = Action::Command(command);
            let actions = main_window_actions(query, None, &one_project(), &Config::default());
            assert!(actions.contains(&action), "{query}");
            let none = main_window_actions(query, None, &Workspace::default(), &Config::default());
            assert!(!none.contains(&action), "{query}");
            assert!(
                !main_window_actions("", None, &one_project(), &Config::default())
                    .contains(&action)
            );
        }
    }

    #[test]
    fn the_open_worktree_command_lists_the_current_projects_worktrees() {
        let mut runtime = workspace_runtime();
        let root = runtime.state.active_root().unwrap();
        let mut tx = Tx::default();
        run_command(&mut runtime.state, Command::OpenWorktree, &mut tx);
        assert!(runtime.state.palette.open);
        let choice = runtime.state.palette.worktrees.as_ref().unwrap();
        assert_eq!(choice.root, root);
        assert!(!choice.fallback_to_root);
    }

    #[test]
    fn the_create_worktree_command_opens_the_command_centre_with_wt_typed() {
        let mut runtime = palette_runtime();
        press(
            &mut runtime,
            Key::Text("create worktree".into()),
            Mods::default(),
        );
        press(&mut runtime, Key::Enter, Mods::default());
        assert!(runtime.state.palette.open);
        assert_eq!(runtime.state.palette.edit.text, "wt ");
        assert_eq!(runtime.state.palette.edit.range(), (3, 3));
        press(&mut runtime, Key::Text("feature-x".into()), Mods::default());
        assert_eq!(
            runtime.state.palette_rows(false).actions[0],
            Action::CreateWorktree("feature-x".to_owned())
        );
    }

    fn create_worktree_from_palette(shell: PathBuf, branch: &str) -> Runtime<State> {
        let mut state = workspace_state();
        state.config.shell = shell;
        state.workspace.projects[0].root = std::env::temp_dir();
        let mut runtime = runtime_with(state);
        press_command(&mut runtime, Command::Palette);
        press(
            &mut runtime,
            Key::Text(format!("wt {branch}")),
            Mods::default(),
        );
        press(&mut runtime, Key::Enter, Mods::default());
        runtime
    }

    fn poll_until_created(runtime: &mut Runtime<State>) {
        frame_until(runtime, "the worktree creation to end", |runtime| {
            runtime.state.palette.creation.is_none()
        });
    }

    #[test]
    fn creating_a_worktree_runs_wt_switch_and_opens_the_result_as_a_project() {
        let created = tempfile::tempdir().unwrap();
        let (_directory, shell, record) = stub_shell(&format!(
            "echo '{{\"path\":\"{}\"}}'",
            created.path().display()
        ));
        let mut runtime = create_worktree_from_palette(shell, "feature-x");
        assert!(runtime.state.palette.open);
        assert!(runtime.find("palette-0").is_some());
        poll_until_created(&mut runtime);
        let workspace = &runtime.state.workspace;
        assert_eq!(workspace.projects.len(), 3);
        assert_eq!(workspace.active, 2);
        assert_eq!(workspace.projects[2].root, created.path());
        assert!(!runtime.state.palette.open);
        let args = std::fs::read_to_string(record).unwrap();
        assert!(
            args.contains("wt switch --create --no-cd --format=json \"$@\""),
            "{args}"
        );
        assert!(args.contains("benches\nfeature-x\n"));
    }

    #[test]
    fn a_refused_worktree_keeps_the_command_centre_open_with_the_reason() {
        let (_directory, shell, _) =
            stub_shell("echo '✗ Branch feature-x already exists' >&2; exit 1");
        let mut runtime = create_worktree_from_palette(shell, "feature-x");
        poll_until_created(&mut runtime);
        runtime.frame(1000.0);
        assert_eq!(runtime.state.workspace.projects.len(), 2);
        assert!(runtime.state.palette.open);
        assert_eq!(
            runtime.state.palette.error.as_deref(),
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
        let pid = wait_for_pid(&pid_file);
        press(&mut runtime, Key::Escape, Mods::default());
        assert!(runtime.state.palette.creation.is_none());
        assert!(!runtime.state.palette.open);
        wait_for_pid_exit(pid, Duration::from_secs(15), "wt");
        runtime.frame(1000.0);
        assert_eq!(runtime.state.workspace.projects.len(), 2);
    }

    #[test]
    fn a_changed_query_kills_the_superseded_jump_lookup() {
        let pids = tempfile::tempdir().unwrap();
        let pid_file = pids.path().join("pid");
        let (_directory, shell, _) =
            stub_shell(&format!("echo $$ > '{}'; sleep 30", pid_file.display()));
        let mut state = workspace_state();
        state.config.shell = shell;
        state.config.jump_command = Some("j".to_owned());
        let mut runtime = runtime_with(state);
        press_command(&mut runtime, Command::Palette);
        press(&mut runtime, Key::Text("site".into()), Mods::default());
        runtime.frame(0.0);
        runtime.frame(1000.0);
        let pid = wait_for_pid(&pid_file);
        press(&mut runtime, Key::Text("x".into()), Mods::default());
        runtime.frame(2000.0);
        wait_for_pid_exit(pid, JUMP_TIMEOUT / 2, "lookup");
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
        let actions = main_window_actions("stop", None, &state.workspace, &Config::default());
        assert!(actions.contains(&Action::StopTray(0, "pnpm dev".to_owned())));
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, Action::StopTray(1, _)))
        );
        let actions = main_window_actions("restart", None, &state.workspace, &Config::default());
        assert_eq!(
            actions[1..3],
            [
                Action::RestartTray(0, "pnpm dev".to_owned()),
                Action::RestartTray(1, "storybook".to_owned()),
            ]
        );
        let actions =
            main_window_actions("restart sto", None, &state.workspace, &Config::default());
        assert_eq!(actions[1], Action::RestartTray(1, "storybook".to_owned()));
        assert!(
            main_window_actions("", None, &state.workspace, &Config::default())
                .iter()
                .all(|action| {
                    !matches!(action, Action::RestartTray(..) | Action::StopTray(..))
                })
        );
    }

    fn history_runtime() -> Runtime<State> {
        let mut runtime = palette_runtime();
        runtime.state.palette.history = ["git status", "ls", "git push", "make git"]
            .map(str::to_owned)
            .into();
        runtime
    }

    fn shell(command: &str) -> Action {
        Action::Run(command.to_owned())
    }

    fn rows(runtime: &Runtime<State>) -> Vec<Action> {
        runtime.state.palette_rows(false).actions
    }

    #[test]
    fn a_bang_query_offers_only_running_it_and_matching_history() {
        let mut runtime = history_runtime();
        press(&mut runtime, Key::Text("! git".into()), Mods::default());
        assert_eq!(
            rows(&runtime),
            [
                shell("git"),
                shell("git status"),
                shell("git push"),
                shell("make git")
            ]
        );
        press(&mut runtime, Key::Text(" status".into()), Mods::default());
        assert_eq!(rows(&runtime), [shell("git status")]);
    }

    #[test]
    fn a_lone_bang_lists_recent_history_and_nothing_else() {
        let mut runtime = history_runtime();
        press(&mut runtime, Key::Text("!".into()), Mods::default());
        assert_eq!(
            rows(&runtime),
            [
                shell("git status"),
                shell("ls"),
                shell("git push"),
                shell("make git")
            ]
        );
        runtime.state.palette.history.clear();
        assert_eq!(rows(&runtime), []);
        press(&mut runtime, Key::Enter, command());
        press(&mut runtime, Key::Enter, Mods::default());
        let project = &runtime.state.workspace.projects[0];
        assert!(project.tray.is_empty());
        assert_eq!(project.tabs.len(), 3);
    }

    #[test]
    fn tab_or_right_accepts_the_suggested_history_entry() {
        let mut runtime = history_runtime();
        press(&mut runtime, Key::Text("!git p".into()), Mods::default());
        assert!(runtime.find("palette-ghost").is_some());
        runtime.state.palette.error = Some("failed".to_owned());
        press(&mut runtime, Key::Tab, Mods::default());
        assert_eq!(runtime.state.palette.edit.text, "!git push");
        assert_eq!(runtime.state.palette.error, None);
        assert!(runtime.find("palette-ghost").is_none());
        let mut runtime = history_runtime();
        press(&mut runtime, Key::Text("!g".into()), Mods::default());
        press(&mut runtime, Key::Right, Mods::default());
        assert_eq!(runtime.state.palette.edit.text, "!git status");
        assert!(runtime.state.palette.open);
    }

    #[test]
    fn modified_tab_and_right_leave_the_suggestion() {
        for mods in [
            Mods {
                shift: true,
                ..Mods::default()
            },
            Mods {
                alt: true,
                ..Mods::default()
            },
            command(),
        ] {
            for key in [Key::Tab, Key::Right] {
                let mut runtime = history_runtime();
                press(&mut runtime, Key::Text("!g".into()), Mods::default());
                press(&mut runtime, key.clone(), mods);
                assert_eq!(runtime.state.palette.edit.text, "!g", "{key:?} {mods:?}");
            }
        }
    }

    #[test]
    fn a_suggestion_too_long_for_the_input_is_not_offered() {
        let mut runtime = history_runtime();
        let long = format!("git {}", "x".repeat(200));
        runtime.state.palette.history.insert(0, long.clone());
        press(&mut runtime, Key::Text("!git".into()), Mods::default());
        assert!(runtime.find("palette-ghost").is_none());
        press(&mut runtime, Key::Tab, Mods::default());
        assert_eq!(runtime.state.palette.edit.text, "!git");
        assert_eq!(rows(&runtime)[1], shell(&long));
    }

    #[test]
    fn right_away_from_the_end_only_moves_the_caret() {
        let mut runtime = history_runtime();
        press(&mut runtime, Key::Text("!g".into()), Mods::default());
        press(&mut runtime, Key::Left, Mods::default());
        press(&mut runtime, Key::Right, Mods::default());
        assert_eq!(runtime.state.palette.edit.text, "!g");
    }

    #[test]
    fn enter_runs_the_bang_command_in_a_new_tab() {
        let mut runtime = history_runtime();
        press(&mut runtime, Key::Text("!make".into()), Mods::default());
        press(&mut runtime, Key::Enter, Mods::default());
        let project = &runtime.state.workspace.projects[0];
        assert_eq!(project.tabs.len(), 4);
        assert_eq!(project.tabs[3].title, "make");
        assert!(project.tray.is_empty());
        assert!(!runtime.state.palette.open);
    }

    #[test]
    fn command_enter_runs_the_selected_history_row_in_the_tray() {
        let mut runtime = history_runtime();
        press(&mut runtime, Key::Text("!git".into()), Mods::default());
        press(&mut runtime, Key::Down, Mods::default());
        press(&mut runtime, Key::Down, Mods::default());
        press(&mut runtime, Key::Enter, command());
        let project = &runtime.state.workspace.projects[0];
        let tray: Vec<_> = project.tray.iter().map(|tab| tab.title.as_str()).collect();
        assert_eq!(tray, ["git push"]);
        assert_eq!(project.tabs.len(), 3);
    }

    #[test]
    fn run_rows_offer_the_tray_while_command_is_held() {
        let mut runtime = history_runtime();
        press(&mut runtime, Key::Text("!ls".into()), Mods::default());
        assert_eq!(
            runtime.state.palette_rows(true).actions,
            [Action::Tray("ls".to_owned())]
        );
        runtime.state.palette.edit = TextEdit::new("ls");
        let runs = |tray| -> Vec<Action> {
            runtime
                .state
                .palette_rows(tray)
                .actions
                .into_iter()
                .filter(|action| matches!(action, Action::Run(_) | Action::Tray(_)))
                .collect()
        };
        assert_eq!(runs(true), [Action::Tray("ls".to_owned())]);
        assert_eq!(runs(false), [Action::Run("ls".to_owned())]);
    }

    #[test]
    fn loaded_history_replaces_the_previous_one_and_selects_the_top_row() {
        let mut runtime = history_runtime();
        press(&mut runtime, Key::Text("!".into()), Mods::default());
        press(&mut runtime, Key::Down, Mods::default());
        let (sender, receiver) = mpsc::channel();
        runtime.state.palette.history_pending = Some(receiver);
        assert!(!poll_history(&mut runtime.state));
        sender
            .send(Some((history::Stamp::default(), vec!["htop".to_owned()])))
            .unwrap();
        assert!(poll_history(&mut runtime.state));
        assert_eq!(runtime.state.palette.history, ["htop"]);
        assert_eq!(runtime.state.palette.selected, 0);
        assert!(runtime.state.palette.history_pending.is_none());
    }

    #[test]
    fn an_unchanged_or_failed_load_keeps_the_history() {
        let mut runtime = history_runtime();
        for answer in [Some(None), None] {
            let (sender, receiver) = mpsc::channel();
            runtime.state.palette.history_pending = Some(receiver);
            if let Some(answer) = answer {
                sender.send(answer).unwrap();
            }
            drop(sender);
            assert!(!poll_history(&mut runtime.state));
            assert!(runtime.state.palette.history_pending.is_none());
            assert_eq!(runtime.state.palette.history.len(), 4);
        }
    }

    fn multi_line_runtime() -> Runtime<State> {
        let mut runtime = history_runtime();
        runtime.state.palette.history = ["for x in a b; do\necho $x\ndone", "echo 'a\nb'"]
            .map(str::to_owned)
            .into();
        runtime
    }

    #[test]
    fn multi_line_history_rows_show_marked_newlines_and_run_the_original() {
        let mut runtime = multi_line_runtime();
        press(&mut runtime, Key::Text("!for".into()), Mods::default());
        let original = "for x in a b; do\necho $x\ndone";
        assert_eq!(rows(&runtime)[1], shell(original));
        assert_eq!(
            rows(&runtime)[1].label(Path::new("/")),
            "Run “for x in a b; do ⏎ echo $x ⏎ done” in a new tab"
        );
        press(&mut runtime, Key::Down, Mods::default());
        press(&mut runtime, Key::Enter, Mods::default());
        let tab = &runtime.state.workspace.projects[0].tabs[3];
        assert_eq!(tab.command.as_deref(), Some(original));
        assert_eq!(tab.title, "for x in a b; do ⏎ echo $x ⏎ done");
    }

    #[test]
    fn accepting_a_multi_line_suggestion_joins_it_into_one_line() {
        let mut runtime = multi_line_runtime();
        press(&mut runtime, Key::Text("!for".into()), Mods::default());
        press(&mut runtime, Key::Tab, Mods::default());
        assert_eq!(
            runtime.state.palette.edit.text,
            "!for x in a b; do echo $x; done"
        );
    }

    #[test]
    fn a_newline_inside_quotes_offers_the_row_but_no_suggestion() {
        let mut runtime = multi_line_runtime();
        press(&mut runtime, Key::Text("!echo".into()), Mods::default());
        assert_eq!(rows(&runtime)[1], shell("echo 'a\nb'"));
        assert!(runtime.find("palette-ghost").is_none());
        press(&mut runtime, Key::Tab, Mods::default());
        assert_eq!(runtime.state.palette.edit.text, "!echo");
    }

    #[test]
    fn bang_rows_hint_at_the_tray() {
        let mut runtime = history_runtime();
        press(&mut runtime, Key::Text("!ls".into()), Mods::default());
        runtime.frame(0.0);
        assert!(runtime.find("palette-alternative").is_some());
    }

    #[test]
    fn a_bang_query_skips_the_jump_lookup() {
        let mut state = workspace_state();
        state.config.jump_command = Some("j".to_owned());
        let mut runtime = runtime_with(state);
        press_command(&mut runtime, Command::Palette);
        press(&mut runtime, Key::Text("!site".into()), Mods::default());
        runtime.frame(0.5);
        assert!(runtime.state.palette.jump.due.is_none());
    }

    #[test]
    fn moving_a_project_is_offered_only_beside_another_project() {
        let moves = |workspace: &Workspace| {
            main_window_actions("", None, workspace, &Config::default())
                .contains(&Action::Command(Command::MoveToNewWindow))
        };
        assert!(!moves(&one_project()));
        assert!(moves(&workspace_state().workspace));
        assert_eq!(
            Action::Command(Command::MoveToNewWindow).label(Path::new("/")),
            "Move project to new window"
        );
    }

    #[test]
    fn rejoining_the_main_window_is_offered_only_in_a_torn_off_window() {
        let workspace = one_project();
        let rejoins = |query, torn_off| {
            palette_actions(query, None, &workspace, &Config::default(), torn_off)
                .into_iter()
                .filter(|action| {
                    matches!(
                        action,
                        Action::Command(Command::MoveToMainWindow | Command::MergeIntoMain)
                    )
                })
                .count()
        };
        for query in ["", "main"] {
            assert_eq!(rejoins(query, false), 0);
            assert_eq!(rejoins(query, true), 2);
        }
    }
}
