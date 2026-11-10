use std::path::PathBuf;

use crate::session::Session;

pub struct Tab {
    pub title: String,
    pub session: Session,
}

pub struct Project {
    pub name: String,
    pub root: PathBuf,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    /// Background processes without a tab; shown from M4.
    pub tray: Vec<Tab>,
}

impl Project {
    pub fn new(root: PathBuf) -> Self {
        let name = root.file_name().map_or_else(
            || root.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        Self {
            name,
            root,
            tabs: Vec::new(),
            active_tab: 0,
            tray: Vec::new(),
        }
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

    pub fn active_session(&self) -> Option<&Session> {
        let project = self.project()?;
        Some(&project.tabs.get(project.active_tab)?.session)
    }

    pub fn sessions(&self) -> impl Iterator<Item = &Session> {
        self.projects
            .iter()
            .flat_map(|project| project.tabs.iter().chain(&project.tray))
            .map(|tab| &tab.session)
    }

    pub fn project_index(&self, root: &std::path::Path) -> Option<usize> {
        self.projects
            .iter()
            .position(|project| project.root == root)
    }

    /// Adds `project` and shows it, with `tabs` in order and the first one active.
    pub fn add_project(&mut self, mut project: Project, tabs: Vec<Tab>) {
        for tab in &tabs {
            tab.session.set_visible(false);
        }
        project.tabs = tabs;
        project.active_tab = 0;
        self.hide_active();
        self.projects.push(project);
        self.active = self.projects.len() - 1;
        self.show_active();
    }

    /// Adds `tab` to the active project and shows it.
    pub fn add_tab(&mut self, tab: Tab) {
        tab.session.set_visible(false);
        self.hide_active();
        if let Some(project) = self.projects.get_mut(self.active) {
            project.tabs.push(tab);
            project.active_tab = project.tabs.len() - 1;
        }
        self.show_active();
    }

    pub fn select_project(&mut self, index: usize) {
        if index < self.projects.len() && index != self.active {
            self.hide_active();
            self.active = index;
            self.show_active();
        }
    }

    pub fn select_tab(&mut self, index: usize) {
        let Some(project) = self.projects.get(self.active) else {
            return;
        };
        if index < project.tabs.len() && index != project.active_tab {
            self.hide_active();
            if let Some(project) = self.projects.get_mut(self.active) {
                project.active_tab = index;
            }
            self.show_active();
        }
    }

    pub fn cycle_tab(&mut self, step: isize) {
        let Some(project) = self.projects.get(self.active) else {
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
        let Some(project) = self.projects.get_mut(self.active) else {
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

    pub fn set_window_focused(&mut self, focused: bool) {
        self.window_focused = focused;
        if let Some(session) = self.active_session() {
            session.focus(focused);
        }
    }

    fn hide_active(&self) {
        if let Some(session) = self.active_session() {
            session.set_visible(false);
            if self.window_focused {
                session.focus(false);
            }
        }
    }

    fn show_active(&self) {
        if let Some(session) = self.active_session() {
            session.set_visible(true);
            if self.window_focused {
                session.focus(true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Grid;

    fn tab(title: &str) -> Tab {
        Tab {
            title: title.to_owned(),
            session: Session::fixture(Grid::default()),
        }
    }

    fn workspace() -> Workspace {
        let mut workspace = Workspace::default();
        workspace.add_project(Project::new("/a".into()), vec![tab("claude"), tab("shell")]);
        workspace.add_project(Project::new("/b".into()), vec![tab("vim")]);
        workspace
    }

    fn active_title(workspace: &Workspace) -> Option<&str> {
        let project = workspace.project()?;
        Some(&project.tabs.get(project.active_tab)?.title)
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
    fn sessions_cover_every_tab() {
        assert_eq!(workspace().sessions().count(), 3);
    }

    #[test]
    fn projects_are_found_by_root() {
        let workspace = workspace();
        assert_eq!(workspace.project_index(std::path::Path::new("/b")), Some(1));
    }
}
