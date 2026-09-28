//! The showcase `cargo run -p gpui_perf --release` opens: a component gallery
//! shaped like a real application — a sidebar of pages, a scrolled page of
//! component sections, a data table refreshed by a timer — that can scroll
//! itself, and shows what each frame costs.
//!
//! It is laid out the way GPUI Kit's story gallery is: the root view renders
//! the sidebar, a container view owns the scrolled area, and the page inside
//! it is a view of its own whose sections are plain elements. Scrolling the
//! sidebar notifies the root, scrolling the page notifies the container and
//! moves the page view, and the data table's timer changes the table without
//! notifying it, notifying the page around it instead.
//!
//! The toolbar picks what scrolls itself, and switches the data refresh and
//! retained views. Every command has a key: `1`–`4` for what scrolls, `R` for
//! the refresh, `V` for retained views, and the arrow keys to move through
//! the sidebar. The status bar shows, every half second, the frame rate, the
//! process's CPU and the main thread's, its resident memory, what build,
//! prepaint, layout and paint
//! took per frame, and how many views were built and reused per frame.
//!
//! With `--auto`, it runs every scenario with retained views on and then off,
//! prints what each cost per frame, and quits. `--only <scenario>`,
//! `--retention on|off` and `--frames <n>` narrow it down.
//!
//! Built with the `upstream` feature it runs on upstream GPUI, the
//! `gpui-pre` snapshot GPUI Kit pins, for comparison; see `backend.rs`.

mod auto;
mod backend;
mod controls;
mod metrics;
mod pages;
mod theme;

#[path = "../../../gpui/examples/example_support/fonts.rs"]
mod example_support;

use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    App, Bounds, Context, Entity, FocusHandle, FontWeight, KeyBinding, Pixels, Render,
    ScrollHandle, Subscription, WeakEntity, Window, WindowBounds, WindowOptions, actions, div,
    prelude::*, px, size,
};
use gpui_platform::application;

use auto::AutoRun;
use controls::{Tooltip, segment, segment_track, segmented, switch};
use metrics::StatusBar;
use pages::{Container, PageKind};
use theme::{Theme, theme};

actions!(
    showcase,
    [
        ScrollOff,
        ScrollSidebar,
        ScrollPage,
        ScrollTable,
        ScrollList,
        ToggleRefresh,
        ToggleRetention,
        SelectNext,
        SelectPrevious,
    ]
);

const KEY_CONTEXT: &str = "Showcase";

/// The sidebar's groups and the pages in each: as many as a large component
/// library's gallery has, so that scrolling it goes a long way.
fn groups() -> &'static [(&'static str, Vec<&'static str>)] {
    static GROUPS: std::sync::OnceLock<Vec<(&'static str, Vec<&'static str>)>> =
        std::sync::OnceLock::new();
    GROUPS.get_or_init(|| {
        const COMPONENTS: [&str; 48] = [
            "Accordion",
            "Alert",
            "Avatar",
            "Badge",
            "Breadcrumb",
            "Button",
            "Calendar",
            "Card",
            "Checkbox",
            "Clipboard",
            "Collapsible",
            "Combobox",
            "DataTable",
            "DatePicker",
            "Dialog",
            "Dropdown",
            "Editor",
            "Form",
            "GroupBox",
            "Icon",
            "Image",
            "Input",
            "Kbd",
            "Label",
            "List",
            "Menu",
            "Notification",
            "NumberInput",
            "Pagination",
            "Popover",
            "Progress",
            "Radio",
            "Rating",
            "Resizable",
            "Scrollbar",
            "Select",
            "Separator",
            "Settings",
            "Sheet",
            "Sidebar",
            "Skeleton",
            "Slider",
            "Spinner",
            "Switch",
            "Table",
            "Tabs",
            "Tag",
            "Tooltip",
        ];
        let mut groups = vec![
            (
                "Getting started",
                vec!["Introduction", "Installation", "Theming"],
            ),
            ("Components", COMPONENTS.to_vec()),
        ];
        // Further groups of examples, each an entry per component, named
        // once and kept for the life of the program.
        for group in ["Recipes", "Patterns", "Layouts", "Accessibility"] {
            let pages = COMPONENTS
                .iter()
                .map(|component| {
                    &*Box::leak(format!("{component} {}", group.to_lowercase()).into_boxed_str())
                })
                .collect();
            groups.push((group, pages));
        }
        groups
    })
}

