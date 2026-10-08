//! Scrolling with a real wheel: a gallery shaped like GPUI Kit's, a sidebar
//! beside a scrolled page, scrolled the way a user scrolls it, by
//! `ScrollWheelEvent`s dispatched with the pointer over the content.
//!
//! Unlike scenarios that set a scroll offset and notify a view, the wheel
//! goes through dispatch: hit testing, the scroll container's own wheel
//! listener, which moves the offset and notifies the view that painted it,
//! and hover, since the content moves under a still pointer.
//!
//! Each scenario scrolls one kind of content, one scroll-layer pattern each:
//!
//! - `scroll-child-view`: a scrolling `div` whose content is a child view, as
//!   GPUI Kit's gallery shows a story;
//! - `scroll-same-view`: a scrolling `div` whose content is plain elements of
//!   the view that owns the `div`;
//! - `scroll-uniform-list`: a `uniform_list`;
//! - `scroll-list`: a `list` of rows of varying height;
//! - `scroll-list-tables`: the same, every third row a table in a rounded
//!   frame whose corners are paths drawn under its border, as GPUI Kit's
//!   markdown tables are.
//!
//! Each runs again with a scrollbar over the content, drawn and driven as
//! GPUI Kit's is (`scenarios::scrollbar`), as `<name>-scrollbar`, again with
//! that scrollbar asking for an animation frame from its prepaint, as GPUI
//! Kit's does while it fades, on every frame, as `<name>-animated-scrollbar`,
//! and on 30 frames of every 100, as `<name>-fading-scrollbar`; with it,
//! turned by three wheel events a frame, as a trackpad sends them, as
//! `<name>-scrollbar-burst`; and the three content kinds once more with the
//! scrollbar's thumb dragged instead of the wheel turned, as
//! `scrollbar-drag-*`.

use std::{cell::Cell, rc::Rc};

use std::borrow::Cow;

use gpui::{
    AnyElement, AnyView, App, AssetSource, Context, Entity, FontWeight, Hsla, ListAlignment,
    ListState, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, PlatformInput, Render,
    Result, ScrollDelta, ScrollHandle, ScrollWheelEvent, SharedString, TouchPhase,
    UniformListScrollHandle, Window, div, hsla, list, point, prelude::*, px, svg, uniform_list,
};

use super::scrollbar::{self, Scrolled, TRACK_WIDTH};
use crate::Scenario;

/// Sections of the gallery page.
pub const SECTIONS: usize = 24;
/// Buttons in a section.
const BUTTONS: usize = 8;
/// Width of the sidebar, in logical pixels.
const SIDEBAR_WIDTH: f32 = 240.;
/// How far one wheel event scrolls, in logical pixels.
const WHEEL_STEP: f32 = 40.;
/// Frames scrolled in one direction before turning back.
const FRAMES_PER_SWEEP: usize = 50;
/// How far the pointer drags the thumb each frame, in logical pixels.
const DRAG_STEP: f32 = 8.;
/// Where the thumb is grabbed: inside it while the content is at the top.
const GRAB_Y: f32 = 6.;

const ICONS: [&str; 6] = [
    "icons/check.svg",
    "icons/star.svg",
    "icons/plus.svg",
    "icons/arrow.svg",
    "icons/circle.svg",
    "icons/square.svg",
];

const LABELS: [&str; 8] = [
    "Primary",
    "Secondary",
    "Danger",
    "Ghost",
    "Outline",
    "Link",
    "Small",
    "With icon",
];

/// The icons the gallery shows, served to the headless app so that `svg()`
/// elements rasterize something.
pub struct ScenarioAssets;

impl AssetSource for ScenarioAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let shape = match path {
            "icons/check.svg" => {
                r#"<path d="M3 8l3 3 7-7" stroke="black" stroke-width="2" fill="none"/>"#
            }
            "icons/star.svg" => {
                r#"<path d="M8 1l2 5h5l-4 3 2 6-5-4-5 4 2-6-4-3h5z" fill="black"/>"#
            }
            "icons/plus.svg" => r#"<path d="M7 2h2v5h5v2H9v5H7V9H2V7h5z" fill="black"/>"#,
            "icons/arrow.svg" => r#"<path d="M2 7h9l-3-3 1-1 5 5-5 5-1-1 3-3H2z" fill="black"/>"#,
            "icons/circle.svg" => r#"<circle cx="8" cy="8" r="6" fill="black"/>"#,
            "icons/square.svg" => {
                r#"<rect x="2" y="2" width="12" height="12" rx="2" fill="black"/>"#
            }
            _ => return Ok(None),
        };
        Ok(Some(Cow::Owned(
            format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16">{shape}</svg>"#
            )
            .into_bytes(),
        )))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|icon| icon.starts_with(path))
            .map(|icon| SharedString::from(*icon))
            .collect())
    }
}

