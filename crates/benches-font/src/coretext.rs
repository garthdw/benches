use std::marker::PhantomData;
use std::ptr::NonNull;

use anyhow::{Context, Result};
use objc2_core_foundation::{
    CFArray, CFAttributedString, CFData, CFDictionary, CFNumber, CFRange, CFRetained, CFString,
    CFType, CGPoint, CGRect, kCFBooleanTrue,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo, CGImageByteOrderInfo,
};
use objc2_core_text::{
    CTFont, CTFontDescriptor, CTFontManagerCreateFontDescriptorFromData, CTFontOrientation,
    CTFontSymbolicTraits, CTLine, CTRun, kCTFontAttributeName, kCTFontFeatureSettingsAttribute,
    kCTFontOpenTypeFeatureTag, kCTFontOpenTypeFeatureValue,
    kCTForegroundColorFromContextAttributeName,
};

use crate::constraint::Bounds;
use crate::{Backend, FaceMetrics, FontFeature, FontStyle, Glyph, InkExtent, Placement};

pub(crate) struct CoreText {
    color_space: Option<CFRetained<CGColorSpace>>,
}

/// A bitmap context drawing into the buffer it borrows.
pub(crate) struct Canvas<'a> {
    context: CFRetained<CGContext>,
    pixels: PhantomData<&'a mut [u8]>,
}

/// Keeps the pixels borrowed until the context is released, not just until the canvas's last use.
impl Drop for Canvas<'_> {
    fn drop(&mut self) {}
}

fn face(name: &str, pixels: f64) -> CFRetained<CTFont> {
    // SAFETY: a null matrix means identity.
    unsafe { CTFont::with_name(&CFString::from_str(name), pixels, std::ptr::null()) }
}

fn feature_dictionary(feature: FontFeature) -> CFRetained<CFDictionary<CFString, CFType>> {
    let tag = CFString::from_str(&String::from_utf8_lossy(&feature.tag));
    let value = CFNumber::new_i32(feature.value);
    let values: [&CFType; 2] = [tag.as_ref(), value.as_ref()];
    // SAFETY: the extern statics are immutable CoreFoundation constants.
    let keys = unsafe { [kCTFontOpenTypeFeatureTag, kCTFontOpenTypeFeatureValue] };
    CFDictionary::from_slices(&keys, &values)
}

impl Backend for CoreText {
    type Face = CFRetained<CTFont>;
    type Line = CFRetained<CTLine>;
    type Canvas<'a> = Canvas<'a>;

    fn new() -> Self {
        Self {
            color_space: CGColorSpace::new_device_rgb(),
        }
    }

    fn installed_face(&self, family: &str, pixels: f64) -> Option<Self::Face> {
        let regular = face(family, pixels);
        // CoreText silently substitutes another family when a name isn't installed.
        // SAFETY: a plain getter on a live font.
        (unsafe { regular.family_name() }.to_string() == family).then_some(regular)
    }

    fn styled(&self, face: &Self::Face, style: FontStyle) -> Option<Self::Face> {
        let mut traits = CTFontSymbolicTraits::empty();
        if style.bold {
            traits |= CTFontSymbolicTraits::TraitBold;
        }
        if style.italic {
            traits |= CTFontSymbolicTraits::TraitItalic;
        }
        // SAFETY: a zero size keeps the font's size and a null matrix means identity.
        unsafe { face.copy_with_symbolic_traits(0.0, std::ptr::null(), traits, traits) }
    }

    fn embedded_face(&self, bytes: &'static [u8], pixels: f64) -> Result<Self::Face> {
        // SAFETY: the data is live for the call, and a null matrix means identity.
        unsafe {
            let descriptor =
                CTFontManagerCreateFontDescriptorFromData(&CFData::from_static_bytes(bytes))
                    .context("CoreText couldn't read the font data")?;
            Ok(CTFont::with_font_descriptor(
                &descriptor,
                pixels,
                std::ptr::null(),
            ))
        }
    }

