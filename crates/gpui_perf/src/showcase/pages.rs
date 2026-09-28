//! The pages the showcase scrolls: a page of component sections inside the
//! container that owns the scrolled area, as GPUI Kit's `StoryContainer` does;
//! a data table refreshed by a timer; and a list of messages of different
//! heights.

use std::time::Duration;

use gpui::{
    AnyElement, Context, Entity, FontWeight, Hsla, IntoElement, ListAlignment, ListState, Render,
    ScrollHandle, SharedString, Task, UniformListScrollHandle, Window, div, hsla, list, point,
    prelude::*, px, uniform_list,
};

use super::theme::{Theme, theme};

pub const TABLE_ROWS: usize = 5_000;
pub const MESSAGES: usize = 5_000;

/// What a page shows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    Components,
    Table,
    List,
}

/// The scrolled area around the page being shown. Scrolling it notifies this
/// view and moves the page view inside it.
pub struct Container {
    pub scroll: ScrollHandle,
    page: Entity<ComponentsPage>,
    table_page: Option<Entity<TablePage>>,
    pub table: Option<Entity<Table>>,
    pub messages: Option<Entity<MessageList>>,
    showing: PageKind,
    pub refreshing: bool,
}

impl Container {
    pub fn new(page: usize, cx: &mut Context<Self>) -> Self {
        Self {
            scroll: ScrollHandle::new(),
            page: cx.new(|_| ComponentsPage { seed: page }),
            table_page: None,
            table: None,
            messages: None,
            showing: PageKind::Components,
            refreshing: false,
        }
    }

    /// Shows the page `seed` stands for, of the given kind.
    pub fn show(&mut self, seed: usize, kind: PageKind, cx: &mut Context<Self>) {
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.showing = kind;
        match kind {
            PageKind::Table if self.table_page.is_none() => {
                let table = cx.new(|_| Table::new());
                self.table = Some(table.clone());
                self.table_page = Some(cx.new(|_| TablePage {
                    table,
                    refresh: None,
                }));
            }
            PageKind::List if self.messages.is_none() => {
                self.messages = Some(cx.new(|_| MessageList::new()));
            }
            PageKind::Components => self.page.update(cx, |page, cx| {
                page.seed = seed;
                cx.notify();
            }),
            _ => {}
        }
        cx.notify();
    }

    /// Starts or stops refreshing the table's rows.
    pub fn toggle_refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(page) = &self.table_page {
            self.refreshing = page.update(cx, |page, cx| page.toggle_refresh(window, cx));
        }
    }
}

impl Render for Container {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let content: AnyElement = match (self.showing, &self.table_page, &self.messages) {
            (PageKind::Table, Some(table_page), _) => div()
                .size_full()
                .p_6()
                .child(table_page.clone())
                .into_any_element(),
            (PageKind::List, _, Some(messages)) => {
                div().size_full().child(messages.clone()).into_any_element()
            }
            _ => div()
                .id("page-scroll")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .child(div().w_full().p_6().child(self.page.clone()))
                .into_any_element(),
        };
        div().flex_1().min_h_0().child(content)
    }
}

/// A page of component sections, as a GPUI Kit story is: one view whose
/// sections are plain elements.
pub struct ComponentsPage {
    seed: usize,
}

impl Render for ComponentsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let seed = self.seed;
        let theme = theme(cx);
        div()
            .flex()
            .flex_col()
            .gap_6()
            .children((0..24).map(|section_ix| section(seed, section_ix, theme)))
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Buttons,
    Avatars,
    Badges,
    Inputs,
}