fn color(hue: f32, saturation: f32, lightness: f32) -> Hsla {
    hsla(hue / 360., saturation, lightness, 1.)
}

fn background() -> Hsla {
    color(0., 0., 1.)
}

fn sidebar_background() -> Hsla {
    color(220., 0.15, 0.97)
}

fn border() -> Hsla {
    color(220., 0.13, 0.88)
}

fn text() -> Hsla {
    color(220., 0.2, 0.15)
}

fn muted() -> Hsla {
    color(220., 0.1, 0.45)
}

fn accent() -> Hsla {
    color(215., 0.8, 0.52)
}

/// Where the wheel is turned: over the page, clear of the sidebar, in the
/// runner's 1440 × 900 window.
fn pointer() -> gpui::Point<gpui::Pixels> {
    point(px(SIDEBAR_WIDTH + 600.), px(450.))
}

/// Where the scrollbar's track is: along the window's right edge, in the
/// runner's 1440-wide window.
fn track_x() -> gpui::Pixels {
    px(1440. - TRACK_WIDTH / 2.)
}

/// The event of frame `frame` that drags the thumb: pressed on it at frame
/// 0, then moved `DRAG_STEP` a frame, `FRAMES_PER_SWEEP` frames down, as
/// many back up, and again, with the button held.
fn drag(frame: usize) -> PlatformInput {
    if frame == 0 {
        return PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: point(track_x(), px(GRAB_Y)),
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
    }
    let sweep = frame % (2 * FRAMES_PER_SWEEP);
    let travel = if sweep < FRAMES_PER_SWEEP {
        sweep
    } else {
        2 * FRAMES_PER_SWEEP - sweep
    };
    PlatformInput::MouseMove(MouseMoveEvent {
        position: point(track_x(), px(GRAB_Y + DRAG_STEP * travel as f32)),
        pressed_button: Some(MouseButton::Left),
        modifiers: Modifiers::default(),
    })
}

/// The wheel event of frame `frame`: `FRAMES_PER_SWEEP` frames down, as many
/// back up, and again.
fn wheel(frame: usize) -> PlatformInput {
    wheel_part(frame, 1)
}

/// One of `parts` wheel events that together scroll as far as the wheel
/// event of frame `frame` does, as a trackpad or a fast display sends
/// several a frame.
fn wheel_part(frame: usize, parts: usize) -> PlatformInput {
    let down = (frame / FRAMES_PER_SWEEP).is_multiple_of(2);
    let step = WHEEL_STEP / parts as f32;
    let delta = if down { -step } else { step };
    PlatformInput::ScrollWheel(ScrollWheelEvent {
        position: pointer(),
        delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
    })
}

/// A button as a component library draws one: a bordered, rounded box with
/// a hover style, an optional icon and a label.
fn button(section: usize, ix: usize) -> AnyElement {
    let primary = ix == 0;
    let danger = ix == 2;
    let (bg, fg, hover) = if primary {
        (accent(), color(0., 0., 1.), color(215., 0.8, 0.45))
    } else if danger {
        (
            color(0., 0.75, 0.55),
            color(0., 0., 1.),
            color(0., 0.75, 0.48),
        )
    } else {
        (color(0., 0., 1.), text(), color(220., 0.2, 0.95))
    };
    div()
        .id(("button", section * BUTTONS + ix))
        .flex()
        .items_center()
        .gap_1()
        .h(px(if ix == 6 { 24. } else { 32. }))
        .px_3()
        .rounded_md()
        .border_1()
        .border_color(border())
        .bg(bg)
        .text_color(fg)
        .text_sm()
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .when(ix == 7 || ix == 0, |this| {
            this.child(
                svg()
                    .path(ICONS[(section + ix) % ICONS.len()])
                    .size(px(14.))
                    .text_color(fg),
            )
        })
        .child(LABELS[ix])
        .into_any_element()
}

