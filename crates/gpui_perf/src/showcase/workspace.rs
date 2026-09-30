//! The trading workspace page: a docked window of market panels built the way
//! Longbridge Pro builds its main window on GPUI Kit, with a live stream of
//! quotes that a timer pushes in.
//!
//! The component pages keep to one screen. A real application puts many of
//! them in one window and wires them to a shared data feed, and most of what
//! a frame costs there comes from how the parts are wired together, and from
//! how much each panel draws:
//!
//! - The workspace holds a focused search box. The window asks the focused
//!   input whether it accepts text on every frame, through its entity.
//! - It also holds a text selection layer, as GPUI Kit's `Root` does: while
//!   it is prepainted it makes sure a global exists (`has_global`) and resets
//!   that global's per-frame counter (`global_mut`). A small overlay view
//!   reads that global.
//! - Around the dock: a toolbar of tabs, the search box and the account, a
//!   sidebar of icons, and a status bar of market indices.
//! - A dock area splits it into resizable panels. Each panel writes its
//!   bounds into the dock's resize state while it is prepainted, and notifies
//!   it whether they changed or not, as GPUI Kit's resizable panel does.
//! - Each dock slot is a tab group whose active panel is drawn as a cached
//!   view (`AnyView::cached`), as GPUI Kit's tab panel draws it.
//! - Panels: a watchlist table filling most of the width (a `uniform_list` of
//!   rows of fifteen columns — market tags, company icons, badges, a 52-week
//!   range bar and a sparkline drawn as paths — striped, highlighting on
//!   hover, under a header with sort arrows); a quote header with a grid of
//!   statistics; a candlestick chart with moving averages, volume and price
//!   axis; an order book with depth bars; two columns of time and sales; and
//!   a donut of transaction statistics.
//! - A market feed entity emits one event per quote. Every panel subscribes
//!   to it, visible or not — a dozen hidden panels do too — so every event
//!   updates every subscriber, and most of them ignore it without notifying.
//!   The quote store is updated and notified, and the watchlist observes it.
//!   The quote panels' symbol ticks every other time, and redraws them all.
//!
//! The same workspace, without a window, is the headless `workspace-*`
//! scenarios in `scenarios/workspace.rs`.

use std::{cell::Cell, ops::Range, rc::Rc, time::Duration};

use gpui::{
    AnyView, Bounds, Context, ElementInputHandler, Entity, EntityInputHandler, EventEmitter,
    FocusHandle, FontWeight, Global, Hsla, IntoElement, PathBuilder, Pixels, Render, SharedString,
    StyleRefinement, Subscription, Task, UTF16Selection, UniformListScrollHandle, Window, canvas,
    div, fill, point, prelude::*, px, relative, rems, size, transparent_black, uniform_list,
};

use super::theme::{Theme, theme};

pub const SYMBOLS: usize = 200;
const HIDDEN_PANELS: usize = 12;
pub const WATCHLIST_ROW_HEIGHT: Pixels = px(32.);
/// Rows on the first screen of the watchlist, some of which every tick
/// updates.
const FIRST_SCREEN: usize = 40;
/// Points in each row's sparkline.
const SPARK_POINTS: usize = 40;
/// Trades in time and sales, in two columns.
const TRADES: usize = 80;
const BOOK_LEVELS: usize = 10;
const CANDLES: usize = 120;
/// Quotes of the chart's symbol that make up one candle.
const QUOTES_PER_CANDLE: usize = 40;
/// The symbol the quote panels show.
const SELECTED: usize = 7;

/// How often the stream pushes quotes: about sixty times a second, as a busy
/// market's feed does.
const STREAM_EVERY: Duration = Duration::from_millis(16);

/// Quotes the stream pushes each time, unless an automatic run says
/// otherwise.
pub const QUOTES_PER_TICK: usize = 16;

/// One quote as the feed delivers it.
#[derive(Clone, Copy)]
struct QuoteEvent {
    symbol: usize,
    last: f64,
    volume: u64,
}

#[derive(Clone, Copy)]
struct Quote {
    last: f64,
    prev_close: f64,
    open: f64,
    high: f64,
    low: f64,
    volume: u64,
    turnover: f64,
    pre_market: f64,
    high_52: f64,
    low_52: f64,
    /// Shares that trade, for the turnover ratio.
    float_shares: f64,
    /// The average daily volume, for the volume ratio.
    average_volume: f64,
}

/// Every symbol's latest quote.
struct QuoteStore {
    quotes: Vec<Quote>,
    names: Vec<SharedString>,
    codes: Vec<SharedString>,
    markets: Vec<&'static str>,
    /// Each symbol's day so far, one point a few minutes, for its sparkline.
    sparks: Vec<Vec<f32>>,
}

const NAME_WORDS: [&str; 16] = [
    "Pacific",
    "Golden",
    "Northern",
    "Silver",
    "Eastern",
    "United",
    "Harbor",
    "Summit",
    "Crystal",
    "Pioneer",
    "Atlas",
    "Meridian",
    "Evergreen",
    "Horizon",
    "Sterling",
    "Orion",
];
const NAME_KINDS: [&str; 10] = [
    "Technology",
    "Energy",
    "Holdings",
    "Motors",
    "Semiconductor",
    "Pharmaceuticals",
    "Bank",
    "Networks",
    "Materials",
    "Logistics",
];
const MARKETS: [&str; 7] = ["US", "HK", "US", "SH", "US", "HK", "SG"];

impl QuoteStore {
    fn new() -> Self {
        let quotes: Vec<Quote> = (0..SYMBOLS)
            .map(|ix| {
                let base = 5. + (ix * 37 % 1_400) as f64 + (ix % 7) as f64 * 0.13;
                let prev_close = base * (1. + ((ix % 11) as f64 - 5.) / 300.);
                let volume = 1_000_000 + (ix as u64 * 7_919_113) % 90_000_000;
                Quote {
                    last: base,
                    prev_close,
                    open: prev_close * 1.002,
                    high: base * 1.02,
                    low: base * 0.98,
                    volume,
                    turnover: volume as f64 * base,
                    pre_market: prev_close * (1. + ((ix % 9) as f64 - 4.) / 400.),
                    high_52: base * 1.6,
                    low_52: base * 0.55,
                    float_shares: 2e8 + (ix as f64 * 3.7e7) % 9e9,
                    average_volume: volume as f64 * (0.6 + (ix % 5) as f64 * 0.2),
                }
            })
            .collect();
        let letter = |n: usize| char::from(b'A' + (n % 26) as u8);
        Self {
            sparks: quotes
                .iter()
                .enumerate()
                .map(|(ix, quote)| {
                    (0..SPARK_POINTS)
                        .map(|point| {
                            let wave = ((point * 7 + ix * 3) % 17) as f64 - 8.;
                            (quote.prev_close * (1. + wave / 400.)) as f32
                        })
                        .collect()
                })
                .collect(),
            quotes,
            names: (0..SYMBOLS)
                .map(|ix| {
                    SharedString::from(format!(
                        "{} {}",
                        NAME_WORDS[ix % NAME_WORDS.len()],
                        NAME_KINDS[ix * 7 % NAME_KINDS.len()]
                    ))
                })
                .collect(),
            codes: (0..SYMBOLS)
                .map(|ix| match MARKETS[ix % MARKETS.len()] {
                    "HK" => SharedString::from(format!("{:05}", ix * 97 % 9_999)),
                    "SH" => SharedString::from(format!("6{:05}", ix * 131 % 99_999)),
                    _ => {
                        let mut code: String = [
                            letter(ix * 7 + 3),
                            letter(ix * 11 + 5),
                            letter(ix / 26 + ix * 3),
                        ]
                        .into_iter()
                        .collect();
                        if ix % 3 != 0 {
                            code.push(letter(ix * 5));
                        }
                        code.into()
                    }
                })
                .collect(),
            markets: (0..SYMBOLS).map(|ix| MARKETS[ix % MARKETS.len()]).collect(),
        }
    }

