use std::ptr::NonNull;

use objc2_core_foundation::{CFRetained, CFString, CGPoint};
use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo};
use objc2_core_text::{CTFont, CTFontOrientation};

/// A monospaced face rasterised into whole-cell coverage bitmaps, in device pixels.
pub struct Font {
    regular: CFRetained<CTFont>,
    bold: CFRetained<CTFont>,
    pub cell_w: u32,
    pub cell_h: u32,
    baseline: f64,
}

fn face(name: &str, px: f64) -> CFRetained<CTFont> {
    // SAFETY: a null matrix means identity.
    unsafe { CTFont::with_name(&CFString::from_str(name), px, std::ptr::null()) }
}

fn glyph_for(font: &CTFont, ch: char) -> Option<u16> {
    let mut utf16 = [0u16; 2];
    let len = ch.encode_utf16(&mut utf16).len();
    let mut glyphs = [0u16; 2];
    // SAFETY: both buffers hold `len` elements.
    let found = unsafe {
        font.glyphs_for_characters(
            NonNull::from(&mut utf16).cast(),
            NonNull::from(&mut glyphs).cast(),
            len as isize,
        )
    };
    found.then_some(glyphs[0])
}

impl Font {
    pub fn new(points: f64, scale: f32) -> Self {
        let px = points * f64::from(scale);
        let regular = face("Menlo-Regular", px);
        let bold = face("Menlo-Bold", px);
        let mut glyph = glyph_for(&regular, 'M').expect("Menlo has M");
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
        let (ascent, descent, leading) =
            unsafe { (regular.ascent(), regular.descent(), regular.leading()) };
        Self {
            regular,
            bold,
            cell_w: advance.ceil() as u32,
            cell_h: (ascent + descent + leading).ceil() as u32,
            baseline: (descent + leading / 2.0).round(),
        }
    }

    /// Coverage for `ch`, `cell_w * cell_h` bytes with the top row first; `false` if the face lacks it.
    pub fn rasterize(&self, ch: char, bold: bool, out: &mut Vec<u8>) -> bool {
        let font = if bold { &self.bold } else { &self.regular };
        let Some(mut glyph) = glyph_for(font, ch) else {
            return false;
        };
        out.clear();
        out.resize((self.cell_w * self.cell_h) as usize, 0);
        let gray = CGColorSpace::new_device_gray();
        // SAFETY: `out` holds `cell_w * cell_h` bytes and outlives the context.
        let context = unsafe {
            CGBitmapContextCreate(
                out.as_mut_ptr().cast(),
                self.cell_w as usize,
                self.cell_h as usize,
                8,
                self.cell_w as usize,
                gray.as_deref(),
                CGImageAlphaInfo::None.0,
            )
        }
        .expect("glyph bitmap context");
        CGContext::set_gray_fill_color(Some(&context), 1.0, 1.0);
        let mut position = CGPoint {
            x: 0.0,
            y: self.baseline,
        };
        // SAFETY: one glyph and one position, drawn into a live context.
        unsafe {
            font.draw_glyphs(
                NonNull::from(&mut glyph),
                NonNull::from(&mut position),
                1,
                &context,
            )
        };
        true
    }
}