/// Every page, in sidebar order.
fn pages() -> impl Iterator<Item = &'static str> {
    groups().iter().flat_map(|(_, pages)| pages.iter().copied())
}

fn page_count() -> usize {
    groups().iter().map(|(_, pages)| pages.len()).sum()
}

fn page_name(page: usize) -> &'static str {
    pages().nth(page).unwrap_or_default()
}

/// The page showing the data table.
fn table_page() -> usize {
    pages().position(|name| name == "DataTable").unwrap_or(0)
}

/// The page showing a list of messages.
fn list_page() -> usize {
    pages().position(|name| name == "List").unwrap_or(0)
}

fn page_kind(page: usize) -> PageKind {
    if page == table_page() {
        PageKind::Table
    } else if page == list_page() {
        PageKind::List
    } else {
        PageKind::Components
    }
}

/// The page showing components, when the table is not wanted.
const BUTTON_PAGE: usize = 8;

const SAMPLE_EVERY: Duration = Duration::from_millis(500);

/// What scrolls itself, one step every frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scroll {
    Off,
    Sidebar,
    Page,
    Table,
    List,
}

/// Opens the showcase, running every scenario and quitting if `auto`.
/// How long `--demo` scrolls each thing.
const DEMO_STEP: Duration = Duration::from_secs(6);

/// Opens the showcase. With `auto`, it runs every scenario and quits; with
/// `demo`, it scrolls the sidebar, a page, the table and the list in turn,
/// for as long as it is open, for recording or watching two GPUIs side by
/// side.
pub fn run(auto: bool, demo: bool) {
    application().run(move |cx: &mut App| {
        if !example_support::load_fonts(cx) {
            return;
        }
        cx.bind_keys([
            KeyBinding::new("1", ScrollOff, Some(KEY_CONTEXT)),
            KeyBinding::new("2", ScrollSidebar, Some(KEY_CONTEXT)),
            KeyBinding::new("3", ScrollPage, Some(KEY_CONTEXT)),
            KeyBinding::new("4", ScrollTable, Some(KEY_CONTEXT)),
            KeyBinding::new("5", ScrollList, Some(KEY_CONTEXT)),
            KeyBinding::new("r", ToggleRefresh, Some(KEY_CONTEXT)),
            KeyBinding::new("v", ToggleRetention, Some(KEY_CONTEXT)),
            KeyBinding::new("down", SelectNext, Some(KEY_CONTEXT)),
            KeyBinding::new("up", SelectPrevious, Some(KEY_CONTEXT)),
        ]);
        cx.open_window(
            WindowOptions {
                focus: true,
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1280.), px(820.)),
                    cx,
                ))),
                window_min_size: Some(size(px(800.), px(480.))),
                ..Default::default()
            },
            |window, cx| {
                Theme::follow(window, cx);
                cx.new(|cx| Showcase::new(auto, demo, window, cx))
            },
        )
        .unwrap();
        cx.activate(true);
    });
}

pub struct Showcase {
    focus_handle: FocusHandle,
    active: usize,
    sidebar_scroll: ScrollHandle,
    container: Entity<Container>,
    status_bar: Entity<StatusBar>,
    /// What scrolls, as the toolbar shows it; the driver does the scrolling.
    scroll: Scroll,
    /// Whether the table's rows are being refreshed, as the toolbar shows
    /// it. Kept here rather than read from the container, so that this view
    /// does not depend on the container, which scrolling the page notifies.
    refreshing: bool,
    driver: Rc<RefCell<Driver>>,
    _appearance: Subscription,
}

/// What steps the showcase before every frame: the scroll and the automatic
/// run. It lives outside the views and only notifies the view that owns what
/// it scrolls, as a scroll wheel or a dragged scrollbar would, so that no
/// other view counts as changed.
pub struct Driver {
    scroll: Scroll,
    direction: f32,
    auto: Option<AutoRun>,
    /// When `--demo` started, for it to scroll the sidebar, a page, the
    /// table and the list in turn, [`DEMO_STEP`] each.
    demo: Option<Instant>,
    /// Whether frames are being asked for.
    running: bool,
}

