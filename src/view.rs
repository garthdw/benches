use std::path::PathBuf;

use libghostty_vt::mouse;
use rune::tok;
use rune::{
    Color, Cursor, Cx, Drag, El, Icon, Phase, PointerButton, PointerEvent, PopupItem, PopupMenu,
    Tx, WheelEvent, col, effect, el, icon, row, text,
};

use crate::app::{
    Confirm, State, answer_full_disk_access, open_project, pick_new_tab, request_close_project,
    resolve_confirm, tray_action,
};
use crate::find;
use crate::input::{self, focus_on_press, pointer_phase, send_pointer};
use crate::keymap::Command;
use crate::palette::{self, open_palette};
use crate::render;
use crate::session::{Attention, Session};
use crate::windows::{move_to_main_window, move_to_new_window, torn_off};
use crate::workspace::{Project, Status, Tab, Workspace};
use benches_core::protocol::{Progress, ProgressKind};
use benches_core::terminal_input::{Hover, PointerPhase, WheelInput};

pub(crate) const TERMINAL_MARK: u32 = 1;
pub(crate) const DRAWER_MARK: u32 = 2;
pub(crate) const OVERLAY_MARK: u32 = 3;
const TRAY_BAR_HEIGHT: f32 = 34.0;
/// Fixed, so the drawer's terminal keeps its size while the drawer slides.
pub(crate) const DRAWER_HEIGHT: f32 = 300.0;
const OVERLAY_INSET: f32 = 40.0;
const OVERLAY_PADDING: f32 = 6.0;
const DOT: f32 = 7.0;
/// Wider than the traffic lights, which sit over the sidebar.
pub(crate) const SIDEBAR_WIDTH: f32 = 200.0;
/// The sidebar's top padding too, so the traffic lights clear the first project.
const TAB_STRIP_HEIGHT: f32 = 36.0;
const TAB_MAX_WIDTH: f32 = 220.0;
/// The collapsed sidebar: narrower than the traffic lights, which sit above its tabs.
pub(crate) const RAIL_WIDTH: f32 = 44.0;
const RAIL_TAB: f32 = 32.0;
static SIDEBAR_ICON: Icon = Icon {
    name: "sidebar",
    view: 24.0,
    body: r#"<rect x="3" y="4.5" width="18" height="15" rx="3" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M9.5 4.5v15" stroke="currentColor" stroke-width="1.6"/>"#,
};

static LOCK_ICON: Icon = Icon {
    name: "lock",
    view: 24.0,
    body: r#"<rect x="5" y="11" width="14" height="9.5" rx="2" fill="currentColor"/><path d="M8 11V8a4 4 0 0 1 8 0v3" fill="none" stroke="currentColor" stroke-width="1.8"/>"#,
};

fn dot(color: Color) -> El {
    el().size(DOT, DOT).radius(DOT / 2.0).fill(color)
}

/// The right padding, where the thumb is drawn, while there is history to scroll.
fn scrollbar_strip(
    key: &str,
    drawer: Option<bool>,
    pick: fn(&Workspace) -> Option<&Session>,
) -> El {
    el().key(format!("{key}-scrollbar"))
        .pin(None, Some(0.0), Some(0.0), Some(0.0))
        .w(render::PADDING_POINTS)
        .cursor(Cursor::Default)
        .on_hover_change(move |state: &mut State, hovered, tx: &mut Tx| {
            if let Some(session) = pick(&state.workspace) {
                session.hover_scrollbar(hovered);
            }
            tx.live_only();
        })
        .on_drag(move |state: &mut State, drag: &mut Drag, tx: &mut Tx| {
            let phase = pointer_phase(drag.phase);
            focus_on_press(state, drawer, phase, tx);
            if let Some(session) = pick(&state.workspace) {
                session.drag_scrollbar(phase, drag.y);
            }
        })
}

