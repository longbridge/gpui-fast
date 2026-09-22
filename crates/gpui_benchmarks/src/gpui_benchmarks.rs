//! Fixtures for GPUI benchmarks.
//!
//! [`QuoteTable`] is a deliberately ordinary application view: a scrolling table
//! of rows, each built from nested flex containers with a mix of fixed-width
//! columns and content-sized ones. It exists to exercise the layout engine with
//! a tree whose *shape* is stable across frames while its *content* is not,
//! which is the common case in a live application and the case the layout
//! engine has the most room to exploit.

use gpui::{
    AppContext, Context, Entity, Hsla, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StyleRefinement, Styled, Window, div, hsla, px,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

/// How a frame differs from the one before it.
///
/// Each variant isolates one kind of change so a benchmark can attribute layout
/// cost to it rather than to an undifferentiated "redraw".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mutation {
    /// The model is untouched; the view is only asked to redraw.
    None,
    /// Colors change. No text and no geometry changes, so nothing that reaches
    /// the layout engine's idea of the tree changes at all.
    Colors,
    /// Cell text changes. Styles and tree shape are untouched, but the measured
    /// size of some leaves may differ.
    Text,
    /// Rows are added and removed at the end, changing the shape of the tree
    /// without disturbing the rows already in it.
    Rows,
    /// Rows are added and removed at the *front*, so every remaining row shifts
    /// position. Rows keyed only by position are rebuilt wholesale; rows
    /// carrying a stable [`ElementId`](gpui::ElementId) should not be.
    RowsAtHead,
}

struct Row {
    /// Stable across the row's life, independent of where it currently sits.
    id: u64,
    symbol: SharedString,
    name: SharedString,
    last: SharedString,
    change: SharedString,
    volume: SharedString,
    up: bool,
}

impl Row {
    fn new(index: usize) -> Self {
        let mut row = Row {
            id: index as u64,
            symbol: SharedString::default(),
            name: SharedString::default(),
            last: SharedString::default(),
            change: SharedString::default(),
            volume: SharedString::default(),
            up: index % 3 != 0,
        };
        row.symbol = format!("{:04}.HK", (index * 37) % 9999).into();
        row.name = NAMES[index % NAMES.len()].into();
        row.retick(index as u64);
        row
    }

    /// Rewrites the numeric cells the way a quote feed would.
    fn retick(&mut self, tick: u64) {
        let seed = tick
            .wrapping_mul(2_654_435_761)
            .wrapping_add(self.symbol.len() as u64);
        let price = 10.0 + (seed % 90_000) as f64 / 1000.0;
        let change = (seed % 2_000) as f64 / 100.0 - 10.0;
        self.last = format!("{price:.3}").into();
        self.change = format!("{change:+.2}%").into();
        self.volume = format!("{}.{}M", seed % 900 + 10, seed % 10).into();
        self.up = change >= 0.0;
    }
}

const NAMES: &[&str] = &[
    "Tencent Holdings",
    "Alibaba Group",
    "HSBC Holdings",
    "Meituan",
    "China Mobile",
    "AIA Group",
    "Xiaomi Corporation",
    "BYD Company",
    "Ping An Insurance",
    "JD.com",
    "NetEase",
    "Li Auto",
];

/// A panel that never changes, standing in for the parts of an application that
/// are redrawn every frame despite having nothing new to say: sidebars,
/// toolbars, status bars, inactive tabs.
pub struct StaticPanel {
    entries: usize,
}

impl Render for StaticPanel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .w(px(240.))
            .children((0..self.entries).map(|index| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .h(px(28.))
                    .border_b_1()
                    .border_color(BORDER)
                    .child(div().w(px(6.)).h(px(6.)).rounded_full().bg(FG_MUTED))
                    .child(div().flex_1().child(NAMES[index % NAMES.len()]))
                    .child(div().w(px(48.)).text_right().text_xs().child("--"))
            }))
    }
}

