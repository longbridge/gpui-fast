//! Frames drawn with everything a window retains between frames must match the
//! frames it would draw with none of it.
//!
//! Each run drives two windows holding the same view through the same random
//! history of changes. One draws every frame as an application would, reusing
//! retained layout nodes, shaped lines and last frame's orderings. The other
//! forgets all of that before every frame, so it draws each one as though it
//! were its first. Every frame, the two must paint the same primitives in the
//! same places and leave the same hitboxes; any difference is something a
//! retained shortcut got wrong.
//!
//! A shortcut taken within a single frame is taken by both windows alike, so
//! it is invisible here; its own tests have to cover it.

use std::{borrow::Cow, sync::Arc};

use rand::{Rng as _, SeedableRng as _, rngs::StdRng};

use crate::{
    AnyElement, Bounds, Context, DevicePixels, Entity, Font, FontId, FontMetrics, FontRun, GlyphId,
    Hsla, InputEvent as _, IntoElement, LineLayout, ListAlignment, ListOffset, ListState,
    MouseMoveEvent, NoopTextSystem, Pixels, PlatformTextSystem, Render, RenderGlyphParams, Result,
    SharedString, Size, StyleRefinement, TestAppContext, TextRenderingMode,
    UniformListScrollHandle, Window, WindowHandle, div, hsla, list, memo, point, prelude::*, px,
    size, uniform_list,
};

const WORDS: [&str; 10] = [
    "a",
    "grid",
    "cell",
    "ticking",
    "value",
    "with a longer label",
    "42",
    "lorem ipsum dolor",
    "x",
    "a label long enough to be cut short",
];

const PALETTE: [Hsla; 5] = [
    hsla(0.0, 0.0, 0.1, 1.0),
    hsla(0.6, 0.7, 0.5, 1.0),
    hsla(0.3, 0.6, 0.4, 1.0),
    hsla(0.0, 0.8, 0.6, 1.0),
    hsla(0.1, 0.9, 0.5, 0.5),
];

const GRID_CELLS: usize = 24;
const ROW_HEIGHT: f32 = 20.;
const INITIAL_ROWS: u64 = 30;

