use rune::{Chord, Key, KeyEvent, Tx};

use crate::workspace::Workspace;
use crate::{State, run_command};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    NewShell,
    ToggleLazygit,
    OpenProject,
    ChangeFolder,
    MoveToNewWindow,
    Palette,
    ToggleSidebar,
    ToggleDrawer,
    CloseTab,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    PrevTab,
    NextTab,
}

pub struct CommandSpec {
    pub command: Command,
    pub label: &'static str,
    pub chord: Option<&'static str>,
    pub in_palette: bool,
    pub enabled: fn(&Workspace) -> bool,
}

const fn spec(command: Command, label: &'static str, chord: Option<&'static str>) -> CommandSpec {
    CommandSpec {
        command,
        label,
        chord,
        in_palette: false,
        enabled: |_| true,
    }
}

const fn palette(mut spec: CommandSpec) -> CommandSpec {
    spec.in_palette = true;
    spec
}

/// Palette rows appear in this order, ahead of every dynamic action.
pub const COMMANDS: &[CommandSpec] = &[
    palette(spec(Command::NewShell, "New shell tab", Some("mod+t"))),
    palette(spec(
        Command::ToggleLazygit,
        "Toggle lazygit",
        Some("mod+g"),
    )),
    palette(spec(Command::OpenProject, "Open project…", Some("mod+n"))),
    palette(CommandSpec {
        enabled: |workspace| workspace.project().is_some(),
        ..spec(Command::ChangeFolder, "Change project folder…", None)
    }),
    palette(CommandSpec {
        enabled: |workspace| workspace.projects.len() > 1,
        ..spec(Command::MoveToNewWindow, "Move project to new window", None)
    }),
    spec(Command::Palette, "Command palette", Some("mod+p")),
    spec(Command::ToggleSidebar, "Toggle sidebar", Some("mod+b")),
    spec(Command::ToggleDrawer, "Toggle drawer", Some("mod+j")),
    spec(Command::CloseTab, "Close tab", Some("mod+w")),
    spec(Command::ZoomIn, "Zoom in", Some("mod+=")),
    spec(Command::ZoomOut, "Zoom out", Some("mod+-")),
    spec(Command::ZoomReset, "Reset zoom", Some("mod+0")),
    spec(Command::PrevTab, "Previous tab", Some("mod+shift+[")),
    spec(Command::NextTab, "Next tab", Some("mod+shift+]")),
];

impl Command {
    pub fn label(self) -> &'static str {
        COMMANDS
            .iter()
            .find(|spec| spec.command == self)
            .map_or("", |spec| spec.label)
    }
}

/// Workspace shortcuts. These keys never reach the terminal.
pub fn shortcut(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    if !matches!(event.key, Key::Command(_)) && !event.mods.ctrl {
        return false;
    }
    let pressed = |binding: &str| pressed(binding, event, tx);
    let digit = |prefix: &str| (1..=9usize).find(|digit| pressed(&format!("{prefix}{digit}")));
    if let Some(spec) = COMMANDS.iter().find(|spec| spec.chord.is_some_and(pressed)) {
        run_command(state, spec.command, tx);
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

pub fn pressed(binding: &str, event: &KeyEvent, tx: &Tx) -> bool {
    Chord::parse(binding).is_ok_and(|chord| chord.matches_tx(event, tx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_chord_parses_and_is_unique() {
        let chords: Vec<_> = COMMANDS.iter().filter_map(|spec| spec.chord).collect();
        assert!(chords.iter().all(|chord| Chord::parse(chord).is_ok()));
        let mut unique = chords.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), chords.len());
    }
}