/// A watchlist-shaped view: a toolbar above a table of quote rows.
pub struct QuoteTable {
    rows: Vec<Row>,
    base_row_count: usize,
    tick: u64,
    next_row_id: u64,
    mutation: Mutation,
    /// Whether each row carries an [`ElementId`](gpui::ElementId) of its own.
    ///
    /// Without one a row is identified by its index among its siblings, which
    /// is only stable while nothing is inserted ahead of it.
    keyed: bool,
    /// The half of the interface that has nothing new to say each frame.
    panel: Option<Entity<StaticPanel>>,
    /// Whether that half is embedded as a cached view, which decides whether
    /// its subtree is rendered again every frame or reused.
    cache_panel: bool,
}

impl QuoteTable {
    /// Builds a table of `row_count` rows that will apply `mutation` on each tick.
    pub fn new(row_count: usize, mutation: Mutation) -> Self {
        QuoteTable {
            rows: (0..row_count).map(Row::new).collect(),
            base_row_count: row_count,
            tick: 0,
            next_row_id: row_count as u64,
            mutation,
            keyed: false,
            panel: None,
            cache_panel: false,
        }
    }

    /// Adds a panel of `entries` rows that never changes, and says whether to
    /// embed it as a cached view.
    pub fn with_static_panel(
        mut self,
        entries: usize,
        cached: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        self.panel = Some(cx.new(|_| StaticPanel { entries }));
        self.cache_panel = cached;
        self
    }

    /// Gives every row an `ElementId` derived from its own identity rather than
    /// from where it currently sits.
    pub fn keyed(mut self, keyed: bool) -> Self {
        self.keyed = keyed;
        self
    }

    fn new_row(&mut self) -> Row {
        let mut row = Row::new(self.rows.len());
        row.id = self.next_row_id;
        self.next_row_id += 1;
        row
    }

    /// Advances the model by one frame's worth of change.
    pub fn tick(&mut self) {
        self.tick += 1;
        match self.mutation {
            Mutation::None => {}
            Mutation::Colors => {
                for row in &mut self.rows {
                    row.up = !row.up;
                }
            }
            Mutation::Text => {
                let tick = self.tick;
                for row in &mut self.rows {
                    row.retick(tick);
                }
            }
            Mutation::Rows => {
                // Oscillate around the configured size so the row count, and
                // therefore the shape of the tree, differs every frame.
                let target = self.base_row_count - (self.tick % 8) as usize;
                while self.rows.len() > target {
                    self.rows.pop();
                }
                while self.rows.len() < target {
                    let row = self.new_row();
                    self.rows.push(row);
                }
            }
            Mutation::RowsAtHead => {
                let target = self.base_row_count - (self.tick % 8) as usize;
                while self.rows.len() > target {
                    self.rows.remove(0);
                }
                while self.rows.len() < target {
                    let row = self.new_row();
                    self.rows.insert(0, row);
                }
            }
        }
    }
}

const FG: Hsla = hsla(0.0, 0.0, 0.85, 1.0);
const FG_MUTED: Hsla = hsla(0.0, 0.0, 0.55, 1.0);
const BG: Hsla = hsla(0.62, 0.15, 0.12, 1.0);
const BORDER: Hsla = hsla(0.62, 0.10, 0.22, 1.0);
const UP: Hsla = hsla(0.38, 0.55, 0.55, 1.0);
const DOWN: Hsla = hsla(0.99, 0.60, 0.60, 1.0);

impl Render for QuoteTable {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let panel = self.panel.clone().map(|panel| {
            if self.cache_panel {
                // A definite size is what the current API asks for in exchange
                // for skipping the subtree's render.
                panel
                    .cached(StyleRefinement::default().w(px(240.)).h(px(4000.)))
                    .into_any_element()
            } else {
                panel.into_any_element()
            }
        });

