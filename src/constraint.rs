use crate::nerd_font_table::{CONSTRAINTS, RANGES};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Size {
    None,
    Fit,
    Cover,
    /// Fit down, or cover up to one cell (Nerd Fonts' `pa`).
    FitCover,
    Stretch,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Align {
    None,
    Start,
    End,
    /// Centred in the first cell even when the glyph may span two.
    CenterFirst,
}

/// Ghostty's `Glyph.RenderOptions.Constraint`: how a glyph is sized and placed in its cell or cells.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Constraint {
    pub size: Size,
    pub align_vertical: Align,
    pub align_horizontal: Align,
    pub pad_top: f64,
    pub pad_left: f64,
    pub pad_right: f64,
    pub pad_bottom: f64,
    /// Size and bearings of the glyph relative to the bounding box of its scale group.
    pub relative_width: f64,
    pub relative_height: f64,
    pub relative_x: f64,
    pub relative_y: f64,
    pub max_xy_ratio: Option<f64>,
    pub max_constraint_width: u32,
    /// Size against the icon height rather than the whole line height.
    pub icon_height: bool,
}

/// The font and cell measurements a constraint works in, in device pixels.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub cell_width: f64,
    pub cell_height: f64,
    pub face_width: f64,
    pub face_height: f64,
    /// From the bottom of the cell to the bottom of the face's bounding box.
    pub face_y: f64,
    pub icon_height: f64,
    pub icon_height_single: f64,
}

