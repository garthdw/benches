use std::process::ExitStatus;

use libghostty_vt::effect::ClipboardLocation;
use libghostty_vt::key;

use crate::config::TerminalSettings;
use crate::grid::Surface;
use crate::terminal_input::{KeyInput, PointerInput, PointerPhase, WheelInput};

pub(crate) enum Msg {
    Key(KeyInput),
    Focus(bool),
    Pointer(PointerInput),
    Wheel(WheelInput),
    Scrollbar {
        phase: PointerPhase,
        y: f32,
    },
    ScrollbarHover(bool),
    Modifiers(key::Mods),
    Copy,
    /// Scroll to the previous (negative) or next prompt marked by the shell.
    PromptJump(isize),
    SelectLastOutput,
    Paste {
        text: String,
        allow_unsafe: bool,
    },
    Search(String),
    SearchStep {
        older: bool,
    },
    /// The answer to `Event::ClipboardRead`; `None` is a refusal, which the program sees as an
    /// empty clipboard.
    ClipboardReply {
        location: ClipboardLocation,
        text: Option<String>,
    },
    Resize(Surface),
    Visibility(Visibility),
    Configure(TerminalSettings),
    Stop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressKind {
    Normal,
    Error,
    Paused,
    Indeterminate,
}

/// A program's OSC 9;4 progress report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub kind: ProgressKind,
    pub percent: u8,
}

/// Sent from the actor for the UI to pick up in `poll`.
pub enum Event {
    Copied(String),
    /// A program set the clipboard (OSC 52); `ask` is whether the user must agree first.
    ClipboardWrite {
        text: String,
        ask: bool,
    },
    /// A program wants the clipboard (OSC 52 `?`); the actor holds its reply until
    /// `Msg::ClipboardReply`.
    ClipboardRead {
        location: ClipboardLocation,
        ask: bool,
    },
    PasteRejected(String),
    /// The program's title (OSC 0 or 2); empty when it clears it.
    Title(String),
    Exited(ExitStatus),
    /// A bell or desktop notification while not shown with keys, once until it next is.
    /// A bell has an empty title and body; OSC 9 has no title.
    Notify {
        title: String,
        body: String,
    },
    /// The program's progress, or `None` once it removes it, lets it go stale or exits.
    Progress(Option<Progress>),
    /// A command ended, by the shell's OSC 133 D; `failed` is a non-zero exit code.
    CommandFinished {
        failed: bool,
    },
    /// What the pointer over the terminal would do changed; read `Session::hover`.
    Hover,
}

/// How much of a session the UI shows, which decides what it publishes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Visibility {
    /// The terminal itself, published every frame.
    Shown,
    /// Only its tab label, so titles still come back.
    Listed,
    /// Nothing; the latest title waits until it is listed or shown.
    Hidden,
}
