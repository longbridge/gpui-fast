//! Scrolling a chat transcript, shaped like an AI chat client's: a `list` of
//! messages whose Markdown bodies are views of their own, scrolled by the
//! wheel with the pointer over the messages.
//!
//! The view holding the list does what such a client's does:
//!
//! - its list's rows are rendered by a closure that reads the view, as a
//!   transcript reads its session to find the message at a row;
//! - each message is a row of plain elements — a bubble, or an answer with a
//!   toolbar shown on hover — around the message's body, a view of its own
//!   (`MessageBody`), as a Markdown text view keeps its parsed state in an
//!   entity: paragraphs with bold, inline code and links, code blocks and
//!   tables. A body shows its first block until it is parsed, which it is
//!   the frame after it first renders, as Markdown is parsed in the
//!   background;
//! - it renders a composer, writing its options into the composer's input
//!   state every time, as an input component does;
//! - after building the list, it reads which message the list shows first,
//!   to mark it in an outline beside the transcript;
//! - it follows the transcript's end until the wheel scrolls away from it,
//!   which the list's scroll handler works out in a deferred update of the
//!   view, notifying it only when that changed;
//! - it is notified every two seconds for something the transcript does not
//!   show, as a client's view is by a task it runs;
//! - in `chat-scroll`, as it renders it asks the list whether it is scrolled
//!   to its end, to show a "back to bottom" button, which it fades in and
//!   out a step a frame; `chat-scroll-no-button` has no button, to tell what
//!   it costs.
//!
//! `scenarios::chat_patterns` builds transcripts mirroring real chat views
//! from this one's message bodies.

use gpui::{
    AnyElement, AnyView, App, Context, Entity, FontWeight, Hsla, ListAlignment, ListState,
    Modifiers, PlatformInput, Render, ScrollDelta, ScrollWheelEvent, SharedString, StyledText,
    TextRun, TouchPhase, WeakEntity, Window, div, font, hsla, list, point, prelude::*, px,
};

use crate::Scenario;

/// Messages in the transcript: a question and an answer per round.
const MESSAGES: usize = 160;
/// How far beyond its viewport the list renders, as the desktop client's.
const OVERDRAW: f32 = 2048.;
/// How far one wheel event scrolls, in logical pixels.
const WHEEL_STEP: f32 = 40.;
/// Frames scrolled in one direction before turning back.
const FRAMES_PER_SWEEP: usize = 120;
/// How often the view holding the transcript is notified for something
/// else, in frames: every two seconds at 60 frames a second.
const NOTIFY_EVERY_FRAMES: usize = 120;

pub(crate) fn color(hue: f32, saturation: f32, lightness: f32) -> Hsla {
    hsla(hue / 360., saturation, lightness, 1.)
}

pub(crate) fn text_color() -> Hsla {
    color(220., 0.2, 0.15)
}

pub(crate) fn muted() -> Hsla {
    color(220., 0.1, 0.45)
}

pub(crate) fn border() -> Hsla {
    color(220., 0.13, 0.88)
}

pub(crate) fn code_background() -> Hsla {
    color(220., 0.2, 0.95)
}

pub(crate) fn link() -> Hsla {
    color(215., 0.8, 0.45)
}

/// Words the generated paragraphs are made of.
const WORDS: [&str; 24] = [
    "revenue",
    "margin",
    "guidance",
    "quarter",
    "growth",
    "segment",
    "demand",
    "pricing",
    "the",
    "and",
    "of",
    "for",
    "while",
    "operating",
    "cash",
    "flow",
    "analysts",
    "expect",
    "a",
    "stronger",
    "second",
    "half",
    "with",
    "inventory",
];

/// A deterministic paragraph of about `words` words for message `seed`.
fn sentence(seed: usize, words: usize) -> String {
    let mut text = String::new();
    for ix in 0..words {
        if ix > 0 {
            text.push(' ');
        }
        text.push_str(WORDS[(seed * 7 + ix * 13 + ix / 3) % WORDS.len()]);
    }
    text.push('.');
    text
}

/// One block of a Markdown body.
enum Block {
    /// A paragraph: its text and runs of bold, inline code and links over it.
    Paragraph(SharedString, Vec<(std::ops::Range<usize>, Style)>),
    Code(Vec<SharedString>),
    Table(Vec<Vec<SharedString>>),
}