/// One section of the gallery page: a title, a paragraph and a row of
/// buttons in a bordered card.
fn section(ix: usize) -> AnyElement {
    div()
        .id(("section", ix))
        .flex()
        .flex_col()
        .gap_2()
        .p_4()
        .mb_4()
        .rounded_lg()
        .border_1()
        .border_color(border())
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    svg()
                        .path(ICONS[ix % ICONS.len()])
                        .size(px(16.))
                        .text_color(accent()),
                )
                .child(
                    div()
                        .text_color(text())
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(SharedString::from(format!("Section {}", ix + 1))),
                ),
        )
        .child(
            div()
                .text_sm()
                .text_color(muted())
                .child(SharedString::from(format!(
                    "Buttons trigger an action. Section {} shows every variant, each with a \
             hover style, some with an icon, laid out the way a component gallery shows them.",
                    ix + 1
                ))),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap_2()
                .children((0..BUTTONS).map(|button_ix| button(ix, button_ix))),
        )
        .into_any_element()
}

/// The gallery's sidebar: a view of its own, one item per section.
pub struct Sidebar;

impl Render for Sidebar {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .p_2()
            .gap_0p5()
            .bg(sidebar_background())
            .border_r_1()
            .border_color(border())
            .children((0..SECTIONS).map(|ix| {
                div()
                    .id(("nav", ix))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .h(px(28.))
                    .rounded_md()
                    .text_sm()
                    .text_color(text())
                    .hover(|style| style.bg(border()))
                    .when(ix == 0, |this| this.bg(border()))
                    .child(
                        svg()
                            .path(ICONS[ix % ICONS.len()])
                            .size(px(14.))
                            .text_color(muted()),
                    )
                    .child(SharedString::from(format!("Section {}", ix + 1)))
            }))
    }
}

/// The page a gallery shows, as a view of its own (pattern A).
pub struct Page;

impl Render for Page {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .p_6()
            .children((0..SECTIONS).map(section))
    }
}

/// What the gallery scrolls.
enum Content {
    /// A child view (pattern A).
    ChildView(Entity<Page>),
    /// Plain elements of the gallery's own view (pattern B).
    SameView,
    /// A `uniform_list` of this many rows.
    UniformList(usize, UniformListScrollHandle),
    /// A `list`; with `true`, every third row holds a table in a rounded
    /// frame drawn as GPUI Kit's markdown tables are.
    List(ListState, bool),
}

/// The gallery: a sidebar view and the scrolled content, on an opaque
/// background.
pub struct Gallery {
    sidebar: Entity<Sidebar>,
    scroll: ScrollHandle,
    content: Content,
    /// The scrollbar's state, when the content has one.
    scrollbar: Option<Rc<Cell<scrollbar::State>>>,
}

/// The scrollbar a gallery draws over its content.
#[derive(Clone, Copy, PartialEq)]
enum Bar {
    None,
    Still,
    /// One animating on this many frames of every 100.
    Animated(u32),
}

impl Gallery {
    fn new(content: Content, bar: Bar, cx: &mut Context<Self>) -> Self {
        let scrollbar = match bar {
            Bar::None => None,
            Bar::Still => Some(scrollbar::State::default()),
            Bar::Animated(frames) => Some(scrollbar::State::animated(frames)),
        };
        Self {
            sidebar: cx.new(|_| Sidebar),
            scroll: ScrollHandle::new(),
            content,
            scrollbar: scrollbar.map(|state| Rc::new(Cell::new(state))),
        }
    }

    /// What the scrollbar scrolls.
    fn scrolled(&self) -> Scrolled {
        match &self.content {
            Content::ChildView(_) | Content::SameView => Scrolled::Div(self.scroll.clone()),
            Content::UniformList(_, handle) => Scrolled::UniformList(handle.clone()),
            Content::List(state, _) => Scrolled::List(state.clone()),
        }
    }
}

