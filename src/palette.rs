use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use rune::tok;
use rune::{El, Key, KeyEvent, TextEdit, Tx, col, text, text_input};

use crate::LogErr;
use crate::app::{State, open_project, open_tab, open_tray, run_command, tray_action};
use crate::jump;
use crate::keymap::{COMMANDS, Command, CommandSpec, chord_hint};
use crate::workspace::{Status, Workspace};

const PALETTE_WIDTH: f32 = 520.0;
const PALETTE_TOP: f32 = 96.0;
/// Waits for typing to pause, as the lookup starts a login shell.
const JUMP_DEBOUNCE: Duration = Duration::from_millis(250);
const JUMP_TIMEOUT: Duration = Duration::from_secs(5);
/// Longer than a lookup, as `wt switch --create` runs the project's `pre-start` hooks first.
const WORKTREE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Default)]
pub(crate) struct Palette {
    pub(crate) edit: TextEdit,
    pub(crate) open: bool,
    pub(crate) selected: usize,
    pub(crate) jump: JumpLookup,
    pub(crate) worktrees: Option<WorktreeChoice>,
    /// A `wt switch --create` running off the UI thread, answering with the new worktree's path.
    pub(crate) creation: Option<Creation>,
    /// Why the last worktree creation failed, shown in the command centre until the next edit.
    pub(crate) error: Option<String>,
}

/// The jump command's folder for the command centre's query.
#[derive(Default)]
pub(crate) struct JumpLookup {
    query: String,
    /// When to start the lookup, once typing pauses.
    due: Option<f64>,
    pending: Option<mpsc::Receiver<Option<PathBuf>>>,
    cancel: jump::Cancel,
    found: Option<PathBuf>,
}

pub(crate) struct Creation {
    result: mpsc::Receiver<anyhow::Result<PathBuf>>,
    cancel: jump::Cancel,
}

/// The repository worktrees the command centre offers after ⌘↵ on the jump command's folder.
#[derive(Default)]
pub(crate) struct WorktreeChoice {
    pub(crate) root: PathBuf,
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
    Run(String),
    Tray(String),
}