/// The views and state the driver steps.
#[derive(Clone)]
pub struct Handles {
    showcase: WeakEntity<Showcase>,
    sidebar_scroll: ScrollHandle,
    container: Entity<Container>,
    status_bar: Entity<StatusBar>,
}

/// How far each frame scrolls: as fast as a scrollbar dragged a long way in
/// one go.
fn scroll_speed(_: Scroll) -> Pixels {
    px(32.)
}

/// Asks for the next frame, stepping the driver before it is drawn.
fn drive(driver: Rc<RefCell<Driver>>, handles: Handles, window: &mut Window) {
    window.on_next_frame(move |window, cx| {
        if step(&driver, &handles, window, cx) {
            drive(driver, handles, window);
        } else {
            driver.borrow_mut().running = false;
        }
    });
}

/// Moves whatever scrolls by one frame's worth and notifies the view that
/// owns it. Returns whether to keep going.
fn step(
    driver: &Rc<RefCell<Driver>>,
    handles: &Handles,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let demo = driver.borrow().demo;
    if let Some(started) = demo {
        const ORDER: [Scroll; 4] = [Scroll::Sidebar, Scroll::Page, Scroll::Table, Scroll::List];
        let scroll =
            ORDER[(started.elapsed().as_secs() / DEMO_STEP.as_secs()) as usize % ORDER.len()];
        if scroll != driver.borrow().scroll {
            let mut driver = driver.borrow_mut();
            driver.scroll = scroll;
            driver.direction = 1.;
            drop(driver);
            handles
                .showcase
                .update(cx, |showcase, cx| showcase.show_scroll(scroll, cx))
                .ok();
        }
    }
    let auto = driver.borrow_mut().auto.take();
    if let Some(mut auto) = auto {
        let keep_going = auto.step(driver, handles, window, cx);
        driver.borrow_mut().auto = Some(auto);
        if !keep_going {
            return false;
        }
    }
    let mut driver = driver.borrow_mut();
    let scroll = driver.scroll;
    let speed = scroll_speed(scroll) * driver.direction;
    let mut bounce = |handle: &ScrollHandle| {
        let max = handle.max_offset().y;
        let mut offset = handle.offset();
        offset.y -= speed;
        if offset.y <= -max {
            offset.y = -max;
            driver.direction = -1.;
        } else if offset.y >= px(0.) {
            offset.y = px(0.);
            driver.direction = 1.;
        }
        handle.set_offset(offset);
    };
    match scroll {
        Scroll::Off => return driver.auto.is_some(),
        Scroll::Sidebar => {
            bounce(&handles.sidebar_scroll);
            cx.notify(handles.showcase.entity_id());
        }
        Scroll::Page => {
            let handle = handles.container.read(cx).scroll.clone();
            bounce(&handle);
            cx.notify(handles.container.entity_id());
        }
        Scroll::Table => {
            let Some(table) = handles.container.read(cx).table.clone() else {
                return true;
            };
            let handle = table.read(cx).scroll.0.borrow().base_handle.clone();
            bounce(&handle);
            cx.notify(table.entity_id());
        }
        Scroll::List => {
            let Some(messages) = handles.container.read(cx).messages.clone() else {
                return true;
            };
            let state = messages.read(cx).state.clone();
            let offset = -state.scroll_px_offset_for_scrollbar().y;
            let max = state.max_offset_for_scrollbar().y;
            if offset + speed >= max {
                driver.direction = -1.;
            } else if offset + speed <= px(0.) {
                driver.direction = 1.;
            }
            state.scroll_by(speed);
            cx.notify(messages.entity_id());
        }
    }
    true
}