fn section(seed: usize, section_ix: usize, theme: &Theme) -> impl IntoElement {
    let kind = [Kind::Buttons, Kind::Avatars, Kind::Badges, Kind::Inputs][(seed + section_ix) % 4];
    let items = 3 + (seed * 7 + section_ix * 5) % 9;
    let (title, description): (SharedString, &str) = match kind {
        Kind::Buttons => (
            format!("Buttons {}", section_ix + 1).into(),
            "Default, outline and ghost buttons, at the medium size.",
        ),
        Kind::Avatars => (
            format!("Avatars {}", section_ix + 1).into(),
            "Avatars with a count of unread messages.",
        ),
        Kind::Badges => (
            format!("Badges {}", section_ix + 1).into(),
            "Neutral badges classifying an item.",
        ),
        Kind::Inputs => (
            format!("Inputs {}", section_ix + 1).into(),
            "Empty inputs showing their placeholder.",
        ),
    };
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(description),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .p_4()
                .rounded(theme.radius * 1.5)
                .border_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .w_full()
                        .justify_center()
                        .items_center()
                        .gap_4()
                        .children((0..items).map(|item| match kind {
                            Kind::Buttons => button(section_ix, item, theme).into_any_element(),
                            Kind::Avatars => avatar(section_ix, item, theme).into_any_element(),
                            Kind::Badges => badge(item, theme).into_any_element(),
                            Kind::Inputs => input(item, theme).into_any_element(),
                        })),
                ),
        )
}

fn button(section_ix: usize, item: usize, theme: &Theme) -> impl IntoElement {
    const LABELS: [&str; 6] = [
        "Save",
        "Duplicate",
        "Export…",
        "Share…",
        "Archive",
        "Rename…",
    ];
    let base = div()
        .id(("button", section_ix * 100 + item))
        .flex()
        .items_center()
        .gap_2()
        .h_8()
        .px_3()
        .rounded(theme.radius)
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .child(LABELS[item % LABELS.len()]);
    match item % 3 {
        // Default.
        0 => base
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .hover(|this| this.bg(theme.accent))
            .active(|this| this.bg(theme.muted)),
        // Outline.
        1 => base
            .border_1()
            .border_color(theme.border)
            .hover(|this| this.bg(theme.accent))
            .active(|this| this.bg(theme.muted)),
        // Ghost.
        _ => base
            .hover(|this| this.bg(theme.accent))
            .active(|this| this.bg(theme.muted)),
    }
}

fn avatar(section_ix: usize, item: usize, theme: &Theme) -> impl IntoElement {
    // An avatar's color stands for the person, so it is data, not a token.
    let hue = ((section_ix * 31 + item * 17) % 100) as f32 / 100.;
    let unread = item * 7 % 120;
    div()
        .relative()
        .size_10()
        .child(
            div()
                .size_full()
                .rounded_full()
                .bg(hsla(hue, 0.45, 0.55, 1.))
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(theme.danger_foreground)
                .child(["JL", "AB", "ZY", "MK"][item % 4]),
        )
        .when(unread > 0, |this| {
            this.child(
                div()
                    .absolute()
                    .top_neg_1()
                    .right_neg_1p5()
                    .px_1()
                    .rounded_full()
                    .bg(theme.danger)
                    .text_color(theme.danger_foreground)
                    .text_xs()
                    .child(if unread > 99 {
                        "99+".to_string()
                    } else {
                        unread.to_string()
                    }),
            )
        })
}

fn badge(item: usize, theme: &Theme) -> impl IntoElement {
    const LABELS: [&str; 5] = ["Draft", "Shared", "Archived", "Internal", "Beta"];
    div()
        .px_2()
        .py_0p5()
        .rounded_full()
        .border_1()
        .border_color(theme.border)
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .child(LABELS[item % LABELS.len()])
}

fn input(item: usize, theme: &Theme) -> impl IntoElement {
    const PLACEHOLDERS: [&str; 4] = ["Name", "Email address", "Search", "Project"];
    div()
        .w_48()
        .h_8()
        .px_3()
        .flex()
        .items_center()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .text_sm()
        .text_color(theme.muted_foreground)
        .child(PLACEHOLDERS[item % PLACEHOLDERS.len()])
}

/// The page around the data table, as GPUI Kit's DataTable story has it: its
/// timer changes the table's rows and notifies this page, not the table.
pub struct TablePage {
    table: Entity<Table>,
    refresh: Option<Task<()>>,
}

impl TablePage {
    /// Starts or stops the refresh, returning whether it runs.
    fn toggle_refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        cx.notify();
        if self.refresh.take().is_some() {
            return false;
        }
        self.refresh = Some(cx.spawn_in(window, async move |this, cx| {
            let mut tick = 0usize;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(33))
                    .await;
                tick += 1;
                let updated = this.update(cx, |this, cx| {
                    this.table.update(cx, |table, _| table.tick(tick));
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        }));
        true
    }
}