/// A glyph's ink box with `y` measured up from the bottom of the cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Constraint {
    pub const NONE: Self = Self {
        size: Size::None,
        align_vertical: Align::None,
        align_horizontal: Align::None,
        pad_top: 0.0,
        pad_left: 0.0,
        pad_right: 0.0,
        pad_bottom: 0.0,
        relative_width: 1.0,
        relative_height: 1.0,
        relative_x: 0.0,
        relative_y: 0.0,
        max_xy_ratio: None,
        max_constraint_width: 2,
        icon_height: false,
    };

    /// Ghostty's constraint for a glyph: the Nerd Font patcher's rule if it has one, else a symbol
    /// just fits inside its cells.
    pub fn of(character: char) -> Option<Self> {
        let codepoint = u32::from(character);
        let index = RANGES.partition_point(|&(_, end, _)| end < codepoint);
        match RANGES.get(index) {
            Some(&(start, _, constraint)) if start <= codepoint => {
                CONSTRAINTS.get(usize::from(constraint)).copied()
            }
            _ => is_symbol(character).then_some(Self {
                size: Size::Fit,
                ..Self::NONE
            }),
        }
    }

    /// Where `glyph` ends up when it may use `cells` cells.
    pub fn constrain(&self, glyph: Bounds, mut metrics: Metrics, cells: u32) -> Bounds {
        if self.size == Size::None
            && self.align_horizontal == Align::None
            && self.align_vertical == Align::None
        {
            return glyph;
        }
        if self.size != Size::Stretch {
            return self.constrain_inner(glyph, metrics, cells);
        }
        // Stretched glyphs line up across cell boundaries, so they follow the grid, not the face.
        metrics.face_width = metrics.cell_width;
        metrics.face_height = metrics.cell_height;
        metrics.face_y = 0.0;
        let unpadded = Self {
            pad_top: self.pad_top.max(0.0),
            pad_left: self.pad_left.max(0.0),
            pad_right: self.pad_right.max(0.0),
            pad_bottom: self.pad_bottom.max(0.0),
            ..*self
        };
        unpadded.constrain_inner(glyph, metrics, cells)
    }

    fn constrain_inner(&self, glyph: Bounds, metrics: Metrics, cells: u32) -> Bounds {
        // font_patcher never stretches a glyph across two cells of an extra wide face.
        let cells = if self.size == Size::Stretch && metrics.face_width > 0.9 * metrics.face_height
        {
            1
        } else {
            self.max_constraint_width.min(cells)
        };
        let mut group = {
            let width = glyph.width / self.relative_width;
            let height = glyph.height / self.relative_height;
            Bounds {
                x: glyph.x - width * self.relative_x,
                y: glyph.y - height * self.relative_y,
                width,
                height,
            }
        };
        let (width_factor, height_factor) = self.scale_factors(group, metrics, cells);
        let center_x = group.x + group.width / 2.0;
        let center_y = group.y + group.height / 2.0;
        group.width *= width_factor;
        group.height *= height_factor;
        group.x = center_x - group.width / 2.0;
        group.y = center_y - group.height / 2.0;
        group.y = self.aligned_y(group, metrics);
        group.x = self.aligned_x(group, metrics, cells);
        Bounds {
            x: group.x + group.width * self.relative_x,
            y: group.y + group.height * self.relative_y,
            width: width_factor * glyph.width,
            height: height_factor * glyph.height,
        }
    }

    fn scale_factors(&self, group: Bounds, metrics: Metrics, cells: u32) -> (f64, f64) {
        if self.size == Size::None {
            return (1.0, 1.0);
        }
        let multi_cell = cells > 1;
        let target_width =
            (f64::from(cells) - (self.pad_left + self.pad_right)) * metrics.face_width;
        let target_height = (1.0 - (self.pad_bottom + self.pad_top))
            * match (self.icon_height, multi_cell) {
                (false, _) => metrics.face_height,
                (true, true) => metrics.icon_height,
                (true, false) => metrics.icon_height_single,
            };
        let mut width_factor = target_width / group.width;
        let mut height_factor = target_height / group.height;
        match self.size {
            Size::None | Size::Stretch => {}
            Size::Fit => {
                height_factor = height_factor.min(width_factor).min(1.0);
                width_factor = height_factor;
            }
            Size::Cover => {
                height_factor = height_factor.min(width_factor);
                width_factor = height_factor;
            }
            Size::FitCover => {
                height_factor = height_factor.min(width_factor);
                if multi_cell && height_factor > 1.0 {
                    // An icon must not shrink when a space opens up after it.
                    height_factor = self.scale_factors(group, metrics, 1).1.max(1.0);
                }
                width_factor = height_factor;
            }
        }
        if let Some(ratio) = self.max_xy_ratio
            && group.width * width_factor > group.height * height_factor * ratio
        {
            width_factor = group.height * height_factor * ratio / group.width;
        }
        (width_factor, height_factor)
    }

    fn aligned_y(&self, group: Bounds, metrics: Metrics) -> f64 {
        if self.size == Size::None && self.align_vertical == Align::None {
            return group.y;
        }
        let start = metrics.face_y + self.pad_bottom * metrics.face_height;
        let end = metrics.face_y
            + (metrics.face_height - group.height - self.pad_top * metrics.face_height);
        let center = (start + end) / 2.0;
        match self.align_vertical {
            Align::None if end < start => center,
            Align::None => start.max(group.y.min(end)),
            Align::Start => start,
            Align::End => end,
            Align::CenterFirst => center,
        }
    }

    fn aligned_x(&self, group: Bounds, metrics: Metrics, cells: u32) -> f64 {
        if self.size == Size::None && self.align_horizontal == Align::None {
            return group.x;
        }
        // Any offset centring the face in a wider pixel cell is added after constraining.
        let span = metrics.face_width + f64::from(cells - 1) * metrics.cell_width;
        let start = self.pad_left * metrics.face_width;
        let end = span - group.width - self.pad_right * metrics.face_width;
        match self.align_horizontal {
            Align::None => start.max(group.x.min(end)),
            Align::Start => start,
            Align::End => start.max(end),
            Align::CenterFirst => {
                let end_first =
                    metrics.face_width - group.width - self.pad_right * metrics.face_width;
                start.max((start + end_first) / 2.0)
            }
        }
    }
}

pub fn is_private_use(character: char) -> bool {
    matches!(
        u32::from(character),
        0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD
    )
}

/// Ghostty's metric modifier: a percentage change such as `20%` or a pixel delta such as `-2`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Modifier {
    /// A factor of the value, where `20%` is 1.2 and `-100%` or less is 0.
    Percent(f64),
    Absolute(i32),
}