impl Showcase {
    fn new(auto: bool, demo: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let container = cx.new(|cx| Container::new(BUTTON_PAGE, cx));
        let status_bar = cx.new(|_| StatusBar::new());
        backend::reset_stats(window);

        let sampled = status_bar.downgrade();
        cx.spawn_in(window, async move |_, cx| {
            loop {
                cx.background_executor().timer(SAMPLE_EVERY).await;
                let Some(status_bar) = sampled.upgrade() else {
                    break;
                };
                let sampled = cx.update(|window, cx| {
                    status_bar.update(cx, |status_bar, cx| {
                        status_bar.sample(window);
                        cx.notify();
                    })
                });
                if sampled.is_err() {
                    break;
                }
            }
        })
        .detach();

        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            Theme::follow(window, cx);
            window.refresh();
        });

        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);

        let mut this = Self {
            focus_handle,
            active: BUTTON_PAGE,
            sidebar_scroll: ScrollHandle::new(),
            container,
            status_bar,
            scroll: Scroll::Off,
            refreshing: false,
            driver: Rc::new(RefCell::new(Driver {
                scroll: Scroll::Off,
                direction: 1.,
                auto: auto.then(AutoRun::new),
                demo: demo.then(Instant::now),
                running: false,
            })),
            _appearance: appearance,
        };
        if auto || demo {
            this.start_frames(window, cx);
        }
        this
    }

    fn select(&mut self, page: usize, cx: &mut Context<Self>) {
        self.active = page;
        self.container.update(cx, |container, cx| {
            container.show(page, page_kind(page), cx);
        });
        cx.notify();
    }

    fn set_scroll(&mut self, scroll: Scroll, window: &mut Window, cx: &mut Context<Self>) {
        self.driver.borrow_mut().scroll = scroll;
        self.show_scroll(scroll, cx);
        if scroll != Scroll::Off {
            self.start_frames(window, cx);
        }
    }

    /// Shows what scrolls in the toolbar, and the page it scrolls.
    fn show_scroll(&mut self, scroll: Scroll, cx: &mut Context<Self>) {
        self.scroll = scroll;
        match scroll {
            Scroll::Table if self.active != table_page() => self.select(table_page(), cx),
            Scroll::List if self.active != list_page() => self.select(list_page(), cx),
            Scroll::Page if page_kind(self.active) != PageKind::Components => {
                self.select(BUTTON_PAGE, cx)
            }
            _ => {}
        }
        cx.notify();
    }

    fn toggle_refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active != table_page() {
            self.select(table_page(), cx);
        }
        self.container
            .update(cx, |container, cx| container.toggle_refresh(window, cx));
        self.refreshing = self.container.read(cx).refreshing;
        cx.notify();
    }

    fn toggle_retention(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(enabled) = backend::view_retention(window) {
            backend::set_view_retention(window, !enabled);
            cx.notify();
        }
    }

    /// Asks for frames, stepping the scroll before each one, for as long as
    /// something scrolls or an automatic run is under way.
    fn start_frames(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut driver = self.driver.borrow_mut();
        if driver.running {
            return;
        }
        driver.running = true;
        drop(driver);
        let handles = Handles {
            showcase: cx.entity().downgrade(),
            sidebar_scroll: self.sidebar_scroll.clone(),
            container: self.container.clone(),
            status_bar: self.status_bar.clone(),
        };
        drive(self.driver.clone(), handles, window);
    }

    fn toolbar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (border, build, build_foreground) = {
            let theme = theme(cx);
            let build = if backend::UPSTREAM {
                theme.build_upstream
            } else {
                theme.build_fast
            };
            (theme.border, build, theme.build_foreground)
        };
        let refreshing = self.refreshing;
        let retention = backend::view_retention(window);
        let scroll_option = |scroll: Scroll,
                             label: &'static str,
                             tip: &'static str,
                             key: &'static str,
                             cx: &mut Context<Self>| {
            segment(label, label, self.scroll == scroll, cx)
                .tooltip(Tooltip::text(tip, Some(key)))
                .on_click(
                    cx.listener(move |this, _, window, cx| this.set_scroll(scroll, window, cx)),
                )
        };
        div()
            .flex()
            .items_center()
            .flex_shrink_0()
            .gap_4()
            .h_10()
            .px_4()
            .border_b_1()
            .border_color(border)
            .text_sm()
            .child(
                div()
                    .flex()
                    .items_center()
                    .h_6()
                    .px_2()
                    .rounded(theme(cx).radius)
                    .bg(build)
                    .text_color(build_foreground)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(backend::GPUI),
            )
            .child(div().flex_1())
            .child(
                segmented("Auto-scroll", cx).child(
                    segment_track(cx)
                        .child(scroll_option(Scroll::Off, "Off", "Stop scrolling", "1", cx))
                        .child(scroll_option(
                            Scroll::Sidebar,
                            "Sidebar",
                            "Scroll the sidebar",
                            "2",
                            cx,
                        ))
                        .child(scroll_option(
                            Scroll::Page,
                            "Page",
                            "Scroll a page of components",
                            "3",
                            cx,
                        ))
                        .child(scroll_option(
                            Scroll::Table,
                            "Table",
                            "Scroll the data table",
                            "4",
                            cx,
                        ))
                        .child(scroll_option(
                            Scroll::List,
                            "List",
                            "Scroll a list of messages",
                            "5",
                            cx,
                        )),
                ),
            )
            .child(div().w_px().h_4().bg(border))
            .child(
                switch("refresh", "Refresh data", refreshing, cx)
                    .tooltip(Tooltip::text("Update table rows every 33 ms", Some("R")))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_refresh(window, cx))),
            )
            // Upstream GPUI has no retained views to switch.
            .when_some(retention, |this, retention| {
                this.child(
                    switch("retention", "Retained views", retention, cx)
                        .tooltip(Tooltip::text(
                            "Draw unchanged views from the last frame",
                            Some("V"),
                        ))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.toggle_retention(window, cx)),
                        ),
                )
            })
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let mut index = 0;
        let groups = groups().iter().map(|(title, pages)| {
            let rows = pages.iter().map(|name| {
                let page = index;
                index += 1;
                let selected = page == self.active;
                div()
                    .id(("page", page))
                    .flex()
                    .items_center()
                    .h_7()
                    .px_2()
                    .rounded(theme.radius)
                    .text_sm()
                    .when(selected, |this| {
                        this.bg(theme.sidebar_accent)
                            .font_weight(FontWeight::MEDIUM)
                    })
                    .when(!selected, |this| {
                        this.hover(|this| this.bg(theme.sidebar_accent.opacity(0.6)))
                    })
                    .child(*name)
                    .on_click(cx.listener(move |this, _, _, cx| this.select(page, cx)))
            });
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    div()
                        .px_2()
                        .pt_4()
                        .pb_1()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.muted_foreground)
                        .child(*title),
                )
                .children(rows.collect::<Vec<_>>())
        });
        div()
            .flex()
            .flex_col()
            .w_64()
            .h_full()
            .flex_shrink_0()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .id("sidebar-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.sidebar_scroll)
                    .px_2()
                    .pb_4()
                    .children(groups.collect::<Vec<_>>()),
            )
    }

    fn page_header(&self, cx: &App) -> impl IntoElement {
        let theme = theme(cx);
        let name = page_name(self.active);
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap_1()
            .px_6()
            .py_4()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(name),
            )
            .child(div().text_sm().text_color(theme.muted_foreground).child(
                if self.active == table_page() {
                    "A virtualized table of quotes that a timer keeps updating.".to_string()
                } else if self.active == list_page() {
                    "A conversation of messages of different heights, in a gpui::list.".to_string()
                } else {
                    format!("Examples of {name}, in sections of components.")
                },
            ))
    }
}

impl Render for Showcase {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        div()
            .track_focus(&self.focus_handle)
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(|this, _: &ScrollOff, window, cx| {
                this.set_scroll(Scroll::Off, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ScrollSidebar, window, cx| {
                this.set_scroll(Scroll::Sidebar, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ScrollPage, window, cx| {
                this.set_scroll(Scroll::Page, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ScrollTable, window, cx| {
                this.set_scroll(Scroll::Table, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ScrollList, window, cx| {
                this.set_scroll(Scroll::List, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ToggleRefresh, window, cx| this.toggle_refresh(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleRetention, window, cx| {
                this.toggle_retention(window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| {
                this.select((this.active + 1).min(page_count() - 1), cx)
            }))
            .on_action(cx.listener(|this, _: &SelectPrevious, _, cx| {
                this.select(this.active.saturating_sub(1), cx)
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(self.toolbar(window, cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.sidebar(cx))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(self.page_header(cx))
                            .child(self.container.clone()),
                    ),
            )
            .child(self.status_bar.clone())
            .children(backend::frame_counter())
    }
}
