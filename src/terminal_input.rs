use std::io::Write;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use libghostty_vt::effect::ClipboardMime;
use libghostty_vt::key::{Key as Vt, Mods as VtMods};
use libghostty_vt::mode::Mode;
use libghostty_vt::point::{Coordinate, Point};
use libghostty_vt::screen::{CellWide, Screen};
use libghostty_vt::selection::gesture::{
    Autoscroll, AutoscrollTickEvent, DragEvent, Geometry, Gesture, PressEvent, ReleaseEvent,
};
use libghostty_vt::terminal::ScrollViewport;
use libghostty_vt::{Terminal, fmt, key, mouse, paste};

use crate::LogErr;
use crate::grid::{Scrollbar, Surface};
use crate::link;

const AUTOSCROLL: Duration = Duration::from_millis(50);
const MULTI_CLICK: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, PartialEq)]
pub struct KeyInput {
    pub key: Vt,
    pub mods: VtMods,
    pub consumed: VtMods,
    pub text: Option<String>,
    pub unshifted: char,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerPhase {
    Press,
    Drag,
    Release,
    Move,
}

/// Positions are in points, relative to the terminal surface.
#[derive(Clone, Copy, Debug)]
pub struct PointerInput {
    pub phase: PointerPhase,
    pub button: Option<mouse::Button>,
    pub x: f32,
    pub y: f32,
    pub mods: key::Mods,
}

/// What the pointer over the terminal would do, which picks its cursor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Hover {
    #[default]
    Text,
    /// ⌘ is held over a link, which a click opens.
    Link,
    /// The program tracks the mouse, so a click reaches it instead of selecting.
    Report,
}

#[derive(Clone, Copy, Debug)]
pub struct WheelInput {
    pub dy: f32,
    pub x: f32,
    pub y: f32,
    pub mods: key::Mods,
}

/// Moves whole lines out of `remainder`, keeping the fraction for the next delta.
fn whole_lines(remainder: &mut f32, delta: f32) -> i32 {
    *remainder += delta;
    let lines = remainder.trunc();
    *remainder -= lines;
    lines as i32
}

pub(crate) fn open_link(url: &str) {
    let link = link::target(url);
    thread::spawn(move || crate::platform::open(&link).log_err("open link"));
}

