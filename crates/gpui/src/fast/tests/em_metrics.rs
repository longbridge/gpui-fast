//! Tests of `em` measurements in fonts without an `m`. See
//! [`crate::fast::em_metrics`].

use std::{borrow::Cow, sync::Arc};

use crate::{
    Bounds, DevicePixels, Font, FontId, FontMetrics, FontRun, GlyphId, LineLayout, NoopTextSystem,
    Pixels, PlatformTextSystem, RenderGlyphParams, Result, ShapedGlyph, ShapedRun, Size,
    TextRenderingMode, TextSystem, point, px, size,
};

/// A font with Latin letters, which shaping falls back to.
const LATIN: FontId = FontId(1);
/// A Hebrew font: no `m`.
const HEBREW: FontId = FontId(2);
/// A symbol font with no `m`, for which shaping finds no other font either.
const SYMBOLS: FontId = FontId(3);
const M: GlyphId = GlyphId(80);

struct Fonts;

impl PlatformTextSystem for Fonts {
    fn add_fonts(&self, _: Vec<Cow<'static, [u8]>>) -> Result<()> {
        Ok(())
    }

    fn all_font_names(&self) -> Vec<String> {
        Vec::new()
    }

    fn font_id(&self, _: &Font) -> Result<FontId> {
        Ok(LATIN)
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        FontMetrics {
            // The fallback's units differ from the requested font's.
            units_per_em: if font_id == LATIN { 2048 } else { 1000 },
            ..NoopTextSystem.font_metrics(font_id)
        }
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        assert_eq!((font_id, glyph_id), (LATIN, M));
        Ok(Bounds {
            origin: point(100., 0.),
            size: size(1536., 1100.),
        })
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        assert_eq!((font_id, glyph_id), (LATIN, M));
        Ok(size(1792., 0.))
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        (font_id == LATIN && ch == 'm').then_some(M)
    }

    fn glyph_raster_bounds(&self, _: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        Ok(Default::default())
    }

    fn rasterize_glyph(
        &self,
        _: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        Ok((raster_bounds.size, Vec::new()))
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        assert_eq!(text, "m");
        // Hebrew falls back to the Latin font; the symbol font draws a
        // missing glyph in itself.
        let (font_id, id) = if runs[0].font_id == SYMBOLS {
            (SYMBOLS, GlyphId(0))
        } else {
            (LATIN, M)
        };
        LineLayout {
            font_size,
            width: px(14.),
            ascent: px(9.),
            descent: px(3.),
            runs: vec![ShapedRun {
                font_id,
                glyphs: vec![ShapedGlyph {
                    id,
                    position: point(px(0.), px(0.)),
                    index: 0,
                    is_emoji: false,
                }],
            }],
            len: text.len(),
        }
    }

    fn recommended_rendering_mode(&self, _: FontId, _: Pixels) -> TextRenderingMode {
        TextRenderingMode::Grayscale
    }
}

#[test]
fn a_font_without_m_measures_the_m_shaping_falls_back_to() {
    let text_system = TextSystem::new(Arc::new(Fonts));
    let font_size = px(16.);

    // A Latin font's own `m`, measured as upstream measures it.
    let latin_width = text_system.em_width(LATIN, font_size).unwrap();
    let latin_advance = text_system.em_advance(LATIN, font_size).unwrap();
    assert_eq!(latin_width, px(1536. / 2048. * 16.));
    assert_eq!(latin_advance, px(1792. / 2048. * 16.));
    assert_eq!(
        latin_width,
        text_system
            .typographic_bounds(LATIN, font_size, 'm')
            .unwrap()
            .size
            .width
    );
    assert_eq!(
        latin_advance,
        text_system.advance(LATIN, font_size, 'm').unwrap().width
    );

    // A Hebrew font has none: its `m` is the fallback's, in the fallback's
    // units, rather than an error.
    assert!(
        text_system
            .typographic_bounds(HEBREW, font_size, 'm')
            .is_err()
    );
    assert_eq!(
        text_system.em_width(HEBREW, font_size).unwrap(),
        latin_width
    );
    assert_eq!(
        text_system.em_advance(HEBREW, font_size).unwrap(),
        latin_advance
    );

    // With no font to fall back to, the error is upstream's.
    let error = text_system.em_width(SYMBOLS, font_size).unwrap_err();
    assert!(error.to_string().contains("glyph not found"), "{error}");
    assert!(text_system.em_advance(SYMBOLS, font_size).is_err());
}
