//! Synthetic italic for faces without an italic style (longbridge/nsh#2529).
//!
//! Most CJK fonts (Noto Sans CJK, Source Han, PingFang) ship no italic face.
//! Upstream matching then falls back to the upright face, so italic text in
//! Chinese, Japanese or Korean renders upright, unlike browsers, Android and
//! CoreText, which slant the outline. When italic is requested and the matched
//! face is upright, the text system hands out a second `FontId` for the same
//! face, marked synthetic. Shaping and advances are unchanged (so selection
//! and carets are too); only rasterization slants the outline, which keeps the
//! glyph as sharp as the upright one and gives it correct raster bounds. The
//! separate `FontId` keeps slanted glyphs apart in the atlas without a new
//! field in `RenderGlyphParams`.

use collections::HashMap;
use cosmic_text::Style;
use gpui::{FontId, RenderGlyphParams};
use swash::{
    scale::{Render, Source},
    zeno::Transform,
};

use crate::cosmic_text_system::CosmicTextSystemState;

/// tan(12°), the slant of CoreText's and DirectWrite's oblique simulation.
pub(crate) const SKEW: f32 = 0.212_556_56;

/// Which loaded faces are synthetic italic twins of which upright faces.
#[derive(Default)]
pub(crate) struct Twins {
    twin_of_base: HashMap<FontId, FontId>,
    base_of_twin: HashMap<FontId, FontId>,
}

impl Twins {
    pub(crate) fn is_twin(&self, font_id: FontId) -> bool {
        self.base_of_twin.contains_key(&font_id)
    }

    /// The upright face `font_id` was made from, or `font_id` itself.
    pub(crate) fn base(&self, font_id: FontId) -> FontId {
        self.base_of_twin.get(&font_id).copied().unwrap_or(font_id)
    }
}

/// The face to draw `font_id` with for a request of `style`: when italic is
/// wanted and the face is upright, its synthetic italic twin.
pub(crate) fn for_request(
    state: &mut CosmicTextSystemState,
    font_id: FontId,
    style: gpui::FontStyle,
) -> FontId {
    if !wants_italic(style) || state.synthetic_italic.is_twin(font_id) {
        return font_id;
    }
    let base = &state.loaded_fonts[font_id.0];
    if base.is_known_emoji_font {
        return font_id;
    }
    match state.font_system.db().face(base.font.id()) {
        Some(face) if needs_slant(face.style) => {}
        _ => return font_id,
    }
    if let Some(&twin) = state.synthetic_italic.twin_of_base.get(&font_id) {
        return twin;
    }
    // Pushed after its base, so a lookup of the face by database id (as
    // `font_id_for_cosmic_id` does) still finds the upright face first.
    let twin = FontId(state.loaded_fonts.len());
    state.loaded_fonts.push(base.clone());
    state.synthetic_italic.twin_of_base.insert(font_id, twin);
    state.synthetic_italic.base_of_twin.insert(twin, font_id);
    twin
}

/// The face for a glyph cosmic-text set in `fallback_id` within a run of
/// `run_font_id`: italic runs stay italic in a fallback face without one.
pub(crate) fn for_fallback(
    state: &mut CosmicTextSystemState,
    run_font_id: FontId,
    fallback_id: FontId,
) -> FontId {
    let italic = state.synthetic_italic.is_twin(run_font_id)
        || state
            .font_system
            .db()
            .face(state.loaded_fonts[run_font_id.0].font.id())
            .is_some_and(|face| !needs_slant(face.style));
    if italic {
        for_request(state, fallback_id, gpui::FontStyle::Italic)
    } else {
        fallback_id
    }
}

/// The style cosmic-text matches `font_id` with: a twin asks for italic, so
/// cosmic-text's own fallback prefers italic faces where a family has them.
pub(crate) fn style(twins: &Twins, font_id: FontId, face_style: Style) -> Style {
    if twins.is_twin(font_id) {
        Style::Italic
    } else {
        face_style
    }
}

/// Embedded bitmaps can't be slanted, so a twin renders from its outline.
pub(crate) fn sources<'a>(
    twins: &Twins,
    params: &RenderGlyphParams,
    sources: &'a [Source],
) -> &'a [Source] {
    if twins.is_twin(params.font_id) {
        &[Source::Outline]
    } else {
        sources
    }
}

/// Slants a twin's outline before it is rasterized.
pub(crate) fn slant(twins: &Twins, params: &RenderGlyphParams, renderer: &mut Render) {
    if twins.is_twin(params.font_id) {
        renderer.transform(Some(transform()));
    }
}

