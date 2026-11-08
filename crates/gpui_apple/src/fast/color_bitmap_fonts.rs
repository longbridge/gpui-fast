//! Fonts whose glyphs are color bitmaps, found by their `sbix` table rather
//! than by name.
//!
//! A glyph GPUI shapes is drawn as a color sprite only if the text system
//! says its font is an emoji font. Upstream says so for Apple Color Emoji by
//! its PostScript name, so any other font with `sbix` bitmaps (an
//! application's own emoji font, a user-installed one) had its glyphs drawn
//! into an alpha-only mask and tinted with the text color: a flat silhouette
//! instead of the picture.

use collections::HashSet;
use font_kit::font::Font as FontKitFont;
use gpui::FontId;

/// The ids of loaded fonts that have an `sbix` table.
#[derive(Default)]
pub(crate) struct ColorBitmapFonts(HashSet<FontId>);

impl ColorBitmapFonts {
    /// Notes whether `font`, just given `font_id`, draws color bitmaps. The
    /// font's table directory is read once here, so that the per-glyph
    /// question, [`ColorBitmapFonts::contains`], is a set lookup.
    pub(crate) fn note(&mut self, font_id: FontId, font: &FontKitFont) {
        if has_sbix_table(font) {
            self.0.insert(font_id);
        }
    }

    /// Whether the font `font_id` has an `sbix` table.
    pub(crate) fn contains(&self, font_id: FontId) -> bool {
        self.0.contains(&font_id)
    }
}

fn has_sbix_table(font: &FontKitFont) -> bool {
    const SBIX: u32 = u32::from_be_bytes(*b"sbix");
    font.native_font()
        .get_available_font_tables()
        .is_some_and(|tables| tables.iter().any(|tag| *tag == SBIX))
}

#[cfg(test)]
mod tests {
    use crate::AppleTextSystem;
    use gpui::{FontRun, PlatformTextSystem, RenderGlyphParams, font, point, px};
    use std::borrow::Cow;

    /// One glyph, a 32 by 32 opaque red square at U+E000, stored only as an
    /// `sbix` PNG strike, built by `assets/fonts/sbix-test/make_sbix_test_font.py`.
    const SBIX_TEST: &[u8] = include_bytes!("../../../../assets/fonts/sbix-test/SbixTest.ttf");

    /// A font with an `sbix` table that isn't Apple Color Emoji has its glyphs
    /// shaped as emoji and rasterized in color, as Apple Color Emoji's are.
    #[test]
    fn an_application_sbix_font_draws_its_glyphs_in_color() {
        let fonts = AppleTextSystem::new();
        fonts.add_fonts(vec![Cow::Borrowed(SBIX_TEST)]).unwrap();
        let font_id = fonts.font_id(&font("GPUI Sbix Test")).unwrap();
        let text = "\u{E000}";
        let runs = [FontRun {
            font_id,
            len: text.len(),
        }];
        let layout = fonts.layout_line(text, px(32.), &runs);
        let glyph = layout.runs[0].glyphs[0].clone();
        assert_eq!(layout.runs[0].font_id, font_id);
        assert!(glyph.is_emoji, "an sbix glyph was shaped as a mask glyph");

        let params = RenderGlyphParams {
            font_id,
            glyph_id: glyph.id,
            font_size: px(32.),
            subpixel_variant: point(0, 0),
            scale_factor: 1.,
            is_emoji: glyph.is_emoji,
            subpixel_rendering: false,
            dilation: 0,
        };
        let bounds = fonts.glyph_raster_bounds(&params).unwrap();
        assert!(
            bounds.size.width.0 >= 32 && bounds.size.height.0 >= 32,
            "the square's bounds are {bounds:?}"
        );
        let (size, bytes) = fonts.rasterize_glyph(&params, bounds).unwrap();
        let pixels = (size.width.0 * size.height.0) as usize;
        assert_eq!(bytes.len(), pixels * 4, "the glyph was not drawn in color");
        // BGRA, straight alpha: the square's middle is opaque red.
        let middle =
            (size.height.0 as usize / 2 * size.width.0 as usize + size.width.0 as usize / 2) * 4;
        let [blue, green, red, alpha] = bytes[middle..middle + 4] else {
            unreachable!()
        };
        assert!(
            red > 240 && green < 16 && blue < 16 && alpha > 240,
            "the square's middle is {:?}, not red",
            [blue, green, red, alpha]
        );
    }

    /// Apple Color Emoji, which upstream recognizes by name, is still an emoji
    /// font, and an outline font isn't one.
    #[test]
    fn apple_color_emoji_is_still_color_and_outline_fonts_are_not() {
        let fonts = AppleTextSystem::new();
        let emoji = fonts.font_id(&font("Apple Color Emoji")).unwrap();
        let helvetica = fonts.font_id(&font("Helvetica")).unwrap();
        for (font_id, text, color) in [(emoji, "😀", true), (helvetica, "A", false)] {
            let runs = [FontRun {
                font_id,
                len: text.len(),
            }];
            let layout = fonts.layout_line(text, px(16.), &runs);
            assert_eq!(layout.runs[0].font_id, font_id);
            assert_eq!(layout.runs[0].glyphs[0].is_emoji, color, "{text}");
        }
    }
}