#[derive(Clone, Copy)]
enum Style {
    Bold,
    Code,
    Link,
}

/// A paragraph with some of its words styled.
fn paragraph(seed: usize, words: usize) -> Block {
    let text = sentence(seed, words);
    let mut runs = Vec::new();
    let mut start = 0;
    for (ix, word) in text.split(' ').enumerate() {
        let end = start + word.len();
        let style = match (seed + ix) % 11 {
            0 => Some(Style::Bold),
            4 => Some(Style::Code),
            8 => Some(Style::Link),
            _ => None,
        };
        if let Some(style) = style {
            runs.push((start..end.min(text.len()), style));
        }
        start = end + 1;
    }
    Block::Paragraph(text.into(), runs)
}

/// The body of message `ix`: a question is a short paragraph; an answer a
/// few paragraphs, and every few answers a code block or a table.
fn blocks(ix: usize) -> Vec<Block> {
    if ix.is_multiple_of(2) {
        return vec![paragraph(ix, 12 + ix % 20)];
    }
    let mut blocks = Vec::new();
    for p in 0..2 + ix % 4 {
        blocks.push(paragraph(ix * 5 + p, 30 + (ix + p * 17) % 60));
    }
    if ix % 3 == 1 {
        blocks.push(Block::Code(
            (0..6 + ix % 8)
                .map(|line| {
                    SharedString::from(format!(
                        "let value_{line} = segment.revenue({line}) * margin + {ix};"
                    ))
                })
                .collect(),
        ));
    }
    if ix % 5 == 3 {
        blocks.push(Block::Table(
            (0..6)
                .map(|row| {
                    (0..4)
                        .map(|col| {
                            SharedString::from(if row == 0 {
                                format!("Column {col}")
                            } else {
                                format!("{:.2}", (ix * 31 + row * 7 + col * 3) as f32 / 9.)
                            })
                        })
                        .collect()
                })
                .collect(),
        ));
    }
    blocks.push(paragraph(ix * 5 + 9, 20 + ix % 30));
    blocks
}

/// A message's Markdown body: a view of its own, holding its parsed blocks,
/// as a Markdown text view holds its state in an entity.
pub struct MessageBody {
    blocks: Vec<Block>,
    /// Whether the body rendered, and whether it was parsed since.
    pub(crate) rendered: bool,
    pub(crate) parsed: bool,
}

impl MessageBody {
    /// The body of message `ix`, not rendered yet.
    pub(crate) fn new(ix: usize) -> Self {
        Self {
            blocks: blocks(ix),
            rendered: false,
            parsed: false,
        }
    }
}