impl Render for TablePage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_xs().text_color(theme.muted_foreground).child(
                if self.refresh.is_some() {
                    "Updating a third of the first 2,000 rows every 33 ms."
                } else {
                    "5,000 quotes. Turn on “Refresh data” to update rows every 33 ms."
                },
            ))
            .child(self.table.clone())
    }
}

struct Stock {
    symbol: SharedString,
    name: SharedString,
    price: f64,
    change: f64,
    volume: u64,
    high: f64,
    low: f64,
}

/// A virtualized table of quotes, which owns its scrolling.
pub struct Table {
    rows: Vec<Stock>,
    pub scroll: UniformListScrollHandle,
}

impl Table {
    fn new() -> Self {
        let rows = (0..TABLE_ROWS)
            .map(|ix| {
                let price = 10. + (ix * 37 % 1000) as f64 / 7.;
                Stock {
                    symbol: format!("S{ix:04}").into(),
                    name: format!("Company {ix}").into(),
                    price,
                    change: 0.,
                    volume: (ix as u64 * 7919) % 1_000_000,
                    high: price * 1.05,
                    low: price * 0.95,
                }
            })
            .collect();
        Self {
            rows,
            scroll: UniformListScrollHandle::new(),
        }
    }

    fn tick(&mut self, tick: usize) {
        for (ix, row) in self.rows.iter_mut().take(2_000).enumerate() {
            if (ix + tick).is_multiple_of(3) {
                let delta = (((ix * 13 + tick * 7) % 21) as f64 - 10.) / 100.;
                row.price = (row.price + delta).max(0.01);
                row.change = delta;
                row.volume += (ix % 50) as u64;
                row.high = row.high.max(row.price);
                row.low = row.low.min(row.price);
            }
        }
    }
}

/// How a column is laid out: its width, and whether its values are numbers,
/// which align to the trailing edge so that they can be compared.
#[derive(Clone, Copy)]
enum Column {
    Fixed(fn(gpui::Div) -> gpui::Div, bool),
    Fill,
}

const COLUMNS: [(&str, Column); 8] = [
    ("#", Column::Fixed(|d| d.w_16(), true)),
    ("Symbol", Column::Fixed(|d| d.w_20(), false)),
    ("Name", Column::Fill),
    ("Price", Column::Fixed(|d| d.w_24(), true)),
    ("Change", Column::Fixed(|d| d.w_24(), true)),
    ("Volume", Column::Fixed(|d| d.w_32(), true)),
    ("High", Column::Fixed(|d| d.w_24(), true)),
    ("Low", Column::Fixed(|d| d.w_24(), true)),
];

fn cell(column: Column) -> gpui::Div {
    let cell = div()
        .flex()
        .items_center()
        .px_3()
        .overflow_hidden()
        .whitespace_nowrap();
    match column {
        Column::Fixed(width, numeric) => width(cell)
            .flex_shrink_0()
            .when(numeric, |this| this.justify_end()),
        Column::Fill => cell.flex_1().min_w_32(),
    }
}

impl Render for Table {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let table = cx.entity();
        let theme = theme(cx);
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .rounded(theme.radius * 1.5)
            .border_1()
            .border_color(theme.border)
            .overflow_hidden()
            .text_sm()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .h_8()
                    .bg(theme.muted)
                    .border_b_1()
                    .border_color(theme.border)
                    .text_color(theme.muted_foreground)
                    .font_weight(FontWeight::MEDIUM)
                    .children(
                        COLUMNS
                            .iter()
                            .map(|(title, column)| cell(*column).child(*title)),
                    ),
            )
            .child(
                uniform_list("rows", self.rows.len(), move |range, _, cx| {
                    let theme = super::theme::theme(cx);
                    let rows = &table.read(cx).rows;
                    range
                        .map(|ix| row(ix, &rows[ix], theme).into_any_element())
                        .collect::<Vec<_>>()
                })
                .flex_1()
                .track_scroll(&self.scroll),
            )
    }
}