    /// CoreText's defaults already enable `calt` and `liga`.
    fn with_features(face: Self::Face, features: &[FontFeature]) -> Self::Face {
        if features.is_empty() {
            return face;
        }
        let settings = CFArray::from_retained_objects(
            &features
                .iter()
                .map(|feature| feature_dictionary(*feature))
                .collect::<Vec<_>>(),
        );
        let values: [&CFType; 1] = [settings.as_ref()];
        // SAFETY: the extern static is an immutable CoreFoundation constant.
        let key = unsafe { kCTFontFeatureSettingsAttribute };
        let attributes = CFDictionary::<CFString, CFType>::from_slices(&[key], &values);
        // SAFETY: the dictionary holds the settings array CoreText documents for this key, a zero
        // size keeps the font's size and a null matrix means identity.
        unsafe {
            let descriptor = CTFontDescriptor::with_attributes(attributes.as_opaque());
            face.copy_with_attributes(0.0, std::ptr::null(), Some(&descriptor))
        }
    }

    fn glyph(face: &Self::Face, character: char) -> Option<u16> {
        let mut units = [0u16; 2];
        let units = character.encode_utf16(&mut units);
        let mut glyphs = [0u16; 2];
        // SAFETY: both pointers cover whole buffers of at least the `units.len()` elements the
        // count names.
        let found = unsafe {
            face.glyphs_for_characters(
                NonNull::from(&mut *units).cast(),
                NonNull::from(&mut glyphs).cast(),
                units.len() as isize,
            )
        };
        found.then_some(glyphs[0])
    }

    fn advance(face: &Self::Face, glyph: u16) -> f64 {
        let mut glyph = glyph;
        // SAFETY: one glyph, and a null advances buffer is allowed when only the total is needed.
        unsafe {
            face.advances_for_glyphs(
                CTFontOrientation::Horizontal,
                NonNull::from(&mut glyph),
                std::ptr::null_mut(),
                1,
            )
        }
    }

    fn metrics(face: &Self::Face) -> FaceMetrics {
        // SAFETY: plain metric getters on a live font.
        unsafe {
            FaceMetrics {
                ascent: face.ascent(),
                descent: face.descent(),
                leading: face.leading(),
                underline_position: face.underline_position(),
                underline_thickness: face.underline_thickness(),
                x_height: face.x_height(),
                cap_height: face.cap_height(),
            }
        }
    }

    fn shape(face: &Self::Face, text: &str) -> Option<Vec<Glyph>> {
        let line = Self::line(face, text)?;
        // SAFETY: a plain getter on a live line.
        let runs = unsafe { line.glyph_runs() };
        // SAFETY: a line's glyph runs are `CTRun`s.
        let runs = unsafe { runs.cast_unchecked::<CTRun>() };
        let mut shaped = Vec::new();
        for run in runs.iter() {
            // SAFETY: plain getter.
            let count = unsafe { run.glyph_count() };
            let length = usize::try_from(count).ok()?;
            let range = CFRange {
                location: 0,
                length: count,
            };
            let (mut glyphs, mut positions) =
                (vec![0u16; length], vec![CGPoint { x: 0.0, y: 0.0 }; length]);
            // SAFETY: each buffer holds the `count` elements the range names.
            unsafe {
                run.glyphs(range, NonNull::from(glyphs.as_mut_slice()).cast());
                run.positions(range, NonNull::from(positions.as_mut_slice()).cast());
            }
            shaped.extend(
                glyphs
                    .into_iter()
                    .zip(positions)
                    .map(|(id, position)| Glyph {
                        id,
                        x: position.x,
                        y: position.y,
                    }),
            );
        }
        Some(shaped)
    }

    fn glyph_ink(face: &Self::Face, glyph: u16) -> InkExtent {
        let mut glyph = glyph;
        let mut rect = CGRect::ZERO;
        // SAFETY: one glyph and one rect, matching the count of 1.
        unsafe {
            face.bounding_rects_for_glyphs(
                CTFontOrientation::Horizontal,
                NonNull::from(&mut glyph),
                &mut rect,
                1,
            );
        }
        (rect.size.width > 0.0).then_some((rect.origin.x, rect.origin.x + rect.size.width))
    }

