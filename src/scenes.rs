use std::path::PathBuf;

use rune::TextEdit;

use libghostty_vt::effect::ClipboardLocation;

use crate::app::{Confirm, State};
use crate::grid::Grid;
use crate::jump;
use crate::palette::WorktreeChoice;
use crate::protocol::{Progress, ProgressKind};
use crate::session::Session;
use crate::workspace::{Project, Status, Tab};

pub(crate) const SCENE_FIXTURE: &[u8] = b"\x1b[1;32magentd\x1b[0m fixture\r\n\x1b[31mred\x1b[0m \x1b[42;30m on green \x1b[0m \x1b[7minverse\x1b[0m\r\na != b // c == d ... \x1b[1m!=\x1b[0m \x1b[3m!=\x1b[0m\r\n$ ";
const SCENE_UNICODE: &str = concat!(
    "wide \u{4e16}\u{754c} kana \u{30ab}\u{30bf}\u{30ab}\u{30ca}|\r\n",
    "emoji \u{1F600}\u{1F680} zwj \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} vs16 \u{2764}\u{FE0F}|\r\n",
    "marks e\u{301}a\u{308} symbols \u{23FA} \u{23BF} \u{2713}|\r\n",
    "\u{250C}\u{2500}\u{252C}\u{2500}\u{2510} \u{256D}\u{2500}\u{256E} \u{2554}\u{2550}\u{2557} \u{2588}\u{2593}\u{2592}\u{2591} \u{2580}\u{2584}\u{258C}\u{2590}\r\n",
    "\u{2502} \u{253C} \u{2502} \u{2570}\u{2500}\u{256F} \u{255A}\u{2550}\u{255D} \u{E0B0}\u{E0B1}\u{E0B2}\u{E0B3}\r\n",
    "\x1b[1mbold\x1b[0m \x1b[3mitalic\x1b[0m \x1b[1;3mboth\x1b[0m \x1b[2mfaint\x1b[0m \x1b[9mstrike\x1b[0m \x1b[53mover\x1b[0m\r\n",
    "\x1b[4msingle\x1b[0m \x1b[4:2mdouble\x1b[0m \x1b[4:3;58;2;255;80;80mcurly\x1b[0m \x1b[4:4mdotted\x1b[0m \x1b[4:5mdashed\x1b[0m\r\n",
    "\x1b[41m\u{1F41F}\x1b[31;43m\u{E0C6}\x1b[30m ~ \x1b[33;42m\u{E0C6}\x1b[32;49m\u{E0C6}\x1b[0m\r\n",
    "\x1b[32m\u{F432}\x1b[0m $ \r\n",
    "\x1b[44;37m \u{E7A8} \x1b[34;42m\u{E0C6}\x1b[30m \u{F418} main \x1b[32;43m\u{E0C6}\x1b[30m \u{F0E7} \x1b[33;49m\u{E0C6}\x1b[0m \u{F07B} \u{F418}x \u{F07B}\u{F418} \u{F07B}\r\n"
);

pub(crate) fn fixture_grid(bytes: &[u8]) -> Grid {
    Grid::from_vt(bytes, 60, 12)
        .unwrap_or_else(|error| Grid::message(&format!("failed to build fixture: {error:#}")))
}

pub(crate) fn fixture_tab(title: &str, bytes: &[u8]) -> Tab {
    Tab::new(title, Session::fixture(fixture_grid(bytes)))
}

/// Rune applies a scene over the state `init` built, so drop the paths that would load and save the user's layout.
fn isolate_scene(state: &mut State) {
    state.started = true;
    state.paths = None;
}

pub(crate) fn show_tab(state: &mut State, tab: Tab) {
    isolate_scene(state);
    state
        .workspace
        .add_project(Project::new("/fixture".into(), vec![tab]));
}

pub(crate) fn show_scene(state: &mut State, bytes: &[u8]) {
    show_tab(state, fixture_tab("shell", bytes));
}

/// History with the scrollbar as it shows just after scrolling, or while `hovered`.
pub(crate) fn scrollback_tab(hovered: bool) -> Tab {
    let lines: String = (1..=40).map(|line| format!("{line}\r\n")).collect();
    let mut grid = fixture_grid(lines.as_bytes());
    if let Some(scrollbar) = &mut grid.scrollbar {
        (scrollbar.opacity, scrollbar.wide) = (1.0, hovered);
    }
    Tab::new("shell", Session::fixture(grid))
}

pub(crate) fn show_workspace_scene(state: &mut State) {
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

pub(crate) fn add_scene_tray(state: &mut State) {
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

fn show_progress_scene(state: &mut State) {
    show_workspace_scene(state);
    add_scene_tray(state);
    let progress = |kind, percent| Some(Progress { kind, percent });
    let project = &mut state.workspace.projects[0];
    project.tabs[0].progress = progress(ProgressKind::Normal, 62);
    project.tabs[1].progress = progress(ProgressKind::Error, 30);
    project.tabs[2].progress = progress(ProgressKind::Indeterminate, 0);
    project.tray[0].progress = progress(ProgressKind::Paused, 80);
    project.tabs[1].session.alert();
}

const SCENE_LAZYGIT: &[u8] = b"\x1b[1mStatus\x1b[0m\r\n\x1b[32m\xe2\x9c\x93\x1b[0m agentd -> lazygit-overlay\r\n\r\n\x1b[1mFiles\x1b[0m\r\n\x1b[32m M\x1b[0m src/main.rs\r\n\x1b[32m M\x1b[0m src/workspace.rs\r\n\x1b[31m??\x1b[0m notes.txt\r\n";

fn show_lazygit_scene(state: &mut State) {
    show_workspace_scene(state);
    state
        .workspace
        .show_overlay(Some(fixture_tab("lazygit", SCENE_LAZYGIT)));
}

type Scene = (&'static str, fn(&mut State));

pub(crate) const SCENES: &[Scene] = &[
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
        state.palette.edit = TextEdit::new("lz");
        state.palette.open = true;
    }),
    ("palette-commands", |state| {
        show_workspace_scene(state);
        state.palette.edit = TextEdit::new("zoom");
        state.palette.open = true;
    }),
    ("palette-error", |state| {
        show_workspace_scene(state);
        add_scene_tray(state);
        state.palette.edit = TextEdit::new("site");
        state.palette.open = true;
        state.palette.error = Some("✗ Branch site already exists".to_owned());
    }),
    ("worktrees", |state| {
        show_workspace_scene(state);
        state.palette.edit = TextEdit::new("site");
        state.palette.open = true;
        state.palette.worktrees = Some(WorktreeChoice {
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
    ("progress", show_progress_scene),
    ("lazygit", show_lazygit_scene),
    ("confirm-stack", |state| {
        show_workspace_scene(state);
        let session = state.session().map_or(0, Session::id);
        state.confirms.push(Confirm::Paste {
            session,
            text: "rm -rf x\n".to_owned(),
        });
        state.confirms.push(Confirm::Close {
            session,
            project: None,
        });
    }),
    ("secure-input", |state| {
        let mut grid = fixture_grid(b"[sudo] password for me: ");
        grid.password_input = true;
        show_tab(state, Tab::new("shell", Session::fixture(grid)));
        state.workspace.set_window_focused(true);
    }),
    ("clipboard-read", |state| {
        show_workspace_scene(state);
        let session = state.session().map_or(0, Session::id);
        state.confirms.push(Confirm::ClipboardRead {
            session,
            location: ClipboardLocation::Standard,
        });
    }),
    ("empty", isolate_scene),
    ("collapsed", |state| {
        show_workspace_scene(state);
        state.sidebar_hidden = true;
    }),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

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
}
