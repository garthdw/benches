use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::ExitStatus;

use serde::{Deserialize, Serialize};

use crate::session::{Attention, Grid, Session, Visibility};

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Running,
    /// Ended by a stop the user asked for.
    Stopped,
    /// Ended on its own with success.
    Exited,
    /// Ended on its own with a failure, or never started; says why.
    Failed(String),
}

pub struct Tab {
    /// What the tab runs, shown until the program sets a title of its own.
    pub title: String,
    pub program_title: Option<String>,
    /// What a restart runs; `None` is the launch shell.
    pub command: Option<String>,
    /// Where the process started, so a restart runs there again.
    pub cwd: Option<PathBuf>,
    pub session: Session,
    pub status: Status,
    pub attention: Attention,
    /// Respawn once the stopped process has exited, so the new one can take its port.
    pub restart: bool,
    stopping: bool,
}

impl Tab {
    pub fn new(title: &str, session: Session) -> Self {
        Self {
            title: title.to_owned(),
            program_title: None,
            command: None,
            cwd: None,
            session,
            status: Status::Running,
            attention: Attention::None,
            restart: false,
            stopping: false,
        }
    }

    pub fn label(&self) -> &str {
        self.program_title.as_deref().unwrap_or(&self.title)
    }

    pub fn exited(&mut self, status: ExitStatus) {
        self.status = if self.stopping {
            Status::Stopped
        } else if status.success() {
            Status::Exited
        } else if let Some(code) = status.code() {
            Status::Failed(format!("exit {code}"))
        } else {
            Status::Failed(format!("signal {}", status.signal().unwrap_or_default()))
        };
    }

    pub fn stop(&mut self) {
        if self.status == Status::Running {
            self.stopping = true;
            self.session.stop();
        }
    }
}

pub struct Project {
    pub name: String,
    pub root: PathBuf,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    /// Background processes without a tab.
    pub tray: Vec<Tab>,
    /// The tray process the drawer shows, kept while it is closed.
    pub drawer: usize,
    pub drawer_open: bool,
    pub drawer_focused: bool,
    /// False for a restored project until it is first shown, when its tabs start.
    pub launched: bool,
}

impl Project {
    /// A project at `root` with `tabs` in order and the first one active.
    pub fn new(root: PathBuf, tabs: Vec<Tab>) -> Self {
        let name = root.file_name().map_or_else(
            || root.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        Self {
            name,
            root,
            tabs,
            active_tab: 0,
            tray: Vec::new(),
            drawer: 0,
            drawer_open: false,
            drawer_focused: false,
            launched: true,
        }
    }

    pub fn drawer_tab(&self) -> Option<&Tab> {
        self.tray.get(self.drawer).filter(|_| self.drawer_open)
    }

    pub fn drawer_shows(&self, index: usize) -> bool {
        self.drawer_open && self.drawer == index
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active_tab)
    }

    pub fn tab(&self, slot: Slot) -> Option<&Tab> {
        match slot {
            Slot::Tab(index) => self.tabs.get(index),
            Slot::Tray(index) => self.tray.get(index),
        }
    }

    pub fn tab_mut(&mut self, slot: Slot) -> Option<&mut Tab> {
        match slot {
            Slot::Tab(index) => self.tabs.get_mut(index),
            Slot::Tray(index) => self.tray.get_mut(index),
        }
    }

    /// The strongest signal among its tabs; a failed tray process alerts too.
    pub fn attention(&self) -> Attention {
        let failed = self
            .tray
            .iter()
            .any(|tab| matches!(tab.status, Status::Failed(_)));
        let tabs = self.tabs.iter().map(|tab| tab.attention).max();
        tabs.unwrap_or_default().max(if failed {
            Attention::Alert
        } else {
            Attention::None
        })
    }
}

/// Every selection change goes through here, so only the shown session publishes and has focus.
#[derive(Default)]
pub struct Workspace {
    pub projects: Vec<Project>,
    pub active: usize,
    pub window_focused: bool,
}

impl Workspace {
    pub fn project(&self) -> Option<&Project> {
        self.projects.get(self.active)
    }

