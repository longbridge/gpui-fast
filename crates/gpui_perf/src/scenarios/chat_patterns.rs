//! Scrolling a chat transcript the way real chat views scroll it, with the
//! patterns of real applications that keep a `list` from being composited
//! from its scroll layer: `chat-scroll-plain` has none of them,
//! `chat-scroll-animates` and `chat-scroll-trackpad` one each, and
//! `chat-scroll-allsum` puts them all together as Allsum's chat page has
//! them.
//!
//! The code mirrored, cited as `path:line`:
//!
//! - `ai-chat` at `670d8c04572587b84119d12c8eee5854daca8595`
//!   (`/home/jason/work/ai-chat`, read with `git show`): `ChatView` in
//!   `crates/chat_view/src/view.rs` (`view.rs`) and
//!   `crates/chat_view/src/transcript_scroll.rs` (`transcript_scroll.rs`),
//!   the outline in `crates/outline/src/outline.rs` (`outline.rs`);
//! - `allsum-desktop` at `4f975246` (`ChatApp` in `src/app/render.rs`,
//!   `PreviewSplit` in `src/app/preview_split.rs`);
//! - `gpui-kit` at `40ad5270` (`ScrollBounce` in
//!   `crates/base/src/scroll_bounce.rs`; its `Scrollbar`,
//!   `crates/base/src/scrollbar.rs`, is mirrored by `scenarios::scrollbar`).
//!
//! What every scenario here shares with `ChatView` (the "plain" base,
//! `chat-scroll-plain`):
//!
//! - a `list` of messages with `2048` px of overdraw (`view.rs:122`), its
//!   rows rendered by a closure that reads the view (`view.rs:6441`); a row
//!   scans the active path for the latest assistant message
//!   (`view.rs:2790`), whose toolbar always shows, and draws a question as a
//!   bubble and an answer as its Markdown body (`scenarios::chat::MessageBody`,
//!   a view of its own) and a toolbar. Bodies are parsed before the first
//!   frame, as a loaded session's are: `sync_history_states` builds every
//!   history message's Markdown state as the view renders (`view.rs:5779`);
//! - the list's scroll handler, deferred because the list is borrowed while
//!   it runs, which works out whether the list is at its end
//!   (`on_user_scroll`, `transcript_scroll.rs:161`: `is_scrolled_to_end`,
//!   falling back to `logical_scroll_top` and `transcript_at_end`) and
//!   notifies the view only when following the end changed
//!   (`view.rs:3007`).
//!
//! Every scenario starts at the end and sweeps up `FRAMES_PER_SWEEP` frames,
//! then down until the list is at its end again (`Sweep`): rows measured
//! only as they show (height hints) change the distance to it, and the end
//! is where following, the button and the bounce change.
//!
//! The patterns, one flag each (`Patterns`):
//!
//! - `reads_offset`: after building the list, `render` works out
//!   which turns the outline highlights (`view.rs:6526`), with
//!   `outline::visible_turn_range` (`outline.rs:884`): `item_is_above_viewport`
//!   and `item_is_below_viewport` per turn, then `logical_scroll_top`;
//! - `reads_at_end`: the list has height hints instead of being
//!   measured up front (`reset_with_uniform_height(n, 160px)`,
//!   `view.rs:3170`, and `restore_list_height_hints` when its width changes,
//!   `view.rs:3180`), and before building the list `render` asks whether the
//!   "back to bottom" button is wanted (`sync_scroll_to_bottom_fade`,
//!   `view.rs:6393` → `scroll_to_bottom_wanted`, `transcript_scroll.rs:200`
//!   → `transcript_at_end`, `transcript_scroll.rs:132`: `is_scrolled_to_end`,
//!   else `max_offset_for_scrollbar` + `scroll_px_offset_for_scrollbar`,
//!   `view.rs:4096`), and fades it a 0.12 step a frame by an `on_next_frame`
//!   loop that notifies the view (`view.rs:4392`);
//! - `notifies`: the list is wrapped in a `ScrollBounce`, as on
//!   macOS (`view.rs:6462`), whose wheel listener reads the list's offset
//!   before and after the list scrolls and, when a wheel pushes past an edge,
//!   pulls the content and notifies the view (`scroll_bounce.rs:276-401`);
//!   the pull settles over the next frames, each requesting another
//!   (`scroll_bounce.rs:244`). The sweep stays at the end for `DWELL_FRAMES`
//!   frames of wheel events, so it pushes past it;
//! - `animates` (`chat-scroll-animates`): an outline jump lands every `JUMP_EVERY_FRAMES`
//!   frames and highlights its turn for 1800 ms (`outline.rs:1064`, 108
//!   frames at 60 Hz, counted in frames here), the row reading the highlight
//!   and `render` requesting an animation frame while it shows
//!   (`sync_outline_highlight`, `view.rs:4365`);
//! - `sticks`: before building the list, `render` calls
//!   `scroll_to_end` every time while following the end (`view.rs:6428`),
//!   and the sweep stays at the end for `DWELL_FRAMES` frames of wheel events;
//! - `parent_writes`: the view is held as Allsum holds it:
//!   `ChatApp`, whose render writes the chat view (`set_active`,
//!   `render.rs:86`) and the preview split (`render.rs:72`) and draws the
//!   session sidebar inline, holds `PreviewSplit` as a plain child
//!   (`render.rs:259`), which holds the chat view `.cached()`
//!   (`preview_split.rs:186`);
//! - `chrome`: what `ChatView` draws besides: it writes its
//!   composer's state as it renders (`view.rs:6590`), draws a bottom fade
//!   (`view.rs:6668`) and a GPUI Kit scrollbar (`view.rs:6676`), and
//!   measures itself with a canvas that defers an update of the view every
//!   frame (`render_measure`, `view.rs:2680`). The scrollbar's wheel
//!   listener notifies the view whenever the offset moved since the
//!   scrollbar was last prepainted (`scrollbar.rs:1645`), which a second
//!   wheel event in one frame finds;
//! - `trackpad` (`chat-scroll-trackpad`): scrolled by a trackpad, four 10 px events a
//!   frame, instead of a mouse wheel's one 40 px event;
//! - `chat-scroll-allsum`: all of the above, scrolled by a trackpad;
//!   `chat-scroll-allsum-wheel` scrolls it by a mouse wheel.
//!
//! Where the real code differs from what was asked for: `ChatView`'s scroll
//! handler notifies only when following the end changed, and `ScrollBounce`
//! only when a wheel pushes past an edge, so no handler notifies on every
//! wheel event; the only animations while a finished transcript scrolls are
//! the button's fade and an outline jump's highlight; the list is
//! top-aligned (`view.rs:3006`).
//!
//! Left out: streaming, and what runs only while an answer streams (reasoning
//! and timer animations, `view.rs:6296-6350`); the outline jump's own
//! programmatic scroll (`view.rs:4352`); `ScrollBounce`'s wall-time physics
//! (a frame-count decay here); hit-testing the bounce by hitbox (by bounds
//! here); the scrollbar's fade and width animations; and the Markdown
//! views' real parser and layout.

