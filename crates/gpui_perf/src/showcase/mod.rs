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
//! Its last page is a trading workspace, built the way Longbridge Pro builds
//! its main window on GPUI Kit: docked panels in cached tab groups, a focused
//! search box, a text selection layer, and a market feed every panel
//! subscribes to, which a timer streams quotes into; see `workspace.rs`.
//!
//! Its first page is Allsum Desktop's chat window, built from the views of
//! the headless `chat-scroll-allsum` scenario, which nothing scrolls but the
//! user: `--demo` waits while it shows.
//!
//! The toolbar picks what scrolls itself, and switches the data refresh and
//! retained views. Every command has a key: `1`–`6` for what scrolls, `R` for
//! the refresh, `Q` for the workspace's quote stream, `V` for retained views,
//! and the arrow keys to move through the sidebar. The status bar shows,
//! every half second, the frame rate, the process's CPU and the main
//! thread's, its resident memory, what build, prepaint, layout and paint took
//! per frame, and how many views were built and reused per frame.
//!
//! With `--auto`, it runs every scenario with retained views on and then off,
//! prints what each cost per frame, and quits. `--only <scenario>`,
//! `--retention on|off` and `--frames <n>` narrow it down, and `--list` prints
//! the scenarios instead. On macOS it holds the CPU's clock up while it
//! measures; see `clock.rs`.
//!
//! Built with the `upstream` feature it runs on upstream GPUI, the
//! `gpui-pre` snapshot GPUI Kit pins, for comparison; see `backend.rs`.

mod app_state;
mod auto;
mod backend;
pub mod clock;
mod controls;
mod metrics;
mod pages;
mod theme;
mod workspace;

#[path = "../../../gpui/examples/example_support/fonts.rs"]
mod example_support;

use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    Animation, AnimationExt as _, App, Bounds, Context, Entity, FocusHandle, FontWeight,
    KeyBinding, Modifiers, Pixels, PlatformInput, Render, ScrollDelta, ScrollHandle,
    ScrollWheelEvent, SharedString, Subscription, TouchPhase, WeakEntity, Window, WindowBounds,
    WindowOptions, actions, div, point, prelude::*, px, size,
};
use gpui_platform::application;

use app_state::{AppState, SharedAppState, app_state};
use auto::AutoRun;
use controls::{Tooltip, segment, segment_track, segmented, switch};
use metrics::Stats;
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
        ScrollWatchlist,
        ToggleRefresh,
        ToggleStreaming,
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
            // First, at `CHAT_PAGE`.
            ("Chat", vec!["Allsum chat"]),
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
        // Last, so that the pages before it keep their places.
        groups.push(("Applications", vec!["Trading workspace"]));
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

/// The page showing the trading workspace.
fn workspace_page() -> usize {
    pages()
        .position(|name| name == "Trading workspace")
        .unwrap_or(0)
}

fn page_kind(page: usize) -> PageKind {
    if page == CHAT_PAGE {
        PageKind::Chat
    } else if page == workspace_page() {
        PageKind::Workspace
    } else if page == table_page() {
        PageKind::Table
    } else if page == list_page() {
        PageKind::List
    } else {
        PageKind::Components
    }
}

/// The page showing Allsum's chat window, scrolled by hand: the first.
const CHAT_PAGE: usize = 0;

/// The page showing components, when the table is not wanted.
const BUTTON_PAGE: usize = 9;

/// What a page of components is made from: its place in the sidebar,
/// counted as before the chat page came first, so that every page of
/// components shows what it did.
fn page_seed(page: usize) -> usize {
    page.saturating_sub(CHAT_PAGE + 1)
}

const SAMPLE_EVERY: Duration = Duration::from_millis(500);

/// How often the application's state changes.
const APP_STATE_EVERY: Duration = Duration::from_secs(2);

/// What scrolls itself, one step every frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scroll {
    Off,
    Sidebar,
    Page,
    Table,
    List,
    /// The trading workspace's watchlist.
    Watchlist,
}