        div()
            .flex()
            .flex_row()
            .size_full()
            .bg(BG)
            .text_color(FG)
            .children(panel)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_3()
                            .px_4()
                            .py_2()
                            .border_b_1()
                            .border_color(BORDER)
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("Watchlist"),
                            )
                            .child(div().flex_1())
                            .children(["All", "HK", "US", "A"].map(|label| {
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .bg(BORDER)
                                    .text_sm()
                                    .child(label)
                            })),
                    )
                    .children(self.rows.iter().map(|row| {
                        let tone = if row.up { UP } else { DOWN };
                        let row_element = div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .px_4()
                            .py_1()
                            .border_b_1()
                            .border_color(BORDER)
                            .child(div().w(px(6.)).h(px(6.)).rounded_full().bg(tone))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .w(px(110.))
                                    .child(row.symbol.clone())
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(FG_MUTED)
                                            .child(row.name.clone()),
                                    ),
                            )
                            // Content-sized: text here genuinely participates in layout.
                            .child(
                                div()
                                    .flex_1()
                                    .text_xs()
                                    .text_color(FG_MUTED)
                                    .child(row.name.clone()),
                            )
                            // Fixed-width columns: text here cannot move anything.
                            .child(div().w(px(96.)).text_right().child(row.last.clone()))
                            .child(
                                div()
                                    .w(px(84.))
                                    .text_right()
                                    .text_color(tone)
                                    .child(row.change.clone()),
                            )
                            .child(
                                div()
                                    .w(px(84.))
                                    .text_right()
                                    .text_color(FG_MUTED)
                                    .child(row.volume.clone()),
                            );
                        if self.keyed {
                            row_element.id(("row", row.id)).into_any_element()
                        } else {
                            row_element.into_any_element()
                        }
                    })),
            )
    }
}

/// Prints the layout work behind a benchmark, averaged per frame.
///
/// Criterion reports how long a frame took; these counters say where the time
/// went, which is what tells a real improvement apart from a benchmark that
/// merely stopped doing the work it was supposed to measure.
pub fn report_layout_stats(label: &str, stats: gpui::LayoutStats, allocations: (u64, u64)) {
    let frames = stats.frames.max(1);
    let per_frame = |n: u64| n as f64 / frames as f64;
    let touched = stats.nodes_created + stats.nodes_reused;
    let reuse_pct = if touched == 0 {
        0.0
    } else {
        100.0 * stats.nodes_reused as f64 / touched as f64
    };
    println!(
        "\n  layout/{label}: {frames} frames\n    \
         nodes/frame       {:>9.1} created  {:>9.1} reused  ({reuse_pct:.1}% reused)\n    \
         writes/frame      {:>9.1} style    {:>9.1} children  {:>9.1} measure-rebind\n    \
         style compares    {:>9.1}/frame\n    \
         measure calls     {:>9.1}/frame  {:>9.1}µs/frame ({:.0}% answered from a kept result)\n    \
         taffy compute     {:>9.1}µs/frame ({:.1} calls/frame), of which {:.0}% is measuring\n    \
         frame phases      {:>9.1}µs build  {:>9.1}µs prepaint  {:>9.1}µs paint\n    \
         allocations       {:>9.1}/frame  {:>9.1} KiB/frame",
        per_frame(stats.nodes_created),
        per_frame(stats.nodes_reused),
        per_frame(stats.style_writes),
        per_frame(stats.children_writes),
        per_frame(stats.measure_rebinds),
        per_frame(stats.style_compares),
        per_frame(stats.measure_calls),
        stats.measure_time.as_secs_f64() * 1e6 / frames as f64,
        if stats.measure_calls == 0 {
            0.0
        } else {
            100.0 * stats.measure_reuses as f64 / stats.measure_calls as f64
        },
        stats.compute_layout_time.as_secs_f64() * 1e6 / frames as f64,
        per_frame(stats.compute_layout_calls),
        if stats.compute_layout_time.is_zero() {
            0.0
        } else {
            100.0 * stats.measure_time.as_secs_f64() / stats.compute_layout_time.as_secs_f64()
        },
        stats.build_time.as_secs_f64() * 1e6 / frames as f64,
        stats.prepaint_time.as_secs_f64() * 1e6 / frames as f64,
        stats.paint_time.as_secs_f64() * 1e6 / frames as f64,
        allocations.0 as f64 / frames as f64,
        allocations.1 as f64 / 1024.0 / frames as f64,
    );
}

/// Counts every allocation the process makes, so a frame's cost can be split
/// into work the allocator did and work it did not.
pub struct CountingAllocator;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        ALLOCATED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Allocations and bytes handed out so far.
pub fn allocations() -> (u64, u64) {
    (
        ALLOCATIONS.load(Ordering::Relaxed),
        ALLOCATED_BYTES.load(Ordering::Relaxed),
    )
}