    fn tick(&mut self, symbol: usize, tick: usize) -> QuoteEvent {
        let quote = &mut self.quotes[symbol];
        let step = ((tick * 31 + symbol * 17) % 21) as f64 - 10.;
        quote.last = (quote.last * (1. + step / 2_000.)).max(0.01);
        quote.high = quote.high.max(quote.last);
        quote.low = quote.low.min(quote.last);
        let traded = 100 + (tick as u64 % 9) * 100;
        quote.volume += traded;
        quote.turnover += traded as f64 * quote.last;
        *self.sparks[symbol].last_mut().unwrap() = quote.last as f32;
        QuoteEvent {
            symbol,
            last: quote.last,
            volume: quote.volume,
        }
    }
}

/// Which symbol the `i`th quote of a tick is for: the quote panels' symbol
/// first every other tick, then a few rows of the watchlist's first screen,
/// so that rows in view change every tick, then the rest of the list.
fn quote_symbol(tick: usize, i: usize) -> usize {
    match i {
        0 if tick.is_multiple_of(2) => SELECTED,
        1..=4 => (tick * 5 + i * 11) % FIRST_SCREEN,
        _ => (tick * 7 + i * 13) % SYMBOLS,
    }
}

/// The feed every panel subscribes to.
struct MarketFeed;

impl EventEmitter<QuoteEvent> for MarketFeed {}

/// Per-frame bookkeeping kept in a global, as GPUI Kit's text selection keeps
/// the order its selectable texts paint in.
#[derive(Default)]
struct SelectionFrame {
    order: u64,
    participants: usize,
}

impl Global for SelectionFrame {}

/// A gain or a loss, told by its sign as well as this color.
fn change_color(change: f64, theme: &Theme) -> Hsla {
    if change >= 0. {
        theme.success
    } else {
        theme.danger
    }
}

/// `value` to `decimals` places, its thousands grouped: `1,053.980`.
pub fn grouped(value: f64, decimals: usize) -> String {
    let text = format!("{:.*}", decimals, value.abs());
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    let mut out = String::with_capacity(text.len() + whole.len() / 3 + 1);
    if value < 0. && text.bytes().any(|b| (b'1'..=b'9').contains(&b)) {
        out.push('-');
    }
    for (ix, digit) in whole.chars().enumerate() {
        if ix > 0 && (whole.len() - ix).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    if !fraction.is_empty() {
        out.push('.');
        out.push_str(fraction);
    }
    out
}

fn price(value: f64) -> SharedString {
    grouped(value, 3).into()
}

/// A change, signed: `+12.300`, `-2.610`.
fn signed(value: f64) -> SharedString {
    let text = grouped(value, 3);
    if text.starts_with('-') {
        text.into()
    } else {
        format!("+{text}").into()
    }
}

/// A change in percent, signed: `+0.37%`, `-2.61%`.
fn percent(value: f64) -> SharedString {
    format!("{value:+.2}%").into()
}

/// A large amount, shortened: `22.23M`, `1.05B`, `953.20K`.
fn amount(value: f64) -> String {
    if value >= 1e9 {
        format!("{:.2}B", value / 1e9)
    } else if value >= 1e6 {
        format!("{:.2}M", value / 1e6)
    } else if value >= 1e3 {
        format!("{:.2}K", value / 1e3)
    } else {
        format!("{value:.0}")
    }
}

/// The page: a toolbar with the search box, the icon sidebar, the dock, the
/// status bar, and the text selection layer. Its timer streams quotes into
/// the store and the feed; it does not notify the workspace itself.
pub struct Workspace {
    store: Entity<QuoteStore>,
    feed: Entity<MarketFeed>,
    pub search: Entity<SearchBox>,
    pub watchlist: Entity<Watchlist>,
    dock: Entity<DockArea>,
    status: Entity<StatusBar>,
    overlay: Entity<SelectionOverlay>,
    /// Panels that are not shown but still receive the feed, as panels in
    /// background tabs and closed docks do.
    _hidden: Vec<Entity<HiddenPanel>>,
    stream: Option<Task<()>>,
    /// Quotes each tick of the stream pushes.
    pub quotes_per_tick: usize,
    ticks: usize,
}

impl Workspace {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let store = cx.new(|_| QuoteStore::new());
        let feed = cx.new(|_| MarketFeed);
        let search = cx.new(|cx| SearchBox {
            focus: cx.focus_handle(),
            text: String::new(),
        });

        let watchlist = cx.new(|cx| Watchlist::new(store.clone(), &feed, cx));
        let detail: AnyView = cx
            .new(|cx| QuoteDetail::new(store.clone(), &feed, cx))
            .into();
        let chart: AnyView = cx.new(|cx| Chart::new(store.clone(), &feed, cx)).into();
        let book: AnyView = cx.new(|cx| OrderBook::new(store.clone(), &feed, cx)).into();
        let trades: AnyView = cx
            .new(|cx| TimeAndSales::new(store.clone(), &feed, cx))
            .into();
        let stats: AnyView = cx.new(|cx| TradeStats::new(&feed, cx)).into();
        let hidden = |title: &'static str, cx: &mut Context<Self>| -> AnyView {
            cx.new(|cx| HiddenPanel::new(title, &feed, cx)).into()
        };
        let positions = hidden("Positions", cx);
        let profile = hidden("Profile", cx);
        let intraday = hidden("Intraday", cx);
        let news = hidden("News", cx);

        let groups = vec![
            cx.new(|_| {
                TabGroup::new(vec![
                    ("Watchlist", watchlist.clone().into()),
                    ("Positions", positions),
                ])
            }),
            cx.new(|_| TabGroup::new(vec![("Quote", detail), ("Profile", profile)])),
            cx.new(|_| TabGroup::new(vec![("Candlestick", chart), ("Intraday", intraday)])),
            cx.new(|_| TabGroup::new(vec![("Order Book", book)])),
            cx.new(|_| TabGroup::new(vec![("Time & Sales", trades), ("News", news)])),
            cx.new(|_| TabGroup::new(vec![("Statistics", stats)])),
        ];
        let resize = cx.new(|_| ResizeState {
            sizes: vec![Bounds::default(); groups.len()],
            bounds: vec![Bounds::default(); groups.len()],
        });
        let dock = cx.new(|_| DockArea { groups, resize });

        Self {
            status: cx.new(|_| StatusBar { tick: 0 }),
            overlay: cx.new(|_| SelectionOverlay),
            _hidden: (0..HIDDEN_PANELS)
                .map(|ix| {
                    let title = ["Options", "Warrants", "Filings", "Ranks"][ix % 4];
                    cx.new(|cx| HiddenPanel::new(title, &feed, cx))
                })
                .collect(),
            store,
            feed,
            search,
            watchlist,
            dock,
            stream: None,
            quotes_per_tick: QUOTES_PER_TICK,
            ticks: 0,
        }
    }

    /// Starts or stops streaming quotes, returning whether it streams.
    pub fn toggle_stream(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.stream.take().is_some() {
            return false;
        }
        let interval = std::env::var("GPUI_PERF_STREAM_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|&millis| millis > 0)
            .map(Duration::from_millis)
            .unwrap_or(STREAM_EVERY);
        if std::env::args().any(|arg| arg == "--auto") {
            eprintln!("Workspace quote interval: {} ms", interval.as_millis());
        }
        self.stream = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(interval).await;
                if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                    break;
                }
            }
        }));
        true
    }

    /// Pushes one tick's quotes: each updates the store, which notifies the
    /// watchlist, and goes out on the feed to every panel. The indices tick
    /// every thirtieth time.
    fn tick(&mut self, cx: &mut Context<Self>) {
        self.ticks += 1;
        let tick = self.ticks;
        for i in 0..self.quotes_per_tick {
            let symbol = quote_symbol(tick, i);
            let event = self.store.update(cx, |store, cx| {
                let event = store.tick(symbol, tick);
                cx.notify();
                event
            });
            self.feed.update(cx, |_, cx| cx.emit(event));
        }
        if tick.is_multiple_of(30) {
            self.status.update(cx, |status, cx| {
                status.tick += 1;
                cx.notify();
            });
        }
    }
}