    fn project_mut(&mut self) -> Option<&mut Project> {
        self.projects.get_mut(self.active)
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.project()?.active_tab()
    }

    pub fn active_session(&self) -> Option<&Session> {
        self.active_tab().map(|tab| &tab.session)
    }

    pub fn drawer_session(&self) -> Option<&Session> {
        self.project()?.drawer_tab().map(|tab| &tab.session)
    }

    pub fn focused_session(&self) -> Option<&Session> {
        self.focused_tab().map(|(_, tab)| &tab.session)
    }

    /// Where keys go: the open drawer once clicked into, otherwise the active tab.
    pub fn focused_tab(&self) -> Option<(Slot, &Tab)> {
        let project = self.project()?;
        if project.drawer_focused
            && let Some(tab) = project.drawer_tab()
        {
            return Some((Slot::Tray(project.drawer), tab));
        }
        Some((Slot::Tab(project.active_tab), project.active_tab()?))
    }

    pub fn sessions(&self) -> impl Iterator<Item = &Session> {
        self.tab_sessions().chain(self.tray_sessions())
    }

    pub fn tab_sessions(&self) -> impl Iterator<Item = &Session> {
        self.projects
            .iter()
            .flat_map(|project| &project.tabs)
            .map(|tab| &tab.session)
    }

    pub fn tray_sessions(&self) -> impl Iterator<Item = &Session> {
        self.projects
            .iter()
            .flat_map(|project| &project.tray)
            .map(|tab| &tab.session)
    }

    pub fn tabs_mut(&mut self) -> impl Iterator<Item = &mut Tab> {
        self.projects
            .iter_mut()
            .flat_map(|project| project.tabs.iter_mut().chain(&mut project.tray))
    }

    pub fn project_index(&self, root: &std::path::Path) -> Option<usize> {
        self.projects
            .iter()
            .position(|project| project.root == root)
    }

    pub fn add_project(&mut self, project: Project) {
        self.projects.push(project);
        self.active = self.projects.len() - 1;
        self.show_active();
    }

    pub fn add_tab(&mut self, tab: Tab) {
        if let Some(project) = self.project_mut() {
            project.tabs.push(tab);
            project.active_tab = project.tabs.len() - 1;
        }
        self.show_active();
    }

    pub fn select_project(&mut self, index: usize) {
        if index < self.projects.len() && index != self.active {
            self.active = index;
            self.show_active();
        }
    }

    pub fn select_tab(&mut self, index: usize) {
        let Some(project) = self.project_mut() else {
            return;
        };
        if index < project.tabs.len() && index != project.active_tab {
            project.active_tab = index;
            self.show_active();
        }
    }

    pub fn cycle_tab(&mut self, step: isize) {
        let Some(project) = self.project() else {
            return;
        };
        let count = project.tabs.len() as isize;
        if count > 0 {
            let index = (project.active_tab as isize + step).rem_euclid(count);
            self.select_tab(index as usize);
        }
    }

    /// Closes the active tab, and the project with its last tab. Dropping the session ends its process.
    pub fn close_tab(&mut self) {
        let Some(project) = self.project_mut() else {
            return;
        };
        if project.active_tab >= project.tabs.len() {
            return;
        }
        project.tabs.remove(project.active_tab);
        if project.tabs.is_empty() {
            self.projects.remove(self.active);
            self.active = self.active.min(self.projects.len().saturating_sub(1));
        } else {
            project.active_tab = project.active_tab.min(project.tabs.len() - 1);
        }
        self.show_active();
    }

    pub fn add_tray(&mut self, tab: Tab) {
        if let Some(project) = self.project_mut() {
            project.tray.push(tab);
        }
        self.show_active();
    }

    /// Opens the drawer on tray process `index` with keys going to it, or closes it if it already shows that process.
    pub fn toggle_drawer(&mut self, index: usize) {
        let Some(project) = self.project_mut() else {
            return;
        };
        if index >= project.tray.len() {
            return;
        }
        let open = !project.drawer_shows(index);
        project.drawer = index;
        project.drawer_open = open;
        project.drawer_focused = open;
        self.show_active();
    }