impl Action {
    fn label(&self) -> String {
        match self {
            Action::Command(command) => command.label().to_owned(),
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

/// The commands strongly matching the query, best first, then the jump command's folder, then
/// running the query, so Enter on a typed command always runs it. Below those come the commands
/// matching only loosely, then the open projects and tabs matching the query, exact names first, then the active project's
/// tray restarts and stops. A query of `wt` and one branch name puts creating that worktree
/// first. Everything but the fixed actions appears only once something is typed.
pub(crate) fn palette_actions(
    query: &str,
    jump: Option<&Path>,
    workspace: &Workspace,
) -> Vec<Action> {
    let query = query.trim();
    let needle = query.to_lowercase();
    let mut actions: Vec<Action> = create_worktree_action(query, workspace)
        .into_iter()
        .collect();
    if query.is_empty() {
        actions.extend(
            COMMANDS
                .iter()
                .filter(|spec| spec.default && (spec.enabled)(workspace))
                .map(|spec| Action::Command(spec.command)),
        );
        actions.extend(jump.map(|root| Action::Jump(root.to_owned())));
        return actions;
    }
    let mut matched: Vec<(&CommandSpec, Match)> = COMMANDS
        .iter()
        .filter(|spec| (spec.enabled)(workspace))
        .filter_map(|spec| Some((spec, fuzzy_match(spec.label, query)?)))
        .collect();
    matched.sort_by_key(|(_, found)| Reverse(found.score));
    let (strong, weak): (Vec<_>, Vec<_>) = matched.into_iter().partition(|(_, found)| found.strong);
    let command = |(spec, _): (&CommandSpec, Match)| Action::Command(spec.command);
    actions.extend(strong.into_iter().map(command));
    actions.extend(jump.map(|root| Action::Jump(root.to_owned())));
    actions.push(Action::Run(query.to_owned()));
    actions.push(Action::Tray(query.to_owned()));
    actions.extend(weak.into_iter().map(command));
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

pub(crate) fn open_palette(state: &mut State, tx: &mut Tx) {
    state.palette.edit = TextEdit::new("");
    state.palette.open = true;
    state.palette.selected = 0;
    state.palette.worktrees = None;
    state.palette.error = None;
    tx.focus("palette-input", false);
}

/// Keeps the command centre open until `wt` answers, as its hooks can take a while.
fn create_worktree(state: &mut State, branch: &str) {
    let Some(root) = state.active_root() else {
        return;
    };
    let (sender, receiver) = mpsc::channel();
    let cancel = jump::Cancel::default();
    state.palette.creation = Some(Creation {
        result: receiver,
        cancel: cancel.clone(),
    });
    state.palette.open = true;
    state.palette.error = None;
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

fn run_palette(state: &mut State, index: usize, tx: &mut Tx) {
    let actions = palette_actions(
        &state.palette.edit.text,
        state.jump_target(),
        &state.workspace,
    );
    let Some(action) = actions.get(index.min(actions.len().saturating_sub(1))) else {
        return;
    };
    state.palette.open = false;
    run_action(state, action, tx);
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
        Action::Run(command) => open_tab(state, command, Some(command)),
        Action::Tray(command) => open_tray(state, command),
    }
}

fn list_worktrees(state: &mut State, root: PathBuf, tx: &mut Tx) {
    let (sender, receiver) = mpsc::channel();
    state.palette.worktrees = Some(WorktreeChoice {
        root: root.clone(),
        pending: Some(receiver),
        found: Vec::new(),
    });
    state.palette.edit = TextEdit::new("");
    state.palette.selected = 0;
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
        state.palette.open = false;
        open_project(state, root);
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
    if let Some(creation) = &state.palette.creation {
        if event.key == Key::Escape {
            creation.cancel.cancel().log_err("cancel wt switch");
            state.palette.creation = None;
            state.palette.open = false;
        }
        return true;
    }
    if state.palette.worktrees.is_some() {
        return worktree_key(state, event);
    }
    let actions = palette_actions(
        &state.palette.edit.text,
        state.jump_target(),
        &state.workspace,
    );
    let last = actions.len().saturating_sub(1);
    match event.key {
        Key::Escape => state.palette.open = false,
        Key::Up => state.palette.selected = state.palette.selected.min(last).saturating_sub(1),
        Key::Down => state.palette.selected = (state.palette.selected + 1).min(last),
        Key::Enter
            if event.mods.cmd
                && let Some(Action::Jump(root)) = actions.get(state.palette.selected.min(last)) =>
        {
            list_worktrees(state, root.clone(), tx);
        }
        Key::Enter if event.mods.cmd => {
            let query = state.palette.edit.text.trim().to_owned();
            if !query.is_empty() {
                state.palette.open = false;
                open_tray(state, &query);
            }
        }
        Key::Enter => run_palette(state, state.palette.selected, tx),
        _ => {
            // A poll after the edit lands starts the jump lookup's debounce.
            tx.wake_after(Duration::ZERO);
            state.palette.error = None;
            return false;
        }
    }
    true
}

/// Looks the command centre's query up with the jump command once typing pauses.
pub(crate) fn poll_jump(state: &mut State, now: f64, tx: &mut Tx) -> bool {
    let Some(command) = &state.config.jump_command else {
        return false;
    };
    let query = if state.palette.open {
        state.palette.edit.text.trim()
    } else {
        ""
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
        let (sender, receiver) = mpsc::channel();
        jump.pending = Some(receiver);
        jump.cancel = jump::Cancel::default();
        let cancel = jump.cancel.clone();
        let (shell, command, query) = (
            state.config.shell.clone(),
            command.clone(),
            jump.query.clone(),
        );
        let waker = state.waker.clone();
        std::thread::spawn(move || {
            let found = jump::lookup(&shell, &command, &query, JUMP_TIMEOUT, &cancel)
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

pub(crate) fn view(
    state: &State,
    theme: &tok::Theme,
    pill: &impl Fn(String, &str, bool, Option<El>) -> El,
) -> Option<El> {
    state.palette.open.then(|| {
        let rows: Vec<El> = if state.palette.creation.is_some() {
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
                    .w_full()
                    .on_press(move |state: &mut State| open_worktree(state, index))
            });
            placeholder.into_iter().chain(worktrees).collect()
        } else {
            let actions = palette_actions(
                &state.palette.edit.text,
                state.jump_target(),
                &state.workspace,
            );
            let selected = state.palette.selected.min(actions.len().saturating_sub(1));
            actions
                .iter()
                .enumerate()
                .map(|(index, action)| {
                    let shortcut = match action {
                        Action::Tray(_) => Some("⌘↵".to_owned()),
                        Action::Jump(_) => Some("⌘↵ worktrees".to_owned()),
                        Action::Command(command) => COMMANDS
                            .iter()
                            .find(|spec| spec.command == *command)
                            .and_then(|spec| spec.chord)
                            .map(chord_hint),
                        _ => None,
                    }
                    .map(|hint| {
                        text(&hint)
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
            .on_press(|state: &mut State| state.palette.open = false)
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
                        text_input(&state.palette.edit, tok::type_::BODY)
                            .key("palette-input")
                            .label("Command")
                            .single_line()
                            .w_full()
                            .px(10.0)
                            .py(6.0)
                            .radius(6.0)
                            .fill(theme.control_track)
                            .color(theme.text)
                            .on_edit(|state: &mut State| &mut state.palette.edit)
                            .on_input_key(palette_key),
                    )
                    .children(state.palette.error.as_deref().map(|error| {
                        text(error)
                            .key("palette-error")
                            .color(rune::color::oklch(0.66, 0.2, 25.0))
                            .px(10.0)
                            .py(4.0)
                    }))
                    .children(rows),
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenes::*;
    use crate::test_support::*;
    use rune::{Mods, Runtime};

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
        assert_eq!(runtime.state.palette.edit.text, "x");
    }

    #[test]
    fn the_command_centre_filters_by_letters_in_order() {
        assert_eq!(
            palette_actions("lzg", None, &one_project()),
            [
                Action::Run("lzg".to_owned()),
                Action::Tray("lzg".to_owned()),
                Action::Command(Command::ToggleLazygit),
            ]
        );
        assert_eq!(
            palette_actions("", None, &one_project()),
            [
                Action::Command(Command::NewShell),
                Action::Command(Command::ToggleLazygit),
                Action::Command(Command::OpenProject),
                Action::Command(Command::ChangeFolder)
            ]
        );
        assert!(
            !palette_actions("", None, &Workspace::default())
                .contains(&Action::Command(Command::ChangeFolder))
        );
    }

    fn first_action(query: &str) -> Action {
        palette_actions(query, None, &one_project()).remove(0)
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
    fn loose_matches_follow_the_run_and_tray_rows() {
        let actions = palette_actions("ls", None, &one_project());
        assert_eq!(actions[2], Action::Command(Command::ToggleSidebar));
        let actions = palette_actions("zi", None, &one_project());
        assert_eq!(actions[2], Action::Command(Command::ZoomIn));
    }

    #[test]
    fn every_command_is_searchable_but_the_palette_itself() {
        let actions = palette_actions("tab", None, &one_project());
        assert!(actions.contains(&Action::Command(Command::CloseTab)));
        assert!(actions.contains(&Action::Command(Command::NextTab)));
        assert!(
            !palette_actions("palette", None, &one_project())
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
        assert_eq!(actions[0], Action::Command(Command::NewShell));
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
        runtime.state.palette.jump.query = "site".to_owned();
        runtime.state.palette.jump.found = Some(PathBuf::from("/dev/site"));
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
        press(&mut runtime, Key::Command('p'), command());
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

    fn create_worktree_from_palette(shell: PathBuf, branch: &str) -> Runtime<State> {
        let mut state = workspace_state();
        state.config.shell = shell;
        state.workspace.projects[0].root = std::env::temp_dir();
        let mut runtime = runtime_with(state);
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
        let mut frame = 1.0;
        wait_until(
            Duration::from_secs(15),
            || "timed out".to_owned(),
            || {
                if runtime.state.palette.creation.is_none() {
                    return Some(());
                }
                frame += 1.0;
                runtime.frame(frame / 60.0);
                None
            },
        );
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
        assert!(runtime.state.palette.creation.is_none());
        assert!(!runtime.state.palette.open);
        while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok() {
            assert!(std::time::Instant::now() < deadline, "wt was not killed");
            std::thread::sleep(Duration::from_millis(20));
        }
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
        press(&mut runtime, Key::Command('p'), command());
        press(&mut runtime, Key::Text("site".into()), Mods::default());
        runtime.frame(0.0);
        runtime.frame(1000.0);
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
        press(&mut runtime, Key::Text("x".into()), Mods::default());
        runtime.frame(2000.0);
        let deadline = std::time::Instant::now() + JUMP_TIMEOUT / 2;
        while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "lookup was not killed"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
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
        assert!(actions.contains(&Action::StopTray(0, "pnpm dev".to_owned())));
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

    #[test]
    fn moving_a_project_is_offered_only_beside_another_project() {
        let moves = |workspace: &Workspace| {
            palette_actions("", None, workspace)
                .contains(&Action::Command(Command::MoveToNewWindow))
        };
        assert!(!moves(&one_project()));
        assert!(moves(&workspace_state().workspace));
        assert_eq!(
            Action::Command(Command::MoveToNewWindow).label(),
            "Move project to new window"
        );
    }
}
