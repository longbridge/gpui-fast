//! A WebKitGTK page in a native surface composed between GPUI's content and
//! its overlays, with window composition (zed-industries/zed#62379), on X11.
//! A menu, a popover with a shadow and a dialog with a translucent backdrop
//! paint above the page: on X11 the GPUI content above a native surface goes
//! into an overlay window that the compositing manager blends over the page
//! (XWayland always has one). Hovering the menu checks that the overlay
//! window delivers input to GPUI, and clicking the page closes the overlays.
//!
//! WebKitGTK only embeds into X11 windows, so the example runs on X11, or on
//! XWayland in a Wayland session.
//!
//! ```text
//! cargo run -p gpui_perf --example linux_webview
//! ```

extern crate gpui_fast as gpui;
extern crate gpui_platform_fast as gpui_platform;

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
fn main() {
    eprintln!("The linux_webview example is only available on Linux.");
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn main() {
    app::run();
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod app {
    use std::{cell::Cell, rc::Rc, time::Duration};

    use anyhow::{Context as _, anyhow};
    use gpui::{
        App, Bounds, Context, Div, MouseButton, Pixels, SharedString, Stateful, Window,
        WindowBounds, WindowCompositionSurface, WindowOptions, WindowingModes, canvas, deferred,
        div, prelude::*, px, rgb, rgba, size,
    };
    use gtk::{glib::Propagation, prelude::WidgetExt as _};
    use raw_window_handle::{
        HandleError, HasWindowHandle, RawWindowHandle, WindowHandle, XlibWindowHandle,
    };
    use wry::{
        Rect, WebViewBuilder, WebViewExtUnix as _,
        dpi::{PhysicalPosition, PhysicalSize},
    };

    const URL: &str = "https://github.com/longbridge/gpui-fast";

    #[derive(Clone, Copy, PartialEq)]
    enum Overlay {
        None,
        Menu,
        Popover,
        Dialog,
    }

    struct LinuxWebView {
        webview: Rc<wry::WebView>,
        surface: WindowCompositionSurface,
        overlay: Overlay,
        /// The GPUI scale factor the page zoom was last matched to.
        scale_factor: Rc<Cell<f32>>,
    }

    /// The native surface's X11 window as the Xlib parent Wry requires.
    struct XlibParent(XlibWindowHandle);

    impl HasWindowHandle for XlibParent {
        fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
            // SAFETY: The surface's window outlives the webview built in it.
            Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Xlib(self.0)) })
        }
    }

    impl LinuxWebView {
        fn new(window: &mut Window, cx: &mut Context<Self>) -> anyhow::Result<Self> {
            let surface = window
                .enable_window_composition()?
                .create_native_surface()?;
            let handle = surface.platform_surface()?.platform_handle()?;
            let handle = *handle
                .downcast::<RawWindowHandle>()
                .map_err(|_| anyhow!("the native surface has no window handle"))?;
            let RawWindowHandle::Xcb(handle) = handle else {
                anyhow::bail!("WebKitGTK needs an X11 window; run on X11 or XWayland");
            };
            let mut parent = XlibWindowHandle::new(handle.window.get().into());
            parent.visual_id = handle.visual_id.map_or(0, |id| id.get().into());
            let webview = WebViewBuilder::new()
                .with_url(URL)
                .build_as_child(&XlibParent(parent))
                .context("building the WebKitGTK view")?;

            // GPUI does not see clicks on the page, so close the overlays here.
            let this = cx.weak_entity();
            let async_cx = cx.to_async();
            webview.webview().connect_button_press_event(move |_, _| {
                let this = this.clone();
                async_cx
                    .spawn(async move |cx| {
                        this.update(cx, |this, cx| this.set_overlay(Overlay::None, cx))
                    })
                    .detach();
                Propagation::Proceed
            });

            Ok(Self {
                webview: Rc::new(webview),
                surface,
                overlay: Overlay::None,
                scale_factor: Rc::default(),
            })
        }

        fn set_overlay(&mut self, overlay: Overlay, cx: &mut Context<Self>) {
            self.overlay = overlay;
            cx.notify();
        }

        fn toggle(&mut self, overlay: Overlay, cx: &mut Context<Self>) {
            let overlay = if self.overlay == overlay {
                Overlay::None
            } else {
                overlay
            };
            self.set_overlay(overlay, cx);
        }

        /// Places the native surface over the page's layout bounds and fills it
        /// with the webview.
        fn place(
            webview: &wry::WebView,
            surface: &WindowCompositionSurface,
            zoomed_for: &Cell<f32>,
            bounds: Bounds<Pixels>,
            window: &Window,
        ) -> anyhow::Result<()> {
            let scale_factor = window.scale_factor();
            let device = bounds.to_device_pixels(scale_factor);
            surface.platform_surface()?.set_bounds(device)?;
            webview.set_bounds(Rect {
                position: PhysicalPosition::new(0, 0).into(),
                size: PhysicalSize::new(device.size.width.0, device.size.height.0).into(),
            })?;
            // GDK only scales by integers; zoom the page to GPUI's scale.
            if zoomed_for.replace(scale_factor) != scale_factor {
                let gdk_scale = webview.webview().scale_factor().max(1);
                webview.zoom(f64::from(scale_factor) / f64::from(gdk_scale))?;
            }
            Ok(())
        }
    }

    fn button(id: &'static str, label: &'static str) -> Stateful<Div> {
        div()
            .id(id)
            .px_3()
            .py_1()
            .border_1()
            .border_color(rgb(0xd4d4d8))
            .rounded_md()
            .bg(rgb(0xffffff))
            .hover(|style| style.bg(rgb(0xf4f4f5)))
            .child(label)
    }

    fn panel() -> Div {
        div()
            .p_1()
            .border_1()
            .border_color(rgb(0xd4d4d8))
            .rounded_lg()
            .bg(rgb(0xffffff))
            .shadow_lg()
    }

    fn menu_item(id: &'static str, label: impl Into<SharedString>) -> Stateful<Div> {
        div()
            .id(id)
            .px_3()
            .py_1()
            .rounded_md()
            .hover(|style| style.bg(rgb(0xe4e4e7)))
            .child(label.into())
    }

    impl Render for LinuxWebView {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let webview = self.webview.clone();
            let surface = self.surface.clone();
            let scale_factor = self.scale_factor.clone();

            div()
                .relative()
                .size_full()
                .flex()
                .flex_col()
                .bg(rgb(0xf4f4f5))
                .text_color(rgb(0x18181b))
                .text_sm()
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .p_2()
                        .child(
                            button("menu", "Menu").on_click(
                                cx.listener(|this, _, _, cx| this.toggle(Overlay::Menu, cx)),
                            ),
                        )
                        .child(button("popover", "Popover").on_click(
                            cx.listener(|this, _, _, cx| this.toggle(Overlay::Popover, cx)),
                        ))
                        .child(button("dialog", "Dialog").on_click(
                            cx.listener(|this, _, _, cx| this.set_overlay(Overlay::Dialog, cx)),
                        )),
                )
                .child(
                    div()
                        .flex_1()
                        .mx_2()
                        .mb_2()
                        .border_1()
                        .border_color(rgb(0xd4d4d8))
                        .child(
                            canvas(
                                move |bounds, window, _| {
                                    if let Err(error) = Self::place(
                                        &webview,
                                        &surface,
                                        &scale_factor,
                                        bounds,
                                        window,
                                    ) {
                                        log::error!("placing the webview: {error:#}");
                                    }
                                },
                                |_, _, _, _| {},
                            )
                            .size_full(),
                        ),
                )
                .when(self.overlay == Overlay::Menu, |this| {
                    let reload = self.webview.clone();
                    this.child(deferred(
                        panel()
                            .id("menu-panel")
                            .absolute()
                            .top(px(44.))
                            .left(px(8.))
                            .w(px(200.))
                            .occlude()
                            .on_mouse_down_out(
                                cx.listener(|this, _, _, cx| this.set_overlay(Overlay::None, cx)),
                            )
                            .children((1..=5).map(|ix| {
                                menu_item(
                                    ["item-1", "item-2", "item-3", "item-4", "item-5"][ix - 1],
                                    format!("Hover item {ix}"),
                                )
                            }))
                            .child(menu_item("reload", "Reload").on_click(cx.listener(
                                move |this, _, _, cx| {
                                    reload.reload().ok();
                                    this.set_overlay(Overlay::None, cx);
                                },
                            )))
                            .child(menu_item("open-dialog", "Open Dialog").on_click(
                                cx.listener(|this, _, _, cx| this.set_overlay(Overlay::Dialog, cx)),
                            )),
                    ))
                })
                .when(self.overlay == Overlay::Popover, |this| {
                    this.child(deferred(
                        panel()
                            .id("popover-panel")
                            .absolute()
                            .top(px(44.))
                            .left(px(80.))
                            .w(px(320.))
                            .h(px(200.))
                            .p_3()
                            .occlude()
                            .on_mouse_down_out(
                                cx.listener(|this, _, _, cx| this.set_overlay(Overlay::None, cx)),
                            )
                            .child("Its shadow and rounded corners blend over the page."),
                    ))
                })
                .when(self.overlay == Overlay::Dialog, |this| {
                    this.child(
                        deferred(
                            div()
                                .id("dialog-backdrop")
                                .absolute()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .bg(rgba(0x00000066))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        this.set_overlay(Overlay::None, cx)
                                    }),
                                )
                                .child(
                                    panel()
                                        .id("dialog-panel")
                                        .w(px(360.))
                                        .p_4()
                                        .flex()
                                        .flex_col()
                                        .gap_3()
                                        .occlude()
                                        .child("The page stays visible, dimmed by the backdrop.")
                                        .child(button("close", "Close").on_click(cx.listener(
                                            |this, _, _, cx| this.set_overlay(Overlay::None, cx),
                                        ))),
                                ),
                        )
                        .with_priority(1),
                    )
                })
        }
    }

    pub fn run() {
        // Wry downcasts the default GDK display to X11.
        gtk::gdk::set_allowed_backends("x11");
        gtk::init().expect("failed to initialize GTK");

        gpui_platform::linux(WindowingModes::X11).run(|cx: &mut App| {
            // Drive GTK, which WebKitGTK runs on, from GPUI's event loop.
            cx.spawn(async move |cx| {
                loop {
                    while gtk::events_pending() {
                        gtk::main_iteration_do(false);
                    }
                    cx.background_executor()
                        .timer(Duration::from_millis(16))
                        .await;
                }
            })
            .detach();

            let bounds = Bounds::centered(None, size(px(1000.), px(700.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| {
                    cx.new(|cx| {
                        LinuxWebView::new(window, cx).expect("failed to create the webview")
                    })
                },
            )
            .expect("failed to open the window");
            cx.activate(true);
        });
    }
}
