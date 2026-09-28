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
//! process's CPU and the main thread's, what build, prepaint, layout and paint
//! took per frame, and how many views were built and reused per frame.
//!
//! With `--auto`, it runs every scenario with retained views on and then off,
//! prints what each cost per frame, and quits. `--only <scenario>`,
//! `--retention on|off` and `--frames <n>` narrow it down.

mod auto;
mod controls;
mod metrics;
mod pages;
mod theme;

#[path = "../../../gpui/examples/example_support/fonts.rs"]
mod example_support;

use std::time::Duration;

use gpui::{
    App, Bounds, Context, Entity, FocusHandle, FontWeight, KeyBinding, Render, ScrollHandle,
    Subscription, Window, WindowBounds, WindowOptions, actions, div, prelude::*, px, size,
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

/// The sidebar's groups and the pages in each.
const GROUPS: [(&str, &[&str]); 2] = [
    (
        "Getting started",
        &["Introduction", "Installation", "Theming"],
    ),
    (
        "Components",
        &[
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
        ],
    ),
];

/// Every page, in sidebar order.
fn pages() -> impl Iterator<Item = &'static str> {
    GROUPS.iter().flat_map(|(_, pages)| pages.iter().copied())
}

fn page_count() -> usize {
    GROUPS.iter().map(|(_, pages)| pages.len()).sum()
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
pub fn run(auto: bool) {
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
                cx.new(|cx| Showcase::new(auto, window, cx))
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
    scroll: Scroll,
    scroll_direction: f32,
    auto: Option<AutoRun>,
    _appearance: Subscription,
}

impl Showcase {
    fn new(auto: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let container = cx.new(|cx| Container::new(BUTTON_PAGE, cx));
        let status_bar = cx.new(|_| StatusBar::new());
        window.reset_layout_stats();

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
            scroll_direction: 1.,
            auto: auto.then(AutoRun::new),
            _appearance: appearance,
        };
        if this.auto.is_some() {
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
        let was_off = self.scroll == Scroll::Off;
        self.scroll = scroll;
        match scroll {
            Scroll::Table if self.active != table_page() => self.select(table_page(), cx),
            Scroll::List if self.active != list_page() => self.select(list_page(), cx),
            Scroll::Page if page_kind(self.active) != PageKind::Components => {
                self.select(BUTTON_PAGE, cx)
            }
            _ => {}
        }
        if was_off && scroll != Scroll::Off {
            self.start_frames(window, cx);
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

    fn toggle_retention(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let enabled = window.view_retention();
        window.set_view_retention(!enabled);
        cx.notify();
    }

    /// Asks for the next frame, stepping the scroll before it is drawn, for as
    /// long as something scrolls or an automatic run is under way.
    fn start_frames(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        window.on_next_frame(move |window, cx| {
            let Some(this) = this.upgrade() else {
                return;
            };
            this.update(cx, |this, cx| {
                if this.step(window, cx) {
                    this.start_frames(window, cx);
                }
            });
        });
    }

    /// Moves whatever scrolls by one frame's worth and notifies the view that
    /// owns it, as a scroll wheel or a dragged scrollbar would. Returns
    /// whether to keep going.
    fn step(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some(mut auto) = self.auto.take() {
            let keep_going = auto.step(self, window, cx);
            self.auto = Some(auto);
            if !keep_going {
                return false;
            }
        }
        let speed = px(14.) * self.scroll_direction;
        let mut bounce = |handle: &ScrollHandle| {
            let max = handle.max_offset().y;
            let mut offset = handle.offset();
            offset.y -= speed;
            if offset.y <= -max {
                offset.y = -max;
                self.scroll_direction = -1.;
            } else if offset.y >= px(0.) {
                offset.y = px(0.);
                self.scroll_direction = 1.;
            }
            handle.set_offset(offset);
        };
        match self.scroll {
            Scroll::Off => return self.auto.is_some(),
            Scroll::Sidebar => {
                bounce(&self.sidebar_scroll.clone());
                cx.notify();
            }
            Scroll::Page => {
                let handle = self.container.read(cx).scroll.clone();
                bounce(&handle);
                self.container.update(cx, |_, cx| cx.notify());
            }
            Scroll::Table => {
                let Some(table) = self.container.read(cx).table.clone() else {
                    return true;
                };
                let handle = table.read(cx).scroll.0.borrow().base_handle.clone();
                bounce(&handle);
                table.update(cx, |_, cx| cx.notify());
            }
            Scroll::List => {
                let Some(messages) = self.container.read(cx).messages.clone() else {
                    return true;
                };
                let state = messages.read(cx).state.clone();
                let offset = -state.scroll_px_offset_for_scrollbar().y;
                let max = state.max_offset_for_scrollbar().y;
                if offset + speed >= max {
                    self.scroll_direction = -1.;
                } else if offset + speed <= px(0.) {
                    self.scroll_direction = 1.;
                }
                state.scroll_by(speed);
                messages.update(cx, |_, cx| cx.notify());
            }
        }
        true
    }

    fn toolbar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (border, muted_foreground) = {
            let theme = theme(cx);
            (theme.border, theme.muted_foreground)
        };
        let refreshing = self.container.read(cx).refreshing;
        let retention = window.view_retention();
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
                    .items_baseline()
                    .gap_2()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Showcase"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted_foreground)
                            .child("gpui-fast frame cost"),
                    ),
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
            .child(
                switch("retention", "Retained views", retention, cx)
                    .tooltip(Tooltip::text(
                        "Draw unchanged views from the last frame",
                        Some("V"),
                    ))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_retention(window, cx))),
            )
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let mut index = 0;
        let groups = GROUPS.iter().map(|(title, pages)| {
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
    }
}