/// A terminal drawn into `mark`, taking pointer input for the session `pick` returns.
/// Pressing it sends keys to it, so the drawer and the tab each take focus by click.
fn terminal_surface(
    mark: u32,
    key: &str,
    workspace: &Workspace,
    pick: fn(&Workspace) -> Option<&Session>,
) -> El {
    let drawer = match mark {
        OVERLAY_MARK => None,
        _ => Some(mark == DRAWER_MARK),
    };
    let scrollable = pick(workspace).is_some_and(|session| session.grid().scrollbar.is_some());
    let cursor = match pick(workspace).map(Session::hover) {
        Some(Hover::Link) => Cursor::Pointer,
        Some(Hover::Report) => Cursor::Default,
        Some(Hover::Text) | None => Cursor::Text,
    };
    effect(mark)
        .key(key)
        .cursor(cursor)
        .pointer_buttons([PointerButton::Middle])
        .children(scrollable.then(|| scrollbar_strip(key, drawer, pick)))
        .on_drag(move |state: &mut State, drag: &mut Drag, tx: &mut Tx| {
            let phase = pointer_phase(drag.phase);
            let button = match tx.button() {
                PointerButton::Middle => mouse::Button::Middle,
                _ => mouse::Button::Left,
            };
            focus_on_press(state, drawer, phase, tx);
            let session = pick(&state.workspace);
            send_pointer(session, phase, Some(button), drag.x, drag.y, tx.mods());
        })
        .on_secondary_press_local(move |state: &mut State, event: PointerEvent, tx: &mut Tx| {
            // Rune reports the right button once, with no separate release.
            for phase in [PointerPhase::Press, PointerPhase::Release] {
                let button = Some(mouse::Button::Right);
                let session = pick(&state.workspace);
                send_pointer(session, phase, button, event.x, event.y, tx.mods());
            }
        })
        .on_pointer_move_local(move |state: &mut State, event: PointerEvent, tx: &mut Tx| {
            let session = pick(&state.workspace);
            send_pointer(
                session,
                PointerPhase::Move,
                None,
                event.x,
                event.y,
                tx.mods(),
            );
            tx.live_only();
        })
        .on_wheel(move |state: &mut State, event: WheelEvent, tx: &mut Tx| {
            if let Some(session) = pick(&state.workspace) {
                session.wheel(WheelInput {
                    dy: event.dy,
                    x: event.x,
                    y: event.y,
                    mods: input::vt_mods(event.mods),
                });
            }
            tx.live_only();
            true
        })
}

fn status_text(status: &Status) -> &str {
    match status {
        Status::Running => "running",
        Status::Stopped => "stopped",
        Status::Exited => "exited",
        Status::Failed(reason) => reason,
    }
}

fn tray_menu() -> PopupMenu {
    PopupMenu::new()
        .item(PopupItem::new("restart", "Restart"))
        .item(PopupItem::new("stop", "Stop"))
        .item(PopupItem::new("remove", "Remove"))
}

fn button(theme: &tok::Theme, key: String, label: &str, active: bool) -> El {
    row()
        .key(key)
        .role("button")
        .label(label)
        .items_center()
        .gap(6.0)
        .fill(if active {
            theme.control_off
        } else {
            Color::TRANSPARENT
        })
}

fn pill(theme: &tok::Theme, key: String, label: &str, active: bool, marker: Option<El>) -> El {
    button(theme, key, label, active)
        .px(10.0)
        .py(4.0)
        .radius(6.0)
        .children(marker)
        .child(text(label.to_owned()).color(theme.text).truncate())
}

fn attention_dot(accent: Color, attention: Attention) -> Option<El> {
    match attention {
        Attention::None => None,
        Attention::Alert => Some(dot(accent)),
    }
}

const PROGRESS_WIDTH: f32 = 28.0;

fn progress_bar(theme: &tok::Theme, accent: Color, progress: Option<Progress>) -> Option<El> {
    let progress = progress?;
    let (color, width) = match progress.kind {
        ProgressKind::Normal => (accent, f32::from(progress.percent)),
        ProgressKind::Error => (
            rune::color::oklch(0.66, 0.2, 25.0),
            f32::from(progress.percent),
        ),
        ProgressKind::Paused => (
            rune::color::oklch(0.8, 0.15, 85.0),
            f32::from(progress.percent),
        ),
        ProgressKind::Indeterminate => (theme.text_secondary, 100.0),
    };
    Some(
        row()
            .w(PROGRESS_WIDTH)
            .h(4.0)
            .radius(2.0)
            .clip_rounded(2.0)
            .fill(rune::color::oklcha(0.5, 0.0, 0.0, 0.3))
            .child(el().w(PROGRESS_WIDTH * width / 100.0).h_full().fill(color)),
    )
}