/// A square icon, `side` rems wide: a letter on a fill, standing in for the
/// icons and logos a real application draws as SVGs.
fn letter_icon(
    letter: &'static str,
    side: f32,
    fill: Hsla,
    color: Hsla,
    theme: &Theme,
) -> gpui::Div {
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .size(rems(side))
        .rounded(theme.radius_sm)
        .bg(fill)
        .text_color(color)
        .text_size(rems(side * 0.6))
        .font_weight(FontWeight::SEMIBOLD)
        .child(letter)
}

/// One of a row of choices, such as the toolbar's destinations, the
/// watchlist's groups or the chart's periods. Every row of them shows the
/// selected one the same way: a quiet fill, the foreground color and a
/// heavier weight, rather than the primary color, which the workspace keeps
/// for nothing but its mark.
fn chip(label: &'static str, selected: bool, theme: &Theme) -> gpui::Div {
    div()
        .flex_shrink_0()
        .px_2()
        .rounded(theme.radius_sm)
        .when(selected, |this| {
            this.bg(theme.secondary)
                .text_color(theme.foreground)
                .font_weight(FontWeight::MEDIUM)
        })
        .when(!selected, |this| this.text_color(theme.muted_foreground))
        .child(label)
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        div()
            .id("workspace")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .text_sm()
            .child(self.toolbar(theme))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(icon_sidebar(theme))
                    .child(div().flex_1().min_w_0().child(self.dock.clone())),
            )
            .child(
                self.status
                    .clone()
                    .cached(StyleRefinement::default().h_6().w_full().flex_shrink_0()),
            )
            // GPUI Kit's `Root` keeps its window-wide text selection here.
            .child(
                canvas(
                    |_, _, cx| {
                        if !cx.has_global::<SelectionFrame>() {
                            cx.set_global(SelectionFrame::default());
                        }
                        cx.global_mut::<SelectionFrame>().order = 1;
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_0(),
            )
            .child(self.overlay.clone())
    }
}

impl Workspace {
    /// The top toolbar: the application's tabs, the search box, the market's
    /// session and the account.
    fn toolbar(&self, theme: &Theme) -> impl IntoElement {
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_between()
            .gap_4()
            .h_10()
            .px_3()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        letter_icon("T", 1.5, theme.primary, theme.primary_foreground, theme)
                            .mr_2(),
                    )
                    .children(
                        [
                            "Watchlist",
                            "Markets",
                            "Portfolio",
                            "Trade",
                            "Screener",
                            "News",
                            "Community",
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(ix, title)| {
                            chip(title, ix == 0, theme).py_1().rounded(theme.radius)
                        }),
                    ),
            )
            .child(div().w_72().child(self.search.clone()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .text_xs()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(div().size_2().rounded_full().bg(theme.success))
                            .child("US Market Open"),
                    )
                    .child(letter_icon(
                        "!",
                        1.25,
                        theme.secondary,
                        theme.muted_foreground,
                        theme,
                    ))
                    .child(letter_icon(
                        "*",
                        1.25,
                        theme.secondary,
                        theme.muted_foreground,
                        theme,
                    ))
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child("Account A/C(1637)"),
                    )
                    .child(
                        div()
                            .size_6()
                            .rounded_full()
                            .bg(theme.series[1])
                            .text_color(theme.avatar_foreground)
                            .font_weight(FontWeight::MEDIUM)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child("J"),
                    ),
            )
    }
}

/// The column of icons along the window's left edge.
fn icon_sidebar(theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .items_center()
        .gap_1()
        .w_12()
        .py_2()
        .border_r_1()
        .border_color(theme.border)
        .bg(theme.sidebar)
        .children(
            ["W", "M", "P", "T", "S", "N", "C", "A", "L", "O"]
                .into_iter()
                .enumerate()
                .map(|(ix, letter)| {
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size_9()
                        .rounded(theme.radius)
                        .when(ix == 0, |this| this.bg(theme.sidebar_accent))
                        .child(letter_icon(
                            letter,
                            1.125,
                            transparent_black(),
                            if ix == 0 {
                                theme.foreground
                            } else {
                                theme.muted_foreground
                            },
                            theme,
                        ))
                }),
        )
        .child(div().flex_1())
        .child(letter_icon(
            "?",
            1.125,
            transparent_black(),
            theme.muted_foreground,
            theme,
        ))
}

/// Reads the selection global, as the selection handles' overlay does.
struct SelectionOverlay;

impl Render for SelectionOverlay {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (order, participants) = cx
            .try_global::<SelectionFrame>()
            .map_or((0, 0), |frame| (frame.order, frame.participants));
        div()
            .absolute()
            .size_0()
            .when(order > 1 && participants > 1_000, |this| this.child("…"))
    }
}

/// The search box, focused while the page is shown, as a trading window's
/// symbol search is.
pub struct SearchBox {
    pub focus: FocusHandle,
    text: String,
}

impl Render for SearchBox {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let entity = cx.entity();
        let focus = self.focus.clone();
        let focused = self.focus.is_focused(window);
        div()
            .id("search")
            .track_focus(&self.focus)
            .relative()
            .flex()
            .items_center()
            .h_7()
            .px_2()
            .rounded(theme.radius)
            .border_1()
            .border_color(if focused {
                theme.foreground
            } else {
                theme.input
            })
            .text_color(theme.muted_foreground)
            .child(if self.text.is_empty() {
                SharedString::from("Search symbols")
            } else {
                SharedString::from(self.text.clone())
            })
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, cx| {
                        window.handle_input(&focus, ElementInputHandler::new(bounds, entity), cx);
                    },
                )
                .absolute()
                .size_full(),
            )
    }
}

impl EntityInputHandler for SearchBox {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        self.text.get(range).map(str::to_string)
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.text.len()..self.text.len(),
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        None
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {}

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.text.push_str(text);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_text_in_range(range, text, window, cx);
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(element_bounds)
    }

    fn character_index_for_point(
        &mut self,
        _: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.text.len())
    }
}

/// Where the dock's panels were laid out, written as they are prepainted.
struct ResizeState {
    /// The size each panel is laid out at: the size it first measured, as
    /// GPUI Kit's resizable state keeps it until a panel is dragged.
    sizes: Vec<Bounds<Pixels>>,
    /// Where each panel was last laid out, written on every prepaint.
    bounds: Vec<Bounds<Pixels>>,
}

/// The dock: the watchlist filling most of the width, then a column of the
/// quote and its chart, then a column of the order book, time and sales and
/// transaction statistics, each slot a resizable panel holding a tab group.
struct DockArea {
    groups: Vec<Entity<TabGroup>>,
    resize: Entity<ResizeState>,
}