    fn line(face: &Self::Face, text: &str) -> Option<Self::Line> {
        // SAFETY: the extern statics are immutable CoreFoundation constants.
        let (font_key, color_key, enabled) = unsafe {
            (
                kCTFontAttributeName,
                kCTForegroundColorFromContextAttributeName,
                kCFBooleanTrue?,
            )
        };
        let values: [&CFType; 2] = [face.as_ref(), enabled.as_ref()];
        let attributes =
            CFDictionary::<CFString, CFType>::from_slices(&[font_key, color_key], &values);
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

    fn line_width(line: &Self::Line) -> f64 {
        // SAFETY: null out-parameters are allowed when only the width is needed.
        unsafe {
            line.typographic_bounds(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        }
    }

    fn canvas<'a>(&self, out: &'a mut [u8], width: u32, height: u32) -> Option<Canvas<'a>> {
        if out.len() < width as usize * height as usize * 4 {
            return None;
        }
        // SAFETY: `out` holds that many bytes, and the canvas borrows it for as long as the
        // context lives.
        let context = unsafe {
            CGBitmapContextCreate(
                out.as_mut_ptr().cast(),
                width as usize,
                height as usize,
                8,
                width as usize * 4,
                self.color_space.as_deref(),
                CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0,
            )
        }?;
        // LCD smoothing would give coverage glyphs unequal channels and read as colour.
        CGContext::set_should_smooth_fonts(Some(&context), false);
        CGContext::set_rgb_fill_color(Some(&context), 1.0, 1.0, 1.0, 1.0);
        Some(Canvas {
            context,
            pixels: PhantomData,
        })
    }

    fn line_ink(canvas: &Canvas<'_>, line: &Self::Line) -> Bounds {
        // SAFETY: `line` and the context are live.
        let ink = unsafe { line.image_bounds(Some(&canvas.context)) };
        Bounds {
            x: ink.origin.x,
            y: ink.origin.y,
            width: ink.size.width,
            height: ink.size.height,
        }
    }

    /// Applies the placement as three CTM steps, in its order, since folding them into one matrix
    /// rounds differently.
    fn draw_line(canvas: &mut Canvas<'_>, line: &Self::Line, placement: Placement) {
        let context = Some(&*canvas.context);
        CGContext::translate_ctm(context, placement.offset.0, placement.offset.1);
        CGContext::scale_ctm(context, placement.scale.0, placement.scale.1);
        CGContext::translate_ctm(context, -placement.origin.0, -placement.origin.1);
        CGContext::set_text_position(context, 0.0, 0.0);
        // SAFETY: `line` and the context are live, and the canvas borrows the pixels.
        unsafe { line.draw(&canvas.context) };
    }

    fn draw_glyphs(
        canvas: &mut Canvas<'_>,
        face: &Self::Face,
        glyphs: impl IntoIterator<Item = Glyph>,
    ) {
        let (mut ids, mut positions): (Vec<u16>, Vec<CGPoint>) = glyphs
            .into_iter()
            .map(|glyph| {
                (
                    glyph.id,
                    CGPoint {
                        x: glyph.x,
                        y: glyph.y,
                    },
                )
            })
            .unzip();
        // SAFETY: both buffers hold `ids.len()` elements, and the context is live.
        unsafe {
            face.draw_glyphs(
                NonNull::from(ids.as_mut_slice()).cast(),
                NonNull::from(positions.as_mut_slice()).cast(),
                ids.len(),
                &canvas.context,
            );
        }
    }

    #[cfg(test)]
    fn postscript_name(face: &Self::Face) -> String {
        // SAFETY: a plain getter on a live font.
        unsafe { face.post_script_name() }.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_feature_becomes_a_coretext_tag_and_value_dictionary() {
        let dictionary = feature_dictionary(FontFeature {
            tag: *b"liga",
            value: 0,
        });
        // SAFETY: the extern statics are immutable CoreFoundation constants.
        let (tag, value) = unsafe { (kCTFontOpenTypeFeatureTag, kCTFontOpenTypeFeatureValue) };
        let tag = dictionary.get(tag).unwrap();
        let value = dictionary.get(value).unwrap();
        assert_eq!(tag.downcast_ref::<CFString>().unwrap().to_string(), "liga");
        assert_eq!(value.downcast_ref::<CFNumber>().unwrap().as_i32(), Some(0));
    }
}