/// Whether a request for `style` wants slanted glyphs.
pub(crate) fn wants_italic(style: gpui::FontStyle) -> bool {
    matches!(style, gpui::FontStyle::Italic | gpui::FontStyle::Oblique)
}

/// Whether a face of `style` must be slanted to look italic.
pub(crate) fn needs_slant(style: Style) -> bool {
    style == Style::Normal
}

/// The outline transform for a synthetic italic glyph. swash outlines are
/// y-up and zeno's `yx` is the x shift per unit of y, so this leans the
/// top to the right.
pub(crate) fn transform() -> Transform {
    Transform {
        xx: 1.0,
        xy: 0.0,
        yx: SKEW,
        yy: 1.0,
        x: 0.0,
        y: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use crate::CosmicTextSystem;
    use anyhow::Result;
    use gpui::{FontRun, FontStyle, PlatformTextSystem, RenderGlyphParams, point, px};
    use std::borrow::Cow;

    const LILEX: &[u8] = include_bytes!("../../../../assets/fonts/lilex/Lilex-Regular.ttf");
    const PLEX: &[u8] =
        include_bytes!("../../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf");
    const PLEX_ITALIC: &[u8] =
        include_bytes!("../../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Italic.ttf");

    fn text_system() -> Result<CosmicTextSystem> {
        let system = CosmicTextSystem::new_without_system_fonts("Lilex");
        system.add_fonts(vec![
            Cow::Borrowed(LILEX),
            Cow::Borrowed(PLEX),
            Cow::Borrowed(PLEX_ITALIC),
        ])?;
        Ok(system)
    }

    fn params(font_id: gpui::FontId, glyph_id: gpui::GlyphId) -> RenderGlyphParams {
        RenderGlyphParams {
            font_id,
            glyph_id,
            font_size: px(16.),
            subpixel_variant: point(0, 0),
            scale_factor: 1.,
            is_emoji: false,
            subpixel_rendering: false,
            dilation: 0,
        }
    }

    #[test]
    fn upright_only_face_is_slanted_when_rasterized() -> Result<()> {
        let system = text_system()?;
        let upright = system.font_id(&gpui::font("Lilex"))?;
        let mut italic_font = gpui::font("Lilex");
        italic_font.style = FontStyle::Italic;
        let italic = system.font_id(&italic_font)?;
        assert_ne!(upright, italic);
        assert_eq!(system.font_id(&italic_font)?, italic, "twin is cached");

        let glyph = system.glyph_for_char(upright, '|').expect("glyph");
        assert_eq!(system.glyph_for_char(italic, '|'), Some(glyph));
        assert_eq!(
            system.advance(upright, glyph)?,
            system.advance(italic, glyph)?
        );

        let upright_bounds = system.glyph_raster_bounds(&params(upright, glyph))?;
        let italic_bounds = system.glyph_raster_bounds(&params(italic, glyph))?;
        assert!(italic_bounds.size.width > upright_bounds.size.width);
        let (_, upright_pixels) =
            system.rasterize_glyph(&params(upright, glyph), upright_bounds)?;
        let (_, italic_pixels) = system.rasterize_glyph(&params(italic, glyph), italic_bounds)?;
        assert_ne!(upright_pixels, italic_pixels);
        // The stem leans right: ink in the top row sits right of the bottom row's.
        let width = italic_bounds.size.width.0 as usize;
        let center = |row: &[u8]| {
            let total: f32 = row.iter().map(|&a| a as f32).sum();
            row.iter()
                .enumerate()
                .map(|(x, &a)| x as f32 * a as f32)
                .sum::<f32>()
                / total
        };
        let rows: Vec<&[u8]> = italic_pixels.chunks(width).collect();
        assert!(center(rows[1]) > center(rows[rows.len() - 2]) + 1.0);

        // Shaping is unchanged, so carets and selections stay where they were.
        let text = "italic";
        let run = |font_id| {
            [FontRun {
                len: text.len(),
                font_id,
            }]
        };
        let a = system.layout_line(text, px(16.), &run(upright));
        let b = system.layout_line(text, px(16.), &run(italic));
        assert_eq!(a.width, b.width);
        assert!(b.runs.iter().all(|run| run.font_id == italic));
        Ok(())
    }

    #[test]
    fn real_italic_face_is_not_slanted_again() -> Result<()> {
        let system = text_system()?;
        let mut font = gpui::font("IBM Plex Sans");
        font.style = FontStyle::Italic;
        let italic = system.font_id(&font)?;
        let (_, style) = system.font_weight_and_style(italic)?;
        assert_eq!(style, FontStyle::Italic);
        assert!(!system.0.read().synthetic_italic.is_twin(italic));
        Ok(())
    }
}