impl DockArea {
    /// The resizable panel in slot `ix`, sharing its column's height with
    /// its neighbours in proportion to `grow` until it has been laid out.
    /// A panel draws the hairline to its trailing side and below it, except
    /// against the window's edge or the last in its column: two neighbours
    /// never both draw the line between them.
    fn panel(
        &self,
        ix: usize,
        grow: f32,
        edges: Edges,
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
        let border = theme(cx).border;
        let resize = self.resize.clone();
        let basis = self.resize.read(cx).sizes[ix].size.height;
        let mut panel = div()
            .relative()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .when(basis > px(0.), |this| this.flex_basis(basis.min(px(2000.))))
            .when(!edges.last_column, |this| this.border_r_1())
            .when(!edges.last_in_column, |this| this.border_b_1())
            .border_color(border);
        panel.style().flex_grow = Some(grow);
        panel
            .child(self.groups[ix].clone())
            // As GPUI Kit's resizable panel does: the bounds are written back
            // and the state notified on every prepaint, changed or not.
            .child(
                canvas(
                    move |bounds, _, cx| {
                        resize.update(cx, |state, cx| {
                            if state.sizes[ix].size.height <= px(0.) {
                                state.sizes[ix] = bounds;
                            }
                            state.bounds[ix] = bounds;
                            cx.notify();
                        })
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
    }
}

/// Where a dock panel sits, for which of its hairlines it draws.
#[derive(Clone, Copy)]
struct Edges {
    last_column: bool,
    last_in_column: bool,
}

const MIDDLE: Edges = Edges {
    last_column: false,
    last_in_column: false,
};
const BOTTOM: Edges = Edges {
    last_column: false,
    last_in_column: true,
};
const RIGHT: Edges = Edges {
    last_column: true,
    last_in_column: false,
};
const BOTTOM_RIGHT: Edges = Edges {
    last_column: true,
    last_in_column: true,
};

impl Render for DockArea {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let column = |width: Option<Pixels>| {
            div().flex().flex_col().h_full().map(|this| match width {
                Some(width) => this.w(width).flex_shrink_0(),
                None => this.flex_1().min_w_0(),
            })
        };
        div()
            .size_full()
            .flex()
            .flex_row()
            .child(column(None).child(self.panel(0, 1., BOTTOM, cx)))
            .child(
                column(Some(px(460.)))
                    .child(self.panel(1, 1., MIDDLE, cx))
                    .child(self.panel(2, 1.2, BOTTOM, cx)),
            )
            .child(
                column(Some(px(380.)))
                    .child(self.panel(3, 1., RIGHT, cx))
                    .child(self.panel(4, 1.6, RIGHT, cx))
                    .child(self.panel(5, 0.7, BOTTOM_RIGHT, cx)),
            )
    }
}

/// A tab bar over the active panel, which is drawn as a cached view.
struct TabGroup {
    tabs: Vec<(&'static str, AnyView)>,
    active: usize,
}

impl TabGroup {
    fn new(tabs: Vec<(&'static str, AnyView)>) -> Self {
        Self { tabs, active: 0 }
    }
}

impl Render for TabGroup {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .h_8()
                    .bg(theme.muted)
                    .border_b_1()
                    .border_color(theme.border)
                    .children(self.tabs.iter().enumerate().map(|(ix, (title, _))| {
                        let active = ix == self.active;
                        div()
                            .id(ix)
                            .flex()
                            .items_center()
                            .h_full()
                            .px_3()
                            // The selected tab is told by its own surface,
                            // joined to the panel below, and its weight.
                            .when(active, |this| {
                                this.font_weight(FontWeight::MEDIUM).bg(theme.background)
                            })
                            .when(!active, |this| {
                                this.text_color(theme.muted_foreground)
                                    .hover(|this| this.bg(theme.secondary_hover))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.active = ix;
                                cx.notify();
                            }))
                            .child(*title)
                    })),
            )
            .child(
                div().relative().flex_1().min_h_0().child(
                    self.tabs[self.active]
                        .1
                        .clone()
                        .cached(StyleRefinement::default().absolute().size_full()),
                ),
            )
    }
}

/// The watchlist table: a row of groups, a header, and a `uniform_list` of
/// rows, which owns its scrolling.
pub struct Watchlist {
    store: Entity<QuoteStore>,
    pub scroll: UniformListScrollHandle,
    /// Where the rows were last laid out, in window coordinates, for an
    /// automatic run to move the pointer over them.
    pub rows_bounds: Rc<Cell<Bounds<Pixels>>>,
    _subscriptions: [Subscription; 2],
}

impl Watchlist {
    fn new(store: Entity<QuoteStore>, feed: &Entity<MarketFeed>, cx: &mut Context<Self>) -> Self {
        Self {
            _subscriptions: [
                cx.observe(&store, |_, _, cx| cx.notify()),
                // Its alerts check every quote; nothing to show for most.
                cx.subscribe(feed, |_, _, event: &QuoteEvent, _| {
                    let _ = event.volume;
                }),
            ],
            store,
            scroll: UniformListScrollHandle::new(),
            rows_bounds: Rc::default(),
        }
    }
}

/// The watchlist's columns: a title, a width, and whether the values are
/// numbers, which align to the trailing edge so that they can be compared,
/// and can be sorted by.
const COLUMNS: [(&str, f32, bool); 15] = [
    ("Symbol", 92., false),
    ("Name", 200., false),
    ("Price", 84., true),
    ("% Chg", 76., true),
    ("Chg", 76., true),
    ("Volume", 128., true),
    ("Turnover", 84., true),
    ("Turnover %", 88., true),
    ("Vol Ratio", 76., true),
    ("High", 84., true),
    ("Low", 84., true),
    ("Pre-Mkt", 84., true),
    ("Pre-Mkt %", 84., true),
    ("52wk Range", 108., false),
    ("Trend", 100., false),
];

/// The column the rows are sorted by, descending.
const SORTED_BY: usize = 3;

fn watchlist_cell(column: usize) -> gpui::Div {
    let (_, width, numeric) = COLUMNS[column];
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .w(px(width))
        .px_2()
        .overflow_hidden()
        .whitespace_nowrap()
        .when(numeric, |this| this.justify_end())
}

/// The two small triangles of a sortable column's header, the one the rows
/// are sorted in filled.
fn sort_arrows(sorted: Option<bool>, theme: &Theme) -> impl IntoElement {
    let (idle, active) = (theme.border, theme.foreground);
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let x = bounds.origin.x;
            let y = bounds.origin.y + bounds.size.height / 2.;
            for (up, dy) in [(true, px(-1.)), (false, px(1.))] {
                let tip = if up { y + dy - px(4.) } else { y + dy + px(4.) };
                let base = y + dy;
                let mut path = PathBuilder::fill();
                path.move_to(point(x, base));
                path.line_to(point(x + px(6.), base));
                path.line_to(point(x + px(3.), tip));
                path.close();
                let color = if sorted == Some(up) { active } else { idle };
                if let Ok(path) = path.build() {
                    window.paint_path(path, color);
                }
            }
        },
    )
    .flex_shrink_0()
    .w(px(6.))
    .h(px(12.))
    .ml_1()
}

impl Render for Watchlist {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let store = self.store.clone();
        let rows_bounds = self.rows_bounds.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap_1()
                    .h_8()
                    .px_1()
                    .text_xs()
                    .children(
                        [
                            "All 200",
                            "US Stocks",
                            "HK Stocks",
                            "China A",
                            "Singapore",
                            "Holdings",
                            "ETFs",
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(ix, group)| chip(group, ix == 0, theme).py_0p5()),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .pr_2()
                            .text_color(theme.muted_foreground)
                            .child("Sorted by % Chg"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .h_8()
                    .px_1()
                    .bg(theme.muted)
                    .border_y_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .font_weight(FontWeight::MEDIUM)
                    .children(
                        COLUMNS
                            .iter()
                            .enumerate()
                            .map(|(column, (title, _, sortable))| {
                                watchlist_cell(column)
                                    .child(*title)
                                    .when(*sortable, |this| {
                                        this.child(sort_arrows(
                                            (column == SORTED_BY).then_some(false),
                                            theme,
                                        ))
                                    })
                            }),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list("watchlist-rows", SYMBOLS, move |range, _, cx| {
                            let theme = super::theme::theme(cx);
                            let store = store.read(cx);
                            range
                                .map(|ix| watchlist_row(ix, store, theme).into_any_element())
                                .collect::<Vec<_>>()
                        })
                        .flex_1()
                        .track_scroll(&self.scroll),
                    )
                    .child(
                        canvas(move |bounds, _, _| rows_bounds.set(bounds), |_, _, _, _| {})
                            .absolute()
                            .size_full(),
                    ),
            )
    }
}

