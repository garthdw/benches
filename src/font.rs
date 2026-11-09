use std::ptr::NonNull;

use anyhow::{Context, Result};
use objc2_core_foundation::{CFRetained, CFString, CGPoint};
use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo};
use objc2_core_text::{CTFont, CTFontOrientation};

/// A monospaced face rasterised into whole-cell coverage bitmaps, in device pixels.
pub struct Font {
    regular: CFRetained<CTFont>,
    bold: CFRetained<CTFont>,
    pub cell_width: u32,
    pub cell_height: u32,
    baseline: f64,
}

fn face(name: &str, pixels: f64) -> CFRetained<CTFont> {
    // SAFETY: a null matrix means identity.
    unsafe { CTFont::with_name(&CFString::from_str(name), pixels, std::ptr::null()) }
}

fn glyph_for(font: &CTFont, character: char) -> Option<u16> {
    let mut utf16 = [0u16; 2];
    let length = character.encode_utf16(&mut utf16).len();
    let mut glyphs = [0u16; 2];
    // SAFETY: both buffers hold `length` elements.
    let found = unsafe {
        font.glyphs_for_characters(
            NonNull::from(&mut utf16).cast(),
            NonNull::from(&mut glyphs).cast(),
            length as isize,
        )
    };
    found.then_some(glyphs[0])
}

impl Font {
    pub fn new(points: f64, scale: f32) -> Result<Self> {
        let pixels = points * f64::from(scale);
        let regular = face("Menlo-Regular", pixels);
        let bold = face("Menlo-Bold", pixels);
        let mut glyph = glyph_for(&regular, 'M').context("Menlo has no glyph for M")?;
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
        Ok(Self {
            regular,
            bold,
            cell_width: advance.ceil() as u32,
            cell_height: (ascent + descent + leading).ceil() as u32,
            baseline: (descent + leading / 2.0).round(),
        })
    }

    /// Coverage for `character`, `cell_width * cell_height` bytes with the top row first; `false` if it can't be drawn.
    pub fn rasterize(&self, character: char, bold: bool, out: &mut Vec<u8>) -> bool {
        let font = if bold { &self.bold } else { &self.regular };
        let Some(mut glyph) = glyph_for(font, character) else {
            return false;
        };
        out.clear();
        out.resize((self.cell_width * self.cell_height) as usize, 0);
        let gray = CGColorSpace::new_device_gray();
        // SAFETY: `out` holds `cell_width * cell_height` bytes and outlives the context.
        let Some(context) = (unsafe {
            CGBitmapContextCreate(
                out.as_mut_ptr().cast(),
                self.cell_width as usize,
                self.cell_height as usize,
                8,
                self.cell_width as usize,
                gray.as_deref(),
                CGImageAlphaInfo::None.0,
            )
        }) else {
            return false;
        };
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
