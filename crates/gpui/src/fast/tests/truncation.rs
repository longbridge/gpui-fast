//! Tests of truncation fitted to the shaped text. See
//! [`crate::fast::truncation`].

use std::{borrow::Cow, cell::RefCell, rc::Rc, sync::Arc};

use crate::{
    AppContext as _, Bounds, Context, DevicePixels, Font, FontId, FontMetrics, FontRun, GlyphId,
    IntoElement, LineLayout, NoopTextSystem, ParentElement as _, Pixels, PlatformTextSystem,
    Render, RenderGlyphParams, Result, Size, Styled as _, StyledText, TestAppContext, TextLayout,
    TextRenderingMode, Window, WindowHandle, div, prelude::FluentBuilder as _, px,
};

/// Set half an em wider whenever it is shaped with other characters than
/// on its own, as CoreText sets CJK punctuation between ideographs.
const WIDENED: char = '：';

/// A sentence whose punctuation, shaped in context, makes it several
/// characters wider than its characters shaped one at a time.
const TEXT: &str = "产品：余额通：港币：赎回金额：到账账户：综合账户：完成";

/// The no-op text system, every character 0.6 em wide, except that
/// [`WIDENED`] takes half an em more in any line longer than itself.
struct ContextualTextSystem(NoopTextSystem);

impl PlatformTextSystem for ContextualTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.0.add_fonts(fonts)
    }

    fn all_font_names(&self) -> Vec<String> {
        self.0.all_font_names()
    }

    fn font_id(&self, descriptor: &Font) -> Result<FontId> {
        self.0.font_id(descriptor)
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        self.0.font_metrics(font_id)
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        self.0.typographic_bounds(font_id, glyph_id)
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        self.0.advance(font_id, glyph_id)
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.0.glyph_for_char(font_id, ch)
    }

    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        self.0.glyph_raster_bounds(params)
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        self.0.rasterize_glyph(params, raster_bounds)
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        let mut layout = self.0.layout_line(text, font_size, runs);
        if text.chars().nth(1).is_none() {
            return layout;
        }
        let mut shift = px(0.);
        for glyph in layout.runs.iter_mut().flat_map(|run| run.glyphs.iter_mut()) {
            glyph.position.x += shift;
            if text[glyph.index..].starts_with(WIDENED) {
                shift += font_size * 0.5;
            }
        }
        layout.width += shift;
        layout
    }

    fn recommended_rendering_mode(&self, font_id: FontId, font_size: Pixels) -> TextRenderingMode {
        self.0.recommended_rendering_mode(font_id, font_size)
    }
}

#[derive(Clone, Copy)]
enum Overflow {
    End,
    Start,
    Middle,
    TwoLines,
}

/// [`TEXT`] in a box of a given width, truncated as `overflow` says, which
/// keeps its layout for the test to look at.
struct Truncated {
    width: Pixels,
    overflow: Overflow,
    layout: Rc<RefCell<Option<TextLayout>>>,
}

impl Render for Truncated {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let text = StyledText::new(TEXT);
        *self.layout.borrow_mut() = Some(text.layout().clone());
        let overflow = self.overflow;
        div()
            .w(self.width)
            .when(matches!(overflow, Overflow::End), |this| this.truncate())
            .when(matches!(overflow, Overflow::Start), |this| {
                this.overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis_start()
            })
            .when(matches!(overflow, Overflow::Middle), |this| {
                this.overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis_middle()
            })
            .when(matches!(overflow, Overflow::TwoLines), |this| {
                this.text_ellipsis().line_clamp(2)
            })
            .child(text)
    }
}

/// The lines [`TEXT`] was truncated to in a box `width` wide, each with the
/// width of the last line it wraps into.
fn truncated(width: f32, overflow: Overflow) -> Vec<(String, Pixels, usize)> {
    let mut cx = TestAppContext::with_text_system(Arc::new(ContextualTextSystem(NoopTextSystem)));
    let window: WindowHandle<Truncated> = cx.add_window(move |_, _| Truncated {
        width: px(width),
        overflow,
        layout: Default::default(),
    });
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
    })
    .unwrap();
    window
        .update(&mut cx, |view, _, _| {
            let layout = view.layout.borrow().clone().unwrap();
            let inner = layout.0.borrow();
            inner
                .as_ref()
                .unwrap()
                .lines
                .iter()
                .map(|line| {
                    let layout = &line.unwrapped_layout;
                    let start = line.wrap_boundaries.last().map_or(px(0.), |boundary| {
                        layout.runs[boundary.run_ix].glyphs[boundary.glyph_ix]
                            .position
                            .x
                    });
                    (
                        line.text.to_string(),
                        layout.width - start,
                        line.wrap_boundaries.len() + 1,
                    )
                })
                .collect()
        })
        .unwrap()
}

/// Asserts that truncated text fits in `width`, with no more than a few
/// characters' room to spare.
#[track_caller]
fn assert_fits_closely(lines: &[(String, Pixels, usize)], width: f32) {
    let [(text, last_line_width, _)] = lines else {
        panic!("truncated text should stay one line: {lines:?}");
    };
    assert!(
        text.contains('…') && text.len() < TEXT.len(),
        "the text should be cut short with an ellipsis: {text:?}"
    );
    assert!(
        *last_line_width <= px(width),
        "the shaped line overflows its box, so the box clips the ellipsis: {lines:?}"
    );
    assert!(
        *last_line_width > px(width) - px(40.),
        "the line was cut far shorter than it needed to be: {lines:?}"
    );
}

/// Punctuation shaped wider in context than alone makes a line wider than
/// the sum of its characters' widths, which the line wrapper truncates by:
/// the cut has to be fitted to the shaped line, or its ellipsis overflows
/// and is clipped away.
#[test]
fn text_shaped_wider_than_its_characters_is_cut_to_fit_from_the_end() {
    let lines = truncated(200., Overflow::End);
    assert_fits_closely(&lines, 200.);
    assert!(lines[0].0.ends_with('…'), "{lines:?}");
}

#[test]
fn text_shaped_wider_than_its_characters_is_cut_to_fit_from_the_start() {
    let lines = truncated(200., Overflow::Start);
    assert_fits_closely(&lines, 200.);
    assert!(lines[0].0.starts_with('…'), "{lines:?}");
}

#[test]
fn text_shaped_wider_than_its_characters_is_cut_to_fit_in_the_middle() {
    let lines = truncated(200., Overflow::Middle);
    assert_fits_closely(&lines, 200.);
    let text = &lines[0].0;
    assert!(
        !text.starts_with('…') && !text.ends_with('…'),
        "the ellipsis should be in the middle: {text:?}"
    );
}

/// Text clamped to two lines wraps by its shaped width, so the line wrapper's
/// cut, worked out from the sum of characters, can spill over the second
/// line just the same.
#[test]
fn text_shaped_wider_than_its_characters_is_cut_to_fit_two_lines() {
    let lines = truncated(120., Overflow::TwoLines);
    let [(text, last_line_width, line_count)] = &lines[..] else {
        panic!("truncated text should be one wrapped line: {lines:?}");
    };
    assert!(text.ends_with('…'), "{lines:?}");
    assert!(*line_count <= 2, "{lines:?}");
    assert!(
        *last_line_width <= px(120.),
        "the last line overflows its box, so the box clips the ellipsis: {lines:?}"
    );
}

/// Text that fits once shaped is left whole.
#[test]
fn text_that_fits_once_shaped_is_not_truncated() {
    let lines = truncated(1000., Overflow::End);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].0, TEXT);
}