fn row(ix: usize, stock: &Stock, theme: &Theme) -> impl IntoElement {
    // A gain or a loss is told by its sign as well as its color.
    let change_color: Hsla = if stock.change > 0. {
        theme.success
    } else if stock.change < 0. {
        theme.danger
    } else {
        theme.muted_foreground
    };
    let values: [(SharedString, Option<Hsla>); 8] = [
        ((ix + 1).to_string().into(), Some(theme.muted_foreground)),
        (stock.symbol.clone(), None),
        (stock.name.clone(), None),
        (format!("{:.2}", stock.price).into(), None),
        (
            format!("{:+.2}%", stock.change * 100. / stock.price).into(),
            Some(change_color),
        ),
        (stock.volume.to_string().into(), None),
        (format!("{:.2}", stock.high).into(), None),
        (format!("{:.2}", stock.low).into(), None),
    ];
    div()
        .id(ix)
        .flex()
        .flex_row()
        .w_full()
        .h_8()
        .border_b_1()
        .border_color(theme.border)
        .when(ix % 2 == 1, |this| this.bg(theme.stripe))
        .hover(|this| this.bg(theme.accent))
        .children(
            values
                .into_iter()
                .zip(COLUMNS)
                .map(|((text, color), (_, column))| {
                    cell(column)
                        .when_some(color, |this, color| this.text_color(color))
                        .child(text)
                }),
        )
}

struct Message {
    author: SharedString,
    initials: SharedString,
    hue: f32,
    time: SharedString,
    body: SharedString,
}

/// A conversation of messages of different heights in a `gpui::list`, which
/// measures each item and owns its scrolling, as a chat view does.
pub struct MessageList {
    pub state: ListState,
    messages: Vec<Message>,
}

impl MessageList {
    fn new() -> Self {
        const AUTHORS: [(&str, &str); 5] = [
            ("Jason Lee", "JL"),
            ("Ada Byron", "AB"),
            ("Zhang Yi", "ZY"),
            ("Mira Kant", "MK"),
            ("Theo Rust", "TR"),
        ];
        const SENTENCES: [&str; 6] = [
            "The table scrolls smoothly now.",
            "Retained views keep what did not change from the last frame, so a frame only pays for what moved.",
            "Could you check the sidebar as well?",
            "I measured it with the showcase: most of the time goes to laying out text that did not change, which the new measurement cache skips.",
            "Looks good to me.",
            "Scrolling a long page moves every view in it, and each of them is built again where it lands.",
        ];
        let messages = (0..MESSAGES)
            .map(|ix| {
                let (author, initials) = AUTHORS[ix * 7 % AUTHORS.len()];
                let sentences = 1 + ix * 11 % 4;
                let body = (0..sentences)
                    .map(|n| SENTENCES[(ix + n * 5) % SENTENCES.len()])
                    .collect::<Vec<_>>()
                    .join(" ");
                Message {
                    author: author.into(),
                    initials: initials.into(),
                    hue: (ix * 7 % AUTHORS.len()) as f32 / AUTHORS.len() as f32,
                    time: format!("{:02}:{:02}", 9 + ix / 60 % 12, ix % 60).into(),
                    body: body.into(),
                }
            })
            .collect();
        Self {
            state: ListState::new(MESSAGES, ListAlignment::Top, px(200.)),
            messages,
        }
    }
}

impl Render for MessageList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();
        list(self.state.clone(), move |ix, _, cx| {
            let theme = super::theme::theme(cx);
            let message = &this.read(cx).messages[ix];
            div()
                .flex()
                .gap_3()
                .px_6()
                .py_3()
                .child(
                    div()
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .justify_center()
                        .size_8()
                        .rounded_full()
                        // An avatar's color stands for the person, so it is
                        // data, not a token.
                        .bg(hsla(message.hue, 0.45, 0.55, 1.))
                        .text_xs()
                        .text_color(theme.danger_foreground)
                        .child(message.initials.clone()),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(
                            div()
                                .flex()
                                .items_baseline()
                                .gap_2()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(message.author.clone()),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(message.time.clone()),
                                ),
                        )
                        .child(div().text_sm().child(message.body.clone())),
                )
                .into_any_element()
        })
        .size_full()
    }
}