use std::{cell::Cell, rc::Rc};

use gpui::{
    AnyElement, AnyView, App, Context, DispatchPhase, Entity, ListAlignment, ListState, Modifiers,
    PlatformInput, Render, ScrollDelta, ScrollWheelEvent, SharedString, StyleRefinement,
    TouchPhase, WeakEntity, Window, canvas, div, point, prelude::*, px,
};

use super::chat::{
    ComposerInput, MessageBody, border, code_background, color, link, muted, text_color,
};
use super::scrollbar::{self, Scrolled};
use crate::Scenario;

/// Messages in the transcript, a question and an answer per turn.
const MESSAGES: usize = 200;
/// How far beyond its viewport the list renders (`view.rs:122`).
const OVERDRAW: f32 = 2048.;
/// The height hint of a message not measured yet (`view.rs:132`).
const HEIGHT_HINT: f32 = 160.;
/// How much the "back to bottom" button fades a frame (`view.rs:135`).
const FADE_STEP: f32 = 0.12;
/// Frames scrolled up from the end before turning back.
const FRAMES_PER_SWEEP: usize = 120;
/// Frames the sweep keeps turning the wheel down at the end, when it dwells.
const DWELL_FRAMES: usize = 30;
/// How far a frame scrolls, in logical pixels, by either device.
const FRAME_STEP: f32 = 40.;
/// Trackpad events a frame.
const TRACKPAD_EVENTS: usize = 4;
/// How often an outline jump lands, in frames.
const JUMP_EVERY_FRAMES: usize = 240;
/// How long its highlight shows: 1800 ms at 60 frames a second.
const HIGHLIGHT_FRAMES: usize = 108;
/// The highlight's peak alpha (`outline.rs:1066`).
const HIGHLIGHT_ALPHA: f32 = 0.12;
/// How far the content is pulled past an edge per pixel pushed, and the
/// furthest it goes.
const BOUNCE_PULL: f32 = 0.5;
const BOUNCE_MAX: f32 = 60.;
/// What is left of the pull each frame as it settles.
const BOUNCE_DECAY: f32 = 0.8;
/// Sessions in Allsum's sidebar.
const SESSIONS: usize = 40;
/// Width of Allsum's sidebar.
const SIDEBAR_WIDTH: f32 = 260.;