fn watchlist_row(ix: usize, store: &QuoteStore, theme: &Theme) -> impl IntoElement {
    let quote = store.quotes[ix];
    let change = quote.last - quote.prev_close;
    let color = change_color(change, theme);
    let pre_change = quote.pre_market - quote.prev_close;
    let pre_color = change_color(pre_change, theme);
    let volume = quote.volume as f64;
    let numbers: [(SharedString, Option<Hsla>); 11] = [
        (price(quote.last), Some(color)),
        (percent(change / quote.prev_close * 100.), Some(color)),
        (signed(change), Some(color)),
        (format!("{} shares", amount(volume)).into(), None),
        (amount(quote.turnover).into(), None),
        (
            format!("{:.2}%", volume / quote.float_shares * 100.).into(),
            None,
        ),
        (format!("{:.2}", volume / quote.average_volume).into(), None),
        (price(quote.high), None),
        (price(quote.low), None),
        (price(quote.pre_market), Some(pre_color)),
        (
            percent(pre_change / quote.prev_close * 100.),
            Some(pre_color),
        ),
    ];
    let market = store.markets[ix];
    // Holdings, pre-market and options: classifications, so neutral marks
    // that the gains and losses around them do not have to compete with.
    let badges = [
        (ix.is_multiple_of(4), "H"),
        (ix % 3 == 1, "P"),
        (ix % 5 == 2, "O"),
    ];
    let range = ((quote.last - quote.low_52) / (quote.high_52 - quote.low_52)).clamp(0., 1.);
    div()
        .id(ix)
        .flex()
        .items_center()
        .h(WATCHLIST_ROW_HEIGHT)
        .px_1()
        .border_b_1()
        .border_color(theme.border)
        .when(ix % 2 == 1, |this| this.bg(theme.stripe))
        .hover(|this| this.bg(theme.accent))
        .child(
            watchlist_cell(0)
                .gap_1()
                .child(
                    div()
                        .flex_shrink_0()
                        .px_0p5()
                        .rounded(theme.radius_sm)
                        .bg(theme.secondary)
                        .text_color(theme.muted_foreground)
                        .text_xs()
                        .child(market),
                )
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .child(store.codes[ix].clone()),
                ),
        )
        .child(
            watchlist_cell(1)
                .gap_2()
                .child(
                    letter_icon(
                        ["A", "B", "C", "D", "E", "F", "G", "H"][ix % 8],
                        1.125,
                        theme.secondary,
                        theme.muted_foreground,
                        theme,
                    )
                    .rounded_full(),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .child(store.names[ix].clone()),
                )
                .children(
                    badges
                        .into_iter()
                        .filter(|(shown, _)| *shown)
                        .map(|(_, letter)| {
                            div()
                                .flex()
                                .flex_shrink_0()
                                .items_center()
                                .justify_center()
                                .size_4()
                                .rounded(theme.radius_sm)
                                .border_1()
                                .border_color(theme.input)
                                .text_color(theme.muted_foreground)
                                .text_xs()
                                .child(letter)
                        }),
                ),
        )
        .children(numbers.into_iter().enumerate().map(|(ix, (text, color))| {
            watchlist_cell(ix + 2)
                .when_some(color, |this, color| this.text_color(color))
                .child(text)
        }))
        .child(
            watchlist_cell(13).child(
                // Where the price sits in its 52-week range: a position, not
                // a gain or a loss, so drawn without their colors.
                div()
                    .relative()
                    .w_full()
                    .h_1()
                    .rounded_full()
                    .bg(theme.muted)
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .h_full()
                            .w(relative(range as f32))
                            .rounded_full()
                            .bg(theme.input),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_neg_0p5()
                            .left(relative(range as f32))
                            .w_0p5()
                            .h_2()
                            .bg(theme.foreground),
                    ),
            ),
        )
        .child(watchlist_cell(14).child(sparkline(
            store.sparks[ix].clone(),
            quote.prev_close as f32,
            color,
        )))
}

/// A symbol's day as a line over a faint fill, drawn as paths.
fn sparkline(points: Vec<f32>, baseline: f32, color: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let (low, high) = points
                .iter()
                .fold((baseline, baseline), |(low, high), point| {
                    (low.min(*point), high.max(*point))
                });
            let span = (high - low).max(0.0001);
            let step = bounds.size.width / (points.len() - 1) as f32;
            let at = |ix: usize, value: f32| {
                point(
                    bounds.origin.x + step * ix as f32,
                    bounds.origin.y + bounds.size.height * ((high - value) / span),
                )
            };
            let bottom = bounds.bottom();
            let mut area = PathBuilder::fill();
            let mut line = PathBuilder::stroke(px(1.));
            area.move_to(point(bounds.origin.x, bottom));
            for (ix, value) in points.iter().enumerate() {
                area.line_to(at(ix, *value));
                if ix == 0 {
                    line.move_to(at(ix, *value));
                } else {
                    line.line_to(at(ix, *value));
                }
            }
            area.line_to(point(bounds.right(), bottom));
            area.close();
            if let Ok(area) = area.build() {
                window.paint_path(area, color.opacity(0.12));
            }
            if let Ok(line) = line.build() {
                window.paint_path(line, color);
            }
        },
    )
    .w_full()
    .h_5()
}

/// The selected symbol's price, change and a grid of statistics.
struct QuoteDetail {
    store: Entity<QuoteStore>,
    _subscription: Subscription,
}

impl QuoteDetail {
    fn new(store: Entity<QuoteStore>, feed: &Entity<MarketFeed>, cx: &mut Context<Self>) -> Self {
        Self {
            store,
            _subscription: cx.subscribe(feed, |_, _, event: &QuoteEvent, cx| {
                if event.symbol == SELECTED {
                    cx.notify();
                }
            }),
        }
    }
}