/// Opens the showcase, running every scenario and quitting if `auto`.
/// How long `--demo` scrolls each thing.
const DEMO_STEP: Duration = Duration::from_secs(6);

/// Opens the showcase. With `auto`, it runs every scenario and quits; with
/// `demo`, it scrolls the sidebar, a page, the table and the list in turn,
/// for as long as it is open, for recording or watching two GPUIs side by
/// side.
pub fn run(auto: bool, demo: bool) {
    if auto && std::env::args().any(|arg| arg == "--list") {
        auto::list();
        return;
    }
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
            KeyBinding::new("6", ScrollWatchlist, Some(KEY_CONTEXT)),
            KeyBinding::new("r", ToggleRefresh, Some(KEY_CONTEXT)),
            KeyBinding::new("q", ToggleStreaming, Some(KEY_CONTEXT)),
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
                // GPUI caps a window that isn't focused at 30 fps. Measuring
                // would then depend on whether the window has focus, so it
                // draws at the display's rate either way. A cap of 16.7 ms
                // would not do: frames that arrive a hair early are skipped.
                inactive_frame_interval: None,
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
    /// What the frames cost, which the status bar shows.
    stats: Entity<Stats>,
    /// One entity per page, holding its name, which the sidebar reads, as
    /// GPUI Kit's gallery reads its stories'.
    stories: Vec<Entity<Story>>,
    /// Whether the toolbar shows a spinner, animating every frame.
    spinning: bool,
    /// What scrolls, as the toolbar shows it; the driver does the scrolling.
    scroll: Scroll,
    driver: Rc<RefCell<Driver>>,
    _appearance: Subscription,
}

/// What steps the showcase before every frame: the scroll and the automatic
/// run. It lives outside the views and scrolls with wheel events dispatched
/// over what it scrolls, as a user does: the scroll container's own listener
/// moves the offset and notifies the view that owns it, so that no other view
/// counts as changed, and hover follows the content moving under the pointer.
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
}

/// A page of the gallery.
pub struct Story {
    name: SharedString,
}

/// How far each frame scrolls: as fast as a wheel spun hard.
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

