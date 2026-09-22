//! Layout benchmarks.
//!
//! Each benchmark redraws the same watchlist view every frame, varying only
//! what changed since the previous frame. Comparing the variants isolates what
//! the layout engine charges for a given kind of change: `unchanged` is the
//! floor, `colors` changes nothing the layout engine can see, `text` changes
//! leaf measurements, and `rows` changes the shape of the tree.

use gpui::BenchAppContext;
use gpui_benchmarks::{Mutation, QuoteTable, report_layout_stats};

/// Row counts to sweep. Small enough to stay interactive, large enough that
/// per-node costs dominate per-frame fixed costs.
fn row_counts() -> Vec<usize> {
    vec![50, 200, 800]
}

fn run(cx: &mut BenchAppContext, rows: usize, mutation: Mutation, label: &str) {
    let mut window = cx.add_empty_window();
    let view = window.update(|window, cx| {
        window.replace_root(cx, |_, _| QuoteTable::new(rows, mutation))
    });
    window.update(|window, _| window.reset_layout_stats());

    window.app_context().bench_renderer(view, |table, _, cx| {
        table.tick();
        cx.notify();
    });

    let stats = window.update(|window, _| window.layout_stats());
    report_layout_stats(&format!("{label}/{rows}"), stats);
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

gpui::bench_group!(benches, layout_unchanged, layout_colors, layout_text, layout_rows);
gpui::bench_main!(benches);