#[derive(Clone, Copy, Debug)]
enum CellFlag {
    Background,
    Underline,
    Truncate,
    Hover,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct CellState {
    word: usize,
    color: usize,
    width: f32,
    background: bool,
    underline: bool,
    truncate: bool,
    hover: bool,
}

/// How list rows identify themselves.
#[derive(Clone, Copy, Debug)]
enum RowIdentity {
    Position,
    Id,
    Key,
}

#[derive(Clone, Debug)]
enum Change {
    Word { cell: usize, word: usize },
    Color { cell: usize, color: usize },
    Width { cell: usize, width: f32 },
    Toggle { cell: usize, flag: CellFlag },
    Paragraph { words: usize, width: f32 },
    InsertRow { at: usize },
    RemoveRow { at: usize },
    Scroll { top: f32 },
    InsertChip { at: usize },
    RemoveChip { at: usize },
    RotateChips { by: usize },
    RowIdentity(RowIdentity),
    Direction,
    Badge,
    MoveMouse { x: f32, y: f32 },
    Resize { width: f32, height: f32 },
    Redraw,
}

impl Change {
    fn random(rng: &mut StdRng) -> Self {
        let cell = rng.random_range(0..GRID_CELLS);
        match rng.random_range(0..100) {
            0..20 => Change::Word {
                cell,
                word: rng.random_range(0..WORDS.len()),
            },
            20..27 => Change::Color {
                cell,
                color: rng.random_range(0..PALETTE.len()),
            },
            27..33 => Change::Width {
                cell,
                width: rng.random_range(20.0..220.0),
            },
            33..42 => Change::Toggle {
                cell,
                flag: [
                    CellFlag::Background,
                    CellFlag::Underline,
                    CellFlag::Truncate,
                    CellFlag::Hover,
                ][rng.random_range(0..4)],
            },
            42..48 => Change::Paragraph {
                words: rng.random_range(0..40),
                width: rng.random_range(60.0..400.0),
            },
            48..55 => Change::InsertRow {
                at: rng.random_range(0..64),
            },
            55..61 => Change::RemoveRow {
                at: rng.random_range(0..64),
            },
            61..67 => Change::Scroll {
                top: rng.random_range(0.0..400.0),
            },
            67..69 => Change::InsertChip {
                at: rng.random_range(0..16),
            },
            69..71 => Change::RemoveChip {
                at: rng.random_range(0..16),
            },
            71..72 => Change::RotateChips {
                by: rng.random_range(1..4),
            },
            72..75 => Change::RowIdentity(
                [RowIdentity::Position, RowIdentity::Id, RowIdentity::Key][rng.random_range(0..3)],
            ),
            75..77 => Change::Direction,
            77..83 => Change::Badge,
            83..92 => Change::MoveMouse {
                x: rng.random_range(0.0..900.0),
                y: rng.random_range(0.0..700.0),
            },
            92..95 => Change::Resize {
                width: rng.random_range(300.0..1000.0),
                height: rng.random_range(240.0..800.0),
            },
            _ => Change::Redraw,
        }
    }
}

/// A small application: a grid of cells, wrapping paragraphs, a row of chips,
/// a cached child view and the same rows in a uniform list and a list.
struct OracleView {
    cells: Vec<CellState>,
    paragraph_words: usize,
    paragraph_width: Pixels,
    rows: Vec<u64>,
    /// Chips share the row numbering, so no chip and row are the same thing.
    chips: Vec<u64>,
    next_row: u64,
    row_identity: RowIdentity,
    scroll_top: Pixels,
    column: bool,
    uniform_scroll: UniformListScrollHandle,
    list_state: ListState,
    badge: Entity<Badge>,
}

impl OracleView {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            cells: (0..GRID_CELLS)
                .map(|ix| CellState {
                    word: ix % WORDS.len(),
                    color: ix % PALETTE.len(),
                    width: 40. + (ix % 5) as f32 * 30.,
                    background: ix.is_multiple_of(3),
                    truncate: ix.is_multiple_of(4),
                    ..CellState::default()
                })
                .collect(),
            paragraph_words: 12,
            paragraph_width: px(180.),
            rows: (0..INITIAL_ROWS).collect(),
            chips: (INITIAL_ROWS..INITIAL_ROWS + 6).collect(),
            next_row: INITIAL_ROWS + 6,
            row_identity: RowIdentity::Position,
            scroll_top: px(0.),
            column: false,
            uniform_scroll: UniformListScrollHandle::new(),
            list_state: ListState::new(INITIAL_ROWS as usize, ListAlignment::Top, px(40.)),
            badge: cx.new(|_| Badge { count: 0 }),
        }
    }

    fn apply(&mut self, change: &Change, cx: &mut Context<Self>) {
        match *change {
            Change::Word { cell, word } => self.cells[cell].word = word,
            Change::Color { cell, color } => self.cells[cell].color = color,
            Change::Width { cell, width } => self.cells[cell].width = width,
            Change::Toggle { cell, flag } => {
                let cell = &mut self.cells[cell];
                let value = match flag {
                    CellFlag::Background => &mut cell.background,
                    CellFlag::Underline => &mut cell.underline,
                    CellFlag::Truncate => &mut cell.truncate,
                    CellFlag::Hover => &mut cell.hover,
                };
                *value = !*value;
            }
            Change::Paragraph { words, width } => {
                self.paragraph_words = words;
                self.paragraph_width = px(width);
            }
            Change::InsertRow { at } => {
                let at = at % (self.rows.len() + 1);
                self.rows.insert(at, self.next_row);
                self.next_row += 1;
                self.list_state.splice(at..at, 1);
            }
            Change::RemoveRow { at } => {
                if self.rows.is_empty() {
                    return;
                }
                let at = at % self.rows.len();
                self.rows.remove(at);
                self.list_state.splice(at..at + 1, 0);
            }
            Change::Scroll { top } => self.scroll_top = px(top),
            Change::InsertChip { at } => {
                let at = at % (self.chips.len() + 1);
                self.chips.insert(at, self.next_row);
                self.next_row += 1;
            }
            Change::RemoveChip { at } => {
                if !self.chips.is_empty() {
                    let at = at % self.chips.len();
                    self.chips.remove(at);
                }
            }
            Change::RotateChips { by } => {
                if !self.chips.is_empty() {
                    let by = by % self.chips.len();
                    self.chips.rotate_left(by);
                }
            }
            Change::RowIdentity(identity) => self.row_identity = identity,
            Change::Direction => self.column = !self.column,
            Change::Badge => {
                // Only the child is notified, so the parent's frame reuses
                // whatever it can of the last one around it.
                self.badge.update(cx, |badge, cx| {
                    badge.count += 1;
                    cx.notify();
                });
                return;
            }
            Change::MoveMouse { .. } | Change::Resize { .. } | Change::Redraw => return,
        }
        cx.notify();
    }
}

fn render_cell(cell: CellState) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .w(px(cell.width))
        .h(px(18.))
        .text_color(PALETTE[(cell.color + 1) % PALETTE.len()])
        .when(cell.background, |this| this.bg(PALETTE[cell.color]))
        .when(cell.underline, |this| this.underline())
        .when(cell.truncate, |this| {
            this.overflow_hidden().whitespace_nowrap().text_ellipsis()
        })
        .when(cell.hover, |this| {
            this.hover(|style| style.bg(PALETTE[4]).text_color(PALETTE[0]))
        })
        .child(WORDS[cell.word])
        .into_any_element()
}

