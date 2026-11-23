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

/// Writes an executable script. Another process writes it: a child forked meanwhile by one of
/// this process's threads would hold a write fd until it execs, and Linux refuses to run a file
/// open for writing (ETXTBSY), even under another name.
pub fn write_executable(path: &std::path::Path, contents: &str) {
    let written = std::process::Command::new("/bin/sh")
        .args(["-c", r#"printf '%s' "$1" > "$0" && chmod 755 "$0""#])
        .arg(path)
        .arg(contents)
        .status()
        .unwrap();
    assert!(
        written.success(),
        "{} not written: {written}",
        path.display()
    );
}

/// A plugins folder holding one plugin, `stub`, with `manifest` as its `plugin.toml` and
/// `script` as its `run` executable, a `/bin/sh` script.
pub fn stub_plugin(manifest: &str, script: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    let plugin = directory.path().join("stub");
    std::fs::create_dir(&plugin).unwrap();
    std::fs::write(plugin.join("plugin.toml"), manifest).unwrap();
    write_executable(&plugin.join("run"), &format!("#!/bin/sh\n{script}\n"));
    directory
}
