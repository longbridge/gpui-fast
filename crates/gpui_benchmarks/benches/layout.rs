//! Layout benchmarks.
//!
//! Each benchmark redraws the same watchlist view every frame, varying only
//! what changed since the previous frame. Comparing the variants isolates what
//! the layout engine charges for a given kind of change: `unchanged` is the
//! floor, `colors` changes nothing the layout engine can see, `text` changes
//! leaf measurements, and `rows` changes the shape of the tree.

use gpui::BenchAppContext;
use gpui_benchmarks::{CountingAllocator, Mutation, QuoteTable, allocations, report_layout_stats};

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Row counts to sweep. Small enough to stay interactive, large enough that
/// per-node costs dominate per-frame fixed costs.
fn row_counts() -> Vec<usize> {
    vec![50, 200, 800]
}

fn run(cx: &mut BenchAppContext, rows: usize, mutation: Mutation, label: &str) {
    run_keyed(cx, rows, mutation, false, label)
}

fn run_keyed(cx: &mut BenchAppContext, rows: usize, mutation: Mutation, keyed: bool, label: &str) {
    let mut window = cx.add_empty_window();
    let view = window.update(|window, cx| {
        window.replace_root(cx, |_, _| QuoteTable::new(rows, mutation).keyed(keyed))
    });
    window.update(|window, _| window.reset_layout_stats());
    let allocations_before = allocations();

    window.app_context().bench_renderer(view, |table, _, cx| {
        table.tick();
        cx.notify();
    });

    let allocations_after = allocations();
    let stats = window.update(|window, _| window.layout_stats());
    report_layout_stats(
        &format!("{label}/{rows}"),
        stats,
        (
            allocations_after.0 - allocations_before.0,
            allocations_after.1 - allocations_before.1,
        ),
    );
}

#[gpui::bench(inputs = row_counts(), group = "layout", input_name = "unchanged", sample_size = 20)]
fn layout_unchanged(rows: &usize, cx: &mut BenchAppContext) {
    run(cx, *rows, Mutation::None, "unchanged");
}

#[gpui::bench(inputs = row_counts(), group = "layout", input_name = "colors", sample_size = 20)]
fn layout_colors(rows: &usize, cx: &mut BenchAppContext) {
    run(cx, *rows, Mutation::Colors, "colors");
}

#[gpui::bench(inputs = row_counts(), group = "layout", input_name = "text", sample_size = 20)]
fn layout_text(rows: &usize, cx: &mut BenchAppContext) {
    run(cx, *rows, Mutation::Text, "text");
}

#[gpui::bench(inputs = row_counts(), group = "layout", input_name = "rows", sample_size = 20)]
fn layout_rows(rows: &usize, cx: &mut BenchAppContext) {
    run(cx, *rows, Mutation::Rows, "rows");
}

/// Inserting and removing at the *front* of a list shifts every row that
/// follows. A row identified only by its index among its siblings is a
/// different row afterwards as far as the layout engine can tell.
#[gpui::bench(inputs = row_counts(), group = "layout", input_name = "rows_at_head", sample_size = 20)]
fn layout_rows_at_head(rows: &usize, cx: &mut BenchAppContext) {
    run(cx, *rows, Mutation::RowsAtHead, "rows_at_head");
}

/// The same workload with each row carrying an `ElementId` of its own, which is
/// what lets a row keep its layout nodes as it slides down the list.
#[gpui::bench(inputs = row_counts(), group = "layout", input_name = "rows_at_head_keyed", sample_size = 20)]
fn layout_rows_at_head_keyed(rows: &usize, cx: &mut BenchAppContext) {
    run_keyed(cx, *rows, Mutation::RowsAtHead, true, "rows_at_head_keyed");
}

/// The same interface either way: a live table beside a panel that never
/// changes. The only difference is whether that panel is embedded as a cached
/// view, so the gap between these two is what skipping an unchanged view's
/// render is worth.
fn run_panel(cx: &mut BenchAppContext, rows: usize, cached: bool, label: &str) {
    let mut window = cx.add_empty_window();
    let view = window.update(|window, cx| {
        window.replace_root(cx, |_, cx| {
            QuoteTable::new(rows, Mutation::Text).with_static_panel(rows, cached, cx)
        })
    });
    window.update(|window, _| window.reset_layout_stats());
    let allocations_before = allocations();

    window.app_context().bench_renderer(view, |table, _, cx| {
        table.tick();
        cx.notify();
    });

    let allocations_after = allocations();
    let stats = window.update(|window, _| window.layout_stats());
    report_layout_stats(
        &format!("{label}/{rows}"),
        stats,
        (
            allocations_after.0 - allocations_before.0,
            allocations_after.1 - allocations_before.1,
        ),
    );
}

#[gpui::bench(inputs = row_counts(), group = "layout", input_name = "panel_plain", sample_size = 20)]
fn layout_panel_plain(rows: &usize, cx: &mut BenchAppContext) {
    run_panel(cx, *rows, false, "panel_plain");
}

#[gpui::bench(inputs = row_counts(), group = "layout", input_name = "panel_cached", sample_size = 20)]
fn layout_panel_cached(rows: &usize, cx: &mut BenchAppContext) {
    run_panel(cx, *rows, true, "panel_cached");
}

gpui::bench_group!(
    benches,
    layout_unchanged,
    layout_colors,
    layout_text,
    layout_rows,
    layout_rows_at_head,
    layout_rows_at_head_keyed,
    layout_panel_plain,
    layout_panel_cached
);
gpui::bench_main!(benches);