impl Render for QuoteDetail {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let store = self.store.read(cx);
        let quote = store.quotes[SELECTED];
        let change = quote.last - quote.prev_close;
        let color = change_color(change, theme);
        let pre_change = quote.pre_market - quote.prev_close;
        let volume = quote.volume as f64;
        let shares = quote.float_shares * 1.08;
        let stats: [(&str, SharedString); 24] = [
            ("Open", price(quote.open)),
            ("Prev. Close", price(quote.prev_close)),
            ("High", price(quote.high)),
            ("Low", price(quote.low)),
            ("Volume", format!("{} shares", amount(volume)).into()),
            ("Turnover", amount(quote.turnover).into()),
            ("52wk High", price(quote.high_52)),
            ("52wk Low", price(quote.low_52)),
            ("Mkt Cap", amount(shares * quote.last).into()),
            ("Float Cap", amount(quote.float_shares * quote.last).into()),
            ("Shares", amount(shares).into()),
            ("Float Shares", amount(quote.float_shares).into()),
            ("P/E (TTM)", format!("{:.2}", quote.last / 8.83).into()),
            ("P/E (Static)", format!("{:.2}", quote.last / 7.91).into()),
            ("P/B", format!("{:.2}", quote.last / 21.4).into()),
            ("Dividend (TTM)", "1.060".into()),
            (
                "Div. Yield",
                format!("{:.2}%", 1.06 / quote.last * 100.).into(),
            ),
            (
                "Turnover %",
                format!("{:.2}%", volume / quote.float_shares * 100.).into(),
            ),
            (
                "Vol Ratio",
                format!("{:.2}", volume / quote.average_volume).into(),
            ),
            (
                "Amplitude",
                format!("{:.2}%", (quote.high - quote.low) / quote.prev_close * 100.).into(),
            ),
            ("Avg Price", price(quote.turnover / volume)),
            ("Bid/Ask", format!("{:.2}%", 50. + change * 3.).into()),
            ("Lot Size", "100".into()),
            ("Min Tick", "0.010".into()),
        ];
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(store.codes[SELECTED].clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(store.names[SELECTED].clone()),
                    )
                    .child(
                        div()
                            .px_1()
                            .rounded(theme.radius_sm)
                            .bg(theme.success.opacity(0.15))
                            .text_color(theme.success)
                            .text_xs()
                            .child("Trading"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .text_color(color)
                    .child(
                        div()
                            .text_2xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(price(quote.last)),
                    )
                    .child(signed(change))
                    .child(percent(change / quote.prev_close * 100.)),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Pre-market")
                    .child(
                        div()
                            .text_color(change_color(pre_change, theme))
                            .child(price(quote.pre_market)),
                    )
                    .child(
                        div()
                            .text_color(change_color(pre_change, theme))
                            .child(percent(pre_change / quote.prev_close * 100.)),
                    )
                    .child("04:12 EST"),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .text_xs()
                    // Two columns with one gutter between them, so that the
                    // right column's values end on the panel's edge.
                    .children(stats.into_iter().enumerate().map(|(ix, (label, value))| {
                        div()
                            .flex()
                            .justify_between()
                            .w_1_2()
                            .py_0p5()
                            .when(ix % 2 == 0, |this| this.pr_3())
                            .when(ix % 2 == 1, |this| this.pl_3())
                            .child(div().text_color(theme.muted_foreground).child(label))
                            .child(value)
                    })),
            )
    }
}

#[derive(Clone, Copy)]
struct Candle {
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
}

/// A candlestick chart of the selected symbol: one candle per period, with
/// moving averages, volume, a price axis and a crosshair at the last price.
struct Chart {
    candles: Vec<Candle>,
    /// Quotes into the last candle, which starts a new one every
    /// `QUOTES_PER_CANDLE`.
    quotes: usize,
    _subscription: Subscription,
}

/// The moving averages the chart draws, by their periods.
const AVERAGES: [usize; 3] = [5, 10, 20];
/// Room on the chart's right for its price axis, and at its bottom for its
/// time axis.
const PRICE_AXIS: Pixels = px(64.);
const TIME_AXIS: Pixels = px(18.);

impl Chart {
    fn new(store: Entity<QuoteStore>, feed: &Entity<MarketFeed>, cx: &mut Context<Self>) -> Self {
        let mut close = store.read(cx).quotes[SELECTED].prev_close;
        let candles = (0..CANDLES)
            .map(|ix| {
                let open = close;
                let step = ((ix * 7 % 13) as f64 - 6.) / 300.;
                close = open * (1. + step);
                let reach = ((ix * 5 % 7) as f64 + 1.) / 800.;
                Candle {
                    open,
                    high: open.max(close) * (1. + reach),
                    low: open.min(close) * (1. - reach),
                    close,
                    volume: 2e6 + ((ix * 7_919) % 5_000) as f64 * 1e3,
                }
            })
            .collect();
        Self {
            candles,
            quotes: 0,
            _subscription: cx.subscribe(feed, |this, _, event: &QuoteEvent, cx| {
                if event.symbol != SELECTED {
                    return;
                }
                this.quotes += 1;
                if this.quotes.is_multiple_of(QUOTES_PER_CANDLE) {
                    let close = this.candles.last().unwrap().close;
                    this.candles.remove(0);
                    this.candles.push(Candle {
                        open: close,
                        high: close,
                        low: close,
                        close,
                        volume: 0.,
                    });
                }
                let candle = this.candles.last_mut().unwrap();
                candle.close = event.last;
                candle.high = candle.high.max(event.last);
                candle.low = candle.low.min(event.last);
                candle.volume += (event.volume % 1_000) as f64 * 100.;
                cx.notify();
            }),
        }
    }
}

/// What the chart's canvas paints, computed when the chart renders.
struct ChartPaint {
    candles: Vec<Candle>,
    averages: [Vec<f64>; 3],
    low: f64,
    high: f64,
    max_volume: f64,
    up: Hsla,
    down: Hsla,
    grid: Hsla,
    crosshair: Hsla,
    series: [Hsla; 3],
}

impl ChartPaint {
    fn paint(self, bounds: Bounds<Pixels>, window: &mut Window) {
        let plot = Bounds::new(
            bounds.origin,
            size(
                bounds.size.width - PRICE_AXIS,
                bounds.size.height - TIME_AXIS,
            ),
        );
        let price_height = plot.size.height * 0.75;
        let volume_top = plot.origin.y + plot.size.height * 0.78;
        let volume_height = plot.size.height * 0.22;
        let span = (self.high - self.low).max(0.0001);
        let y = |value: f64| plot.origin.y + price_height * ((self.high - value) / span) as f32;
        let step = plot.size.width / self.candles.len() as f32;
        let x = |ix: usize| plot.origin.x + step * (ix as f32 + 0.5);

        for line in 0..=4 {
            let top = plot.origin.y + price_height * (line as f32 / 4.);
            window.paint_quad(fill(
                Bounds::new(point(plot.origin.x, top), size(plot.size.width, px(1.))),
                self.grid,
            ));
        }
        for line in 0..=5 {
            let left = plot.origin.x + plot.size.width * (line as f32 / 5.);
            window.paint_quad(fill(
                Bounds::new(point(left, plot.origin.y), size(px(1.), plot.size.height)),
                self.grid,
            ));
        }

        let body = (step * 0.7).max(px(1.));
        for (ix, candle) in self.candles.iter().enumerate() {
            let color = if candle.close >= candle.open {
                self.up
            } else {
                self.down
            };
            let center = x(ix);
            window.paint_quad(fill(
                Bounds::new(
                    point(center - px(0.5), y(candle.high)),
                    size(px(1.), (y(candle.low) - y(candle.high)).max(px(1.))),
                ),
                color,
            ));
            let top = y(candle.open.max(candle.close));
            let bottom = y(candle.open.min(candle.close));
            window.paint_quad(fill(
                Bounds::new(
                    point(center - body / 2., top),
                    size(body, (bottom - top).max(px(1.))),
                ),
                color,
            ));
            let height = volume_height * (candle.volume / self.max_volume) as f32;
            window.paint_quad(fill(
                Bounds::new(
                    point(center - body / 2., volume_top + volume_height - height),
                    size(body, height),
                ),
                color.opacity(0.5),
            ));
        }

        for (average, (period, color)) in self
            .averages
            .iter()
            .zip(AVERAGES.into_iter().zip(self.series))
        {
            let mut path = PathBuilder::stroke(px(1.2));
            for (ix, value) in average.iter().enumerate() {
                let at = point(x(ix + period - 1), y(*value));
                if ix == 0 {
                    path.move_to(at);
                } else {
                    path.line_to(at);
                }
            }
            if let Ok(path) = path.build() {
                window.paint_path(path, color);
            }
        }

        let last = self.candles.len() - 1;
        let close_y = y(self.candles[last].close);
        let mut crosshair = PathBuilder::stroke(px(1.)).dash_array(&[px(3.), px(3.)]);
        crosshair.move_to(point(plot.origin.x, close_y));
        crosshair.line_to(point(plot.right(), close_y));
        crosshair.move_to(point(x(last), plot.origin.y));
        crosshair.line_to(point(x(last), plot.bottom()));
        if let Ok(path) = crosshair.build() {
            window.paint_path(path, self.crosshair);
        }
    }
}