/// The real-world patterns a scenario adds to the plain chat view.
#[derive(Clone, Copy, Default)]
struct Patterns {
    reads_offset: bool,
    reads_at_end: bool,
    notifies: bool,
    animates: bool,
    sticks: bool,
    parent_writes: bool,
    /// Composer write, bottom fade, scrollbar and measuring canvas.
    chrome: bool,
    trackpad: bool,
}

impl Patterns {
    /// Whether the sweep stays at the end for a while.
    fn dwells(self) -> bool {
        self.notifies || self.sticks
    }
}

/// Every pattern, as Allsum's chat page has them (`chat-scroll-allsum`).
const ALLSUM: Patterns = Patterns {
    reads_offset: true,
    reads_at_end: true,
    notifies: true,
    animates: true,
    sticks: true,
    parent_writes: true,
    chrome: true,
    trackpad: true,
};

/// A message of the session: whether an assistant sent it.
struct Message {
    assistant: bool,
}

/// The chat view, shaped like `ChatView`.
pub struct ChatView {
    patterns: Patterns,
    list_state: ListState,
    messages: Vec<Message>,
    /// The messages the list shows, by index into `messages`.
    active_path: Vec<usize>,
    bodies: Vec<Entity<MessageBody>>,
    stick_to_bottom: bool,
    scroll_to_bottom_opacity: f32,
    scroll_to_bottom_fading: bool,
    composer: Entity<ComposerInput>,
    active: bool,
    /// The width `render_measure` last measured, and the list's width when
    /// height hints were last restored.
    measured_width: gpui::Pixels,
    list_width: gpui::Pixels,
    /// The question row an outline jump landed on, and frames left of its
    /// highlight.
    highlight: Option<(usize, usize)>,
    /// How far `ScrollBounce` pulled the content past an edge.
    bounce: f32,
    scrollbar: Rc<Cell<scrollbar::State>>,
}

impl ChatView {
    fn new(patterns: Patterns, cx: &mut Context<Self>) -> Self {
        let list_state = ListState::new(MESSAGES, ListAlignment::Top, px(OVERDRAW));
        let list_state = if patterns.reads_at_end {
            list_state.reset_with_uniform_height(MESSAGES, px(HEIGHT_HINT));
            list_state
        } else {
            list_state.measure_all()
        };
        let view = cx.entity().downgrade();
        list_state.set_scroll_handler(move |_, _, cx| {
            let view = view.clone();
            cx.defer(move |cx| {
                let _ = view.update(cx, |this, cx| {
                    if this.on_user_scroll() {
                        cx.notify();
                    }
                });
            });
        });
        let messages = (0..MESSAGES)
            .map(|ix| Message {
                assistant: !ix.is_multiple_of(2),
            })
            .collect();
        let bodies = (0..MESSAGES)
            .map(|ix| {
                let mut body = MessageBody::new(ix);
                body.parsed = true;
                cx.new(|_| body)
            })
            .collect();
        list_state.scroll_to_end();
        Self {
            patterns,
            list_state,
            messages,
            active_path: (0..MESSAGES).collect(),
            bodies,
            stick_to_bottom: true,
            scroll_to_bottom_opacity: 0.,
            scroll_to_bottom_fading: false,
            composer: cx.new(|_| ComposerInput::default()),
            active: false,
            measured_width: px(0.),
            list_width: px(0.),
            highlight: None,
            bounce: 0.,
            scrollbar: Rc::default(),
        }
    }

    fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    /// `transcript_scroll.rs:161`: whether following the end changed.
    fn on_user_scroll(&mut self) -> bool {
        let at_end = match self.list_state.is_scrolled_to_end() {
            Some(at_end) => at_end,
            None if self.list_state.logical_scroll_top().item_ix == self.active_path.len() => true,
            None => self.transcript_at_end(),
        };
        let changed = self.stick_to_bottom != at_end;
        self.stick_to_bottom = at_end;
        changed
    }

    /// `transcript_scroll.rs:132`.
    fn transcript_at_end(&self) -> bool {
        self.list_state
            .is_scrolled_to_end()
            .unwrap_or_else(|| self.distance_from_bottom() <= 1.)
    }

    /// `view.rs:4096`.
    fn distance_from_bottom(&self) -> f32 {
        f32::from(
            self.list_state.max_offset_for_scrollbar().y
                + self.list_state.scroll_px_offset_for_scrollbar().y,
        )
        .max(0.)
    }

    /// `transcript_scroll.rs:200` and `view.rs:4174`.
    fn scroll_to_bottom_target_opacity(&self) -> f32 {
        if !self.stick_to_bottom && !self.transcript_at_end() {
            1.
        } else {
            0.
        }
    }

    /// `view.rs:4373`.
    fn sync_scroll_to_bottom_fade(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.scroll_to_bottom_fading
            || (self.scroll_to_bottom_opacity - self.scroll_to_bottom_target_opacity()).abs()
                < f32::EPSILON
        {
            return;
        }
        self.scroll_to_bottom_fading = true;
        Self::step_scroll_to_bottom_fade(cx.entity().downgrade(), window);
    }

    /// `view.rs:4392`.
    fn step_scroll_to_bottom_fade(weak: WeakEntity<Self>, window: &mut Window) {
        window.on_next_frame(move |window, cx| {
            let mut keep_going = false;
            let _ = weak.update(cx, |this, cx| {
                this.scroll_to_bottom_fading = false;
                let target = this.scroll_to_bottom_target_opacity();
                let next = if this.scroll_to_bottom_opacity < target {
                    (this.scroll_to_bottom_opacity + FADE_STEP).min(target)
                } else {
                    (this.scroll_to_bottom_opacity - FADE_STEP).max(target)
                };
                this.scroll_to_bottom_opacity = next;
                if (next - target).abs() > f32::EPSILON {
                    keep_going = true;
                    this.scroll_to_bottom_fading = true;
                }
                cx.notify();
            });
            if keep_going {
                Self::step_scroll_to_bottom_fade(weak, window);
            }
        });
    }

    /// `outline.rs:884`: the turns (`(first, last)`, by turn) whose question
    /// row meets the viewport, else the turn the viewport's top is in.
    fn outline_visible_turns(&self) -> Option<(usize, usize)> {
        let mut range: Option<(usize, usize)> = None;
        for turn in 0..self.active_path.len() / 2 {
            let list_ix = turn * 2;
            if self.list_state.item_is_above_viewport(list_ix) == Some(true) {
                continue;
            }
            if self.list_state.item_is_below_viewport(list_ix) == Some(true) {
                break;
            }
            range = Some(match range {
                Some((first, _)) => (first, turn),
                None => (turn, turn),
            });
        }
        if range.is_some() {
            return range;
        }
        let top = self.list_state.logical_scroll_top().item_ix;
        (0..self.active_path.len() / 2)
            .rposition(|turn| turn * 2 <= top)
            .map(|turn| (turn, turn))
    }

