use rune::{Chord, Key, KeyEvent, KeyPlatform, Tx};

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

/// A shortcut's chord on macOS and on Linux. Ctrl is `mod` on Linux and programs in the
/// terminal need its plain chords, so app shortcuts there add Shift, as Ghostty's and GNOME
/// Terminal's do.
#[derive(Clone, Copy, Debug)]
pub struct Keys {
    mac: &'static str,
    linux: &'static str,
}

const fn keys(mac: &'static str, linux: &'static str) -> Keys {
    Keys { mac, linux }
}

impl Keys {
    pub const fn on(self, platform: KeyPlatform) -> &'static str {
        match platform {
            KeyPlatform::Mac => self.mac,
            KeyPlatform::Windows | KeyPlatform::Linux => self.linux,
        }
    }

    pub const fn here(self) -> &'static str {
        self.on(KeyPlatform::current())
    }
}

pub const COPY: Keys = keys("mod+c", "ctrl+shift+c");
pub const PASTE: Keys = keys("mod+v", "ctrl+shift+v");
/// Followed by a digit. Tabs follow GNOME Terminal and Ghostty on Linux.
pub(crate) const TAB_DIGIT: Keys = keys("mod+", "alt+");
pub(crate) const PROJECT_DIGIT: Keys = keys("ctrl+", "ctrl+shift+");

pub struct CommandSpec {
    pub command: Command,
    pub label: &'static str,
    pub keys: Option<Keys>,
    /// Listed in the palette before anything is typed.
    pub default: bool,
    /// Offered for this workspace and config, in a torn-off window or not.
    pub enabled: fn(&Workspace, &Config, bool) -> bool,
}

const fn spec(command: Command, label: &'static str, keys: Option<Keys>) -> CommandSpec {
    CommandSpec {
        command,
        label,
        keys,
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
    default(spec(
        Command::NewShell,
        "New shell tab",
        Some(keys("mod+t", "ctrl+shift+t")),
    )),
    default(CommandSpec {
        enabled: |_, config, _| config.git_command().is_some(),
        ..spec(
            Command::ToggleGit,
            "Toggle git",
            Some(keys("mod+g", "ctrl+shift+g")),
        )
    }),
    default(spec(
        Command::OpenProject,
        "Open project…",
        Some(keys("mod+n", "ctrl+shift+n")),
    )),
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
        ..spec(
            Command::Palette,
            "Command palette",
            Some(keys("mod+p", "ctrl+shift+p")),
        )
    },
    spec(
        Command::Find,
        "Find in scrollback",
        Some(keys("mod+f", "ctrl+shift+f")),
    ),
    spec(
        Command::ToggleSidebar,
        "Toggle sidebar",
        Some(keys("mod+b", "ctrl+shift+b")),
    ),
    spec(
        Command::ToggleDrawer,
        "Toggle drawer",
        Some(keys("mod+j", "ctrl+shift+j")),
    ),
    CommandSpec {
        enabled: |_, _, _| benches_platform::SECURE_INPUT_SUPPORTED,
        ..spec(
            Command::ToggleSecureInput,
            "Toggle secure keyboard entry",
            None,
        )
    },
    spec(
        Command::CloseTab,
        "Close tab",
        Some(keys("mod+w", "ctrl+shift+w")),
    ),
    spec(
        Command::ZoomIn,
        "Zoom in",
        Some(keys("mod+=", "ctrl+shift+=")),
    ),
    spec(
        Command::ZoomOut,
        "Zoom out",
        Some(keys("mod+-", "ctrl+shift+-")),
    ),
    spec(
        Command::ZoomReset,
        "Reset zoom",
        Some(keys("mod+0", "ctrl+shift+0")),
    ),
    spec(
        Command::PrevTab,
        "Previous tab",
        Some(keys("mod+shift+[", "ctrl+pageup")),
    ),
    spec(
        Command::NextTab,
        "Next tab",
        Some(keys("mod+shift+]", "ctrl+pagedown")),
    ),
    spec(
        Command::PrevPrompt,
        "Jump to previous prompt",
        Some(keys("mod+up", "ctrl+shift+up")),
    ),
    spec(
        Command::NextPrompt,
        "Jump to next prompt",
        Some(keys("mod+down", "ctrl+shift+down")),
    ),
    spec(
        Command::SelectLastOutput,
        "Select last command output",
        None,
    ),
    spec(
        Command::ReloadConfig,
        "Reload config",
        Some(keys("mod+shift+,", "ctrl+shift+,")),
    ),
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
        self.find_spec().and_then(|spec| spec.keys).map(Keys::here)
    }

    pub fn hint(self) -> Option<String> {
        self.chord().map(chord_hint)
    }
}