impl std::str::FromStr for Modifier {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, String> {
        let invalid = || format!("invalid modifier {input:?}: expected a number or a percentage");
        match input.strip_suffix('%') {
            Some(percent) => {
                let change = percent.parse::<f64>().map_err(|_| invalid())? / 100.0;
                Ok(Self::Percent((1.0 + change).max(0.0)))
            }
            None => input.parse().map(Self::Absolute).map_err(|_| invalid()),
        }
    }
}

impl Modifier {
    pub fn apply(self, value: f64) -> f64 {
        match self {
            Self::Percent(factor) => value * factor,
            Self::Absolute(delta) => value + f64::from(delta),
        }
    }
}

/// Ghostty's `isSymbol`: private use plus the Unicode blocks of arrows, dingbats, emoticons,
/// miscellaneous symbols (and pictographs), enclosed alphanumerics (and supplement), and
/// transport and map symbols.
pub fn is_symbol(character: char) -> bool {
    is_private_use(character)
        || matches!(
            u32::from(character),
            0x2190..=0x21FF
                | 0x2460..=0x24FF
                | 0x2600..=0x26FF
                | 0x2700..=0x27BF
                | 0x1F100..=0x1F1FF
                | 0x1F300..=0x1F5FF
                | 0x1F600..=0x1F64F
                | 0x1F680..=0x1F6FF
        )
}