fn status_dot(theme: &tok::Theme, tab: &Tab) -> El {
    dot(match &tab.status {
        Status::Running if tab.command_failed => rune::color::oklch(0.66, 0.2, 25.0),
        Status::Running => rune::color::oklch(0.74, 0.17, 150.0),
        Status::Failed(_) => rune::color::oklch(0.66, 0.2, 25.0),
        Status::Stopped | Status::Exited => theme.text_secondary,
    })
}

/// Whether the sidebar shows as the rail, its width and its side padding.
fn sidebar_metrics(state: &State) -> (bool, f32, f32) {
    let rail = state.sidebar_hidden && state.workspace.projects.len() > 1;
    match (state.sidebar_hidden, rail) {
        (false, _) => (rail, SIDEBAR_WIDTH, 8.0),
        (true, true) => (rail, RAIL_WIDTH, 6.0),
        (true, false) => (rail, 0.0, 0.0),
    }
}

fn sidebar(state: &State, theme: &tok::Theme, accent: Color) -> El {
    let (rail, sidebar_width, sidebar_padding) = sidebar_metrics(state);
    let projects = state
        .workspace
        .projects
        .iter()
        .enumerate()
        .filter(|_| sidebar_width > 0.0)
        .map(|(index, project)| {
            let key = format!("project-{index}");
            let menu_key = key.clone();
            let active = index == state.workspace.active;
            let shows_close = active || state.hovered_project == Some(index);
            let project_tab = if rail {
                let mut initials = project.name.chars();
                let initials: String = initials
                    .next()
                    .into_iter()
                    .flat_map(char::to_uppercase)
                    .chain(initials.next())
                    .collect();
                let marker = attention_dot(accent, project.attention())
                    .map(|marker| marker.absolute(RAIL_TAB - DOT - 3.0, 3.0));
                button(theme, key, &project.name, active)
                    .size(RAIL_TAB, RAIL_TAB)
                    .center()
                    .radius(8.0)
                    .child(text(initials).color(theme.text))
                    .children(marker)
            } else {
                pill(
                    theme,
                    key,
                    &project.name,
                    active,
                    attention_dot(accent, project.attention()),
                )
                .w_full()
                .children(shows_close.then(|| el().grow(1.0)))
                .children(shows_close.then(|| {
                    el().key(format!("project-close-{index}"))
                        .role("button")
                        .label("Close project")
                        .px(4.0)
                        .radius(4.0)
                        .child(text("×").color(theme.text_secondary))
                        .on_press(move |state: &mut State| request_close_project(state, index))
                }))
            };
            project_tab
                .cursor(if state.dragged_project == Some(index) {
                    Cursor::Grabbing
                } else {
                    Cursor::Default
                })
                .on_hover_change(move |state: &mut State, hovered, _: &mut Tx| {
                    if hovered {
                        state.hovered_project = Some(index);
                    } else if state.hovered_project == Some(index) {
                        state.hovered_project = None;
                    }
                })
                .on_press(move |state: &mut State| state.workspace.select_project(index))
                .on_drag(move |state: &mut State, drag: &mut Drag, _: &mut Tx| {
                    state.dragged_project = (drag.phase == Phase::Move).then_some(index);
                    if drag.phase == Phase::Up
                        && !drag.cancelled()
                        && torn_off(drag.window_point(), sidebar_width)
                    {
                        move_to_new_window(state, index, drag.screen_point());
                    }
                })
                .on_secondary_press(move |state: &mut State, _, tx: &mut Tx| {
                    let to_main = state
                        .is_torn_off()
                        .then(|| PopupItem::new("main", "Move to main window"));
                    let to_new = (state.workspace.projects.len() > 1)
                        .then(|| PopupItem::new("move", "Move to new window"));
                    let items: Vec<_> = to_new.into_iter().chain(to_main).collect();
                    if items.is_empty() {
                        return;
                    }
                    let menu = PopupMenu::new().items(items);
                    let Some(project) = state.workspace.projects.get(index) else {
                        return;
                    };
                    let root = project.root.clone();
                    tx.popup_menu(menu, &menu_key, move |state: &mut State, picked, _| {
                        let Some(index) = state.workspace.project_index(&root) else {
                            return;
                        };
                        if picked == Some("move") {
                            move_to_new_window(state, index, None);
                        } else if picked == Some("main") {
                            move_to_main_window(state, index);
                        }
                    });
                })
        });
    // Stays in the tree so its width springs between expanded, rail and gone.
    col()
        .key("sidebar")
        .w(sidebar_width)
        .h_full()
        .gap(2.0)
        .pt(TAB_STRIP_HEIGHT)
        .px(sidebar_padding)
        .clip_rounded(0.0)
        .fill(theme.card)
        .children(projects)
}