/// Scrolls whatever scrolls by one frame's worth, with a wheel event over it.
/// Returns whether to keep going.
fn step(
    driver: &Rc<RefCell<Driver>>,
    handles: &Handles,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let demo = driver.borrow().demo;
    if let Some(started) = demo {
        // The chat page is scrolled by hand, so the demo waits while it
        // shows, until another page is picked.
        let on_chat = handles
            .showcase
            .upgrade()
            .is_some_and(|showcase| showcase.read(cx).active == CHAT_PAGE);
        if on_chat {
            return false;
        }
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
    // Turns back at either end, then scrolls with a wheel event over the
    // scrolled area, whose listener moves the offset and notifies the view
    // that painted it.
    let mut bounce = |offset: Pixels, max: Pixels, bounds: Bounds<Pixels>| {
        if offset + speed >= max {
            driver.direction = -1.;
        } else if offset + speed <= px(0.) {
            driver.direction = 1.;
        }
        (bounds, speed)
    };
    let handle_wheel =
        |handle: &ScrollHandle| (-handle.offset().y, handle.max_offset().y, handle.bounds());
    let (bounds, delta) = match scroll {
        Scroll::Off => return driver.auto.is_some(),
        Scroll::Sidebar => {
            let (offset, max, bounds) = handle_wheel(&handles.sidebar_scroll);
            bounce(offset, max, bounds)
        }
        Scroll::Page => {
            let handle = handles.container.read(cx).scroll.clone();
            let (offset, max, bounds) = handle_wheel(&handle);
            bounce(offset, max, bounds)
        }
        Scroll::Table => {
            let Some(table) = handles.container.read(cx).table.clone() else {
                return true;
            };
            let handle = table.read(cx).scroll.0.borrow().base_handle.clone();
            let (offset, max, bounds) = handle_wheel(&handle);
            bounce(offset, max, bounds)
        }
        Scroll::List => {
            let Some(messages) = handles.container.read(cx).messages.clone() else {
                return true;
            };
            let state = messages.read(cx).state.clone();
            bounce(
                -state.scroll_px_offset_for_scrollbar().y,
                state.max_offset_for_scrollbar().y,
                state.viewport_bounds(),
            )
        }
        Scroll::Watchlist => {
            let Some(workspace) = handles.container.read(cx).workspace.clone() else {
                return true;
            };
            let watchlist = workspace.read(cx).watchlist.clone();
            let handle = watchlist.read(cx).scroll.0.borrow().base_handle.clone();
            let (offset, max, bounds) = handle_wheel(&handle);
            bounce(offset, max, bounds)
        }
    };
    drop(driver);
    window.dispatch_event(
        PlatformInput::ScrollWheel(ScrollWheelEvent {
            position: bounds.center(),
            delta: ScrollDelta::Pixels(point(px(0.), -delta)),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        }),
        cx,
    );
    true
}

impl Showcase {
    fn new(auto: bool, demo: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let container = cx.new(|cx| {
            let mut container = Container::new(page_seed(BUTTON_PAGE), cx);
            container.title = page_name(BUTTON_PAGE).into();
            container
        });
        let stats = cx.new(|_| Stats::new());
        let stories = pages()
            .map(|name| cx.new(|_| Story { name: name.into() }))
            .collect();
        backend::reset_stats(window);

        // The application's state, which changes every couple of seconds.
        let app_state = cx.new(|_| AppState {
            unread: 3,
            compact: false,
        });
        cx.set_global(SharedAppState(app_state.clone()));
        let ticked = app_state.downgrade();
        cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor().timer(APP_STATE_EVERY).await;
                let Some(app_state) = ticked.upgrade() else {
                    break;
                };
                app_state.update(cx, |state, cx| {
                    state.unread += 1;
                    cx.notify();
                });
            }
        })
        .detach();

        let sampled = stats.downgrade();
        cx.spawn_in(window, async move |_, cx| {
            loop {
                cx.background_executor().timer(SAMPLE_EVERY).await;
                let Some(stats) = sampled.upgrade() else {
                    break;
                };
                let sampled = cx.update(|window, cx| {
                    stats.update(cx, |stats, cx| {
                        stats.sample(window);
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
            stats,
            stories,
            spinning: false,
            scroll: Scroll::Off,
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
        if page == CHAT_PAGE {
            // Nothing scrolls the chat page but the user.
            self.scroll = Scroll::Off;
            self.driver.borrow_mut().scroll = Scroll::Off;
        }
        self.container.update(cx, |container, cx| {
            container.show(page_seed(page), page_name(page).into(), page_kind(page), cx);
        });
        cx.notify();
    }

    /// After the user picks a page: focuses it, and goes on with `--demo`'s
    /// scrolling, which waits while the chat page shows.
    fn page_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_page(window, cx);
        if self.driver.borrow().demo.is_some() && self.active != CHAT_PAGE {
            self.start_frames(window, cx);
        }
    }

    fn set_scroll(&mut self, scroll: Scroll, window: &mut Window, cx: &mut Context<Self>) {
        self.driver.borrow_mut().scroll = scroll;
        self.show_scroll(scroll, cx);
        self.focus_page(window, cx);
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
            Scroll::Watchlist if self.active != workspace_page() => {
                self.select(workspace_page(), cx)
            }
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
        cx.notify();
    }

    /// Starts or stops the workspace's quote stream, showing the workspace.
    fn toggle_streaming(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active != workspace_page() {
            self.select(workspace_page(), cx);
        }
        self.container
            .update(cx, |container, cx| container.toggle_streaming(window, cx));
        self.focus_page(window, cx);
        cx.notify();
    }

    /// Focuses the workspace's search box while the workspace is shown, as a
    /// trading window keeps its symbol search focused, and the showcase
    /// otherwise.
    fn focus_page(&self, window: &mut Window, cx: &mut Context<Self>) {
        let search = (self.active == workspace_page())
            .then(|| self.container.read(cx).workspace.clone())
            .flatten()
            .map(|workspace| workspace.read(cx).search.read(cx).focus.clone());
        search
            .as_ref()
            .unwrap_or(&self.focus_handle)
            .focus(window, cx);
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
        let refreshing = self.container.read(cx).refreshing;
        let streaming = self.container.read(cx).streaming;
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
                    .flex_shrink_0()
                    .h_6()
                    .px_2()
                    .rounded(theme(cx).radius)
                    .bg(build)
                    .text_color(build_foreground)
                    .font_weight(FontWeight::MEDIUM)
                    .child(backend::GPUI),
            )
            .when(self.spinning, |this| this.child(spinner(cx)))
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
                        ))
                        .child(scroll_option(
                            Scroll::Watchlist,
                            "Watchlist",
                            "Scroll the trading workspace's watchlist",
                            "6",
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
            .child(
                switch("streaming", "Stream quotes", streaming, cx)
                    .tooltip(Tooltip::text(
                        "Stream 16 quotes into the trading workspace 60 times a second",
                        Some("Q"),
                    ))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_streaming(window, cx))),
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
            let rows = pages.iter().map(|_| {
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
                    .child(self.stories[page].read(cx).name.clone())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select(page, cx);
                        this.page_selected(window, cx);
                    }))
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
            .child(unread(cx))
    }

    fn page_header(&self, cx: &App) -> impl IntoElement {
        let theme = theme(cx);
        let name = self.container.read(cx).title.clone();
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
                    .child(name.clone()),
            )
            .child(div().text_sm().text_color(theme.muted_foreground).child(
                if self.active == CHAT_PAGE {
                    "Allsum Desktop's chat window: a cached 200-message transcript to scroll by wheel, trackpad or scrollbar."
                        .to_string()
                } else if self.active == workspace_page() {
                    "Docked market panels, every one subscribed to a feed of streaming quotes."
                        .to_string()
                } else if self.active == table_page() {
                    "A virtualized table of quotes that a timer keeps updating.".to_string()
                } else if self.active == list_page() {
                    "A conversation of messages of different heights, in a gpui::list.".to_string()
                } else {
                    format!("Examples of {name}, in sections of components.")
                },
            ))
    }
}

