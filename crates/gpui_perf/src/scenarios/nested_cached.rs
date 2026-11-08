//! Cached views nested in cached views: a dashboard of panels, each a cached
//! view with labels of its own and cells that are cached views too, one cell
//! changing every frame, as a status display or a game HUD built from cached
//! widgets does.
//!
//! Drawing the changed cell should not build the panel around it again.

use gpui::{
    AnyView, App, AppContext as _, Context, Entity, Hsla, IntoElement, Render, StyleRefinement,
    Window, div, hsla, prelude::*, px,
};

pub fn scenarios() -> Vec<Box<dyn crate::Scenario>> {
    vec![Box::new(NestedCached)]
}

const PANELS: usize = 12;
const CELLS: usize = 16;
const LABELS: usize = 48;

const PANEL_BG: Hsla = hsla(0.6, 0.12, 0.12, 1.0);
const TEXT: Hsla = hsla(0., 0., 0.9, 1.0);

struct NestedCached;

impl crate::Scenario for NestedCached {
    fn name(&self) -> &'static str {
        "nested-cached-cells"
    }

    fn description(&self) -> &'static str {
        "12 cached panels of 48 labels and 16 cached cells each, one cell changing every frame."
    }

    fn build(&self, _: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| Dashboard {
            panels: (0..PANELS)
                .map(|panel| {
                    cx.new(|cx| Panel {
                        labels: (0..LABELS)
                            .map(|label| format!("Panel {panel} stat {label}: {}", label * 7))
                            .collect(),
                        cells: (0..CELLS)
                            .map(|cell| {
                                cx.new(|_| Cell {
                                    value: panel * CELLS + cell,
                                })
                            })
                            .collect(),
                    })
                })
                .collect(),
        })
        .into()
    }

    fn step(&self, root: &AnyView, frame: usize, _: &mut Window, cx: &mut App) {
        let dashboard: Entity<Dashboard> = root.clone().downcast().unwrap();
        let panel = dashboard.read(cx).panels[frame % PANELS].clone();
        let cell = panel.read(cx).cells[frame / PANELS % CELLS].clone();
        cell.update(cx, |cell, cx| {
            cell.value += 1;
            cx.notify();
        });
    }
}

struct Dashboard {
    panels: Vec<Entity<Panel>>,
}

impl Render for Dashboard {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_wrap()
            .gap(px(8.))
            .p(px(8.))
            .bg(hsla(0., 0., 0.05, 1.))
            .text_color(TEXT)
            .text_size(px(12.))
            .children(self.panels.iter().map(|panel| {
                panel
                    .clone()
                    .cached(StyleRefinement::default().w(px(300.)).h(px(480.)))
            }))
    }
}

struct Panel {
    labels: Vec<String>,
    cells: Vec<Entity<Cell>>,
}

impl Render for Panel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(4.))
            .p(px(6.))
            .bg(PANEL_BG)
            .rounded(px(4.))
            .child(
                div().flex().flex_wrap().gap(px(2.)).children(
                    self.labels
                        .iter()
                        .map(|label| div().w(px(140.)).child(label.clone())),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(4.))
                    .children(self.cells.iter().map(|cell| {
                        cell.clone()
                            .cached(StyleRefinement::default().w(px(64.)).h(px(28.)))
                    })),
            )
    }
}

struct Cell {
    value: usize,
}

impl Render for Cell {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(hsla((self.value % 12) as f32 / 12., 0.4, 0.3, 1.0))
            .child(format!("{}", self.value))
    }
}