    pub fn focus_drawer(&mut self, focused: bool) {
        let Some(project) = self.project_mut() else {
            return;
        };
        let focused = focused && project.drawer_tab().is_some();
        if project.drawer_focused != focused {
            project.drawer_focused = focused;
            self.show_active();
        }
    }

    pub fn tray_mut(&mut self, index: usize) -> Option<&mut Tab> {
        self.project_mut()?.tray.get_mut(index)
    }

    /// Replaces `project`'s tab or tray process at `slot` with `tab`; dropping the old session ends its process.
    pub fn replace(&mut self, project: usize, slot: Slot, tab: Tab) {
        if let Some(old) = self
            .projects
            .get_mut(project)
            .and_then(|project| project.tab_mut(slot))
        {
            *old = tab;
        }
        self.show_active();
    }

    /// The project and tab or tray slot running `session`.
    pub fn locate(&self, session: u64) -> Option<(usize, Slot)> {
        let runs = |tab: &Tab| tab.session.id() == session;
        self.projects
            .iter()
            .enumerate()
            .find_map(|(index, project)| {
                let slot = project.tabs.iter().position(runs).map(Slot::Tab);
                let slot = slot.or_else(|| project.tray.iter().position(runs).map(Slot::Tray))?;
                Some((index, slot))
            })
    }

    /// Shows the tab or tray process running `session` with keys going to it.
    pub fn reveal(&mut self, session: u64) {
        let Some((project, slot)) = self.locate(session) else {
            return;
        };
        self.active = project;
        if let Some(project) = self.projects.get_mut(project) {
            match slot {
                Slot::Tab(index) => {
                    project.active_tab = index;
                    project.drawer_focused = false;
                }
                Slot::Tray(index) => {
                    project.drawer = index;
                    project.drawer_open = true;
                    project.drawer_focused = true;
                }
            }
        }
        self.show_active();
    }

    /// Dropping the session ends its process.
    pub fn remove_tray(&mut self, index: usize) {
        let Some(project) = self.project_mut() else {
            return;
        };
        if index >= project.tray.len() {
            return;
        }
        project.tray.remove(index);
        if index == project.drawer {
            project.drawer_open = false;
            project.drawer_focused = false;
        } else if index < project.drawer {
            project.drawer -= 1;
        }
        self.show_active();
    }

    /// Ends every process, waiting up to `timeout` in all for them to go.
    pub fn shutdown(&mut self, timeout: std::time::Duration) {
        let finished: Vec<_> = self.tabs_mut().map(|tab| tab.session.finished()).collect();
        self.projects.clear();
        let deadline = std::time::Instant::now() + timeout;
        for receiver in finished {
            // Only disconnection comes, once the process is gone.
            if receiver
                .recv_deadline(deadline)
                .is_err_and(|error| error.is_timeout())
            {
                eprintln!("agentd: processes still running at quit");
                return;
            }
        }
    }

    pub fn set_window_focused(&mut self, focused: bool) {
        self.window_focused = focused;
        self.sync_focus();
    }

    /// Only the session keys go to has focus, and only while the window has it.
    fn sync_focus(&self) {
        let focused = self
            .focused_session()
            .filter(|_| self.window_focused)
            .map(Session::id);
        for session in self.sessions() {
            session.focus(Some(session.id()) == focused);
        }
    }