fn tab_strip(state: &State, theme: &tok::Theme, accent: Color, titlebar_inset: f32) -> El {
    let (_, sidebar_width, _) = sidebar_metrics(state);
    let tabs = state.workspace.project().map_or_else(Vec::new, |project| {
        project
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                pill(
                    theme,
                    format!("tab-{index}"),
                    tab.label(),
                    index == project.active_tab,
                    attention_dot(accent, tab.session.attention()),
                )
                .children(progress_bar(theme, accent, tab.progress))
                .max_w(TAB_MAX_WIDTH)
                .on_press(move |state: &mut State| state.workspace.select_tab(index))
            })
            .collect()
    });
    let sidebar_toggle = el()
        .key("sidebar-toggle")
        .role("button")
        .label("Toggle sidebar")
        .padding(6.0)
        .radius(6.0)
        .child(icon(&SIDEBAR_ICON, 16.0).color(theme.text_secondary))
        .on_press(|state: &mut State| state.sidebar_hidden = !state.sidebar_hidden);
    let new_tab = state.workspace.project().map(|_| {
        pill(theme, "new-tab".to_owned(), "+", false, None).on_press_tx(
            |state: &mut State, tx: &mut Tx| {
                let git = state.config.git_command().is_some();
                let menu = PopupMenu::new()
                    .item(PopupItem::new("shell", "Shell"))
                    .items(git.then(|| PopupItem::new("git", "Git")))
                    .item(PopupItem::new("tray", "Background command…"))
                    .item(PopupItem::new("palette", "Command centre…"));
                tx.popup_menu(menu, "new-tab", pick_new_tab);
            },
        )
    });
    row()
        .key("tab-strip")
        .w_full()
        .h(TAB_STRIP_HEIGHT)
        .gap(4.0)
        // Clears the traffic lights once the sidebar is narrower than them.
        .pl((titlebar_inset + 8.0 - sidebar_width).max(8.0))
        .pr(8.0)
        .items_center()
        .child(sidebar_toggle)
        .children(tabs)
        .children(new_tab)
        .children(state.secure_input.enabled.then(|| {
            row().grow(1.0).justify_end().child(
                icon(&LOCK_ICON, 16.0)
                    .label("Secure keyboard entry")
                    .color(theme.text_secondary),
            )
        }))
}

const CONFIRM_BAR_PITCH: f32 = 40.0;

fn confirm_bar(
    theme: &tok::Theme,
    key: &'static str,
    message: String,
    action: &'static str,
    y: f32,
    resolve: impl Fn(&mut State, &mut Tx, bool) + Clone + 'static,
) -> El {
    row()
        .key(key)
        .absolute(12.0, y)
        .gap(8.0)
        .items_center()
        .px(10.0)
        .py(6.0)
        .radius(6.0)
        .fill(theme.card)
        .child(text(message).color(theme.text))
        .child(
            bar_button(theme, format!("{key}-yes"), action).on_press_tx({
                let resolve = resolve.clone();
                move |state: &mut State, tx: &mut Tx| resolve(state, tx, true)
            }),
        )
        .child(
            bar_button(theme, format!("{key}-no"), "Cancel")
                .on_press_tx(move |state: &mut State, tx: &mut Tx| resolve(state, tx, false)),
        )
}

fn bar_button(theme: &tok::Theme, key: impl Into<String>, label: &'static str) -> El {
    el().key(key.into())
        .role("button")
        .label(label)
        .px(10.0)
        .py(4.0)
        .radius(6.0)
        .fill(theme.bg)
        .child(text(label).color(theme.text))
}