    /// The outline beside the transcript, one tick a turn, `visible`
    /// highlighted.
    fn outline(visible: Option<(usize, usize)>, turns: usize) -> impl IntoElement {
        div()
            .absolute()
            .top(px(48.))
            .right(px(24.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .children((0..turns).map(move |turn| {
                let shown = visible.is_some_and(|(first, last)| (first..=last).contains(&turn));
                div()
                    .id(("outline", turn))
                    .w(px(if shown { 16. } else { 10. }))
                    .h(px(3.))
                    .rounded_sm()
                    .bg(if shown { link() } else { border() })
                    .hover(|style| style.bg(muted()))
            }))
    }

    /// `view.rs:2753`: row `ix`, read from the view.
    fn render_row(&self, ix: usize) -> AnyElement {
        let message_ix = self.active_path[ix];
        let message = &self.messages[message_ix];
        // `view.rs:2790`: every row looks for the latest assistant message.
        let latest_assistant = self
            .active_path
            .iter()
            .map(|ix| (*ix, &self.messages[*ix]))
            .rfind(|(_, message)| message.assistant)
            .map(|(ix, _)| ix);
        let highlight = self
            .highlight
            .filter(|(row, _)| *row == ix)
            .map(|(_, left)| HIGHLIGHT_ALPHA * left as f32 / HIGHLIGHT_FRAMES as f32);
        let body = self.bodies[message_ix].clone();
        let content = if !message.assistant {
            div()
                .flex()
                .justify_end()
                .child(
                    div()
                        .max_w(px(560.))
                        .px_4()
                        .py_2()
                        .rounded_xl()
                        .bg(code_background())
                        .child(body),
                )
                .into_any_element()
        } else {
            let latest = latest_assistant == Some(message_ix);
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(body)
                .child(
                    div()
                        .flex()
                        .gap_1()
                        .when(!latest, |this| {
                            this.invisible()
                                .group_hover("message", |style| style.visible())
                        })
                        .children(
                            ["Copy", "Retry", "Like", "Dislike", "Share"]
                                .into_iter()
                                .map(|label| {
                                    div()
                                        .id(SharedString::from(format!("{label}-{ix}")))
                                        .px_2()
                                        .py_0p5()
                                        .rounded_md()
                                        .text_xs()
                                        .text_color(muted())
                                        .hover(|style| style.bg(code_background()))
                                        .child(label)
                                }),
                        ),
                )
                .into_any_element()
        };
        div()
            .id(("message", ix))
            .group("message")
            .w_full()
            .px_6()
            .py_3()
            .when_some(highlight, |this, alpha| {
                let mut background = link();
                background.a = alpha;
                this.bg(background)
            })
            .child(div().max_w(px(760.)).mx_auto().child(content))
            .into_any_element()
    }

    /// `ScrollBounce`'s wheel listener (`scroll_bounce.rs:276`): reads the
    /// list's offset as the capture phase passes and again as the bubble
    /// phase returns after the list scrolled, and pulls the content when a
    /// wheel pushed past an edge. Painted before the list, as `ScrollBounce`
    /// registers its listener before painting its child, so it bubbles after
    /// the list's.
    fn bounce_listener(view: WeakEntity<Self>, list_state: ListState) -> impl IntoElement {
        canvas(
            |bounds, _, _| bounds,
            move |_, bounds, window, _| {
                let before = Cell::new(px(0.));
                window.on_mouse_event(move |event: &ScrollWheelEvent, phase, _, cx| {
                    let ScrollDelta::Pixels(delta) = event.delta else {
                        return;
                    };
                    if !bounds.contains(&event.position) {
                        return;
                    }
                    let offset = list_state.scroll_px_offset_for_scrollbar().y;
                    if phase == DispatchPhase::Capture {
                        before.set(offset);
                        return;
                    }
                    let max = list_state.max_offset_for_scrollbar().y;
                    let requested = f32::from(delta.y);
                    let at_outward_edge =
                        (requested > 0. && offset >= px(0.)) || (requested < 0. && offset <= -max);
                    let residual = (requested - f32::from(offset - before.get()))
                        .clamp(requested.min(0.), requested.max(0.));
                    if at_outward_edge && residual.abs() > 0.01 {
                        let _ = view.update(cx, |this, cx| {
                            this.bounce = (this.bounce + residual * BOUNCE_PULL)
                                .clamp(-BOUNCE_MAX, BOUNCE_MAX);
                            cx.notify();
                        });
                    }
                });
            },
        )
        .absolute()
        .size_full()
    }

    /// `render_measure` (`view.rs:2680`): a canvas that, every frame it is
    /// prepainted, defers an update of the view recording its size and
    /// restoring the list's height hints when its width changed.
    fn render_measure(&self, cx: &Context<Self>) -> impl IntoElement {
        let weak = cx.entity().downgrade();
        canvas(
            move |bounds, _, cx: &mut App| {
                let width = bounds.size.width;
                cx.defer(move |cx| {
                    let _ = weak.update(cx, |this, _| {
                        if (f32::from(this.measured_width) - f32::from(width)).abs() > 0.5 {
                            this.measured_width = width;
                        }
                        this.restore_list_height_hints();
                    });
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full()
    }

    /// `view.rs:3180`.
    fn restore_list_height_hints(&mut self) {
        let width = self.list_state.viewport_bounds().size.width;
        if width == self.list_width {
            return;
        }
        self.list_width = width;
        if width > px(0.) && self.patterns.reads_at_end {
            self.list_state
                .clone()
                .with_uniform_item_height(px(HEIGHT_HINT));
        }
    }
}

impl Render for ChatView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let patterns = self.patterns;
        if patterns.reads_at_end {
            self.sync_scroll_to_bottom_fade(window, cx);
        }
        if patterns.notifies && self.bounce != 0. {
            // `ScrollBounce` steps its pull and asks for another frame while
            // it settles (`scroll_bounce.rs:244`).
            self.bounce *= BOUNCE_DECAY;
            if self.bounce.abs() < 0.5 {
                self.bounce = 0.;
            }
            window.request_animation_frame();
        }
        if patterns.sticks && self.stick_to_bottom {
            self.list_state.scroll_to_end();
        }
        let view = cx.entity().downgrade();
        let list_view = {
            let view = view.clone();
            gpui::list(self.list_state.clone(), move |ix, _, cx| {
                let Some(this) = view.upgrade() else {
                    return div().into_any_element();
                };
                this.read(cx).render_row(ix)
            })
            .flex_1()
        };
        let list_view = if patterns.notifies {
            div()
                .relative()
                .flex()
                .flex_col()
                .size_full()
                .top(px(self.bounce))
                .child(Self::bounce_listener(view, self.list_state.clone()))
                .child(list_view)
                .into_any_element()
        } else {
            list_view.into_any_element()
        };
        if patterns.animates {
            // `sync_outline_highlight` (`view.rs:4365`).
            if let Some((row, left)) = self.highlight {
                self.highlight = (left > 1).then_some((row, left - 1));
                window.request_animation_frame();
            }
        }
        let outline = patterns
            .reads_offset
            .then(|| Self::outline(self.outline_visible_turns(), self.active_path.len() / 2));
        let composer = if patterns.chrome {
            // `view.rs:6590`: the composer renders inside an update of it.
            self.composer.update(cx, |input, _| {
                input.placeholder = "Ask anything...".into();
                input.disabled = false;
                input.placeholder.clone()
            })
        } else {
            "Ask anything...".into()
        };
        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(gpui::white())
            .when(patterns.chrome, |this| this.child(self.render_measure(cx)))
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(list_view)
                    .when(patterns.chrome, |this| {
                        this.child(
                            div()
                                .absolute()
                                .bottom_0()
                                .left_0()
                                .w_full()
                                .h(px(32.))
                                .bg(gpui::white())
                                .opacity(0.7),
                        )
                    })
                    .children(outline)
                    .when(patterns.chrome, |this| {
                        this.child(scrollbar::scrollbar(
                            Scrolled::List(self.list_state.clone()),
                            self.scrollbar.clone(),
                        ))
                    })
                    .when(self.scroll_to_bottom_opacity > 0., |this| {
                        this.child(
                            div()
                                .id("scroll-to-bottom")
                                .absolute()
                                .bottom_3()
                                .left(px(360.))
                                .size_8()
                                .rounded_full()
                                .border_1()
                                .border_color(border())
                                .bg(gpui::white())
                                .opacity(self.scroll_to_bottom_opacity)
                                // Clicking it follows the end again.
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.stick_to_bottom = true;
                                    this.list_state.scroll_to_end();
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .h(px(96.))
                    .mx_6()
                    .mb_3()
                    .p_3()
                    .rounded_xl()
                    .border_1()
                    .border_color(if self.active { link() } else { border() })
                    .text_color(muted())
                    .child(composer),
            )
    }
}

/// `PreviewSplit` (`preview_split.rs:172`): holds the chat view cached.
pub struct PreviewSplit {
    chat: Entity<ChatView>,
    active: bool,
}

impl PreviewSplit {
    /// `preview_split.rs:254`: changes nothing unless `active` does.
    fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active == active {
            return;
        }
        self.active = active;
        cx.notify();
    }
}

impl Render for PreviewSplit {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("allsum-chat-view")
            .size_full()
            .min_h_0()
            .min_w_0()
            .child(
                self.chat
                    .clone()
                    .cached(StyleRefinement::default().size_full().min_w_0()),
            )
    }
}

/// Allsum's `ChatApp` (`render.rs:57`).
pub struct ChatApp {
    chat: Entity<ChatView>,
    preview_split: Entity<PreviewSplit>,
}

impl Render for ChatApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let chat_visible = true;
        self.preview_split
            .update(cx, |split, cx| split.set_active(chat_visible, cx));
        self.chat
            .update(cx, |view, _| view.set_active(chat_visible));
        div()
            .flex()
            .flex_row()
            .size_full()
            .bg(gpui::white())
            .child(
                // The session sidebar, drawn by `ChatApp` itself
                // (`render.rs:371`).
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .w(px(SIDEBAR_WIDTH))
                    .h_full()
                    .p_2()
                    .gap_0p5()
                    .bg(color(220., 0.15, 0.97))
                    .border_r_1()
                    .border_color(border())
                    .children((0..SESSIONS).map(|ix| {
                        div()
                            .id(("session", ix))
                            .px_2()
                            .h(px(28.))
                            .rounded_md()
                            .text_sm()
                            .text_color(text_color())
                            .hover(|style| style.bg(border()))
                            .when(ix == 0, |this| this.bg(border()))
                            .child(SharedString::from(format!("Session {}", ix + 1)))
                    })),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .h_full()
                    .min_h_0()
                    .min_w_0()
                    .child(self.preview_split.clone()),
            )
    }
}

/// The chat view a scenario's root view is or holds.
fn chat_view(root: &AnyView, cx: &App) -> Option<Entity<ChatView>> {
    match root.clone().downcast::<ChatView>() {
        Ok(view) => Some(view),
        Err(root) => Some(root.downcast::<ChatApp>().ok()?.read(cx).chat.clone()),
    }
}

/// Where a scenario's sweep is: scrolling up from the end (frames done),
/// back down until the list is at its end, or staying there (frames done).
#[derive(Clone, Copy)]
enum Sweep {
    Up(usize),
    Down,
    Dwell(usize),
}

impl Sweep {
    /// What this frame does after `self`, given whether the list is at its
    /// end and how long the sweep stays there: the next state, whether the
    /// frame scrolls up, and whether it starts a gesture.
    fn next(self, at_end: bool, dwell: usize) -> (Sweep, bool, bool) {
        let up_again = (Sweep::Up(1), true, true);
        match self {
            Sweep::Up(done) if done < FRAMES_PER_SWEEP => (Sweep::Up(done + 1), true, done == 0),
            Sweep::Up(_) => (Sweep::Down, false, true),
            Sweep::Down if !at_end => (Sweep::Down, false, false),
            Sweep::Down if dwell > 0 => (Sweep::Dwell(1), false, false),
            Sweep::Down => up_again,
            Sweep::Dwell(done) if done < dwell => (Sweep::Dwell(done + 1), false, false),
            Sweep::Dwell(_) => up_again,
        }
    }
}

/// The events of a frame, over the messages in the runner's 1440 × 900
/// window, scrolling `up` or down: by a trackpad, several small events, the
/// first starting a gesture when `starts`, or by a mouse wheel, one.
fn scroll_events(up: bool, starts: bool, trackpad: bool) -> Vec<PlatformInput> {
    let sign = if up { 1. } else { -1. };
    let event = |delta: f32, touch_phase| {
        PlatformInput::ScrollWheel(ScrollWheelEvent {
            position: point(px(720.), px(400.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(sign * delta))),
            modifiers: Modifiers::default(),
            touch_phase,
        })
    };
    if !trackpad {
        return vec![event(FRAME_STEP, TouchPhase::Moved)];
    }
    (0..TRACKPAD_EVENTS)
        .map(|ix| {
            let phase = if starts && ix == 0 {
                TouchPhase::Started
            } else {
                TouchPhase::Moved
            };
            event(FRAME_STEP / TRACKPAD_EVENTS as f32, phase)
        })
        .collect()
}

/// The root view of a scenario with `patterns`: the chat view, or Allsum's
/// `ChatApp` holding it.
fn build(patterns: Patterns, cx: &mut App) -> AnyView {
    let chat = cx.new(|cx| ChatView::new(patterns, cx));
    if patterns.parent_writes {
        let preview_split = cx.new(|_| PreviewSplit {
            chat: chat.clone(),
            active: false,
        });
        cx.new(|_| ChatApp {
            chat,
            preview_split,
        })
        .into()
    } else {
        chat.into()
    }
}

/// Allsum's chat window as `chat-scroll-allsum` builds it, every pattern
/// included, for the showcase's Chat page to scroll by hand.
pub fn allsum_chat_window(cx: &mut App) -> AnyView {
    build(ALLSUM, cx)
}

struct RealChatScroll {
    name: &'static str,
    description: &'static str,
    patterns: Patterns,
    sweep: Cell<Sweep>,
}

impl Scenario for RealChatScroll {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn build(&self, _: &mut Window, cx: &mut App) -> AnyView {
        build(self.patterns, cx)
    }

    fn step(&self, root: &AnyView, frame: usize, window: &mut Window, cx: &mut App) {
        let mut at_end = false;
        if let Some(view) = chat_view(root, cx) {
            at_end = view.read(cx).list_state.is_scrolled_to_end() == Some(true);
            if self.patterns.animates && frame % JUMP_EVERY_FRAMES == JUMP_EVERY_FRAMES / 4 {
                // An outline jump lands on the question of the turn at the
                // top, which then shows highlighted.
                view.update(cx, |this, cx| {
                    let row = this.list_state.logical_scroll_top().item_ix & !1;
                    this.highlight = Some((row, HIGHLIGHT_FRAMES));
                    cx.notify();
                });
            }
        }
        let dwell = if self.patterns.dwells() {
            DWELL_FRAMES
        } else {
            0
        };
        let (sweep, up, starts) = self.sweep.get().next(at_end, dwell);
        self.sweep.set(sweep);
        for event in scroll_events(up, starts, self.patterns.trackpad) {
            window.dispatch_event(event, cx);
        }
    }
}

pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    let none = Patterns::default();
    let allsum = ALLSUM;
    let variants: [(&'static str, &'static str, Patterns); 5] = [
        (
            "chat-scroll-plain",
            "A 200-message transcript shaped like ai-chat's ChatView, with none of the patterns of chat-scroll-allsum, scrolled by the wheel",
            none,
        ),
        (
            "chat-scroll-animates",
            "chat-scroll-plain where an outline jump's highlight lands on a row every 240 frames and animates for 108 frames by request_animation_frame",
            Patterns {
                animates: true,
                ..none
            },
        ),
        (
            "chat-scroll-trackpad",
            "chat-scroll-plain scrolled by a trackpad, four 10 px events a frame",
            Patterns {
                trackpad: true,
                ..none
            },
        ),
        (
            "chat-scroll-allsum",
            "Allsum's chat page: an outline read, a back-to-bottom fade, a ScrollBounce, outline-jump highlights, sticking to the end, Allsum's ChatApp and PreviewSplit, and ChatView's composer, fade, scrollbar and measuring canvas, scrolled by a trackpad",
            allsum,
        ),
        (
            "chat-scroll-allsum-wheel",
            "chat-scroll-allsum scrolled by a mouse wheel, one event a frame",
            Patterns {
                trackpad: false,
                ..allsum
            },
        ),
    ];
    variants
        .into_iter()
        .map(|(name, description, patterns)| {
            Box::new(RealChatScroll {
                name,
                description,
                patterns,
                sweep: Cell::new(Sweep::Up(0)),
            }) as Box<dyn Scenario>
        })
        .collect()
}