fn render_row(row: u64, identity: RowIdentity) -> AnyElement {
    let word = WORDS[(row as usize * 7) % WORDS.len()];
    let row_element = div()
        .flex()
        .flex_row()
        .gap_2()
        .h(px(ROW_HEIGHT))
        .child(SharedString::from(format!("row {row}")))
        .child(
            div()
                .w(px(8. + (row % 4) as f32 * 6.))
                .h(px(8.))
                .bg(PALETTE[row as usize % PALETTE.len()]),
        )
        .child(
            div()
                .border_1()
                .border_color(PALETTE[1])
                .px_1()
                .when(row.is_multiple_of(3), |this| {
                    this.hover(|style| style.bg(PALETTE[2]))
                })
                .child(word),
        );
    match identity {
        RowIdentity::Position => row_element.into_any_element(),
        RowIdentity::Id => row_element.id(("row", row)).into_any_element(),
        RowIdentity::Key => row_element.key(("row", row)).into_any_element(),
    }
}

/// A chip in a row whose children come and go and change places, which a
/// retained parent has to follow.
fn render_chip(chip: u64, identity: RowIdentity) -> AnyElement {
    let chip_element = div()
        .flex()
        .flex_row()
        .px_1()
        .h(px(16.))
        .min_w(px(10. + (chip % 5) as f32 * 8.))
        .bg(PALETTE[chip as usize % PALETTE.len()])
        .when(chip.is_multiple_of(2), |this| {
            this.border_1().border_color(PALETTE[0])
        })
        .child(SharedString::from(chip.to_string()));
    match identity {
        RowIdentity::Position => chip_element.into_any_element(),
        RowIdentity::Id => chip_element.id(("chip", chip)).into_any_element(),
        RowIdentity::Key => chip_element.key(("chip", chip)).into_any_element(),
    }
}

impl Render for OracleView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let paragraph: String = (0..self.paragraph_words)
            .map(|ix| WORDS[ix % WORDS.len()])
            .collect::<Vec<_>>()
            .join(" ");

        let rows = self.rows.clone();
        let identity = self.row_identity;
        self.uniform_scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), -self.scroll_top));
        let uniform_rows = uniform_list("uniform rows", rows.len(), {
            let rows = rows.clone();
            move |range, _, _| range.map(|ix| render_row(rows[ix], identity)).collect()
        })
        .track_scroll(&self.uniform_scroll)
        .w(px(260.))
        .h(px(120.));

        if !rows.is_empty() {
            let item_ix = ((self.scroll_top / px(ROW_HEIGHT)).floor() as usize).min(rows.len() - 1);
            self.list_state.scroll_to(ListOffset {
                item_ix,
                offset_in_item: px(self.scroll_top.as_f32() % ROW_HEIGHT),
            });
        }
        let list_rows = list(self.list_state.clone(), move |ix, _, _| {
            render_row(rows[ix], identity)
        })
        .w(px(260.))
        .h(px(120.));

        div()
            .size_full()
            .flex()
            .flex_wrap()
            .gap_2()
            .when(self.column, |this| this.flex_col())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .w(px(420.))
                    .gap_1()
                    // Every cell is a memo keyed by its state, so the window
                    // drawing incrementally reuses the ones that did not change
                    // while the one drawing from scratch builds them all.
                    .children(self.cells.iter().copied().enumerate().map(|(ix, cell)| {
                        memo(("cell", ix), cell, move |_, _| render_cell(cell))
                            .w(px(cell.width))
                            .h(px(18.))
                    })),
            )
            .child(
                div()
                    .w(self.paragraph_width)
                    .text_color(PALETTE[0])
                    .child(paragraph.clone()),
            )
            .child(
                // A flex item is measured for its content size before it is
                // shrunk to fit, so this text is shaped unconstrained and then
                // at whatever width it ends up with.
                div()
                    .flex()
                    .flex_row()
                    .w(self.paragraph_width * 0.8)
                    .child(div().text_color(PALETTE[1]).child(paragraph))
                    .child(div().w(px(24.)).h(px(12.)).bg(PALETTE[2])),
            )
            .child(
                self.badge
                    .clone()
                    .cached(StyleRefinement::default().w(px(120.)).h(px(24.))),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .children(self.chips.iter().map(|&chip| render_chip(chip, identity))),
            )
            .child(uniform_rows)
            .child(list_rows)
    }
}

/// A child view that is sometimes notified on its own.
struct Badge {
    count: usize,
}

impl Render for Badge {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .gap_1()
            .size_full()
            .children((0..self.count % 4 + 1).map(|ix| {
                div()
                    .w(px(6. + ix as f32 * 3.))
                    .h(px(10.))
                    .bg(PALETTE[ix % PALETTE.len()])
            }))
            .child(SharedString::from(self.count.to_string()))
    }
}

