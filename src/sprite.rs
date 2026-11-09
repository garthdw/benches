use libghostty_vt::style::Underline;

const NONE: u8 = 0;
const HEAVY: u8 = 2;
const DOUBLE: u8 = 3;

/// Arm weights as `up right down left` for U+2500–254B. Dash codepoints are drawn separately.
const LIGHT_HEAVY_ARMS: &[u8] = concat!(
    "0101020210102020", // ─ ━ │ ┃
    "00000000000000000000000000000000",
    "0110021001200220",             // ┌ ┍ ┎ ┏
    "0011001200210022",             // ┐ ┑ ┒ ┓
    "1100120021002200",             // └ ┕ ┖ ┗
    "1001100220012002",             // ┘ ┙ ┚ ┛
    "1110121021101120212022101220", // ├ ┝ ┞ ┟ ┠ ┡ ┢
    "2220",
    "1011101220111021202120121022", // ┤ ┥ ┦ ┧ ┨ ┩ ┪
    "2022",
    "0111011202110212012101220221", // ┬ ┭ ┮ ┯ ┰ ┱ ┲
    "0222",
    "1101110212011202210121022201", // ┴ ┵ ┶ ┷ ┸ ┹ ┺
    "2202",
    "1111111212111212",             // ┼ ┽ ┾ ┿
    "2111112121212112221111221221", // ╀ ╁ ╂ ╃ ╄ ╅ ╆
    "22121222212222212222",         // ╇ ╈ ╉ ╊ ╋
)
.as_bytes();

/// U+2550–256C.
const DOUBLE_ARMS: &[u8] = concat!(
    "03033030",         // ═ ║
    "0310013003300013", // ╒ ╓ ╔ ╕
    "0031003313003100", // ╖ ╗ ╘ ╙
    "3300100330013003", // ╚ ╛ ╜ ╝
    "1310313033301013", // ╞ ╟ ╠ ╡
    "3031303303130131", // ╢ ╣ ╤ ╥
    "0333130331013303", // ╦ ╧ ╨ ╩
    "131331313333",     // ╪ ╫ ╬
)
.as_bytes();

/// U+2574–257F.
const SHORT_ARMS: &[u8] = concat!(
    "00011000", "01000010", "00022000", "02000020", "02011020", "01022010"
)
.as_bytes();

/// Quadrants of U+2596–259F, as bits: 1 upper left, 2 upper right, 4 lower left, 8 lower right.
const QUADRANTS: [u8; 10] = [4, 8, 1, 13, 9, 7, 11, 2, 6, 14];

/// The start and end of a run `thickness` long, centred in `length`.
fn band(length: u32, thickness: u32) -> (u32, u32) {
    let start = length.saturating_sub(thickness) / 2;
    (start, (start + thickness).min(length))
}