impl Render for Chart {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let (low, high, max_volume) =
            self.candles
                .iter()
                .fold((f64::MAX, f64::MIN, 1.), |(low, high, volume), candle| {
                    (
                        low.min(candle.low),
                        high.max(candle.high),
                        f64::max(volume, candle.volume),
                    )
                });
        let averages = AVERAGES.map(|period| {
            self.candles
                .windows(period)
                .map(|window| window.iter().map(|candle| candle.close).sum::<f64>() / period as f64)
                .collect::<Vec<_>>()
        });
        let last = *self.candles.last().unwrap();
        let change = last.close - last.open;
        let span = (high - low).max(0.0001);
        // Where on the price axis the last price is, as a fraction of the
        // plot's height; the axis spans the top three quarters of it.
        let last_at = ((high - last.close) / span) as f32 * 0.75;
        let paint = ChartPaint {
            candles: self.candles.clone(),
            averages: averages.clone(),
            low,
            high,
            max_volume,
            up: theme.success,
            down: theme.danger,
            grid: theme.border,
            crosshair: theme.muted_foreground,
            series: theme.series,
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .text_xs()
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap_0p5()
                    .h_7()
                    .px_1()
                    .children(
                        ["1m", "5m", "15m", "30m", "1h", "D", "W", "M", "Y"]
                            .into_iter()
                            .map(|period| chip(period, period == "D", theme)),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .pr_2()
                            .text_color(theme.muted_foreground)
                            .child("MA · VOL"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .gap_3()
                    .px_3()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .children(averages.iter().zip(AVERAGES).zip(theme.series).map(
                        |((average, period), color)| {
                            div()
                                .text_color(color)
                                .child(format!("MA{period} {}", price(*average.last().unwrap())))
                        },
                    ))
                    .child(div().text_color(change_color(change, theme)).child(format!(
                        "O {} H {} L {} C {}",
                        price(last.open),
                        price(last.high),
                        price(last.low),
                        price(last.close)
                    ))),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .mx_3()
                    .my_2()
                    .child(
                        canvas(
                            |_, _, _| {},
                            move |bounds, _, window, _| paint.paint(bounds, window),
                        )
                        .absolute()
                        .size_full(),
                    )
                    // The price axis: five prices down the right edge, and
                    // the last price boxed where the crosshair meets it.
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .right_0()
                            .bottom(TIME_AXIS)
                            .w(PRICE_AXIS)
                            .children((0..=4).map(|line| {
                                let at = line as f32 / 4. * 0.75;
                                div()
                                    .absolute()
                                    .top(relative(at))
                                    .left_1()
                                    .text_color(theme.muted_foreground)
                                    .child(price(high - span * (line as f64 / 4.)))
                            }))
                            .child(
                                div()
                                    .absolute()
                                    .top(relative(last_at))
                                    .left_0()
                                    .px_1()
                                    .rounded(theme.radius_sm)
                                    .bg(change_color(change, theme))
                                    .text_color(if change >= 0. {
                                        theme.success_foreground
                                    } else {
                                        theme.danger_foreground
                                    })
                                    .child(price(last.close)),
                            ),
                    )
                    // The time axis along the bottom.
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right(PRICE_AXIS)
                            .bottom_0()
                            .h(TIME_AXIS)
                            .children(
                                ["2026-04", "2026-05", "2026-06", "2026-07", "2026-08"]
                                    .into_iter()
                                    .enumerate()
                                    .map(|(ix, month)| {
                                        div()
                                            .absolute()
                                            .left(relative(ix as f32 / 5.))
                                            .text_color(theme.muted_foreground)
                                            .child(month)
                                    }),
                            ),
                    ),
            )
    }
}

/// Bids and asks for the selected symbol: the ratio of the two, and ten
/// levels a side with a bar for each level's depth.
struct OrderBook {
    mid: f64,
    updates: usize,
    _subscription: Subscription,
}

impl OrderBook {
    fn new(store: Entity<QuoteStore>, feed: &Entity<MarketFeed>, cx: &mut Context<Self>) -> Self {
        Self {
            mid: store.read(cx).quotes[SELECTED].last,
            updates: 0,
            _subscription: cx.subscribe(feed, |this, _, event: &QuoteEvent, cx| {
                if event.symbol == SELECTED {
                    this.mid = event.last;
                    this.updates += 1;
                    cx.notify();
                }
            }),
        }
    }

    /// A level's size, in shares.
    fn size(&self, level: usize, bid: bool) -> f64 {
        let salt = if bid { 13 } else { 7 };
        (((level * 37 + self.updates * salt + 11) % 900) + 20) as f64 * 100.
    }
}

impl Render for OrderBook {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let book = &*self;
        let depth = |bid: bool| -> f64 { (0..BOOK_LEVELS).map(|ix| book.size(ix, bid)).sum() };
        let (bids, asks) = (depth(true), depth(false));
        let ratio = (bids / (bids + asks)) as f32;
        let largest = (0..BOOK_LEVELS)
            .flat_map(|ix| [book.size(ix, true), book.size(ix, false)])
            .fold(1., f64::max);
        let side = |bid: bool| {
            let color = if bid { theme.success } else { theme.danger };
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .children((0..BOOK_LEVELS).map(move |ix| {
                    let offset = (ix + 1) as f64 * 0.01;
                    let value = if bid {
                        book.mid - offset
                    } else {
                        book.mid + offset
                    };
                    let size = book.size(ix, bid);
                    div()
                        .relative()
                        .flex()
                        .items_center()
                        .gap_2()
                        .h_5()
                        .px_1()
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .when(bid, |this| this.right_0())
                                .when(!bid, |this| this.left_0())
                                .w(relative((size / largest) as f32))
                                .bg(color.opacity(0.12)),
                        )
                        .child(
                            div()
                                .w_4()
                                .text_color(theme.muted_foreground)
                                .child(format!("{}", ix + 1)),
                        )
                        .child(div().flex_1().text_color(color).child(price(value)))
                        // Sizes and order counts keep trailing-aligned lanes,
                        // so that levels compare down the column.
                        .child(div().flex().justify_end().w_12().child(amount(size)))
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .w_6()
                                .text_color(theme.muted_foreground)
                                .child(format!("({})", (ix * 7 + book.updates) % 30 + 1)),
                        )
                }))
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .py_2()
            .text_xs()
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_1()
                    .child(
                        div()
                            .text_color(theme.success)
                            .child(format!("Bid {:.2}%", ratio * 100.)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .h_1()
                            .rounded_full()
                            .overflow_hidden()
                            .child(div().h_full().w(relative(ratio)).bg(theme.success))
                            .child(div().h_full().flex_1().bg(theme.danger)),
                    )
                    .child(
                        div()
                            .text_color(theme.danger)
                            .child(format!("{:.2}% Ask", (1. - ratio) * 100.)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .child(side(true))
                    .child(side(false)),
            )
    }
}

#[derive(Clone, Copy)]
struct Trade {
    /// Seconds into the session.
    time: u32,
    price: f64,
    size: u64,
    /// Whether it traded at the ask, a buy, or at the bid.
    buy: bool,
}

/// The latest trades in the selected symbol, newest first, in two columns.
struct TimeAndSales {
    trades: Vec<Trade>,
    _subscription: Subscription,
}