/// A notice along the top of the window. Only its buttons answer it, so nothing typed can.
fn banner(theme: &tok::Theme, key: &'static str, title: &str, detail: El, buttons: Vec<El>) -> El {
    row()
        .key(key)
        .gap(8.0)
        .items_start()
        .px(10.0)
        .py(6.0)
        .radius(6.0)
        .fill(theme.card)
        // Rune's orange accent, whatever accent the system uses.
        .border(1.0, tok::accent(3, true))
        .child(
            col()
                .gap(4.0)
                .grow(1.0)
                .min_w(0.0)
                .child(text(title.to_owned()).color(theme.text))
                .child(detail),
        )
        .children(buttons)
}

fn config_error_banner(state: &State, theme: &tok::Theme) -> Option<El> {
    let error = state.config_error.as_ref()?;
    // TOML errors point at the column with a caret, which needs even widths.
    let detail = text(error.trim_end().to_owned())
        .font(tok::type_::BODY.family(tok::Family::SystemMono))
        .color(theme.text_secondary)
        .wrap();
    let dismiss = bar_button(theme, "config-error-dismiss", "Dismiss")
        .on_press(|state: &mut State| state.config_error = None);
    Some(banner(
        theme,
        "config-error",
        "config.toml not applied",
        detail,
        vec![dismiss],
    ))
}

fn full_disk_access_banner(state: &State, theme: &tok::Theme) -> Option<El> {
    state.full_disk_access.as_ref()?;
    let detail = text(
        "Programs started here share Benches' privacy permissions, so without Full Disk Access \
         some can't read even their own data, such as a browser its profiles."
            .to_owned(),
    )
    .color(theme.text_secondary)
    .wrap();
    let open = bar_button(theme, "full-disk-access-open", "Open Settings")
        .on_press(|state: &mut State| answer_full_disk_access(state, true));
    let dismiss = bar_button(theme, "full-disk-access-dismiss", "Dismiss")
        .on_press(|state: &mut State| answer_full_disk_access(state, false));
    Some(banner(
        theme,
        "full-disk-access",
        "Benches doesn't have Full Disk Access",
        detail,
        vec![open, dismiss],
    ))
}

/// The caret, composition and confirmations belong to whichever terminal takes keys.
fn focus_overlays(state: &State, theme: &tok::Theme) -> Vec<El> {
    let mut overlays = Vec::new();
    if let Some([x, y, width, height]) = state.caret {
        overlays.push(
            el().key("caret")
                .absolute(x, y)
                .size(width, height)
                .ime_cursor()
                .ime_option_as_alt(true),
        );
        if !state.preedit.is_empty() {
            overlays.push(
                el().key("preedit")
                    .absolute(x, y)
                    .fill(theme.bg)
                    .child(text(state.preedit.clone()).color(theme.text)),
            );
        }
    }
    // Enter and Escape answer the newest, drawn last, so it carries the key hint.
    for (index, confirm) in state.confirms.iter().enumerate() {
        let (key, message, action) = match confirm {
            Confirm::Paste { .. } => (
                "paste-confirm",
                "This paste could run commands".to_owned(),
                "Paste",
            ),
            Confirm::ClipboardWrite { .. } => (
                "clipboard-write-confirm",
                "Replace your clipboard?".to_owned(),
                "Allow",
            ),
            Confirm::ClipboardRead { .. } => (
                "clipboard-read-confirm",
                "Share your clipboard?".to_owned(),
                "Allow",
            ),
            Confirm::Close { .. } => (
                "close-confirm",
                "A program is still running here".to_owned(),
                "Close",
            ),
            Confirm::CloseProject { root } => {
                let name = state
                    .workspace
                    .project_index(root)
                    .and_then(|index| state.workspace.projects.get(index))
                    .map_or("here", |project| &project.name);
                (
                    "close-confirm",
                    format!("A program is still running in {name}"),
                    "Close",
                )
            }
        };
        let message = if index + 1 == state.confirms.len() {
            format!("{message} · Enter / Esc")
        } else {
            message
        };
        let y = 12.0 + index as f32 * CONFIRM_BAR_PITCH;
        overlays.push(confirm_bar(
            theme,
            key,
            message,
            action,
            y,
            move |state, tx, confirmed| resolve_confirm(state, index, confirmed, tx),
        ));
    }
    overlays.extend(find::view(state, theme));
    if let Some((_, tab)) = state.workspace.focused_tab()
        && tab.status != Status::Running
    {
        overlays.push(
            row()
                .key("restart-hint")
                .absolute(12.0, 12.0)
                .px(10.0)
                .py(6.0)
                .radius(6.0)
                .fill(theme.card)
                .child(
                    text(format!("{} · Enter restarts", status_text(&tab.status)))
                        .color(theme.text),
                ),
        );
    }
    overlays
}

