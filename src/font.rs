use std::ptr::NonNull;

use anyhow::{Context, Result, ensure};
use libghostty_vt::style::Underline;
use objc2_core_foundation::{
    CFAttributedString, CFData, CFDictionary, CFRetained, CFString, CFType, kCFBooleanTrue,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo, CGImageByteOrderInfo,
};
use objc2_core_text::{
    CTFont, CTFontManagerCreateFontDescriptorFromData, CTFontOrientation, CTFontSymbolicTraits,
    CTLine, kCTFontAttributeName, kCTForegroundColorFromContextAttributeName,
};

use crate::config::DEFAULT_FONT_FAMILY;
use crate::sprite;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FontStyle {
    pub bold: bool,
    pub italic: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlyphKind {
    Coverage,
    Color,
}

/// A monospaced face rasterised into whole-cell RGBA bitmaps, in device pixels.
pub struct Font {
    /// Indexed by `bold + 2 * italic`.
    faces: [CFRetained<CTFont>; 4],
    symbols: CFRetained<CTFont>,
    color_space: Option<CFRetained<CGColorSpace>>,
    pub cell_width: u32,
    pub cell_height: u32,
    pub line_thickness: u32,
    pub underline_top: u32,
    pub strike_top: u32,
    baseline: f64,
}

const SYMBOLS_FONT: &[u8] = include_bytes!("../assets/SymbolsNerdFontMono-Regular.ttf");

fn symbols_face(pixels: f64) -> Result<CFRetained<CTFont>> {
    // SAFETY: the data is live for the call, and a null matrix means identity.
    unsafe {
        let descriptor =
            CTFontManagerCreateFontDescriptorFromData(&CFData::from_static_bytes(SYMBOLS_FONT))
                .context("the bundled symbols font couldn't be loaded")?;
        Ok(CTFont::with_font_descriptor(
            &descriptor,
            pixels,
            std::ptr::null(),
        ))
    }
}

fn face(name: &str, pixels: f64) -> CFRetained<CTFont> {
    // SAFETY: a null matrix means identity.
    unsafe { CTFont::with_name(&CFString::from_str(name), pixels, std::ptr::null()) }
}

/// Regular, bold, italic and bold italic faces of `family`, or `None` if it isn't installed.
/// A style the family lacks reuses the regular face.
fn family_faces(family: &str, pixels: f64) -> Option<[CFRetained<CTFont>; 4]> {
    let regular = face(family, pixels);
    // CoreText silently substitutes another family when a name isn't installed.
    // SAFETY: a plain getter on a live font.
    if unsafe { regular.family_name() }.to_string() != family {
        return None;
    }
    let styled = |traits: CTFontSymbolicTraits| {
        // SAFETY: a zero size keeps the font's size and a null matrix means identity.
        unsafe { regular.copy_with_symbolic_traits(0.0, std::ptr::null(), traits, traits) }
            .unwrap_or_else(|| regular.clone())
    };
    let bold = styled(CTFontSymbolicTraits::TraitBold);
    let italic = styled(CTFontSymbolicTraits::TraitItalic);
    let bold_italic = styled(CTFontSymbolicTraits::TraitBold | CTFontSymbolicTraits::TraitItalic);
    Some([regular, bold, italic, bold_italic])
}

fn is_private_use(character: char) -> bool {
    matches!(
        u32::from(character),
        0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD
    )
}

fn shaped_line(font: &CTFont, text: &str) -> Option<CFRetained<CTLine>> {
    // SAFETY: the extern statics are immutable CoreFoundation constants.
    let (font_key, color_key, enabled) = unsafe {
        (
            kCTFontAttributeName,
            kCTForegroundColorFromContextAttributeName,
            kCFBooleanTrue?,
        )
    };
    let values: [&CFType; 2] = [font.as_ref(), enabled.as_ref()];
    let attributes = CFDictionary::<CFString, CFType>::from_slices(&[font_key, color_key], &values);
    // SAFETY: the dictionary maps CFString attribute names to values of the types CoreText expects.
    let string = unsafe {
        CFAttributedString::new(
            None,
            Some(&CFString::from_str(text)),
            Some(attributes.as_opaque()),
        )
    }?;
    // SAFETY: `string` is a valid attributed string.
    Some(unsafe { CTLine::with_attributed_string(&string) })
}

impl Font {
    pub fn new(family: &str, points: f64, scale: f32) -> Result<Self> {
        let pixels = points * f64::from(scale);
        let faces = family_faces(family, pixels)
            .or_else(|| {
                eprintln!(
                    "agentd: font family {family} isn't installed; using {DEFAULT_FONT_FAMILY}"
                );
                family_faces(DEFAULT_FONT_FAMILY, pixels)
            })
            .with_context(|| format!("neither {family} nor {DEFAULT_FONT_FAMILY} is installed"))?;
        let regular = &faces[0];
        let mut character = u16::from(b'M');
        let mut glyph = 0u16;
        // SAFETY: both pointers are to one element, matching the count.
        let found = unsafe {
            regular.glyphs_for_characters(
                NonNull::from(&mut character),
                NonNull::from(&mut glyph),
                1,
            )
        };
        ensure!(found, "the terminal font has no glyph for M");
        // SAFETY: one glyph, and a null advances buffer is allowed when only the total is needed.
        let advance = unsafe {
            regular.advances_for_glyphs(
                CTFontOrientation::Horizontal,
                NonNull::from(&mut glyph),
                std::ptr::null_mut(),
                1,
            )
        };
        // SAFETY: plain metric getters.
        let (ascent, descent, leading, underline_position, underline_thickness, x_height) = unsafe {
            (
                regular.ascent(),
                regular.descent(),
                regular.leading(),
                regular.underline_position(),
                regular.underline_thickness(),
                regular.x_height(),
            )
        };
        let cell_height = (ascent + descent + leading).ceil() as u32;
        let baseline = (descent + leading / 2.0).round();
        let baseline_from_top = f64::from(cell_height) - baseline;
        let line_thickness = (underline_thickness.round() as u32).max(1);
        let half_line = f64::from(line_thickness) / 2.0;
        // Room for the second line of a double underline.
        let lowest_underline = cell_height.saturating_sub(3 * line_thickness);
        Ok(Self {
            cell_width: advance.ceil() as u32,
            cell_height,
            line_thickness,
            underline_top: ((baseline_from_top - underline_position - half_line).round() as u32)
                .min(lowest_underline),
            strike_top: (baseline_from_top - x_height / 2.0 - half_line).round() as u32,
            baseline,
            faces,
            symbols: symbols_face(pixels)?,
            color_space: CGColorSpace::new_device_rgb(),
        })
    }

    /// Premultiplied RGBA for `text`, `cells * cell_width` by `cell_height` with the top row first.
    pub fn rasterize(
        &self,
        text: &str,
        style: FontStyle,
        cells: u32,
        out: &mut Vec<u8>,
    ) -> Option<GlyphKind> {
        // CoreText's cascade never picks an installed font for private-use codepoints.
        let font = if text.chars().any(is_private_use) {
            &self.symbols
        } else {
            &self.faces[usize::from(style.bold) + 2 * usize::from(style.italic)]
        };
        let width = self.cell_width * cells;
        out.clear();
        out.resize((width * self.cell_height * 4) as usize, 0);
        let mut characters = text.chars();
        if let (Some(character), None, 1) = (characters.next(), characters.next(), cells)
            && sprite::draw(character, width, self.cell_height, self.line_thickness, out)
        {
            return Some(GlyphKind::Coverage);
        }
        let line = shaped_line(font, text)?;
        // SAFETY: `out` holds `4 * width * cell_height` bytes and outlives the context.
        let context = unsafe {
            CGBitmapContextCreate(
                out.as_mut_ptr().cast(),
                width as usize,
                self.cell_height as usize,
                8,
                width as usize * 4,
                self.color_space.as_deref(),
                CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0,
            )
        }?;
        // LCD smoothing would give coverage glyphs unequal channels and read as colour.
        CGContext::set_should_smooth_fonts(Some(&context), false);
        CGContext::set_rgb_fill_color(Some(&context), 1.0, 1.0, 1.0, 1.0);
        // SAFETY: null out-parameters are allowed when only the width is needed.
        let line_width = unsafe {
            line.typographic_bounds(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        let available = f64::from(width);
        // Fallback faces such as CJK are narrower than two terminal cells.
        let centred = ((available - line_width) / 2.0).max(0.0);
        CGContext::translate_ctm(Some(&context), centred, self.baseline);
        if line_width > available {
            let shrink = available / line_width;
            CGContext::scale_ctm(Some(&context), shrink, shrink);
        }
        CGContext::set_text_position(Some(&context), 0.0, 0.0);
        // SAFETY: `line` and `context` are live, and `out` outlives the context.
        unsafe { line.draw(&context) };
        let is_color = out
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[0] != pixel[3] || pixel[1] != pixel[3] || pixel[2] != pixel[3]);
        Some(if is_color {
            GlyphKind::Color
        } else {
            GlyphKind::Coverage
        })
    }

    /// One cell of a tiling underline, in the same format as `rasterize`.
    pub fn rasterize_underline(&self, kind: Underline, out: &mut Vec<u8>) {
        out.clear();
        out.resize((self.cell_width * self.cell_height * 4) as usize, 0);
        sprite::underline(
            kind,
            self.cell_width,
            self.cell_height,
            self.underline_top,
            self.line_thickness,
            out,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGULAR: FontStyle = FontStyle {
        bold: false,
        italic: false,
    };

    /// Rows of the bitmap holding any ink.
    fn ink_height(font: &Font, pixels: &[u8], cells: u32) -> usize {
        pixels
            .chunks_exact((font.cell_width * cells * 4) as usize)
            .filter(|row| row.as_chunks::<4>().0.iter().any(|pixel| pixel[3] != 0))
            .count()
    }

    #[test]
    fn plain_text_is_coverage_and_emoji_is_colour() {
        let font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        let mut pixels = Vec::new();
        assert_eq!(
            font.rasterize("A", REGULAR, 1, &mut pixels),
            Some(GlyphKind::Coverage)
        );
        assert!(ink_height(&font, &pixels, 1) > 0);
        assert_eq!(
            font.rasterize("\u{1F600}", REGULAR, 2, &mut pixels),
            Some(GlyphKind::Color)
        );
    }

    #[test]
    fn a_character_missing_from_the_terminal_font_falls_back_to_another_face() {
        let font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        let mut pixels = Vec::new();
        assert_eq!(
            font.rasterize("世", REGULAR, 2, &mut pixels),
            Some(GlyphKind::Coverage)
        );
        assert!(ink_height(&font, &pixels, 2) > 0);
    }

    #[test]
    fn a_zwj_sequence_shapes_into_one_glyph() {
        let font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        let mut single = Vec::new();
        font.rasterize("\u{1F468}", REGULAR, 2, &mut single);
        let mut family = Vec::new();
        font.rasterize(
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
            REGULAR,
            2,
            &mut family,
        );
        // Three separate emoji would be shrunk to a third of the height to fit.
        assert!(ink_height(&font, &family, 2) * 10 >= ink_height(&font, &single, 2) * 7);
    }

    #[test]
    fn box_drawing_is_procedural_and_touches_the_cell_edge() {
        let font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        let mut pixels = Vec::new();
        font.rasterize("│", REGULAR, 1, &mut pixels);
        assert_eq!(
            ink_height(&font, &pixels, 1),
            font.cell_height as usize,
            "a vertical line must leave no gap between rows"
        );
    }

    #[test]
    fn decoration_lines_fit_inside_the_cell() {
        let font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        assert!(font.underline_top + 3 * font.line_thickness <= font.cell_height);
        assert!(font.strike_top + font.line_thickness <= font.cell_height);
        assert!(font.strike_top < font.underline_top);
    }

    fn face_names(font: &Font) -> Vec<String> {
        font.faces
            .iter()
            // SAFETY: a plain getter on a live font.
            .map(|face| unsafe { face.post_script_name() }.to_string())
            .collect()
    }

    #[test]
    fn a_family_gives_its_four_styles() {
        let font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        assert_eq!(
            face_names(&font),
            [
                "MonaspaceNeon-Regular",
                "MonaspaceNeon-Bold",
                "MonaspaceNeon-Italic",
                "MonaspaceNeon-BoldItalic"
            ]
        );
    }

    #[test]
    fn an_unknown_family_falls_back_to_monaspace_neon() {
        let font = Font::new("No Such Family", 13.0, 2.0).unwrap();
        assert_eq!(face_names(&font)[0], "MonaspaceNeon-Regular");
    }

    #[test]
    fn a_family_without_a_style_reuses_the_regular_face() {
        // Andale Mono has one face.
        let font = Font::new("Andale Mono", 13.0, 2.0).unwrap();
        let names = face_names(&font);
        assert!(names.iter().all(|name| name == &names[0]), "{names:?}");
    }

    #[test]
    fn private_use_icons_are_real_glyphs_rather_than_tofu() {
        let font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        let (mut separator, mut icon) = (Vec::new(), Vec::new());
        font.rasterize("\u{E0C6}", REGULAR, 1, &mut separator);
        font.rasterize("\u{F432}", REGULAR, 1, &mut icon);
        // Every missing private-use character draws the same placeholder.
        assert_ne!(separator, icon);
        // SAFETY: a plain getter on a live font.
        let name = unsafe { font.symbols.post_script_name() }.to_string();
        assert_eq!(name, "SymbolsNFM");
    }

    #[test]
    fn the_bitmap_covers_every_requested_cell() {
        let font = Font::new(DEFAULT_FONT_FAMILY, 13.0, 2.0).unwrap();
        let mut pixels = Vec::new();
        font.rasterize("A", REGULAR, 2, &mut pixels);
        assert_eq!(
            pixels.len(),
            (font.cell_width * 2 * font.cell_height * 4) as usize
        );
    }
}