fn styled_paragraph(text: &SharedString, runs: &[(std::ops::Range<usize>, Style)]) -> StyledText {
    let base = font(".SystemUIFont");
    let mut text_runs = Vec::new();
    let mut at = 0;
    let plain = |len: usize| TextRun {
        len,
        font: base.clone(),
        color: text_color(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    for (range, style) in runs {
        if range.start > at {
            text_runs.push(plain(range.start - at));
        }
        let mut run = plain(range.len());
        match style {
            Style::Bold => run.font.weight = FontWeight::BOLD,
            Style::Code => {
                run.font = font("Menlo");
                run.background_color = Some(code_background());
            }
            Style::Link => {
                run.color = link();
                run.underline = Some(gpui::UnderlineStyle {
                    thickness: px(1.),
                    color: Some(link()),
                    wavy: false,
                });
            }
        }
        text_runs.push(run);
        at = range.end;
    }
    if at < text.len() {
        text_runs.push(plain(text.len() - at));
    }
    StyledText::new(text.clone()).with_runs(text_runs)
}

impl Render for MessageBody {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.rendered = true;
        let shown = if self.parsed { self.blocks.len() } else { 1 };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .text_sm()
            .line_height(px(22.))
            .text_color(text_color())
            .children(self.blocks.iter().take(shown).map(|block| {
                match block {
                    Block::Paragraph(text, runs) => {
                        div().child(styled_paragraph(text, runs)).into_any_element()
                    }
                    Block::Code(lines) => div()
                        .flex()
                        .flex_col()
                        .p_3()
                        .rounded_md()
                        .bg(code_background())
                        .font_family("Menlo")
                        .text_xs()
                        .children(lines.iter().map(|line| div().child(line.clone())))
                        .into_any_element(),
                    Block::Table(rows) => div()
                        .flex()
                        .flex_col()
                        .border_1()
                        .border_color(border())
                        .rounded_md()
                        .children(rows.iter().enumerate().map(|(row_ix, row)| {
                            div()
                                .flex()
                                .when(row_ix > 0, |this| this.border_t_1().border_color(border()))
                                .when(row_ix == 0, |this| this.font_weight(FontWeight::SEMIBOLD))
                                .children(
                                    row.iter().map(|cell| {
                                        div().flex_1().px_2().py_1().child(cell.clone())
                                    }),
                                )
                        }))
                        .into_any_element(),
                }
            }))
    }
}

/// The transcript: a view holding the list of messages, as a chat view does.
pub struct Transcript {
    list_state: ListState,
    bodies: Vec<Entity<MessageBody>>,
    /// Whether the transcript follows its end, as it does until the user
    /// scrolls away from it and again once they scroll back to it.
    stick_to_bottom: bool,
    /// Whether the view shows a "back to bottom" button while the
    /// transcript is away from its end, which it works out as it renders.
    back_to_bottom: bool,
    /// The button's opacity, stepped a frame at a time towards whether it
    /// shows.
    button_opacity: f32,
    fading: bool,
    /// The composer's input state, which rendering writes its options into.
    composer: Entity<ComposerInput>,
}

/// What an input component keeps of the options it is rendered with.
#[derive(Default)]
pub struct ComposerInput {
    pub(crate) placeholder: SharedString,
    pub(crate) disabled: bool,
}

/// How much the "back to bottom" button fades in or out per frame.
const FADE_STEP: f32 = 0.125;

impl Transcript {
    fn new(back_to_bottom: bool, cx: &mut Context<Self>) -> Self {
        let list_state = ListState::new(MESSAGES, ListAlignment::Top, px(OVERDRAW)).measure_all();
        let view = cx.entity().downgrade();
        list_state.set_scroll_handler(move |_, _, cx| {
            let view = view.clone();
            // The list is borrowed while its handler runs.
            cx.defer(move |cx| {
                let _ = view.update(cx, |this, cx| {
                    let at_end = this.list_state.is_scrolled_to_end().unwrap_or(false);
                    if at_end != this.stick_to_bottom {
                        this.stick_to_bottom = at_end;
                        cx.notify();
                    }
                });
            });
        });
        let bodies = (0..MESSAGES)
            .map(|ix| {
                cx.new(|_| MessageBody {
                    blocks: blocks(ix),
                    rendered: false,
                    parsed: false,
                })
            })
            .collect();
        Self {
            list_state,
            bodies,
            stick_to_bottom: true,
            back_to_bottom,
            button_opacity: 0.,
            fading: false,
            composer: cx.new(|_| ComposerInput::default()),
        }
    }

    /// Whether the "back to bottom" button is to show: the transcript is
    /// away from its end, which is read from the list.
    fn button_wanted(&self) -> bool {
        self.back_to_bottom
            && !self.stick_to_bottom
            && self.list_state.is_scrolled_to_end() != Some(true)
    }

    /// Fades the button a step towards whether it is wanted on the next
    /// frame, and on until it gets there.
    fn fade(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = if self.button_wanted() { 1. } else { 0. };
        if self.fading || self.button_opacity == target {
            return;
        }
        self.fading = true;
        let view = cx.entity().downgrade();
        window.on_next_frame(move |_, cx| {
            let _ = view.update(cx, |this, cx| {
                this.fading = false;
                let target = if this.button_wanted() { 1. } else { 0. };
                this.button_opacity = if this.button_opacity < target {
                    (this.button_opacity + FADE_STEP).min(target)
                } else {
                    (this.button_opacity - FADE_STEP).max(target)
                };
                cx.notify();
            });
        });
    }

    fn row(view: &WeakEntity<Self>, ix: usize, cx: &App) -> AnyElement {
        let Some(this) = view.upgrade() else {
            return div().into_any_element();
        };
        let this = this.read(cx);
        let body = this.bodies[ix].clone();
        let question = ix.is_multiple_of(2);
        let content =
            if question {
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
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(body)
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .invisible()
                            .group_hover("message", |style| style.visible())
                            .children(["Copy", "Like", "Dislike", "Share"].into_iter().map(
                                |label| {
                                    div()
                                        .id(SharedString::from(format!("{label}-{ix}")))
                                        .px_2()
                                        .py_0p5()
                                        .rounded_md()
                                        .text_xs()
                                        .text_color(muted())
                                        .hover(|style| style.bg(code_background()))
                                        .child(label)
                                },
                            )),
                    )
                    .into_any_element()
            };
        div()
            .id(("message", ix))
            .group("message")
            .w_full()
            .px_6()
            .py_3()
            .child(div().max_w(px(760.)).mx_auto().child(content))
            .into_any_element()
    }
}