impl TimeAndSales {
    fn new(store: Entity<QuoteStore>, feed: &Entity<MarketFeed>, cx: &mut Context<Self>) -> Self {
        let last = store.read(cx).quotes[SELECTED].last;
        Self {
            trades: (0..TRADES)
                .map(|ix| Trade {
                    time: 37_800 - ix as u32 * 3,
                    price: last + ((ix * 7 % 11) as f64 - 5.) * 0.01,
                    size: 100 * (1 + (ix as u64 * 37) % 60),
                    buy: ix % 3 != 0,
                })
                .collect(),
            _subscription: cx.subscribe(feed, |this, _, event: &QuoteEvent, cx| {
                if event.symbol == SELECTED {
                    let newest = this.trades[0];
                    this.trades.insert(
                        0,
                        Trade {
                            time: newest.time + 1,
                            price: event.last,
                            size: 100 * (1 + event.volume % 60),
                            buy: event.last >= newest.price,
                        },
                    );
                    this.trades.truncate(TRADES);
                    cx.notify();
                }
            }),
        }
    }
}

impl Render for TimeAndSales {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let column = |trades: &[Trade]| {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .children(trades.iter().map(|trade| {
                    let color = change_color(if trade.buy { 1. } else { -1. }, theme);
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .h(rems(1.125))
                        .child(div().text_color(theme.muted_foreground).child(format!(
                            "{:02}:{:02}:{:02}",
                            trade.time / 3_600,
                            trade.time / 60 % 60,
                            trade.time % 60
                        )))
                        .child(
                            div()
                                .flex()
                                .flex_1()
                                .justify_end()
                                .text_color(color)
                                .child(price(trade.price)),
                        )
                        .child(
                            div()
                                .flex()
                                .w_12()
                                .justify_end()
                                .child(grouped(trade.size as f64, 0)),
                        )
                        .child(div().w_0p5().h_3().rounded_full().bg(color))
                }))
        };
        let (left, right) = self.trades.split_at(TRADES / 2);
        div()
            .size_full()
            .flex()
            .flex_row()
            .gap_4()
            .px_3()
            .py_2()
            .text_xs()
            .overflow_hidden()
            .child(column(left))
            .child(column(right))
    }
}

/// How the selected symbol's volume splits between large, medium and small
/// buys and sells, as a donut and a legend.
struct TradeStats {
    /// Large, medium and small buys, then small, medium and large sells.
    buckets: [f64; 6],
    _subscription: Subscription,
}

const BUCKETS: [&str; 6] = [
    "Large buy",
    "Medium buy",
    "Small buy",
    "Small sell",
    "Medium sell",
    "Large sell",
];

impl TradeStats {
    fn new(feed: &Entity<MarketFeed>, cx: &mut Context<Self>) -> Self {
        Self {
            buckets: [4.2e8, 2.9e8, 1.1e8, 1.3e8, 2.4e8, 3.8e8],
            _subscription: cx.subscribe(feed, |this, _, event: &QuoteEvent, cx| {
                if event.symbol == SELECTED {
                    this.buckets[event.volume as usize % 6] += (event.volume % 1_000) as f64 * 1e3;
                    cx.notify();
                }
            }),
        }
    }
}

impl Render for TradeStats {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let total: f64 = self.buckets.iter().sum();
        let colors = [
            theme.success,
            theme.success.opacity(0.7),
            theme.success.opacity(0.4),
            theme.danger.opacity(0.4),
            theme.danger.opacity(0.7),
            theme.danger,
        ];
        let buckets = self.buckets;
        let inflow = buckets[..3].iter().sum::<f64>() - buckets[3..].iter().sum::<f64>();
        div()
            .size_full()
            .flex()
            .items_center()
            .gap_4()
            .p_3()
            .text_xs()
            .overflow_hidden()
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| {
                        let center = bounds.center();
                        let radius = bounds.size.width.min(bounds.size.height) / 2. - px(8.);
                        let at = |turn: f64| {
                            let angle =
                                (turn * std::f64::consts::TAU - std::f64::consts::FRAC_PI_2) as f32;
                            point(
                                center.x + radius * angle.cos(),
                                center.y + radius * angle.sin(),
                            )
                        };
                        let mut start = 0.;
                        for (value, color) in buckets.iter().zip(colors) {
                            let end = start + value / total;
                            let mut arc = PathBuilder::stroke(px(14.));
                            arc.move_to(at(start));
                            arc.arc_to(
                                point(radius, radius),
                                px(0.),
                                end - start > 0.5,
                                true,
                                at(end),
                            );
                            if let Ok(arc) = arc.build() {
                                window.paint_path(arc, color);
                            }
                            start = end;
                        }
                    },
                )
                .flex_shrink_0()
                .size(rems(7.)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .gap_0p5()
                    .children(self.buckets.iter().zip(BUCKETS).zip(colors).map(
                        |((value, label), color)| {
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(div().size_2().rounded_full().bg(color))
                                .child(
                                    div()
                                        .flex_1()
                                        .text_color(theme.muted_foreground)
                                        .child(label),
                                )
                                .child(div().flex().justify_end().w_16().child(amount(*value)))
                                .child(
                                    div()
                                        .w_12()
                                        .flex()
                                        .justify_end()
                                        .child(format!("{:.2}%", value / total * 100.)),
                                )
                        },
                    ))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .mt_1()
                            .pt_1()
                            .border_t_1()
                            .border_color(theme.border)
                            .child("Net inflow")
                            .child(
                                div()
                                    .text_color(change_color(inflow, theme))
                                    .child(amount(inflow.abs())),
                            ),
                    ),
            )
    }
}

/// A panel in a background tab or a closed dock: it receives the feed like
/// every other and ignores what it does not show.
struct HiddenPanel {
    title: &'static str,
    received: usize,
    _subscription: Subscription,
}

impl HiddenPanel {
    fn new(title: &'static str, feed: &Entity<MarketFeed>, cx: &mut Context<Self>) -> Self {
        Self {
            title,
            received: 0,
            _subscription: cx.subscribe(feed, |this, _, _: &QuoteEvent, _| {
                this.received += 1;
            }),
        }
    }
}

impl Render for HiddenPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .child(self.title)
            .child(
                div()
                    .text_xs()
                    .text_color(theme(cx).muted_foreground)
                    .child(format!("{} updates", self.received)),
            )
    }
}

/// Market indices along the bottom of the workspace, and the feed's state.
struct StatusBar {
    tick: usize,
}

impl Render for StatusBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        div()
            .size_full()
            .flex()
            .items_center()
            .gap_4()
            .px_3()
            .overflow_hidden()
            .whitespace_nowrap()
            .border_t_1()
            .border_color(theme.border)
            .text_xs()
            .children(
                ["HSI", "HSCEI", "HSTECH", "SSE", "SPX", "IXIC"]
                    .iter()
                    .enumerate()
                    .map(|(ix, name)| {
                        let value = 20_000. + (ix * 1_234 + self.tick * 7) as f64 * 0.37;
                        let change = if ix % 2 == 0 { -1. } else { 1. } * (ix as f64 + 1.) * 12.3;
                        let color = change_color(change, theme);
                        div()
                            .flex()
                            .gap_1()
                            .child(div().text_color(theme.muted_foreground).child(*name))
                            .child(div().text_color(color).child(price(value)))
                            .child(div().text_color(color).child(signed(change)))
                            .child(
                                div()
                                    .text_color(color)
                                    .child(percent(change / value * 100.)),
                            )
                    }),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(div().size_2().rounded_full().bg(theme.success))
                    .child("Connected · Level 2"),
            )
            .child(div().text_color(theme.muted_foreground).child(format!(
                "10:{:02}:{:02} EST",
                self.tick / 60 % 60,
                self.tick % 60
            )))
    }
}