/// False if libghostty refused the text because it could run commands.
pub(crate) fn paste_text(
    terminal: &mut Terminal<'static, '_>,
    text: &str,
    allow_unsafe: bool,
) -> Result<bool> {
    let mimes = [ClipboardMime::new(b"text/plain")];
    let options = paste::Options::new().with_allow_unsafe(allow_unsafe);
    match terminal.paste(options, &mimes, |_, out: &mut dyn Write| {
        out.write_all(text.as_bytes())
    }) {
        Ok(_) => {
            terminal.scroll_viewport(ScrollViewport::Bottom);
            Ok(true)
        }
        Err(libghostty_vt::Error::Rejected) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn selection_text(terminal: &Terminal<'static, '_>) -> Result<Option<String>> {
    let Some(mut formatter) = fmt::Formatter::new_selection(terminal)? else {
        return Ok(None);
    };
    formatter.set_format(fmt::Format::Plain)?.set_unwrap(true)?;
    let bytes = formatter.format_alloc(None)?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Turns raw input into terminal bytes or selection changes. Lives with the `Terminal` on its actor.
pub(crate) struct TerminalInput {
    key_encoder: key::Encoder<'static>,
    key_event: key::Event<'static>,
    mouse_encoder: mouse::Encoder<'static>,
    mouse_event: mouse::Event<'static>,
    gesture: Gesture<'static>,
    press: PressEvent<'static>,
    drag: DragEvent<'static>,
    release: ReleaseEvent<'static>,
    autoscroll_tick: AutoscrollTickEvent<'static>,
    started: Instant,
    pub(crate) surface: Option<Surface>,
    pub(crate) selecting: bool,
    pressed: Option<mouse::Button>,
    position: (f32, f32),
    pub(crate) next_autoscroll: Option<Instant>,
    wheel_remainder: f32,
    /// How far below the thumb's top the pointer holds it, while dragging the scrollbar.
    pub(crate) scrollbar_grab: Option<f32>,
    /// Set by `select` for the actor to take, as libghostty doesn't mark the render state dirty.
    pub(crate) selection_changed: bool,
    hover: Hover,
    /// The viewport cells, `start..end` counted row by row, of the link ⌘ is over.
    pub(crate) link: Option<(usize, usize)>,
}

impl TerminalInput {
    pub(crate) fn new() -> Result<Self> {
        let mut press = PressEvent::new()?;
        press.set_repeat_interval(MULTI_CLICK)?;
        Ok(Self {
            key_encoder: key::Encoder::new()?,
            key_event: key::Event::new()?,
            mouse_encoder: mouse::Encoder::new()?,
            mouse_event: mouse::Event::new()?,
            gesture: Gesture::new()?,
            press,
            drag: DragEvent::new()?,
            release: ReleaseEvent::new()?,
            autoscroll_tick: AutoscrollTickEvent::new()?,
            started: Instant::now(),
            surface: None,
            selecting: false,
            pressed: None,
            position: (0.0, 0.0),
            next_autoscroll: None,
            wheel_remainder: 0.0,
            scrollbar_grab: None,
            selection_changed: false,
            hover: Hover::Text,
            link: None,
        })
    }

    /// Ends any press, drag or selection, whose release would go to whichever session is shown next.
    /// Returns the release report for a button the program saw pressed, so it isn't left held.
    pub(crate) fn cancel_pointer(&mut self, terminal: &Terminal<'static, '_>) -> Result<Vec<u8>> {
        self.selecting = false;
        self.next_autoscroll = None;
        self.scrollbar_grab = None;
        match (self.pressed.take(), self.surface) {
            (Some(button), Some(surface)) => self.report(
                terminal,
                surface,
                mouse::Action::Release,
                Some(button),
                key::Mods::empty(),
            ),
            _ => Ok(Vec::new()),
        }
    }

    fn select(
        &mut self,
        terminal: &Terminal<'static, '_>,
        selection: Option<&libghostty_vt::selection::Selection<'_>>,
    ) -> Result<()> {
        terminal.set_selection(selection)?;
        self.selection_changed = true;
        Ok(())
    }

    /// The output of the command above the cursor's prompt, as marked by OSC 133.
    pub(crate) fn select_last_output(&mut self, terminal: &Terminal<'static, '_>) -> Result<()> {
        let cursor_row = terminal.scrollback_rows()? as u32 + u32::from(terminal.cursor_y()?);
        // The prompt rows themselves select nothing, so look upwards for the output.
        for y in (0..=cursor_row).rev() {
            let grid_ref = terminal.grid_ref(Point::Screen(Coordinate { x: 0, y }))?;
            if let Some(output) = terminal.select_output(grid_ref)? {
                return self.select(terminal, Some(&output));
            }
        }
        Ok(())
    }

    fn encode_key(&mut self, terminal: &Terminal<'static, '_>, input: KeyInput) -> Result<Vec<u8>> {
        self.key_event
            .set_action(key::Action::Press)
            .set_key(input.key)
            .set_mods(input.mods)
            .set_consumed_mods(input.consumed)
            .set_unshifted_codepoint(input.unshifted)
            .set_utf8(input.text);
        let mut encoded = Vec::new();
        self.key_encoder
            .set_options_from_terminal(terminal)
            .set_macos_option_as_alt(key::OptionAsAlt::True)
            .encode_to_vec(&self.key_event, &mut encoded)?;
        Ok(encoded)
    }

    pub(crate) fn key(
        &mut self,
        terminal: &mut Terminal<'static, '_>,
        input: KeyInput,
    ) -> Result<Vec<u8>> {
        let encoded = self.encode_key(terminal, input)?;
        if !encoded.is_empty() {
            terminal.scroll_viewport(ScrollViewport::Bottom);
        }
        Ok(encoded)
    }

    fn viewport_point(surface: Surface, (x, y): (f32, f32)) -> Coordinate {
        let metrics = surface.metrics;
        let origin = metrics.origin([0.0; 4]);
        let cell = |position: f32, origin: f32, size: u16, count: u16| {
            ((position - origin) / f32::from(size)).clamp(0.0, f32::from(count.saturating_sub(1)))
        };
        Coordinate {
            x: cell(x, origin.0, metrics.cell_width, surface.cols) as u16,
            y: cell(y, origin.1, metrics.cell_height, surface.rows) as u32,
        }
    }

    fn geometry(surface: Surface) -> Geometry {
        let metrics = surface.metrics;
        let padding = metrics.origin([0.0; 4]).0 as u32;
        Geometry {
            columns: surface.cols.into(),
            cell_width: metrics.cell_width.into(),
            padding_left: padding,
            screen_height: u32::from(surface.rows) * u32::from(metrics.cell_height) + 2 * padding,
        }
    }

    fn report(
        &mut self,
        terminal: &Terminal<'static, '_>,
        surface: Surface,
        action: mouse::Action,
        button: Option<mouse::Button>,
        mods: key::Mods,
    ) -> Result<Vec<u8>> {
        let Geometry {
            columns,
            cell_width,
            padding_left: padding,
            screen_height,
            ..
        } = Self::geometry(surface);
        self.mouse_encoder
            .set_options_from_terminal(terminal)
            .set_size(mouse::EncoderSize {
                screen_width: columns * cell_width + 2 * padding,
                screen_height,
                cell_width,
                cell_height: surface.metrics.cell_height.into(),
                padding_top: padding,
                padding_bottom: padding,
                padding_right: padding,
                padding_left: padding,
            })?
            .set_any_button_pressed(self.pressed.is_some())
            .set_track_last_cell(true);
        self.mouse_event
            .set_action(action)
            .set_button(button)
            .set_mods(mods)
            .set_position(mouse::Position {
                x: self.position.0,
                y: self.position.1,
            });
        let mut encoded = Vec::new();
        self.mouse_encoder
            .encode_to_vec(&self.mouse_event, &mut encoded)?;
        Ok(encoded)
    }

    fn schedule_autoscroll(
        &mut self,
        terminal: &Terminal<'static, '_>,
        now: Instant,
    ) -> Result<()> {
        self.next_autoscroll = match self.gesture.autoscroll(terminal)? {
            Autoscroll::None => None,
            _ => Some(now + AUTOSCROLL),
        };
        Ok(())
    }

    /// Drags the thumb, or on a press off it, centres the thumb under the pointer as macOS does.
    /// `y` is in points from the top of the terminal surface.
    pub(crate) fn drag_scrollbar(
        &mut self,
        terminal: &mut Terminal<'static, '_>,
        phase: PointerPhase,
        y: f32,
    ) -> Result<()> {
        if phase == PointerPhase::Release {
            self.scrollbar_grab = None;
        }
        let Some(surface) = self.surface else {
            return Ok(());
        };
        if !matches!(phase, PointerPhase::Press | PointerPhase::Drag) {
            return Ok(());
        }
        let Some(scrollbar) = Scrollbar::of(terminal)? else {
            return Ok(());
        };
        let metrics = surface.metrics;
        let (track, min) = Scrollbar::track(
            surface.rows.into(),
            metrics.cell_height.into(),
            metrics.scale,
        );
        let y = y * metrics.scale - metrics.origin([0.0; 4]).1;
        let grab = match (phase, self.scrollbar_grab) {
            (PointerPhase::Drag, Some(grab)) => grab,
            (PointerPhase::Drag, None) => return Ok(()),
            _ => {
                let (top, height) = scrollbar.thumb(track, min);
                if (top..top + height).contains(&y) {
                    y - top
                } else {
                    height / 2.0
                }
            }
        };
        self.scrollbar_grab = Some(grab);
        terminal.scroll_viewport(ScrollViewport::Row(scrollbar.row_at(y - grab, track, min)));
        Ok(())
    }

    /// The new hover at the last pointer position, when it changed.
    pub(crate) fn hover(
        &mut self,
        terminal: &Terminal<'static, '_>,
        mods: key::Mods,
    ) -> Result<Option<Hover>> {
        let Some(surface) = self.surface else {
            return Ok(None);
        };
        self.link = if mods.contains(key::Mods::SUPER) {
            let position = Self::viewport_point(surface, self.position);
            Self::link_at(terminal, surface, position)?.map(|(cells, _)| cells)
        } else {
            None
        };
        let hover = if self.link.is_some() {
            Hover::Link
        } else if terminal.is_mouse_tracking()? && !mods.contains(key::Mods::SHIFT) {
            Hover::Report
        } else {
            Hover::Text
        };
        Ok((hover != self.hover).then(|| {
            self.hover = hover;
            hover
        }))
    }

    /// The link a ⌘-click starts on. The actor opens it instead of passing the press on, so it
    /// neither selects nor reaches a mouse-tracking program.
    pub(crate) fn link_under(
        &self,
        terminal: &Terminal<'static, '_>,
        input: PointerInput,
    ) -> Result<Option<String>> {
        let Some(surface) = self.surface else {
            return Ok(None);
        };
        if input.phase != PointerPhase::Press
            || input.button != Some(mouse::Button::Left)
            || !input.mods.contains(key::Mods::SUPER)
        {
            return Ok(None);
        }
        let scale = surface.metrics.scale;
        let position = Self::viewport_point(surface, (input.x * scale, input.y * scale));
        Ok(Self::link_at(terminal, surface, position)?.map(|(_, uri)| uri))
    }

    /// An OSC 8 hyperlink, or else a URL in the text, which may wrap across rows, with the
    /// viewport cells it covers, `start..end` counted row by row.
    fn link_at(
        terminal: &Terminal<'static, '_>,
        surface: Surface,
        position: Coordinate,
    ) -> Result<Option<((usize, usize), String)>> {
        let columns = usize::from(surface.cols);
        let index = |x: u16, y: u32| y as usize * columns + usize::from(x);
        let grid_ref = |x, y| terminal.grid_ref(Point::Viewport(Coordinate { x, y }));
        let uri_at = |at: usize| -> Result<Vec<u8>> {
            let mut uri = [0; 4096];
            let x = u16::try_from(at % columns)?;
            let length = grid_ref(x, u32::try_from(at / columns)?)?.hyperlink_uri(&mut uri)?;
            Ok(uri[..length].to_vec())
        };
        let at = index(position.x, position.y);
        let hyperlink = uri_at(at)?;
        if let Ok(uri) = std::str::from_utf8(&hyperlink)
            && link::openable(uri)
        {
            let cells = columns * usize::from(surface.rows);
            let mut start = at;
            while start > 0 && uri_at(start - 1)? == hyperlink {
                start -= 1;
            }
            let mut end = at + 1;
            while end < cells && uri_at(end)? == hyperlink {
                end += 1;
            }
            return Ok(Some(((start, end), uri.to_owned())));
        }
        let mut first = position.y;
        while first > 0 && grid_ref(0, first)?.row()?.is_wrap_continuation()? {
            first -= 1;
        }
        let mut row = Vec::new();
        // The viewport index of each character in `row`.
        let mut indices = Vec::new();
        let mut column = 0;
        let mut y = first;
        loop {
            for x in 0..surface.cols {
                let cell = grid_ref(x, y)?;
                let spacer = matches!(
                    cell.cell()?.wide()?,
                    CellWide::SpacerTail | CellWide::SpacerHead
                );
                if (x, y) == (position.x, position.y) {
                    column = row.len().saturating_sub(usize::from(spacer));
                }
                if spacer {
                    continue;
                }
                let mut character = [' '];
                row.push(match cell.graphemes(&mut character) {
                    Ok(_) => character[0],
                    // A cluster of several codepoints; any non-delimiter stands in for it.
                    Err(libghostty_vt::Error::OutOfSpace { .. }) => '\u{fffd}',
                    Err(error) => return Err(error.into()),
                });
                indices.push(index(x, y));
            }
            y += 1;
            if y >= surface.rows.into() || !grid_ref(0, y - 1)?.row()?.is_wrapped()? {
                break;
            }
        }
        Ok(link::url_at(&row, column).and_then(|(span, url)| {
            let start = *indices.get(span.start)?;
            let end = match indices.get(span.end) {
                Some(&next) => next,
                None => indices.last()? + 1,
            };
            Some(((start, end), url))
        }))
    }

    pub(crate) fn pointer(
        &mut self,
        terminal: &Terminal<'static, '_>,
        input: PointerInput,
    ) -> Result<Vec<u8>> {
        let Some(surface) = self.surface else {
            return Ok(Vec::new());
        };
        let scale = surface.metrics.scale;
        self.position = (input.x * scale, input.y * scale);
        let grid_ref_point = Point::Viewport(Self::viewport_point(surface, self.position));
        let to_report = match input.phase {
            PointerPhase::Press => {
                let reporting =
                    terminal.is_mouse_tracking()? && !input.mods.contains(key::Mods::SHIFT);
                self.selecting = !reporting && input.button == Some(mouse::Button::Left);
                if self.selecting {
                    let grid_ref = terminal.grid_ref(grid_ref_point)?;
                    let selection = self
                        .press
                        .set_repeat_distance(surface.metrics.cell_width.into())?
                        .set_time(self.started.elapsed())?
                        .set_position(self.position.0.into(), self.position.1.into())?
                        .apply(&mut self.gesture, terminal, grid_ref)?;
                    self.select(terminal, selection.as_ref())?;
                    return Ok(Vec::new());
                }
                if reporting {
                    self.pressed = input.button;
                    Some((mouse::Action::Press, input.button))
                } else {
                    None
                }
            }
            PointerPhase::Drag if self.selecting => {
                let grid_ref = terminal.grid_ref(grid_ref_point)?;
                let selection = self
                    .drag
                    .set_rectangle(input.mods.contains(key::Mods::ALT))?
                    .set_position(self.position.0.into(), self.position.1.into())?
                    .apply(
                        &mut self.gesture,
                        terminal,
                        grid_ref,
                        Self::geometry(surface),
                    )?;
                self.select(terminal, selection.as_ref())?;
                self.schedule_autoscroll(terminal, Instant::now())?;
                return Ok(Vec::new());
            }
            PointerPhase::Drag if self.pressed.is_some() => {
                Some((mouse::Action::Motion, self.pressed))
            }
            PointerPhase::Release if self.selecting => {
                let grid_ref = terminal.grid_ref(grid_ref_point).ok();
                self.release.apply(&mut self.gesture, terminal, grid_ref)?;
                self.selecting = false;
                self.next_autoscroll = None;
                None
            }
            PointerPhase::Release if self.pressed.is_some() => {
                Some((mouse::Action::Release, self.pressed.take()))
            }
            // Rune also sends hover moves during a drag, which would read as the button lifting.
            PointerPhase::Move
                if self.pressed.is_none() && !self.selecting && terminal.is_mouse_tracking()? =>
            {
                Some((mouse::Action::Motion, None))
            }
            _ => None,
        };
        let Some((action, button)) = to_report else {
            return Ok(Vec::new());
        };
        self.report(terminal, surface, action, button, input.mods)
    }

    /// Scrolls one line while a selection drag is held past the edge.
    pub(crate) fn tick_autoscroll(
        &mut self,
        terminal: &Terminal<'static, '_>,
        now: Instant,
    ) -> Result<()> {
        let Some(surface) = self.surface else {
            return Ok(());
        };
        if !self.next_autoscroll.is_some_and(|due| now >= due) {
            return Ok(());
        }
        if matches!(self.gesture.autoscroll(terminal)?, Autoscroll::None) {
            self.next_autoscroll = None;
            return Ok(());
        }
        let viewport = Self::viewport_point(surface, self.position);
        let selection = self.autoscroll_tick.apply(
            &mut self.gesture,
            terminal,
            viewport,
            Self::geometry(surface),
        )?;
        self.select(terminal, selection.as_ref())?;
        self.schedule_autoscroll(terminal, now)
    }

    /// Bytes for the program, or `None` when the wheel scrolled the viewport.
    pub(crate) fn wheel(
        &mut self,
        terminal: &mut Terminal<'static, '_>,
        input: WheelInput,
    ) -> Result<Option<Vec<u8>>> {
        let Some(surface) = self.surface else {
            return Ok(Some(Vec::new()));
        };
        let line_height = f32::from(surface.metrics.cell_height) / surface.metrics.scale;
        let lines = whole_lines(&mut self.wheel_remainder, input.dy / line_height);
        let mut bytes = Vec::new();
        if lines == 0 {
            return Ok(Some(bytes));
        }
        let toward_history = lines > 0;
        if terminal.is_mouse_tracking()? {
            self.position = (
                input.x * surface.metrics.scale,
                input.y * surface.metrics.scale,
            );
            let button = Some(if toward_history {
                mouse::Button::Four
            } else {
                mouse::Button::Five
            });
            for _ in 0..lines.unsigned_abs() {
                for action in [mouse::Action::Press, mouse::Action::Release] {
                    bytes.extend(self.report(terminal, surface, action, button, input.mods)?);
                }
            }
        } else if terminal.active_screen()? == Screen::Alternate
            && terminal.mode(Mode::ALT_SCROLL)?
        {
            let arrow = if toward_history {
                key::Key::ArrowUp
            } else {
                key::Key::ArrowDown
            };
            for _ in 0..lines.unsigned_abs() {
                bytes.extend(self.encode_key(
                    terminal,
                    KeyInput {
                        key: arrow,
                        mods: key::Mods::empty(),
                        consumed: key::Mods::empty(),
                        text: None,
                        unshifted: '\0',
                    },
                )?);
            }
        } else {
            terminal.scroll_viewport(ScrollViewport::Delta(-(lines as isize)));
            return Ok(None);
        }
        Ok(Some(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::new_terminal;
    use crate::grid::{DEFAULT_BG, DEFAULT_FG, GridBuilder, Presentation, rgb};
    use crate::test_support::{
        TEST_SURFACE, THEME_BLUE, THEME_RED, fractional_metrics, pointer_at, scrolled_terminal,
        wheel_by,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    fn input_for_test() -> TerminalInput {
        let mut input = TerminalInput::new().unwrap();
        input.surface = Some(TEST_SURFACE);
        input
    }

    fn top_line(terminal: &Terminal<'static, '_>) -> String {
        let grid = GridBuilder::new()
            .unwrap()
            .build(terminal, 1, Presentation::default())
            .unwrap()
            .unwrap();
        grid.lines()[0].trim_end().to_owned()
    }

    fn command_press(column: u16, row: u16) -> PointerInput {
        PointerInput {
            mods: key::Mods::SUPER,
            ..pointer_at(PointerPhase::Press, column, row)
        }
    }

    #[test]
    fn command_click_finds_a_plain_text_url() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\r\na https://a.te/x. ok");
        let input = input_for_test();
        let link = input.link_under(&terminal, command_press(6, 1)).unwrap();
        assert_eq!(link.as_deref(), Some("https://a.te/x"));
        let off_link = input.link_under(&terminal, command_press(1, 1)).unwrap();
        assert_eq!(off_link, None);
    }

    #[test]
    fn command_click_finds_a_url_containing_wide_characters() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write("http://a.te/日本語/x".as_bytes());
        let input = input_for_test();
        for column in [5, 12, 13, 17, 19] {
            let link = input.link_under(&terminal, command_press(column, 0));
            assert_eq!(
                link.unwrap().as_deref(),
                Some("http://a.te/日本語/x"),
                "at {column}"
            );
        }
    }

    #[test]
    fn command_click_joins_a_url_wrapped_across_rows() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"a https://example.com/long/path.\r\nnext");
        let input = input_for_test();
        for (column, row) in [(5, 0), (2, 1), (19, 0)] {
            let link = input.link_under(&terminal, command_press(column, row));
            assert_eq!(
                link.unwrap().as_deref(),
                Some("https://example.com/long/path"),
                "at {column},{row}"
            );
        }
        let after = input.link_under(&terminal, command_press(1, 2)).unwrap();
        assert_eq!(after, None);
    }

    #[test]
    fn hover_reports_only_changes_over_a_link_with_command_held() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"a https://a.te/x ok");
        let mut input = input_for_test();
        let move_to = |input: &mut TerminalInput, column| {
            let hover = pointer_at(PointerPhase::Move, column, 0);
            input.pointer(&terminal, hover).unwrap();
        };
        let none = key::Mods::empty();
        let command = key::Mods::SUPER;
        move_to(&mut input, 5);
        assert_eq!(input.hover(&terminal, none).unwrap(), None);
        assert_eq!(input.hover(&terminal, command).unwrap(), Some(Hover::Link));
        assert_eq!(input.hover(&terminal, command).unwrap(), None);
        move_to(&mut input, 0);
        assert_eq!(input.hover(&terminal, command).unwrap(), Some(Hover::Text));
        move_to(&mut input, 5);
        assert_eq!(input.hover(&terminal, command).unwrap(), Some(Hover::Link));
        assert_eq!(input.hover(&terminal, none).unwrap(), Some(Hover::Text));
    }

    #[test]
    fn command_hover_spans_the_cells_of_the_link_under_it() {
        let span_at = |text: &[u8], column, row| {
            let mut terminal = new_terminal(20, 5).unwrap();
            terminal.vt_write(text);
            let mut input = input_for_test();
            input
                .pointer(&terminal, pointer_at(PointerPhase::Move, column, row))
                .unwrap();
            input.hover(&terminal, key::Mods::SUPER).unwrap();
            let command = input.link;
            input.hover(&terminal, key::Mods::empty()).unwrap();
            assert_eq!(input.link, None, "released");
            command
        };
        assert_eq!(span_at(b"a https://a.te/x ok", 5, 0), Some((2, 16)));
        assert_eq!(span_at(b"a https://a.te/x ok", 0, 0), None);
        // Wraps from row 0, column 15 onto row 1.
        let wrapped = b"abcdefghijklmn https://a.te/xyz/long ok";
        assert_eq!(span_at(wrapped, 2, 1), Some((15, 36)));
        let osc8 = b"\x1b]8;;https://b.te\x1b\\click here\x1b]8;;\x1b\\ x";
        assert_eq!(span_at(osc8, 7, 0), Some((0, 10)));
    }

    #[test]
    fn hover_reports_while_tracking_unless_shift_is_held() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"a https://a.te/x ok");
        let mut input = input_for_test();
        input
            .pointer(&terminal, pointer_at(PointerPhase::Move, 5, 0))
            .unwrap();
        terminal.vt_write(b"\x1b[?1000h");
        let none = key::Mods::empty();
        assert_eq!(input.hover(&terminal, none).unwrap(), Some(Hover::Report));
        let shift = key::Mods::SHIFT;
        assert_eq!(input.hover(&terminal, shift).unwrap(), Some(Hover::Text));
        let command = key::Mods::SUPER;
        assert_eq!(input.hover(&terminal, command).unwrap(), Some(Hover::Link));
        terminal.vt_write(b"\x1b[?1000l");
        assert_eq!(input.hover(&terminal, none).unwrap(), Some(Hover::Text));
    }

    #[test]
    fn command_click_prefers_an_osc_8_hyperlink() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\x1b]8;;https://osc.test/x\x1b\\click me\x1b]8;;\x1b\\");
        let input = input_for_test();
        let link = input.link_under(&terminal, command_press(1, 0)).unwrap();
        assert_eq!(link.as_deref(), Some("https://osc.test/x"));
    }

    #[test]
    fn command_click_ignores_hyperlinks_with_other_schemes() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\x1b]8;;ssh://host\x1b\\click me\x1b]8;;\x1b\\");
        let input = input_for_test();
        let link = input.link_under(&terminal, command_press(1, 0)).unwrap();
        assert_eq!(link, None);
    }

    #[test]
    fn only_a_command_left_press_opens_a_link() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"https://example.com");
        let input = input_for_test();
        let plain = pointer_at(PointerPhase::Press, 3, 0);
        assert_eq!(input.link_under(&terminal, plain).unwrap(), None);
        let release = PointerInput {
            phase: PointerPhase::Release,
            ..command_press(3, 0)
        };
        assert_eq!(input.link_under(&terminal, release).unwrap(), None);
        let right = PointerInput {
            button: Some(mouse::Button::Right),
            ..command_press(3, 0)
        };
        assert_eq!(input.link_under(&terminal, right).unwrap(), None);
    }

    #[test]
    fn a_press_reports_to_a_tracking_program() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\x1b[?1000h\x1b[?1006h");
        let mut input = input_for_test();
        let press = input
            .pointer(&terminal, pointer_at(PointerPhase::Press, 2, 1))
            .unwrap();
        assert_eq!(press, b"\x1b[<0;3;2M");
        let release = input
            .pointer(&terminal, pointer_at(PointerPhase::Release, 2, 1))
            .unwrap();
        assert_eq!(release, b"\x1b[<0;3;2m");
    }

    #[test]
    fn cancelling_a_held_press_reports_its_release() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\x1b[?1000h\x1b[?1006h");
        let mut input = input_for_test();
        input
            .pointer(&terminal, pointer_at(PointerPhase::Press, 2, 1))
            .unwrap();
        assert_eq!(input.cancel_pointer(&terminal).unwrap(), b"\x1b[<0;3;2m");
        assert_eq!(input.cancel_pointer(&terminal).unwrap(), b"");
    }

    /// Rune reports a hover move alongside every drag move, and `main.rs` forwards both.
    fn report_drag(
        terminal: &Terminal<'static, '_>,
        input: &mut TerminalInput,
        mods: key::Mods,
    ) -> Vec<u8> {
        let events = [
            (PointerPhase::Press, 2),
            (PointerPhase::Move, 3),
            (PointerPhase::Drag, 3),
            (PointerPhase::Move, 3),
            (PointerPhase::Release, 3),
        ];
        let mut bytes = Vec::new();
        for (phase, column) in events {
            let mut pointer = pointer_at(phase, column, 1);
            pointer.mods = mods;
            if phase == PointerPhase::Move {
                pointer.button = None;
            }
            bytes.extend(input.pointer(terminal, pointer).unwrap());
        }
        bytes
    }

    #[test]
    fn a_drag_reports_no_buttonless_motion_to_any_motion_tracking() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\x1b[?1003h\x1b[?1006h");
        let bytes = report_drag(&terminal, &mut input_for_test(), key::Mods::empty());
        assert_eq!(bytes, b"\x1b[<0;3;2M\x1b[<32;4;2M\x1b[<0;4;2m");
    }

    #[test]
    fn a_shift_drag_selects_without_reporting_motion() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\x1b[?1003h\x1b[?1006h");
        let bytes = report_drag(&terminal, &mut input_for_test(), key::Mods::SHIFT);
        assert_eq!(bytes, b"");
    }

    #[test]
    fn shift_selects_instead_of_reporting() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\x1b[?1000h\x1b[?1006h");
        let mut input = input_for_test();
        let mut press = pointer_at(PointerPhase::Press, 2, 1);
        press.mods = key::Mods::SHIFT;
        let bytes = input.pointer(&terminal, press).unwrap();
        assert!(bytes.is_empty() && input.selection_changed);
    }

    fn select_hello(terminal: &Terminal<'static, '_>, input: &mut TerminalInput) {
        input
            .pointer(terminal, pointer_at(PointerPhase::Press, 0, 0))
            .unwrap();
        // A cell only joins the selection once the pointer is most of the way across it.
        let mut drag = pointer_at(PointerPhase::Drag, 4, 0);
        drag.x = 49.0;
        input.pointer(terminal, drag).unwrap();
        input
            .pointer(terminal, pointer_at(PointerPhase::Release, 4, 0))
            .unwrap();
    }

    #[test]
    fn dragging_selects_text_that_can_be_copied() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"hello world");
        select_hello(&terminal, &mut input_for_test());
        assert_eq!(selection_text(&terminal).unwrap().as_deref(), Some("hello"));
    }

    pub(crate) const TWO_COMMANDS: &[u8] =
        b"\x1b]133;A\x07$ \x1b]133;B\x07one\r\n\x1b]133;C\x07out1\r\n\
        \x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07two\r\n\x1b]133;C\x07out2a\r\nout2b\r\n\
        \x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07";

    #[test]
    fn the_last_commands_output_can_be_selected() {
        let mut terminal = new_terminal(20, 8).unwrap();
        terminal.vt_write(TWO_COMMANDS);
        let mut input = input_for_test();
        input.select_last_output(&terminal).unwrap();
        assert!(input.selection_changed);
        assert_eq!(
            selection_text(&terminal).unwrap().as_deref(),
            Some("out2a\nout2b")
        );
    }

    #[test]
    fn without_prompt_marks_no_output_is_selected() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"plain\r\n");
        let mut input = input_for_test();
        input.select_last_output(&terminal).unwrap();
        assert!(!input.selection_changed);
        assert_eq!(selection_text(&terminal).unwrap(), None);
    }

    #[test]
    fn with_no_selection_there_is_nothing_to_copy() {
        let terminal = new_terminal(20, 5).unwrap();
        assert_eq!(selection_text(&terminal).unwrap(), None);
    }

    #[test]
    fn dragging_past_the_top_scrolls_one_line_per_tick() {
        let terminal = scrolled_terminal();
        let mut input = input_for_test();
        let before = top_line(&terminal);
        input
            .pointer(&terminal, pointer_at(PointerPhase::Press, 0, 2))
            .unwrap();
        let mut drag = pointer_at(PointerPhase::Drag, 0, 0);
        drag.y = -30.0;
        input.pointer(&terminal, drag).unwrap();
        let due = input
            .next_autoscroll
            .expect("autoscroll should be scheduled");
        input.selection_changed = false;
        input.tick_autoscroll(&terminal, due - AUTOSCROLL).unwrap();
        assert!(!input.selection_changed);
        input.tick_autoscroll(&terminal, due).unwrap();
        assert!(input.selection_changed);
        let number = |line: &str| line.trim_start_matches("line").parse::<u32>().unwrap();
        assert_eq!(number(&before) - number(&top_line(&terminal)), 1);
    }

    #[test]
    fn pressing_the_track_and_dragging_the_thumb_scrolls() {
        let mut terminal = scrolled_terminal();
        let mut input = input_for_test();
        input
            .drag_scrollbar(&mut terminal, PointerPhase::Press, 0.0)
            .unwrap();
        assert_eq!(top_line(&terminal), "line0");
        input
            .drag_scrollbar(&mut terminal, PointerPhase::Drag, 100.0)
            .unwrap();
        assert_eq!(top_line(&terminal), "line26");
        input
            .drag_scrollbar(&mut terminal, PointerPhase::Release, 100.0)
            .unwrap();
        assert_eq!(input.scrollbar_grab, None);
        input
            .drag_scrollbar(&mut terminal, PointerPhase::Drag, 0.0)
            .unwrap();
        assert_eq!(top_line(&terminal), "line26");
    }

    #[test]
    fn a_scrollbar_drag_holds_until_release() {
        let mut terminal = scrolled_terminal();
        let mut input = input_for_test();
        input
            .drag_scrollbar(&mut terminal, PointerPhase::Press, 50.0)
            .unwrap();
        assert!(input.scrollbar_grab.is_some());
        input
            .drag_scrollbar(&mut terminal, PointerPhase::Release, 50.0)
            .unwrap();
        assert!(input.scrollbar_grab.is_none());
    }

    #[test]
    fn wheel_deltas_keep_their_fraction_between_events() {
        let mut remainder = 0.0;
        assert_eq!(whole_lines(&mut remainder, 0.6), 0);
        assert_eq!(whole_lines(&mut remainder, 0.6), 1);
        assert_eq!(whole_lines(&mut remainder, -2.2), -2);
    }

    #[test]
    fn the_wheel_scrolls_scrollback_on_the_primary_screen() {
        let mut terminal = scrolled_terminal();
        let before = top_line(&terminal);
        let bytes = input_for_test()
            .wheel(&mut terminal, wheel_by(60.0))
            .unwrap();
        assert_eq!(bytes, None);
        assert_ne!(top_line(&terminal), before);
    }

    #[test]
    fn the_wheel_sends_arrows_on_the_alternate_screen() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\x1b[?1049h");
        let bytes = input_for_test()
            .wheel(&mut terminal, wheel_by(40.0))
            .unwrap()
            .unwrap();
        assert_eq!(bytes, b"\x1b[A\x1b[A");
    }

    #[test]
    fn the_wheel_reports_buttons_to_a_tracking_program() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"\x1b[?1000h\x1b[?1006h");
        let bytes = input_for_test()
            .wheel(&mut terminal, wheel_by(20.0))
            .unwrap()
            .unwrap();
        assert!(bytes.starts_with(b"\x1b[<64;1;1M"), "{bytes:?}");
    }

    #[test]
    fn typing_returns_the_viewport_to_the_bottom() {
        let mut terminal = scrolled_terminal();
        let bottom = top_line(&terminal);
        let mut input = input_for_test();
        input.wheel(&mut terminal, wheel_by(60.0)).unwrap();
        assert_ne!(top_line(&terminal), bottom);
        let typed = KeyInput {
            key: Vt::Unidentified,
            mods: VtMods::empty(),
            consumed: VtMods::empty(),
            text: Some("a".to_owned()),
            unshifted: '\0',
        };
        input.key(&mut terminal, typed).unwrap();
        assert_eq!(top_line(&terminal), bottom);
    }

    fn terminal_capturing_replies<'cb>() -> (Terminal<'static, 'cb>, Rc<RefCell<Vec<u8>>>) {
        let mut terminal = new_terminal(20, 5).unwrap();
        let written = Rc::new(RefCell::new(Vec::new()));
        let sink = written.clone();
        terminal
            .on_pty_write(move |_, data| sink.borrow_mut().extend_from_slice(data))
            .unwrap();
        (terminal, written)
    }

    #[test]
    fn a_bracketed_paste_is_framed_and_allowed_to_hold_newlines() {
        let (mut terminal, written) = terminal_capturing_replies();
        terminal.vt_write(b"\x1b[?2004h");
        assert!(paste_text(&mut terminal, "a\nb", false).unwrap());
        assert_eq!(written.borrow().as_slice(), b"\x1b[200~a\nb\x1b[201~");
    }

    #[test]
    fn an_unbracketed_newline_is_refused_until_allowed() {
        let (mut terminal, written) = terminal_capturing_replies();
        assert!(!paste_text(&mut terminal, "rm -rf x\n", false).unwrap());
        assert!(written.borrow().is_empty());
        assert!(paste_text(&mut terminal, "rm -rf x\n", true).unwrap());
        assert_eq!(written.borrow().as_slice(), b"rm -rf x\r");
    }

    #[test]
    fn hit_testing_finds_the_cell_drawn_at_any_scale() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let metrics = fractional_metrics(scale);
            let surface = Surface::fit((700.0, 500.0), metrics);
            let origin = metrics.origin([0.0; 4]);
            for (col, row) in [(0, 0), (1, 3), (surface.cols - 1, surface.rows - 1)] {
                let [left, top, width, height] = metrics.cell_rect(origin, col, row);
                for (x, y) in [(left, top), (left + width / 2.0, top + height / 2.0)] {
                    let point = TerminalInput::viewport_point(surface, (x, y));
                    assert_eq!((point.x, u16::try_from(point.y)), (col, Ok(row)), "{scale}");
                }
                let just_before = TerminalInput::viewport_point(surface, (left - 0.01, top - 0.01));
                assert_eq!(just_before.x, col.saturating_sub(1), "{scale}");
            }
        }
    }

    #[test]
    fn selected_cells_swap_their_colours() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"hello world");
        let mut builder = GridBuilder::new().unwrap();
        builder
            .build(&terminal, 1, Presentation::default())
            .unwrap();
        select_hello(&terminal, &mut input_for_test());
        builder.selection_changed();
        let grid = builder
            .build(&terminal, 2, Presentation::default())
            .unwrap()
            .unwrap();
        assert_eq!(grid.cells[0].bg, Some(rgb(DEFAULT_FG)));
        assert_eq!(grid.cells[0].fg, rgb(DEFAULT_BG));
        assert_eq!(grid.cells[6].bg, None);
        assert!(grid.cells[0].selected && grid.cells[4].selected);
        assert!(!grid.cells[5].selected && !grid.cells[6].selected);
    }

    #[test]
    fn theme_selection_colours_replace_the_swap() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"hello world");
        let mut builder = GridBuilder::new().unwrap();
        builder.selection_background = Some(THEME_RED);
        builder.selection_foreground = Some(THEME_BLUE);
        select_hello(&terminal, &mut input_for_test());
        let grid = builder
            .build(&terminal, 1, Presentation::default())
            .unwrap()
            .unwrap();
        assert_eq!(grid.cells[0].bg, Some(rgb(THEME_RED)));
        assert_eq!(grid.cells[0].fg, rgb(THEME_BLUE));
        assert_eq!(grid.cells[6].bg, None);
    }

    #[test]
    fn the_plain_fast_path_reads_the_same_cells_as_reading_each_one() {
        let vt = concat!(
            "he\x1b[1;31mll\x1b[0m\u{4e16}o plain\r\n",
            "\x1b[1mbold\x1b[3mital\x1b[2mfaint\x1b[22;7minverse\x1b[27;8minvisible\x1b[0m\r\n",
            "\x1b[4:3;58;2;1;2;3munder\x1b[0m \x1b[9mstrike\x1b[53mover\x1b[0m\r\n",
            "\x1b[48;5;4m   \x1b[48;2;9;8;7m   \x1b[0m\x1b[44mblue bg\x1b[0m\x1b[2K\r\n",
            "\u{4e16}\u{754c}\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} e\u{301}\u{2764}\u{fe0f}x\r\n",
            "\x1b[38;2;10;20;30mrgb \x1b[38;5;200mpalette\x1b[0m\x1b[K",
        );
        for selection_colours in [None, Some((THEME_RED, THEME_BLUE))] {
            for selected in [false, true] {
                let mut terminal = new_terminal(40, 8).unwrap();
                terminal.vt_write(vt.as_bytes());
                if selected {
                    select_hello(&terminal, &mut input_for_test());
                }
                let build = |plain_fast_path| {
                    let mut builder = GridBuilder::new().unwrap();
                    builder.plain_fast_path = plain_fast_path;
                    builder.selection_background = selection_colours.map(|colours| colours.0);
                    builder.selection_foreground = selection_colours.map(|colours| colours.1);
                    builder
                        .build(&terminal, 1, Presentation::default())
                        .unwrap()
                        .unwrap()
                };
                let (fast, slow) = (build(true), build(false));
                assert_eq!(fast.cells, slow.cells);
                assert_eq!(fast.clusters, slow.clusters);
                assert!(fast.cells.iter().any(|cell| cell.wide));
                assert!(fast.cells.iter().any(|cell| cell.spacer));
                assert!(fast.cells.iter().any(|cell| cell.cluster.is_some()));
                assert!(fast.cells.iter().any(|cell| cell.bg.is_some()));
            }
        }
    }
}