/// Workspace shortcuts. These keys never reach the terminal.
pub fn shortcut(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    let mods = event.mods;
    if !matches!(event.key, Key::Command(_)) && !mods.ctrl && !mods.cmd && !mods.alt {
        return false;
    }
    let pressed = |binding: &str| pressed(binding, event, tx);
    let digit =
        |prefix: Keys| (1..=9usize).find(|digit| pressed(&format!("{}{digit}", prefix.here())));
    if let Some(spec) = COMMANDS
        .iter()
        .find(|spec| spec.keys.map(Keys::here).is_some_and(pressed))
    {
        run_command(state, spec.command, tx);
    } else if let Some(digit) = digit(PROJECT_DIGIT) {
        let last = state.workspace.projects.len().saturating_sub(1);
        state.workspace.select_project((digit - 1).min(last));
    } else if let Some(digit) = digit(TAB_DIGIT) {
        if let Some(project) = state.workspace.project() {
            let last = project.tabs.len().saturating_sub(1);
            state.workspace.select_tab((digit - 1).min(last));
        }
    } else {
        return false;
    }
    true
}

/// `shortcut` for the text a focused field's editor would type, from the field's key handler,
/// which runs first. Off macOS an Alt chord, such as the Alt+digit tab chords, isn't a command
/// chord, so it arrives as text. Other keys are left to the field before `on_key`.
pub fn field_shortcut(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    matches!(event.key, Key::Text(_)) && shortcut(state, event, tx)
}

pub fn pressed(binding: &str, event: &KeyEvent, tx: &Tx) -> bool {
    Chord::parse(binding).is_ok_and(|chord| chord.matches_tx(event, tx))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLATFORMS: [KeyPlatform; 2] = [KeyPlatform::Mac, KeyPlatform::Linux];

    /// Every app chord on `platform`, digit chords included.
    fn chords(platform: KeyPlatform) -> Vec<Chord> {
        let digits = [TAB_DIGIT, PROJECT_DIGIT]
            .into_iter()
            .flat_map(|prefix| (1..=9).map(move |digit| format!("{}{digit}", prefix.on(platform))));
        COMMANDS
            .iter()
            .filter_map(|spec| spec.keys)
            .chain([COPY, PASTE])
            .map(|keys| keys.on(platform).to_owned())
            .chain(digits)
            .map(|chord| Chord::parse(&chord).unwrap())
            .collect()
    }

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
    fn chords_are_unique_on_each_platform() {
        for platform in PLATFORMS {
            let chords = chords(platform);
            for (index, chord) in chords.iter().enumerate() {
                let clash = chords[index + 1..]
                    .iter()
                    .find(|other| chord.same_keys(other, platform));
                assert_eq!(clash, None, "{chord} on {platform:?}");
            }
        }
    }

    #[test]
    fn plain_ctrl_keys_reach_the_terminal_on_linux() {
        let ctrl = Chord::parse("ctrl+c").unwrap();
        for chord in chords(KeyPlatform::Linux) {
            let typed_with_ctrl = matches!(chord.key(), rune::ChordKey::Char(_))
                && chord.mods_on(KeyPlatform::Linux) == ctrl.mods_on(KeyPlatform::Linux);
            assert!(!typed_with_ctrl, "{chord} takes a control key");
        }
    }
}
