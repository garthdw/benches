use rune::{Chord, Key, KeyEvent, Tx};

use crate::app::{State, run_command};
use crate::workspace::Workspace;
use benches_core::config::Config;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    NewShell,
    ToggleGit,
    OpenProject,
    ChangeFolder,
    OpenWorktree,
    CreateWorktree,
    MoveToNewWindow,
    MoveToMainWindow,
    MergeIntoMain,
    Palette,
    Find,
    ToggleSidebar,
    ToggleDrawer,
    ToggleSecureInput,
    CloseTab,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    PrevTab,
    NextTab,
    PrevPrompt,
    NextPrompt,
    SelectLastOutput,
    ReloadConfig,
}

pub struct CommandSpec {
    pub command: Command,
    pub label: &'static str,
    pub chord: Option<&'static str>,
    /// Listed in the palette before anything is typed.
    pub default: bool,
    /// Offered for this workspace and config, in a torn-off window or not.
    pub enabled: fn(&Workspace, &Config, bool) -> bool,
}

const fn spec(command: Command, label: &'static str, chord: Option<&'static str>) -> CommandSpec {
    CommandSpec {
        command,
        label,
        chord,
        default: false,
        enabled: |_, _, _| true,
    }
}

const fn default(mut spec: CommandSpec) -> CommandSpec {
    spec.default = true;
    spec
}

/// Palette rows appear in this order, ahead of every dynamic action.
pub const COMMANDS: &[CommandSpec] = &[
    default(spec(Command::NewShell, "New shell tab", Some("mod+t"))),
    default(CommandSpec {
        enabled: |_, config, _| config.git_command().is_some(),
        ..spec(Command::ToggleGit, "Toggle git", Some("mod+g"))
    }),
    default(spec(Command::OpenProject, "Open project…", Some("mod+n"))),
    default(CommandSpec {
        enabled: |workspace, _, _| workspace.project().is_some(),
        ..spec(Command::ChangeFolder, "Change project folder…", None)
    }),
    CommandSpec {
        enabled: |workspace, _, _| workspace.project().is_some(),
        ..spec(Command::OpenWorktree, "Open worktree…", None)
    },
    CommandSpec {
        enabled: |workspace, _, _| workspace.project().is_some(),
        ..spec(Command::CreateWorktree, "Create worktree…", None)
    },
    default(CommandSpec {
        enabled: |workspace, _, _| workspace.projects.len() > 1,
        ..spec(Command::MoveToNewWindow, "Move project to new window", None)
    }),
    default(CommandSpec {
        enabled: |workspace, _, torn_off| torn_off && workspace.project().is_some(),
        ..spec(
            Command::MoveToMainWindow,
            "Move project to main window",
            None,
        )
    }),
    default(CommandSpec {
        enabled: |_, _, torn_off| torn_off,
        ..spec(Command::MergeIntoMain, "Merge window into main", None)
    }),
    CommandSpec {
        enabled: |_, _, _| false,
        ..spec(Command::Palette, "Command palette", Some("mod+p"))
    },
    spec(Command::Find, "Find in scrollback", Some("mod+f")),
    spec(Command::ToggleSidebar, "Toggle sidebar", Some("mod+b")),
    spec(Command::ToggleDrawer, "Toggle drawer", Some("mod+j")),
    CommandSpec {
        enabled: |_, _, _| benches_platform::SECURE_INPUT_SUPPORTED,
        ..spec(
            Command::ToggleSecureInput,
            "Toggle secure keyboard entry",
            None,
        )
    },
    spec(Command::CloseTab, "Close tab", Some("mod+w")),
    spec(Command::ZoomIn, "Zoom in", Some("mod+=")),
    spec(Command::ZoomOut, "Zoom out", Some("mod+-")),
    spec(Command::ZoomReset, "Reset zoom", Some("mod+0")),
    spec(Command::PrevTab, "Previous tab", Some("mod+shift+[")),
    spec(Command::NextTab, "Next tab", Some("mod+shift+]")),
    spec(
        Command::PrevPrompt,
        "Jump to previous prompt",
        Some("mod+up"),
    ),
    spec(Command::NextPrompt, "Jump to next prompt", Some("mod+down")),
    spec(
        Command::SelectLastOutput,
        "Select last command output",
        None,
    ),
    spec(Command::ReloadConfig, "Reload config", Some("mod+shift+,")),
];

/// A chord as this platform draws it: `⇧⌘]` on macOS, `Ctrl+Shift+]` on Linux.
pub fn chord_hint(chord: &str) -> String {
    Chord::parse(chord)
        .map(|chord| chord.label())
        .unwrap_or_default()
}

impl Command {
    fn find_spec(self) -> Option<&'static CommandSpec> {
        COMMANDS.iter().find(|spec| spec.command == self)
    }

    pub fn label(self) -> &'static str {
        self.find_spec().map_or("", |spec| spec.label)
    }

    pub fn chord(self) -> Option<&'static str> {
        self.find_spec().and_then(|spec| spec.chord)
    }

    pub fn hint(self) -> Option<String> {
        self.chord().map(chord_hint)
    }
}

/// Workspace shortcuts. These keys never reach the terminal.
pub fn shortcut(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    if !matches!(event.key, Key::Command(_)) && !event.mods.ctrl && !event.mods.cmd {
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
    use rune::KeyPlatform;

    #[test]
    fn hints_use_the_platforms_glyphs_or_words() {
        let hint = |chord, platform| Chord::parse(chord).unwrap().label_on(platform);
        assert_eq!(hint("mod+t", KeyPlatform::Mac), "⌘T");
        assert_eq!(hint("mod+shift+]", KeyPlatform::Mac), "⇧⌘]");
        assert_eq!(hint("alt+shift+mod+x", KeyPlatform::Mac), "⌥⇧⌘X");
        assert_eq!(hint("ctrl+shift+t", KeyPlatform::Linux), "Ctrl+Shift+T");
        assert_eq!(hint("ctrl+pageup", KeyPlatform::Linux), "Ctrl+Page Up");
        assert_eq!(
            chord_hint(Command::NewShell.chord().unwrap()),
            hint(Command::NewShell.chord().unwrap(), KeyPlatform::current())
        );
    }

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
