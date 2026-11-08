//! A screen of focusable elements, a few of them changing every frame, as a
//! form or a toolbar-heavy screen is. Run with `GPUI_A11Y_ACTIVE=1` to draw
//! every frame with accessibility active, as with a screen reader on.

use gpui::{
    AnyView, App, AppContext as _, Context, Entity, FocusHandle, IntoElement, Render, Window, div,
    hsla, prelude::*, px,
};

pub fn scenarios() -> Vec<Box<dyn crate::Scenario>> {
    vec![Box::new(Focusable)]
}

const GROUPS: usize = 50;
const CELLS: usize = 20;

struct Focusable;

impl crate::Scenario for Focusable {
    fn name(&self) -> &'static str {
        "focusable-cells"
    }

    fn description(&self) -> &'static str {
        "1,000 focusable cells in 50 views of 20, one view changing every frame."
    }

    fn build(&self, _: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|cx| Screen {
            groups: (0..GROUPS)
                .map(|_| {
                    cx.new(|cx| Group {
                        cells: (0..CELLS)
                            .map(|_| cx.focus_handle().tab_stop(true))
                            .collect(),
                        tick: 0,
                    })
                })
                .collect(),
        })
        .into()
    }

    fn step(&self, root: &AnyView, frame: usize, _: &mut Window, cx: &mut App) {
        let screen: Entity<Screen> = root.clone().downcast().unwrap();
        let group = screen.read(cx).groups[frame % GROUPS].clone();
        group.update(cx, |group, cx| {
            group.tick += 1;
            cx.notify();
        });
    }
}

struct Screen {
    groups: Vec<Entity<Group>>,
}

impl Render for Screen {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_wrap()
            .gap(px(4.))
            .p(px(4.))
            .children(self.groups.iter().cloned())
    }
}

struct Group {
    cells: Vec<FocusHandle>,
    tick: usize,
}

impl Render for Group {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let tick = self.tick;
        div().flex().flex_wrap().w(px(120.)).gap(px(2.)).children(
            self.cells.iter().enumerate().map(|(ix, cell)| {
                div().id(ix).track_focus(cell).size(px(20.)).bg(hsla(
                    ((ix + tick) % 12) as f32 / 12.,
                    0.4,
                    0.4,
                    1.,
                ))
            }),
        )
    }
}