/// Ghostty's `isGraphicsElement`: box drawing, blocks, legacy computing and Powerline glyphs,
/// which sit flush against their neighbours.
pub fn is_graphics_element(character: char) -> bool {
    matches!(
        u32::from(character),
        0x2500..=0x259F | 0x1FB00..=0x1FBFF | 0x1CC00..=0x1CEBF | 0xE0B0..=0xE0D7
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ghostty's own vectors: CoreText at size 12 and 96 DPI, JetBrains Mono.
    const METRICS: Metrics = Metrics {
        cell_width: 10.0,
        cell_height: 22.0,
        face_width: 9.6,
        face_height: 21.12,
        face_y: 0.2,
        icon_height: 21.12,
        icon_height_single: 44.48 / 3.0,
    };

    fn assert_bounds(expected: Bounds, actual: Bounds) {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-6;
        assert!(
            close(expected.x, actual.x)
                && close(expected.y, actual.y)
                && close(expected.width, actual.width)
                && close(expected.height, actual.height),
            "expected {expected:?}, got {actual:?}"
        );
    }

    fn bounds(x: f64, y: f64, width: f64, height: f64) -> Bounds {
        Bounds {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn no_constraint_leaves_a_glyph_alone() {
        let glyph = bounds(1.408, 4.84, 6.784, 15.28);
        for cells in [1, 2] {
            assert_bounds(glyph, Constraint::NONE.constrain(glyph, METRICS, cells));
        }
    }

    #[test]
    fn a_plain_symbol_fits_one_cell_and_is_left_alone_in_two() {
        let constraint = Constraint {
            size: Size::Fit,
            ..Constraint::NONE
        };
        let square = bounds(2.864, 5.304, 10.272, 10.272);
        assert_bounds(
            bounds(0.0, 5.64, 9.6, 9.6),
            constraint.constrain(square, METRICS, 1),
        );
        assert_bounds(square, constraint.constrain(square, METRICS, 2));
    }

    #[test]
    fn a_grouped_icon_scales_its_group_and_keeps_its_size_across_two_cells() {
        let constraint = Constraint::of('\u{EA61}').unwrap();
        assert_eq!(constraint.size, Size::FitCover);
        assert!(constraint.icon_height);
        let glyph = bounds(3.015625, 3.76525, 9.015625, 13.015625);
        assert_bounds(
            bounds(0.8125, 5.950695224719102, 7.2125, 10.4125),
            constraint.constrain(glyph, METRICS, 1),
        );
        assert_bounds(
            bounds(1.015625, 4.7483690308988775, glyph.width, glyph.height),
            constraint.constrain(glyph, METRICS, 2),
        );
    }

    #[test]
    fn a_stretched_glyph_covers_exactly_its_cells() {
        let constraint = Constraint::of('\u{E0C0}').unwrap();
        assert_eq!(constraint.size, Size::Stretch);
        let glyph = bounds(-0.796875, 1.7109375, 16.796875, 16.46875);
        assert_bounds(
            bounds(0.0, 0.0, 10.0, 22.0),
            constraint.constrain(glyph, METRICS, 1),
        );
        assert_bounds(
            bounds(0.0, 0.0, 20.0, 22.0),
            constraint.constrain(glyph, METRICS, 2),
        );
    }

    #[test]
    fn the_table_covers_nerd_font_icons_and_nothing_else() {
        for character in [
            '\u{F418}',
            '\u{F07B}',
            '\u{E7A8}',
            '\u{F0E7}',
            '\u{F0001}',
            '\u{E0B0}',
        ] {
            assert!(Constraint::of(character).is_some(), "{character:?}");
        }
        for character in ['a', ' ', '\u{4E16}', '\u{2500}', '\u{2588}'] {
            assert!(Constraint::of(character).is_none(), "{character:?}");
        }
    }

    #[test]
    fn symbols_are_private_use_and_ghosttys_symbol_blocks() {
        for character in [
            '\u{2192}',
            '\u{2605}',
            '\u{2713}',
            '\u{2460}',
            '\u{1F100}',
            '\u{1F41F}',
            '\u{1F600}',
            '\u{1F680}',
            '\u{E0B0}',
            '\u{F418}',
        ] {
            assert!(is_symbol(character), "{character:?}");
        }
        for character in [
            'a',
            ' ',
            '\u{4E16}',
            '\u{2500}',
            '\u{2588}',
            '\u{23FA}',
            '\u{1FB00}',
        ] {
            assert!(!is_symbol(character), "{character:?}");
        }
    }

    #[test]
    fn a_symbol_without_a_nerd_font_rule_just_fits() {
        for character in ['\u{2192}', '\u{2605}', '\u{2713}', '\u{1F600}'] {
            let constraint = Constraint::of(character).unwrap();
            assert_eq!(constraint.size, Size::Fit, "{character:?}");
            assert_eq!(constraint.align_horizontal, Align::None, "{character:?}");
            assert_eq!(constraint.max_constraint_width, 2, "{character:?}");
        }
    }

    #[test]
    fn graphics_elements_are_box_drawing_blocks_legacy_computing_and_powerline() {
        for character in [
            '\u{2500}',
            '\u{257F}',
            '\u{2580}',
            '\u{259F}',
            '\u{1FB00}',
            '\u{1CC00}',
            '\u{E0B0}',
            '\u{E0D7}',
        ] {
            assert!(is_graphics_element(character), "{character:?}");
        }
        for character in ['a', '\u{2192}', '\u{E0D8}', '\u{F418}'] {
            assert!(!is_graphics_element(character), "{character:?}");
        }
    }

    #[test]
    fn a_modifier_parses_as_ghostty_does() {
        assert_eq!("20%".parse(), Ok(Modifier::Percent(1.2)));
        assert_eq!("-20%".parse(), Ok(Modifier::Percent(0.8)));
        assert_eq!("-100%".parse(), Ok(Modifier::Percent(0.0)));
        assert_eq!("-150%".parse(), Ok(Modifier::Percent(0.0)));
        assert_eq!("5".parse(), Ok(Modifier::Absolute(5)));
        assert_eq!("-5".parse(), Ok(Modifier::Absolute(-5)));
        for invalid in ["", "%", "x", "1.5", "5px"] {
            assert!(invalid.parse::<Modifier>().is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn a_modifier_changes_a_value_by_a_delta() {
        assert_eq!(Modifier::Percent(0.75).apply(100.0), 75.0);
        assert_eq!(Modifier::Absolute(-5).apply(100.0), 95.0);
    }
}
