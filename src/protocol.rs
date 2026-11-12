use std::process::ExitStatus;

use crate::grid::Surface;
use crate::terminal_input::{KeyInput, PointerInput, PointerPhase, WheelInput};

pub(crate) enum Msg {
    Key(KeyInput),
    Focus(bool),
    Pointer(PointerInput),
    Wheel(WheelInput),
    Scrollbar { phase: PointerPhase, y: f32 },
    ScrollbarHover(bool),
    CommandHeld(bool),
    Copy,
    Paste { text: String, allow_unsafe: bool },
    Resize(Surface),
    Visibility(Visibility),
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
    /// ⌘ moved onto or off a link; read `Session::link_hovered`.
    LinkHover,
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