fn tray_bar(project: &Project, theme: &tok::Theme, accent: Color) -> Option<El> {
    (!project.tray.is_empty()).then(|| {
        let chips = project.tray.iter().enumerate().map(|(index, tab)| {
            let key = format!("chip-{index}");
            let open = project.drawer_shows(index);
            let session = tab.session.id();
            pill(
                theme,
                key.clone(),
                tab.label(),
                open,
                Some(status_dot(theme, tab)),
            )
            .children(progress_bar(theme, accent, tab.progress))
            .max_w(TAB_MAX_WIDTH)
            .on_press(move |state: &mut State| state.workspace.toggle_drawer(index))
            .on_secondary_press(move |_: &mut State, _, tx: &mut Tx| {
                tx.popup_menu(tray_menu(), &key, move |state: &mut State, picked, _| {
                    let index = (state.workspace.project()).and_then(|project| {
                        (project.tray.iter()).position(|tab| tab.session.id() == session)
                    });
                    if let Some(index) = index {
                        tray_action(state, index, picked);
                    }
                });
            })
        });
        row()
            .key("tray-bar")
            .w_full()
            .h(TRAY_BAR_HEIGHT)
            .gap(4.0)
            .px(8.0)
            .items_center()
            .fill(theme.card)
            .children(chips)
            .child(
                pill(theme, "tray-new".to_owned(), "+", false, None)
                    .on_press_tx(|state: &mut State, tx: &mut Tx| open_palette(state, tx)),
            )
    })
}

/// Stays in the tree with the last tray process it showed, so it can slide out as well as in.
fn drawer(
    state: &State,
    theme: &tok::Theme,
    project: &Project,
    has_tray_bar: bool,
    overlays: Vec<El>,
) -> El {
    let open = project.drawer_tab().is_some();
    let header = project.tray.get(project.drawer).map(|tab| {
        let index = project.drawer;
        let action = |key: &str, label: &str, picked: &'static str| {
            pill(theme, key.to_owned(), label, false, None).on_press(move |state: &mut State| {
                tray_action(state, index, Some(picked));
            })
        };
        row()
            .w_full()
            .h(TRAY_BAR_HEIGHT)
            .gap(8.0)
            .px(8.0)
            .items_center()
            .child(status_dot(theme, tab))
            .child(text(tab.label().to_owned()).color(theme.text).truncate())
            .child(text(status_text(&tab.status).to_owned()).color(theme.text_secondary))
            .child(el().grow(1.0))
            .child(action("drawer-restart", "Restart", "restart"))
            .child(action("drawer-stop", "Stop", "stop"))
            .child(
                pill(theme, "drawer-close".to_owned(), "Close", false, None)
                    .on_press(move |state: &mut State| state.workspace.toggle_drawer(index)),
            )
    });
    col()
        .key("drawer")
        .pin(
            Some(0.0),
            None,
            Some(0.0),
            Some(if has_tray_bar { TRAY_BAR_HEIGHT } else { 0.0 }),
        )
        .h(if open { DRAWER_HEIGHT } else { 0.0 })
        .justify_end()
        .clip_rounded(0.0)
        .child(
            col()
                .key("drawer-panel")
                .w_full()
                .h(DRAWER_HEIGHT)
                .fill(theme.card)
                .border(1.0, theme.card_border)
                .children(header)
                .child(
                    terminal_surface(
                        DRAWER_MARK,
                        "drawer-terminal",
                        &state.workspace,
                        Workspace::drawer_session,
                    )
                    .w_full()
                    .grow(1.0)
                    .min_h(0.0)
                    .children(overlays),
                ),
        )
}