/// A spinner, animating every frame as a loading indicator does. The
/// animation asks for each frame by notifying the view it is drawn in.
fn spinner(cx: &App) -> impl IntoElement {
    let theme = theme(cx);
    div()
        .size_3()
        .rounded_full()
        .bg(theme.foreground)
        .with_animation(
            "spinner",
            Animation::new(Duration::from_millis(800)).repeat(),
            |this, delta| this.opacity(0.2 + 0.8 * delta),
        )
}

/// The unread count, read from the application's state, at the foot of the
/// sidebar.
fn unread(cx: &App) -> impl IntoElement {
    let theme = theme(cx);
    div()
        .flex_shrink_0()
        .px_4()
        .py_2()
        .border_t_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(format!("{} unread", app_state(cx).unread))
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
            .on_action(cx.listener(|this, _: &ScrollWatchlist, window, cx| {
                this.set_scroll(Scroll::Watchlist, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ToggleRefresh, window, cx| this.toggle_refresh(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleStreaming, window, cx| {
                this.toggle_streaming(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleRetention, window, cx| {
                this.toggle_retention(window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectNext, window, cx| {
                this.select((this.active + 1).min(page_count() - 1), cx);
                this.page_selected(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectPrevious, window, cx| {
                this.select(this.active.saturating_sub(1), cx);
                this.page_selected(window, cx);
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
            .font_features(theme.numbers.clone())
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
            .child(metrics::status_bar(self.stats.read(cx), cx))
            .children(backend::frame_counter())
    }
}