fn distance_to_segment(point: (f32, f32), from: (f32, f32), to: (f32, f32)) -> f32 {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let along =
        (((point.0 - from.0) * dx + (point.1 - from.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    (point.0 - from.0 - along * dx).hypot(point.1 - from.1 - along * dy)
}

/// Writes premultiplied white, so a pixel's alpha is its coverage.
struct Canvas<'a> {
    width: u32,
    height: u32,
    pixels: &'a mut [u8],
}

impl Canvas<'_> {
    fn set(&mut self, x: u32, y: u32, coverage: u8) {
        let start = (y * self.width + x) as usize * 4;
        if let Some(pixel) = self.pixels.get_mut(start..start + 4) {
            pixel.fill(coverage);
        }
    }

    fn fill(&mut self, x: (u32, u32), y: (u32, u32), coverage: u8) {
        for row in y.0..y.1.min(self.height) {
            for column in x.0..x.1.min(self.width) {
                self.set(column, row, coverage);
            }
        }
    }

    /// Antialiases `inside` by sampling each pixel on a 4x4 grid.
    fn shape(&mut self, inside: impl Fn(f32, f32) -> bool) {
        for row in 0..self.height {
            for column in 0..self.width {
                let mut hits = 0u32;
                for sample_y in 0..4 {
                    for sample_x in 0..4 {
                        let x = column as f32 + (sample_x as f32 + 0.5) / 4.0;
                        let y = row as f32 + (sample_y as f32 + 0.5) / 4.0;
                        hits += u32::from(inside(x, y));
                    }
                }
                if hits > 0 {
                    self.set(column, row, (hits * 255 / 16) as u8);
                }
            }
        }
    }

    /// A rectangle given along and across an axis, so vertical and horizontal arms share code.
    fn axis_fill(&mut self, vertical: bool, along: (u32, u32), across: (u32, u32), coverage: u8) {
        if vertical {
            self.fill(across, along, coverage);
        } else {
            self.fill(along, across, coverage);
        }
    }

    fn axis_lengths(&self, vertical: bool) -> (u32, u32) {
        if vertical {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }

    fn lines(&mut self, [up, right, down, left]: [u8; 4], light: u32) {
        let axes = [
            (true, up, down, left, right),
            (false, left, right, up, down),
        ];
        // Doubles are painted solid and then hollowed, which leaves mitred inner corners.
        for (vertical, near, far, cross_a, cross_b) in axes {
            let (length, across) = self.axis_lengths(vertical);
            let (outer_start, outer_end) = band(length, 3 * light);
            let crossed = cross_a == DOUBLE || cross_b == DOUBLE;
            let near_end = if crossed { outer_end } else { length / 2 };
            let far_start = if crossed { outer_start } else { length / 2 };
            let span = band(across, 3 * light);
            if near == DOUBLE {
                self.axis_fill(vertical, (0, near_end), span, 255);
            }
            if far == DOUBLE {
                self.axis_fill(vertical, (far_start, length), span, 255);
            }
        }
        for (vertical, near, far, ..) in axes {
            let (length, across) = self.axis_lengths(vertical);
            let (channel_start, channel_end) = band(length, light);
            let channel = band(across, light);
            if near == DOUBLE {
                self.axis_fill(vertical, (0, channel_end), channel, 0);
            }
            if far == DOUBLE {
                self.axis_fill(vertical, (channel_start, length), channel, 0);
            }
        }
        let thickness = |weight| if weight == HEAVY { 2 * light } else { light };
        for (vertical, near, far, cross_a, cross_b) in axes {
            let (length, across) = self.axis_lengths(vertical);
            let crossed = cross_a == DOUBLE || cross_b == DOUBLE;
            for (weight, is_near) in [(near, true), (far, false)] {
                if weight == NONE || weight == DOUBLE {
                    continue;
                }
                let width = thickness(weight);
                let (near_end, far_start) = if crossed {
                    band(length, light)
                } else {
                    let widest = [cross_a, cross_b]
                        .into_iter()
                        .filter(|arm| *arm != NONE)
                        .map(thickness)
                        .fold(width, u32::max);
                    let (start, end) = band(length, widest);
                    (end, start)
                };
                let along = if is_near {
                    (0, near_end)
                } else {
                    (far_start, length)
                };
                self.axis_fill(vertical, along, band(across, width), 255);
            }
        }
    }

    fn dashes(&mut self, vertical: bool, segments: u32, thickness: u32) {
        let (length, across) = self.axis_lengths(vertical);
        let period = length / segments;
        let gap = (period / 3).max(1);
        for segment in 0..segments {
            let start = segment * period + gap / 2;
            self.axis_fill(
                vertical,
                (start, start + period - gap),
                band(across, thickness),
                255,
            );
        }
    }

    fn arc(&mut self, code: u32, light: u32) {
        let (sign_x, sign_y) = match code {
            0x256D => (1.0, 1.0),
            0x256E => (-1.0, 1.0),
            0x256F => (-1.0, -1.0),
            _ => (1.0, -1.0),
        };
        let (width, half) = (self.width as f32, light as f32 / 2.0);
        let center_x = band(self.width, light).0 as f32 + half;
        let center_y = band(self.height, light).0 as f32 + half;
        let radius = if sign_x > 0.0 {
            width - center_x
        } else {
            center_x
        };
        let circle = (center_x + sign_x * radius, center_y + sign_y * radius);
        self.shape(|x, y| {
            (x - circle.0) * sign_x <= 0.0
                && (y - circle.1) * sign_y <= 0.0
                && ((x - circle.0).hypot(y - circle.1) - radius).abs() <= half
        });
        let vertical_end = circle.1.clamp(0.0, self.height as f32) as u32;
        let along = if sign_y > 0.0 {
            (vertical_end, self.height)
        } else {
            (0, vertical_end)
        };
        self.fill(band(self.width, light), along, 255);
    }

    fn diagonals(&mut self, code: u32, light: u32) {
        let (width, height, half) = (self.width as f32, self.height as f32, light as f32 / 2.0);
        self.shape(|x, y| {
            let rising = distance_to_segment((x, y), (0.0, height), (width, 0.0)) <= half;
            let falling = distance_to_segment((x, y), (0.0, 0.0), (width, height)) <= half;
            match code {
                0x2571 => rising,
                0x2572 => falling,
                _ => rising || falling,
            }
        });
    }

    fn block(&mut self, code: u32) {
        let (width, height) = (self.width, self.height);
        let eighth_x = |eighths| width * eighths / 8;
        let eighth_y = |eighths| height * eighths / 8;
        match code {
            0x2580 => self.fill((0, width), (0, eighth_y(4)), 255),
            0x2581..=0x2588 => {
                let start = eighth_y(8 - (code - 0x2580));
                self.fill((0, width), (start, height), 255);
            }
            0x2589..=0x258F => {
                let end = eighth_x(8 - (code - 0x2588));
                self.fill((0, end), (0, height), 255);
            }
            0x2590 => self.fill((eighth_x(4), width), (0, height), 255),
            0x2591 => self.fill((0, width), (0, height), 64),
            0x2592 => self.fill((0, width), (0, height), 128),
            0x2593 => self.fill((0, width), (0, height), 191),
            0x2594 => self.fill((0, width), (0, eighth_y(1)), 255),
            _ => self.fill((eighth_x(7), width), (0, height), 255),
        }
    }

    fn quadrants(&mut self, mask: u8) {
        let (width, height) = (self.width, self.height);
        let (middle_x, middle_y) = (width / 2, height / 2);
        let parts = [
            (1, (0, middle_x), (0, middle_y)),
            (2, (middle_x, width), (0, middle_y)),
            (4, (0, middle_x), (middle_y, height)),
            (8, (middle_x, width), (middle_y, height)),
        ];
        for (bit, x, y) in parts {
            if mask & bit != 0 {
                self.fill(x, y, 255);
            }
        }
    }

    fn powerline(&mut self, code: u32, light: u32) {
        let (width, height, half) = (self.width as f32, self.height as f32, light as f32 / 2.0);
        let points_left = code >= 0xE0B2;
        let solid = code == 0xE0B0 || code == 0xE0B2;
        self.shape(|x, y| {
            let x = if points_left { width - x } else { x };
            if solid {
                x / width <= 1.0 - (2.0 * y / height - 1.0).abs()
            } else {
                let tip = (width, height / 2.0);
                distance_to_segment((x, y), (0.0, 0.0), tip) <= half
                    || distance_to_segment((x, y), (0.0, height), tip) <= half
            }
        });
    }
}

fn arms(table: &[u8], index: u32) -> Option<[u8; 4]> {
    let start = index as usize * 4;
    let digits = table.get(start..start + 4)?;
    Some(std::array::from_fn(|arm| digits[arm] - b'0'))
}

/// Draws `character` if it's procedural, so that it meets its neighbours with no seams.
pub fn draw(character: char, width: u32, height: u32, light: u32, out: &mut [u8]) -> bool {
    let code = u32::from(character);
    let mut canvas = Canvas {
        width,
        height,
        pixels: out,
    };
    let lines = |canvas: &mut Canvas<'_>, table, index| match arms(table, index) {
        Some(weights) => {
            canvas.lines(weights, light);
            true
        }
        None => false,
    };
    match code {
        0x2504..=0x250B => {
            let offset = code - 0x2504;
            let thickness = light * (1 + (offset & 1));
            canvas.dashes(offset & 2 != 0, if offset < 4 { 3 } else { 4 }, thickness);
            true
        }
        0x254C..=0x254F => {
            let offset = code - 0x254C;
            canvas.dashes(offset >= 2, 2, light * (1 + (offset & 1)));
            true
        }
        0x2500..=0x254B => lines(&mut canvas, LIGHT_HEAVY_ARMS, code - 0x2500),
        0x2550..=0x256C => lines(&mut canvas, DOUBLE_ARMS, code - 0x2550),
        0x2574..=0x257F => lines(&mut canvas, SHORT_ARMS, code - 0x2574),
        0x256D..=0x2570 => {
            canvas.arc(code, light);
            true
        }
        0x2571..=0x2573 => {
            canvas.diagonals(code, light);
            true
        }
        0x2580..=0x2595 => {
            canvas.block(code);
            true
        }
        0x2596..=0x259F => {
            canvas.quadrants(QUADRANTS[(code - 0x2596) as usize]);
            true
        }
        0xE0B0..=0xE0B3 => {
            canvas.powerline(code, light);
            true
        }
        _ => false,
    }
}

/// An underline that tiles across cells. Straight underlines are plain quads instead.
pub fn underline(
    kind: Underline,
    width: u32,
    height: u32,
    top: u32,
    thickness: u32,
    out: &mut [u8],
) {
    let mut canvas = Canvas {
        width,
        height,
        pixels: out,
    };
    let (full_width, half) = (width as f32, thickness as f32 / 2.0);
    match kind {
        Underline::Curly => {
            let amplitude = thickness as f32;
            let center = (top as f32 + half).min(height as f32 - amplitude - half);
            canvas.shape(|x, y| {
                let wave = (x / full_width * std::f32::consts::TAU).sin();
                (y - (center + amplitude * wave)).abs() <= half
            });
        }
        Underline::Dotted => {
            let center = top as f32 + half;
            let period = full_width / 2.0;
            canvas.shape(|x, y| {
                let dot = (x / period).floor() * period + period / 2.0;
                (x - dot).hypot(y - center) <= half
            });
        }
        Underline::Dashed => canvas.fill((0, width * 5 / 8), (top, top + thickness), 255),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDTH: u32 = 16;
    const HEIGHT: u32 = 30;
    const LINE: u32 = 2;

    fn drawn(character: char) -> Vec<u8> {
        let mut pixels = vec![0; (WIDTH * HEIGHT * 4) as usize];
        assert!(draw(character, WIDTH, HEIGHT, LINE, &mut pixels));
        pixels
    }

    fn alpha(pixels: &[u8], x: u32, y: u32) -> u8 {
        pixels[((y * WIDTH + x) * 4 + 3) as usize]
    }

    #[test]
    fn the_arm_tables_have_an_entry_per_codepoint() {
        assert_eq!(LIGHT_HEAVY_ARMS.len(), 0x4C * 4);
        assert_eq!(DOUBLE_ARMS.len(), 0x1D * 4);
        assert_eq!(SHORT_ARMS.len(), 0x0C * 4);
    }

    #[test]
    fn horizontal_and_vertical_lines_reach_the_cell_edges() {
        let horizontal = drawn('─');
        assert_ne!(alpha(&horizontal, 0, HEIGHT / 2), 0);
        assert_ne!(alpha(&horizontal, WIDTH - 1, HEIGHT / 2), 0);
        let vertical = drawn('│');
        assert_ne!(alpha(&vertical, WIDTH / 2, 0), 0);
        assert_ne!(alpha(&vertical, WIDTH / 2, HEIGHT - 1), 0);
    }

    #[test]
    fn a_corner_only_reaches_its_own_edges() {
        let corner = drawn('┌');
        assert_ne!(alpha(&corner, WIDTH - 1, HEIGHT / 2), 0);
        assert_ne!(alpha(&corner, WIDTH / 2, HEIGHT - 1), 0);
        assert_eq!(alpha(&corner, 0, HEIGHT / 2), 0);
        assert_eq!(alpha(&corner, WIDTH / 2, 0), 0);
    }

    #[test]
    fn heavy_lines_are_thicker_than_light_ones() {
        let thickness = |pixels: &[u8]| {
            (0..HEIGHT)
                .filter(|&y| alpha(pixels, WIDTH / 2, y) != 0)
                .count()
        };
        assert_eq!(thickness(&drawn('─')), LINE as usize);
        assert_eq!(thickness(&drawn('━')), 2 * LINE as usize);
    }

    #[test]
    fn a_double_cross_is_hollow_in_the_middle() {
        let cross = drawn('╬');
        assert_eq!(alpha(&cross, WIDTH / 2, HEIGHT / 2), 0);
        let (start, _) = band(HEIGHT, 3 * LINE);
        assert_ne!(alpha(&cross, 0, start), 0);
        assert_eq!(alpha(&cross, 0, HEIGHT / 2), 0);
    }

    #[test]
    fn a_single_line_joining_a_double_stops_at_the_nearest_rail() {
        let tee = drawn('╤');
        let (_, channel_end) = band(HEIGHT, LINE);
        assert_eq!(alpha(&tee, WIDTH / 2, HEIGHT / 2), 0);
        assert_ne!(alpha(&tee, WIDTH / 2, channel_end), 0);
        assert_ne!(alpha(&tee, WIDTH / 2, HEIGHT - 1), 0);
    }

    #[test]
    fn an_arc_reaches_both_of_its_edges() {
        let arc = drawn('╭');
        assert_ne!(alpha(&arc, WIDTH - 1, band(HEIGHT, LINE).0), 0);
        assert_ne!(alpha(&arc, band(WIDTH, LINE).0, HEIGHT - 1), 0);
        assert_eq!(alpha(&arc, 0, 0), 0);
    }

    #[test]
    fn half_blocks_tile_with_no_gap() {
        let (upper, lower) = (drawn('▀'), drawn('▄'));
        for y in 0..HEIGHT {
            assert_ne!(
                alpha(&upper, 0, y) != 0,
                alpha(&lower, 0, y) != 0,
                "row {y}"
            );
        }
    }

    #[test]
    fn the_full_block_covers_every_pixel() {
        assert!(drawn('█').iter().all(|&byte| byte == 255));
    }

    #[test]
    fn quadrants_cover_only_their_corners() {
        let quadrant = drawn('▘');
        assert_ne!(alpha(&quadrant, 0, 0), 0);
        assert_eq!(alpha(&quadrant, WIDTH - 1, 0), 0);
        assert_eq!(alpha(&quadrant, 0, HEIGHT - 1), 0);
    }

    #[test]
    fn a_powerline_triangle_meets_the_left_edge_and_points_right() {
        let triangle = drawn('\u{E0B0}');
        assert_eq!(alpha(&triangle, 0, HEIGHT / 2), 255);
        assert_ne!(alpha(&triangle, WIDTH - 1, HEIGHT / 2), 0);
        assert_eq!(alpha(&triangle, WIDTH - 1, 0), 0);
    }

    #[test]
    fn ordinary_characters_are_not_sprites() {
        let mut pixels = vec![0; (WIDTH * HEIGHT * 4) as usize];
        assert!(!draw('A', WIDTH, HEIGHT, LINE, &mut pixels));
    }

    #[test]
    fn a_curly_underline_stays_inside_the_cell_and_tiles() {
        let mut pixels = vec![0; (WIDTH * HEIGHT * 4) as usize];
        underline(
            Underline::Curly,
            WIDTH,
            HEIGHT,
            HEIGHT - 3,
            LINE,
            &mut pixels,
        );
        let row_of = |x| (0..HEIGHT).find(|&y| alpha(&pixels, x, y) != 0);
        assert!(row_of(WIDTH / 4).is_some());
        let (first, last) = (row_of(0).unwrap(), row_of(WIDTH - 1).unwrap());
        assert!(first.abs_diff(last) <= 1, "{first} and {last} must meet");
    }
}