impl Render for Transcript {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.stick_to_bottom {
            self.list_state.scroll_to_end();
        }
        self.fade(window, cx);
        let placeholder = self.composer.update(cx, |input, _| {
            input.placeholder = "Ask anything...".into();
            input.disabled = false;
            input.placeholder.clone()
        });
        let view = cx.entity().downgrade();
        let list = list(self.list_state.clone(), move |ix, _, cx| {
            Self::row(&view, ix, cx)
        })
        .size_full();
        let first_shown = self.list_state.logical_scroll_top().item_ix;
        div()
            .relative()
            .size_full()
            .bg(gpui::white())
            .child(list)
            .child(
                div()
                    .absolute()
                    .top(px(16. + (first_shown % 40) as f32 * 8.))
                    .right(px(16.))
                    .w(px(12.))
                    .h(px(4.))
                    .rounded_sm()
                    .bg(link()),
            )
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .left(px(340.))
                    .w(px(760.))
                    .h(px(96.))
                    .p_3()
                    .rounded_xl()
                    .border_1()
                    .border_color(border())
                    .bg(gpui::white())
                    .text_color(muted())
                    .child(placeholder),
            )
            .when(self.button_opacity > 0., |this| {
                this.child(
                    div()
                        .absolute()
                        .bottom_4()
                        .left(px(700.))
                        .size_8()
                        .rounded_full()
                        .border_1()
                        .border_color(border())
                        .bg(gpui::white())
                        .opacity(self.button_opacity),
                )
            })
    }
}

/// The wheel event of frame `frame`, over the messages in the runner's
/// 1440 × 900 window: up from the end for `FRAMES_PER_SWEEP` frames, back
/// down as many, and again.
fn wheel(frame: usize) -> PlatformInput {
    let up = (frame / FRAMES_PER_SWEEP).is_multiple_of(2);
    let delta = if up { WHEEL_STEP } else { -WHEEL_STEP };
    PlatformInput::ScrollWheel(ScrollWheelEvent {
        position: point(px(720.), px(450.)),
        delta: ScrollDelta::Pixels(point(px(0.), px(delta))),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
    })
}

struct ChatScroll {
    name: &'static str,
    description: &'static str,
    back_to_bottom: bool,
}

impl Scenario for ChatScroll {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn build(&self, _: &mut Window, cx: &mut App) -> AnyView {
        let back_to_bottom = self.back_to_bottom;
        cx.new(|cx| Transcript::new(back_to_bottom, cx)).into()
    }

    fn step(&self, root: &AnyView, frame: usize, window: &mut Window, cx: &mut App) {
        // The bodies that rendered for the first time are parsed now.
        if let Ok(transcript) = root.clone().downcast::<Transcript>() {
            // The view is notified now and then for something the transcript
            // does not show, as a client's is by a task it runs.
            if frame % NOTIFY_EVERY_FRAMES == NOTIFY_EVERY_FRAMES / 2 {
                transcript.update(cx, |_, cx| cx.notify());
            }
            let bodies = transcript.read(cx).bodies.clone();
            for body in bodies {
                if body.read(cx).rendered && !body.read(cx).parsed {
                    body.update(cx, |body, cx| {
                        body.parsed = true;
                        cx.notify();
                    });
                }
            }
        }
        window.dispatch_event(wheel(frame), cx);
    }
}

pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(ChatScroll {
            name: "chat-scroll",
            description: "A 160-message chat transcript of Markdown body views, scrolled by the wheel away from its end and back, fading a back-to-bottom button in and out",
            back_to_bottom: true,
        }),
        Box::new(ChatScroll {
            name: "chat-scroll-no-button",
            description: "The chat transcript of chat-scroll without its back-to-bottom button, scrolled the same way",
            back_to_bottom: false,
        }),
    ]
}