    /// Shows the active tab and open drawer and lists the rest of the project's tabs.
    /// Other projects' tabs are hidden, so their titles wait. Showing a tab clears its attention.
    fn show_active(&mut self) {
        let active = self.active;
        for (index, project) in self.projects.iter_mut().enumerate() {
            let shown = index == active;
            for (tab_index, tab) in project.tabs.iter_mut().enumerate() {
                let visibility = if !shown {
                    Visibility::Hidden
                } else if tab_index == project.active_tab {
                    tab.attention = Attention::None;
                    Visibility::Shown
                } else {
                    Visibility::Listed
                };
                tab.session.set_visibility(visibility);
            }
            for (tray_index, tab) in project.tray.iter().enumerate() {
                let visibility = if shown && project.drawer_open && tray_index == project.drawer {
                    Visibility::Shown
                } else {
                    Visibility::Hidden
                };
                tab.session.set_visibility(visibility);
            }
        }
        self.sync_focus();
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Slot {
    Tab(usize),
    Tray(usize),
}

/// The layout `state.json` keeps between launches.
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedWorkspace {
    pub active: usize,
    pub sidebar_hidden: bool,
    pub projects: Vec<SavedProject>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedProject {
    pub root: PathBuf,
    pub active_tab: usize,
    pub tabs: Vec<SavedTab>,
    pub tray: Vec<SavedTab>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedTab {
    pub title: String,
    pub command: Option<String>,
    pub cwd: Option<PathBuf>,
}

impl SavedTab {
    /// A shell is saved where it has `cd`ed to, so it comes back there.
    fn new(tab: &Tab) -> Self {
        let live_cwd = tab.command.is_none().then(|| tab.session.cwd()).flatten();
        Self {
            title: tab.title.clone(),
            command: tab.command.clone(),
            cwd: live_cwd.or_else(|| tab.cwd.clone()),
        }
    }

    /// A placeholder with no process, holding what starts it.
    fn restore(self, status: Status) -> Tab {
        let mut tab = Tab::new(&self.title, Session::fixture(Grid::default()));
        tab.command = self.command;
        tab.cwd = self.cwd;
        tab.status = status;
        tab
    }
}

impl Workspace {
    pub fn snapshot(&self, sidebar_hidden: bool) -> SavedWorkspace {
        let tabs = |tabs: &[Tab]| tabs.iter().map(SavedTab::new).collect();
        SavedWorkspace {
            active: self.active,
            sidebar_hidden,
            projects: self
                .projects
                .iter()
                .map(|project| SavedProject {
                    root: project.root.clone(),
                    active_tab: project.active_tab,
                    tabs: tabs(&project.tabs),
                    tray: tabs(&project.tray),
                })
                .collect(),
        }
    }

    /// Projects come back unlaunched and their tray processes stopped. Missing folders are dropped.
    pub fn restore(saved: SavedWorkspace) -> Self {
        let mut workspace = Workspace::default();
        for saved_project in saved.projects {
            if saved_project.tabs.is_empty() || !saved_project.root.is_dir() {
                continue;
            }
            let tabs = saved_project.tabs.into_iter();
            let mut project = Project::new(
                saved_project.root,
                tabs.map(|tab| tab.restore(Status::Running)).collect(),
            );
            project.active_tab = saved_project.active_tab.min(project.tabs.len() - 1);
            let tray = saved_project.tray.into_iter();
            project.tray = tray.map(|tab| tab.restore(Status::Stopped)).collect();
            project.launched = false;
            workspace.projects.push(project);
        }
        workspace.active = saved.active.min(workspace.projects.len().saturating_sub(1));
        workspace.show_active();
        workspace
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Grid;

    fn tab(title: &str) -> Tab {
        Tab::new(title, Session::fixture(Grid::default()))
    }

    fn workspace() -> Workspace {
        let mut workspace = Workspace::default();
        workspace.add_project(Project::new("/a".into(), vec![tab("claude"), tab("shell")]));
        workspace.add_project(Project::new("/b".into(), vec![tab("vim")]));
        workspace
    }

    fn active_title(workspace: &Workspace) -> Option<&str> {
        workspace.active_tab().map(|tab| tab.title.as_str())
    }

    #[test]
    fn a_new_project_is_named_after_its_folder_and_shown() {
        let workspace = workspace();
        assert_eq!(workspace.active, 1);
        assert_eq!(workspace.projects[0].name, "a");
        assert_eq!(active_title(&workspace), Some("vim"));
    }

    #[test]
    fn selecting_out_of_range_changes_nothing() {
        let mut workspace = workspace();
        workspace.select_project(5);
        workspace.select_tab(5);
        assert_eq!(workspace.active, 1);
        assert_eq!(active_title(&workspace), Some("vim"));
    }

    #[test]
    fn cycling_tabs_wraps_both_ways() {
        let mut workspace = workspace();
        workspace.select_project(0);
        workspace.cycle_tab(-1);
        assert_eq!(active_title(&workspace), Some("shell"));
        workspace.cycle_tab(1);
        assert_eq!(active_title(&workspace), Some("claude"));
    }

    #[test]
    fn adding_a_tab_shows_it() {
        let mut workspace = workspace();
        workspace.add_tab(tab("lazygit"));
        assert_eq!(workspace.projects[1].tabs.len(), 2);
        assert_eq!(active_title(&workspace), Some("lazygit"));
    }

    #[test]
    fn closing_the_last_tab_closes_the_project() {
        let mut workspace = workspace();
        workspace.close_tab();
        assert_eq!(workspace.projects.len(), 1);
        assert_eq!(workspace.active, 0);
        assert_eq!(active_title(&workspace), Some("claude"));
    }

    #[test]
    fn closing_the_rightmost_tab_selects_its_neighbour() {
        let mut workspace = workspace();
        workspace.select_project(0);
        workspace.select_tab(1);
        workspace.close_tab();
        assert_eq!(active_title(&workspace), Some("claude"));
    }

    #[test]
    fn a_program_title_replaces_the_command_name_until_cleared() {
        let mut tab = tab("shell");
        tab.program_title = Some("vim notes.md".to_owned());
        assert_eq!(tab.label(), "vim notes.md");
        tab.program_title = None;
        assert_eq!(tab.label(), "shell");
    }

    #[test]
    fn the_active_project_lists_its_other_tabs_and_hides_the_rest() {
        let mut workspace = workspace();
        workspace.select_project(0);
        let visibilities: Vec<_> = workspace.sessions().map(Session::visibility).collect();
        assert_eq!(
            visibilities,
            [Visibility::Shown, Visibility::Listed, Visibility::Hidden]
        );
    }

    fn tray_workspace() -> Workspace {
        let mut workspace = workspace();
        workspace.add_tray(tab("pnpm dev"));
        workspace.add_tray(tab("test"));
        workspace
    }

    fn tray_visibilities(workspace: &Workspace) -> Vec<Visibility> {
        workspace.projects[1]
            .tray
            .iter()
            .map(|tab| tab.session.visibility())
            .collect()
    }

    #[test]
    fn tray_processes_stay_hidden_until_their_drawer_opens() {
        let mut workspace = tray_workspace();
        assert_eq!(active_title(&workspace), Some("vim"));
        assert_eq!(tray_visibilities(&workspace), [Visibility::Hidden; 2]);
        workspace.toggle_drawer(1);
        assert_eq!(
            tray_visibilities(&workspace),
            [Visibility::Hidden, Visibility::Shown]
        );
        assert_eq!(
            workspace.focused_session().map(Session::id),
            workspace.projects[1].tray[1].session.id().into()
        );
        workspace.toggle_drawer(1);
        assert_eq!(tray_visibilities(&workspace), [Visibility::Hidden; 2]);
        assert_eq!(
            workspace.focused_session().map(Session::id),
            workspace.active_session().map(Session::id)
        );
    }

    #[test]
    fn focus_moves_between_the_tab_and_the_open_drawer() {
        let mut workspace = tray_workspace();
        workspace.toggle_drawer(0);
        workspace.focus_drawer(false);
        assert_eq!(
            workspace.focused_session().map(Session::id),
            workspace.active_session().map(Session::id)
        );
        workspace.focus_drawer(true);
        assert_eq!(
            workspace.focused_session().map(Session::id),
            workspace.drawer_session().map(Session::id)
        );
    }

    #[test]
    fn only_the_focused_session_has_focus_while_the_window_does() {
        let mut workspace = tray_workspace();
        let focused = |workspace: &Workspace| {
            let sessions = workspace.sessions().filter(|session| session.focused());
            sessions.map(Session::id).collect::<Vec<_>>()
        };
        workspace.set_window_focused(true);
        workspace.toggle_drawer(0);
        assert_eq!(
            focused(&workspace),
            Vec::from_iter(workspace.drawer_session().map(Session::id))
        );
        workspace.focus_drawer(false);
        assert_eq!(
            focused(&workspace),
            Vec::from_iter(workspace.active_session().map(Session::id))
        );
        workspace.set_window_focused(false);
        assert!(focused(&workspace).is_empty());
    }

    #[test]
    fn switching_projects_hides_the_drawer_and_keeps_it_for_return() {
        let mut workspace = tray_workspace();
        workspace.toggle_drawer(0);
        workspace.select_project(0);
        assert!(workspace.drawer_session().is_none());
        assert_eq!(tray_visibilities(&workspace)[0], Visibility::Hidden);
        workspace.select_project(1);
        assert_eq!(tray_visibilities(&workspace)[0], Visibility::Shown);
    }

    #[test]
    fn removing_the_open_tray_process_closes_its_drawer() {
        let mut workspace = tray_workspace();
        workspace.toggle_drawer(1);
        workspace.remove_tray(1);
        assert!(workspace.drawer_session().is_none());
        workspace.toggle_drawer(0);
        workspace.remove_tray(0);
        assert!(workspace.projects[1].tray.is_empty());
        assert!(workspace.drawer_session().is_none());
    }

    #[test]
    fn removing_a_tray_process_before_the_open_one_keeps_it_open() {
        let mut workspace = tray_workspace();
        let open = workspace.projects[1].tray[1].session.id();
        workspace.toggle_drawer(1);
        workspace.remove_tray(0);
        assert_eq!(workspace.drawer_session().map(Session::id), Some(open));
    }

    fn exit_status(code: i32) -> std::process::ExitStatus {
        std::os::unix::process::ExitStatusExt::from_raw(code << 8)
    }

    #[test]
    fn an_exit_status_becomes_exited_failed_or_stopped() {
        let mut process = tab("pnpm dev");
        assert_eq!(process.status, Status::Running);
        process.exited(exit_status(0));
        assert_eq!(process.status, Status::Exited);
        process.exited(exit_status(3));
        assert_eq!(process.status, Status::Failed("exit 3".to_owned()));
        let mut process = tab("pnpm dev");
        process.stop();
        process.exited(std::os::unix::process::ExitStatusExt::from_raw(1));
        assert_eq!(process.status, Status::Stopped);
        let mut killed = tab("killed");
        killed.exited(std::os::unix::process::ExitStatusExt::from_raw(9));
        assert_eq!(killed.status, Status::Failed("signal 9".to_owned()));
    }

    #[test]
    fn restarting_the_open_tray_process_keeps_its_drawer_shown() {
        let mut workspace = tray_workspace();
        workspace.toggle_drawer(0);
        let fresh = tab("pnpm dev");
        let id = fresh.session.id();
        let old = workspace.projects[1].tray[0].session.id();
        assert_eq!(workspace.locate(old), Some((1, Slot::Tray(0))));
        workspace.replace(1, Slot::Tray(0), fresh);
        assert_eq!(workspace.drawer_session().map(Session::id), Some(id));
        assert_eq!(tray_visibilities(&workspace)[0], Visibility::Shown);
        workspace.toggle_drawer(0);
        assert_eq!(tray_visibilities(&workspace)[0], Visibility::Hidden);
    }

    #[test]
    fn revealing_a_session_shows_its_project_and_tab_or_drawer() {
        let mut workspace = tray_workspace();
        workspace.toggle_drawer(0);
        let shell = workspace.projects[0].tabs[1].session.id();
        workspace.reveal(shell);
        assert_eq!(workspace.active, 0);
        assert_eq!(active_title(&workspace), Some("shell"));
        assert_eq!(workspace.focused_session().map(Session::id), Some(shell));
        let test = workspace.projects[1].tray[1].session.id();
        workspace.reveal(test);
        assert_eq!(workspace.active, 1);
        assert_eq!(workspace.focused_session().map(Session::id), Some(test));
        assert_eq!(
            tray_visibilities(&workspace),
            [Visibility::Hidden, Visibility::Shown]
        );
        let vim = workspace.projects[1].tabs[0].session.id();
        workspace.reveal(vim);
        assert_eq!(workspace.focused_session().map(Session::id), Some(vim));
        workspace.reveal(0);
        assert_eq!(workspace.focused_session().map(Session::id), Some(vim));
    }

    #[test]
    fn showing_a_tab_clears_its_attention() {
        let mut workspace = workspace();
        workspace.projects[0].tabs[1].attention = Attention::Alert;
        assert_eq!(workspace.projects[0].attention(), Attention::Alert);
        workspace.select_project(0);
        assert_eq!(workspace.projects[0].attention(), Attention::Alert);
        workspace.select_tab(1);
        assert_eq!(workspace.projects[0].attention(), Attention::None);
    }

    #[test]
    fn a_failed_tray_process_alerts_its_project() {
        let mut workspace = tray_workspace();
        workspace.projects[1].tray[0].exited(exit_status(1));
        assert_eq!(workspace.projects[1].attention(), Attention::Alert);
    }

    #[test]
    fn sessions_cover_every_tab() {
        assert_eq!(workspace().sessions().count(), 3);
    }

    #[test]
    fn a_snapshot_round_trips_through_json() {
        let mut workspace = tray_workspace();
        workspace.projects[1].tray[0].command = Some("pnpm dev".to_owned());
        workspace.projects[0].tabs[0].cwd = Some("/tmp".into());
        let saved = workspace.snapshot(true);
        let json = serde_json::to_string(&saved).unwrap();
        assert_eq!(
            serde_json::from_str::<SavedWorkspace>(&json).unwrap(),
            saved
        );
        assert!(saved.sidebar_hidden);
        assert_eq!(saved.active, 1);
        assert_eq!(saved.projects[0].tabs[0].cwd, Some("/tmp".into()));
        assert_eq!(
            saved.projects[1].tray[0].command.as_deref(),
            Some("pnpm dev")
        );
    }

    fn saved_tab(title: &str) -> SavedTab {
        SavedTab {
            title: title.to_owned(),
            command: Some(title.to_owned()),
            cwd: None,
        }
    }

    fn saved_project(root: &str, active_tab: usize) -> SavedProject {
        SavedProject {
            root: root.into(),
            active_tab,
            tabs: vec![saved_tab("claude"), saved_tab("shell")],
            tray: vec![saved_tab("pnpm dev")],
        }
    }

    #[test]
    fn a_restored_workspace_waits_to_launch_with_its_tray_stopped() {
        let saved = SavedWorkspace {
            active: 1,
            sidebar_hidden: false,
            projects: vec![saved_project("/", 1), saved_project("/tmp", 0)],
        };
        let workspace = Workspace::restore(saved);
        assert_eq!(workspace.active, 1);
        assert_eq!(workspace.projects[0].active_tab, 1);
        assert!(workspace.projects.iter().all(|project| !project.launched));
        let tray = &workspace.projects[1].tray[0];
        assert_eq!(tray.status, Status::Stopped);
        assert_eq!(tray.command.as_deref(), Some("pnpm dev"));
    }

    #[test]
    fn restoring_skips_missing_folders_and_empty_projects_and_clamps_indices() {
        let mut empty = saved_project("/tmp", 0);
        empty.tabs.clear();
        let saved = SavedWorkspace {
            active: 7,
            sidebar_hidden: false,
            projects: vec![
                saved_project("/no/such/folder", 0),
                empty,
                saved_project("/", 9),
            ],
        };
        let workspace = Workspace::restore(saved);
        assert_eq!(workspace.projects.len(), 1);
        assert_eq!(workspace.active, 0);
        assert_eq!(workspace.projects[0].active_tab, 1);
    }

    #[test]
    fn launching_replaces_a_tab_and_keeps_it_shown() {
        let mut workspace = workspace();
        let fresh = tab("vim");
        let id = fresh.session.id();
        workspace.replace(1, Slot::Tab(0), fresh);
        assert_eq!(workspace.active_session().map(Session::id), Some(id));
        assert_eq!(
            workspace.projects[1].tabs[0].session.visibility(),
            Visibility::Shown
        );
    }

    #[test]
    fn projects_are_found_by_root() {
        let workspace = workspace();
        assert_eq!(workspace.project_index(std::path::Path::new("/b")), Some(1));
    }
}