/// A row of the lists: an icon, a title, a line of detail and a button.
/// Rows of the variable list wrap a paragraph whose length varies.
fn row(ix: usize, variable: bool) -> AnyElement {
    div()
        .id(("row", ix))
        .flex()
        .items_center()
        .gap_3()
        .px_4()
        .when(!variable, |this| this.h(px(48.)))
        .when(variable, |this| this.py_2())
        .border_b_1()
        .border_color(border())
        .hover(|style| style.bg(color(220., 0.2, 0.97)))
        .child(
            svg()
                .path(ICONS[ix % ICONS.len()])
                .size(px(16.))
                .text_color(accent()),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_sm()
                        .text_color(text())
                        .child(SharedString::from(format!("Item {ix}"))),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(muted())
                        .when(!variable, |this| this.truncate())
                        .child(SharedString::from(
                            "Details of the item, long enough to fill the row and, where rows \
                             wrap, to wrap onto a second or a third line. "
                                .repeat(if variable { 1 + ix % 4 } else { 1 }),
                        )),
                ),
        )
        .child(button(ix % SECTIONS, ix % BUTTONS))
        .into_any_element()
}

/// A row holding a small table in a rounded frame, drawn as GPUI Kit draws
/// a markdown table (`horizontal_scroll_area` and `RoundedFrameCover`):
/// paths fill the frame's corner notches with the background, and the
/// frame's border, painted after its children, draws over them.
fn table_row(ix: usize) -> AnyElement {
    const RADIUS: f32 = 6.;
    let cell = |text: String, header: bool| {
        div()
            .flex_1()
            .px_2()
            .py_1()
            .border_r_1()
            .border_color(border())
            .when(header, |this| this.bg(color(220., 0.1, 0.95)))
            .text_xs()
            .child(SharedString::from(text))
    };
    let line = |row: usize| {
        div()
            .flex()
            .border_b_1()
            .border_color(border())
            .children((0..4).map(move |column| {
                cell(
                    if row == 0 {
                        format!("Column {column}")
                    } else {
                        format!("{} · {}", ix + row, column * 7 + row)
                    },
                    row == 0,
                )
            }))
    };
    let notches = gpui::canvas(
        |_, _, _| {},
        |bounds, _, window, _| {
            let radius = px(RADIUS);
            for (corner, x, y) in [
                (bounds.origin, 1., 1.),
                (bounds.top_right(), -1., 1.),
                (bounds.bottom_right(), -1., -1.),
                (bounds.bottom_left(), 1., -1.),
            ] {
                let mut path = gpui::Path::new(corner);
                path.line_to(corner + point(radius * x, px(0.)));
                path.curve_to(corner + point(px(0.), radius * y), corner);
                path.line_to(corner);
                window.paint_path(path, background());
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full();
    div()
        .id(("table-row", ix))
        .px_4()
        .py_2()
        .border_b_1()
        .border_color(border())
        .child(
            div()
                .relative()
                .overflow_hidden()
                .border_1()
                .border_color(border())
                .rounded(px(RADIUS))
                .children((0..4).map(line))
                .child(notches),
        )
        .into_any_element()
}

impl Render for Gallery {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.content {
            Content::ChildView(page) => div()
                .id("page")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .child(page.clone())
                .into_any_element(),
            Content::SameView => div()
                .id("page")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .p_6()
                        .children((0..SECTIONS).map(section)),
                )
                .into_any_element(),
            Content::UniformList(rows, handle) => uniform_list("rows", *rows, |range, _, _| {
                range.map(|ix| row(ix, false)).collect()
            })
            .track_scroll(handle)
            .size_full()
            .into_any_element(),
            Content::List(state, tables) => {
                let tables = *tables;
                list(state.clone(), move |ix, _, _| {
                    if tables && ix % 3 == 0 {
                        table_row(ix)
                    } else {
                        row(ix, true)
                    }
                })
                .size_full()
                .into_any_element()
            }
        };
        div()
            .flex()
            .size_full()
            .bg(background())
            .text_color(text())
            .child(self.sidebar.clone())
            .child(
                div()
                    .relative()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .child(content)
                    .when_some(self.scrollbar.clone(), |this, state| {
                        this.child(scrollbar::scrollbar(self.scrolled(), state))
                    }),
            )
    }
}

/// How a scenario scrolls.
#[derive(Clone, Copy, PartialEq)]
enum Drive {
    /// The wheel over the content.
    Wheel,
    /// The wheel over the content, in three events a frame.
    WheelBurst,
    /// The scrollbar's thumb, dragged.
    Thumb,
}

/// A scenario scrolling one kind of content, by the wheel or by dragging the
/// scrollbar's thumb.
struct WheelScroll {
    name: &'static str,
    description: &'static str,
    content: fn(&mut App) -> Content,
    scrollbar: Bar,
    drive: Drive,
}

impl Scenario for WheelScroll {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn build(&self, _: &mut Window, cx: &mut App) -> AnyView {
        let content = (self.content)(cx);
        let scrollbar = self.scrollbar;
        cx.new(|cx| Gallery::new(content, scrollbar, cx)).into()
    }

    fn step(&self, _: &AnyView, frame: usize, window: &mut Window, cx: &mut App) {
        if matches!(self.scrollbar, Bar::Animated(_)) {
            // The test platform draws a dirty window without running what
            // was scheduled for the next frame, as a platform's frame
            // callback does first: the animation frame the scrollbar asked
            // for notifies the view drawing it here.
            window.simulate_next_frame(cx);
        }
        let event = match self.drive {
            Drive::Wheel => wheel(frame),
            Drive::WheelBurst => {
                for _ in 0..2 {
                    window.dispatch_event(wheel_part(frame, 3), cx);
                }
                wheel_part(frame, 3)
            }
            Drive::Thumb => drag(frame),
        };
        window.dispatch_event(event, cx);
    }
}

/// The content kinds: name, description, content.
const KINDS: [(&str, &str, fn(&mut App) -> Content); 4] = [
    (
        "child-view",
        "A gallery page, a child view of 24 sections of buttons",
        |cx| Content::ChildView(cx.new(|_| Page)),
    ),
    (
        "same-view",
        "A gallery page of 24 sections drawn by the scrolling view itself",
        |_| Content::SameView,
    ),
    ("uniform-list", "A 10,000-row uniform_list", |_| {
        Content::UniformList(10_000, UniformListScrollHandle::new())
    }),
    ("list", "A 2,000-row list of rows of varying height", |_| {
        Content::List(ListState::new(2_000, ListAlignment::Top, px(200.)), false)
    }),
];

pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    let leak = |text: String| -> &'static str { Box::leak(text.into_boxed_str()) };
    let mut scenarios: Vec<Box<dyn Scenario>> = Vec::new();
    for (kind, description, content) in KINDS {
        scenarios.push(Box::new(WheelScroll {
            name: leak(format!("scroll-{kind}")),
            description: leak(format!("{description}, scrolled by the wheel")),
            content,
            scrollbar: Bar::None,
            drive: Drive::Wheel,
        }));
    }
    scenarios.push(Box::new(WheelScroll {
        name: "scroll-list-tables",
        description: "A 2,000-row list of rows of varying height, every third a table in a \
                      rounded frame whose corners are paths under its border, scrolled by \
                      the wheel",
        content: |_| Content::List(ListState::new(2_000, ListAlignment::Top, px(200.)), true),
        scrollbar: Bar::None,
        drive: Drive::Wheel,
    }));
    for (kind, description, content) in KINDS {
        scenarios.push(Box::new(WheelScroll {
            name: leak(format!("scroll-{kind}-scrollbar")),
            description: leak(format!(
                "{description}, with a GPUI Kit scrollbar, scrolled by the wheel"
            )),
            content,
            scrollbar: Bar::Still,
            drive: Drive::Wheel,
        }));
    }
    for (kind, description, content) in KINDS {
        for (name, frames, how) in [
            ("animated", 100, "on every frame"),
            ("fading", 30, "on 30 frames of every 100"),
        ] {
            scenarios.push(Box::new(WheelScroll {
                name: leak(format!("scroll-{kind}-{name}-scrollbar")),
                description: leak(format!(
                    "{description}, with a GPUI Kit scrollbar animating {how}, scrolled by the \
                     wheel"
                )),
                content,
                scrollbar: Bar::Animated(frames),
                drive: Drive::Wheel,
            }));
        }
        scenarios.push(Box::new(WheelScroll {
            name: leak(format!("scroll-{kind}-scrollbar-burst")),
            description: leak(format!(
                "{description}, with a GPUI Kit scrollbar, scrolled by three wheel events a frame"
            )),
            content,
            scrollbar: Bar::Still,
            drive: Drive::WheelBurst,
        }));
    }
    for (kind, description, content) in KINDS {
        if kind == "same-view" {
            continue;
        }
        scenarios.push(Box::new(WheelScroll {
            name: leak(format!("scrollbar-drag-{kind}")),
            description: leak(format!(
                "{description}, scrolled by dragging a GPUI Kit scrollbar's thumb"
            )),
            content,
            scrollbar: Bar::Still,
            drive: Drive::Thumb,
        }));
    }
    scenarios
}