/// The no-op text system, except that every glyph rasterizes to a small box,
/// so text paints a sprite per glyph and where each glyph went is compared.
struct GlyphBoxTextSystem(NoopTextSystem);

impl PlatformTextSystem for GlyphBoxTextSystem {
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

    fn glyph_raster_bounds(&self, _params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        Ok(Bounds {
            origin: point(DevicePixels(0), DevicePixels(-8)),
            size: size(DevicePixels(5), DevicePixels(9)),
        })
    }

    fn rasterize_glyph(
        &self,
        _params: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        let area = raster_bounds.size.width.0 * raster_bounds.size.height.0;
        Ok((raster_bounds.size, vec![u8::MAX; area as usize]))
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        self.0.layout_line(text, font_size, runs)
    }

    fn recommended_rendering_mode(&self, font_id: FontId, font_size: Pixels) -> TextRenderingMode {
        self.0.recommended_rendering_mode(font_id, font_size)
    }
}

fn apply(cx: &mut TestAppContext, window: WindowHandle<OracleView>, change: &Change) {
    match *change {
        Change::MoveMouse { x, y } => {
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(
                    MouseMoveEvent {
                        position: point(px(x), px(y)),
                        pressed_button: None,
                        modifiers: Default::default(),
                    }
                    .to_platform_input(),
                    cx,
                );
            })
            .unwrap();
        }
        Change::Resize { width, height } => {
            cx.simulate_window_resize(window.into(), size(px(width), px(height)));
        }
        _ => window
            .update(cx, |view, _, cx| view.apply(change, cx))
            .unwrap(),
    }
}

/// Draws a frame, after forgetting what the window retains if asked, and
/// returns what it drew with the layout nodes it reused.
fn draw(
    cx: &mut TestAppContext,
    window: WindowHandle<OracleView>,
    from_scratch: bool,
) -> (Vec<String>, u64) {
    cx.update_window(window.into(), |_, window, cx| {
        if from_scratch {
            window.forget_retained_state();
        }
        window.reset_layout_stats();
        window.draw(cx).clear(cx);
        (
            window.describe_rendered_frame(),
            window.layout_stats().nodes_reused,
        )
    })
    .unwrap()
}

/// Drives both windows through one random history and returns how many
/// layout nodes the incremental window reused along the way.
fn run(seed: u64, steps: usize) -> u64 {
    let mut cx = TestAppContext::with_text_system(Arc::new(GlyphBoxTextSystem(NoopTextSystem)));
    let incremental = cx.add_window(|_, cx| OracleView::new(cx));
    let from_scratch = cx.add_window(|_, cx| OracleView::new(cx));
    let mut rng = StdRng::seed_from_u64(seed);
    let mut history: Vec<Vec<Change>> = Vec::new();
    let mut reused = 0;

    for step in 0..steps {
        let changes: Vec<Change> = if step == 0 {
            Vec::new()
        } else {
            (0..rng.random_range(1..=3))
                .map(|_| Change::random(&mut rng))
                .collect()
        };
        for change in &changes {
            apply(&mut cx, incremental, change);
            apply(&mut cx, from_scratch, change);
        }
        history.push(changes);

        let (expected, reused_from_scratch) = draw(&mut cx, from_scratch, true);
        let (actual, reused_incrementally) = draw(&mut cx, incremental, false);
        assert_eq!(
            reused_from_scratch, 0,
            "a window that forgot its layout nodes cannot have reused any"
        );
        reused += reused_incrementally;

        if actual != expected {
            let first = actual
                .iter()
                .zip(&expected)
                .position(|(actual, expected)| actual != expected)
                .unwrap_or(actual.len().min(expected.len()));
            let excerpt = |lines: &[String]| {
                lines
                    .iter()
                    .enumerate()
                    .skip(first.saturating_sub(2))
                    .take(5)
                    .map(|(ix, line)| format!("  {ix}: {line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let history = history
                .iter()
                .enumerate()
                .map(|(step, changes)| format!("  {step}: {changes:?}"))
                .collect::<Vec<_>>()
                .join("\n");
            panic!(
                "seed {seed}, step {step}: the incremental frame differs from the frame drawn \
                 from scratch at line {first} ({} lines against {})\n\
                 incremental:\n{}\nfrom scratch:\n{}\nchanges so far:\n{history}",
                actual.len(),
                expected.len(),
                excerpt(&actual),
                excerpt(&expected),
            );
        }
    }
    reused
}

#[test]
fn incremental_frames_match_frames_drawn_from_scratch() {
    let reused: u64 = (0..24).map(|seed| run(seed, 60)).sum();
    assert!(
        reused > 0,
        "the incremental window never reused a layout node, so nothing was compared"
    );
}
