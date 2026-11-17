use libghostty_vt::color::Rgb;
use libghostty_vt::{Terminal, key, mouse};

use crate::grid::{Metrics, Surface, new_terminal};
use crate::terminal_input::{PointerInput, PointerPhase, WheelInput};

pub fn scrolled_terminal<'cb>() -> Terminal<'static, 'cb> {
    let mut terminal = new_terminal(20, 5).unwrap();
    for line in 0..30 {
        terminal.vt_write(format!("line{line}\r\n").as_bytes());
    }
    terminal
}

pub fn fractional_metrics(scale: f32) -> Metrics {
    Metrics {
        cell_width: 9,
        cell_height: 19,
        padding: 12.0 * scale,
        scale,
    }
}

pub const THEME_RED: Rgb = Rgb {
    r: 0xff,
    g: 0,
    b: 0,
};
pub const THEME_BLUE: Rgb = Rgb {
    r: 0,
    g: 0,
    b: 0xff,
};

pub const TEST_SURFACE: Surface = Surface {
    cols: 20,
    rows: 5,
    metrics: Metrics {
        cell_width: 10,
        cell_height: 20,
        padding: 0.0,
        scale: 1.0,
    },
};

pub fn pointer_at(phase: PointerPhase, column: u16, row: u16) -> PointerInput {
    PointerInput {
        phase,
        button: Some(mouse::Button::Left),
        x: f32::from(column) * 10.0 + 5.0,
        y: f32::from(row) * 20.0 + 10.0,
        mods: key::Mods::empty(),
    }
}

pub fn wheel_by(dy: f32) -> WheelInput {
    WheelInput {
        dy,
        x: 5.0,
        y: 10.0,
        mods: key::Mods::empty(),
    }
}
