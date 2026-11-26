//! A window drawn at a zoom, as a browser's page zoom draws a page.
//!
//! Everything here is sized in pixels, as most interfaces size their borders,
//! paddings and icons, so `Window::set_rem_size` would leave it as it is.
//! `Window::set_zoom` draws all of it larger, sharp, and still answers clicks
//! where it is drawn. Click the buttons, and the box below them.

extern crate gpui_fast as gpui;
extern crate gpui_platform_fast as gpui_platform;

use gpui::{
    App, Bounds, ClickEvent, Context, SharedString, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, rgb, size,
};

/// From 50% to 300%, a quarter at a time.
fn zoom_by(window: &mut Window, cx: &mut App, step: f32) {
    let zoom = if step == 0. {
        1.
    } else {
        (window.zoom() + step).clamp(0.5, 3.)
    };
    window.set_zoom(zoom, cx);
}

struct Example {
    clicks: usize,
}

impl Example {
    fn button(
        id: &'static str,
        label: &'static str,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .px(px(12.))
            .py(px(6.))
            .border(px(1.))
            .border_color(rgb(0x888888))
            .rounded(px(6.))
            .bg(rgb(0x2a2a2a))
            .hover(|style| style.bg(rgb(0x3a3a3a)))
            .cursor_pointer()
            .child(label)
            .on_click(on_click)
    }
}

impl Render for Example {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let zoom: SharedString = format!("{:.0}%", window.zoom() * 100.).into();
        div()
            .id("root")
            .size_full()
            .flex()
            .flex_col()
            .gap(px(16.))
            .p(px(24.))
            .bg(rgb(0x1e1e1e))
            .text_color(rgb(0xe0e0e0))
            .text_size(px(14.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(Self::button("out", "−", |_, window, cx| {
                        zoom_by(window, cx, -0.25)
                    }))
                    .child(div().w(px(56.)).text_center().child(zoom))
                    .child(Self::button("in", "+", |_, window, cx| {
                        zoom_by(window, cx, 0.25)
                    }))
                    .child(Self::button("reset", "Actual size", |_, window, cx| {
                        zoom_by(window, cx, 0.)
                    })),
            )
            .child(
                div()
                    .flex()
                    .gap(px(12.))
                    .child(div().size(px(48.)).rounded(px(8.)).bg(rgb(0x3b82f6)))
                    .child(div().size(px(48.)).rounded(px(24.)).bg(rgb(0x22c55e)))
                    .child(
                        div()
                            .size(px(48.))
                            .border(px(4.))
                            .border_color(rgb(0xf59e0b))
                            .shadow_md(),
                    ),
            )
            .child(
                div()
                    .id("target")
                    .w(px(240.))
                    .p(px(12.))
                    .border(px(2.))
                    .border_dashed()
                    .border_color(rgb(0x888888))
                    .cursor_pointer()
                    .child(format!("Clicked {} times", self.clicks))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.clicks += 1;
                        cx.notify();
                    })),
            )
    }
}

fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(640.), px(420.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Example { clicks: 0 }),
        )
        .unwrap();
        cx.activate(true);
    });
}