fn overlay_layer(state: &State, theme: &tok::Theme, overlays: Vec<El>) -> El {
    col()
        .key("overlay-layer")
        .pin(Some(0.0), Some(TAB_STRIP_HEIGHT), Some(0.0), Some(0.0))
        .padding(OVERLAY_INSET)
        .fill(theme.shadow_soft)
        .on_press(|state: &mut State| state.workspace.hide_overlay())
        .child(
            col()
                .key("overlay-panel")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .padding(OVERLAY_PADDING)
                .radius(10.0)
                .clip_rounded(10.0)
                .fill(theme.card)
                .border(1.0, theme.card_border)
                .shadow(12.0, 40.0, theme.shadow)
                // Keeps clicks inside the panel from reaching the layer, which hides it.
                .on_press(|_: &mut State| {})
                .child(
                    terminal_surface(
                        OVERLAY_MARK,
                        "overlay-terminal",
                        &state.workspace,
                        Workspace::overlay_session,
                    )
                    .w_full()
                    .grow(1.0)
                    .min_h(0.0)
                    .children(overlays),
                ),
        )
}

pub(crate) fn view(state: &State, cx: &mut Cx<State>) -> El {
    let theme = tok::theme(true);
    let accent = cx.accent();
    let mut overlays = Vec::new();
    if state.workspace.projects.is_empty() {
        overlays.push(
            col()
                .key("empty-hint")
                .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .center()
                .child(
                    text(format!(
                        "Open a folder with {}, or drop one here",
                        Command::OpenProject.hint().unwrap_or_default()
                    ))
                    .color(theme.text_secondary),
                ),
        );
    }
    let banners: Vec<El> = [
        config_error_banner(state, &theme),
        full_disk_access_banner(state, &theme),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !banners.is_empty() {
        overlays.push(
            col()
                .key("banners")
                .pin(Some(12.0), None, Some(12.0), Some(12.0))
                .gap(8.0)
                .children(banners),
        );
    }
    let focus_overlays = focus_overlays(state, &theme);
    let project = state.workspace.project();
    let drawer_focused = project.is_some_and(|project| project.drawer_focused);
    let mut drawer_overlays = Vec::new();
    let mut overlay_overlays = Vec::new();
    if project.is_some_and(|project| project.overlay_open) {
        overlay_overlays = focus_overlays;
    } else if drawer_focused {
        drawer_overlays = focus_overlays;
    } else {
        overlays.extend(focus_overlays);
    }
    let tray_bar = project.and_then(|project| tray_bar(project, &theme, accent));
    let drawer =
        project.map(|project| drawer(state, &theme, project, tray_bar.is_some(), drawer_overlays));
    let overlay = project
        .and_then(Project::overlay_tab)
        .map(|_| overlay_layer(state, &theme, overlay_overlays));
    let terminal = col()
        .h_full()
        .grow(1.0)
        .min_w(0.0)
        .child(tab_strip(state, &theme, accent, cx.titlebar_inset()))
        .child(
            terminal_surface(
                TERMINAL_MARK,
                "terminal",
                &state.workspace,
                Workspace::active_session,
            )
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .children(overlays),
        )
        .children(tray_bar)
        .children(drawer)
        .children(overlay);
    let palette = palette::view(state, &theme, &|key, label, active, marker| {
        pill(&theme, key, label, active, marker)
    });
    row()
        .w_full()
        .h_full()
        .fill(theme.bg)
        .child(
            el().key("repaint")
                .absolute(0.0, 0.0)
                .size(0.0, 0.0)
                .live_opacity(&state.repaint.live),
        )
        .on_drop_files(|state: &mut State, paths: &[PathBuf], _: &mut Tx| {
            for path in paths.iter().filter(|path| path.is_dir()) {
                open_project(state, path.clone());
            }
        })
        .child(sidebar(state, &theme, accent))
        .child(terminal)
        .children(palette)
}

pub(crate) fn window_title(workspace: &Workspace) -> String {
    workspace.project().map_or_else(
        || "Benches".to_owned(),
        |project| format!("{} — Benches", project.name),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::workspace_state;

    #[test]
    fn the_window_title_names_the_active_project() {
        assert_eq!(window_title(&Workspace::default()), "Benches");
        let mut workspace = workspace_state().workspace;
        assert_eq!(window_title(&workspace), "benches — Benches");
        workspace.select_project(1);
        assert_eq!(window_title(&workspace), "site — Benches");
    }
}
