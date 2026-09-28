//! The showcase's design tokens: colors by semantic role, for light and dark
//! windows, and the corner radius. Everything else reads them from here;
//! these definitions are the only place a color value is written down.

use gpui::{App, Global, Hsla, Pixels, Window, WindowAppearance, hsla, px, rgb};

pub struct Theme {
    /// The window's main surface and the text on it.
    pub background: Hsla,
    pub foreground: Hsla,
    /// Quiet fills: table headers, segmented control tracks, pressed states.
    pub muted: Hsla,
    /// Secondary text: descriptions, labels, metadata.
    pub muted_foreground: Hsla,
    /// Hairlines between regions and around cards and controls.
    pub border: Hsla,
    /// The navigation sidebar's surface, and its hovered and selected rows.
    pub sidebar: Hsla,
    pub sidebar_accent: Hsla,
    /// Hovered rows and quiet buttons.
    pub accent: Hsla,
    /// Selection emphasis: switches that are on.
    pub primary: Hsla,
    pub primary_foreground: Hsla,
    /// Every other row of a table.
    pub stripe: Hsla,
    /// Gains and losses, always shown with a sign as well.
    pub success: Hsla,
    pub danger: Hsla,
    /// Text on a `danger` fill, such as a count badge.
    pub danger_foreground: Hsla,
    /// Tooltips and other surfaces above the window.
    pub popover: Hsla,
    pub radius: Pixels,
}

impl Global for Theme {}

impl Theme {
    fn light() -> Self {
        Self {
            background: rgb(0xffffff).into(),
            foreground: rgb(0x0a0a0a).into(),
            muted: rgb(0xf4f4f5).into(),
            muted_foreground: rgb(0x71717a).into(),
            border: rgb(0xe4e4e7).into(),
            sidebar: rgb(0xfafafa).into(),
            sidebar_accent: rgb(0xececee).into(),
            accent: rgb(0xf4f4f5).into(),
            primary: rgb(0x18181b).into(),
            primary_foreground: rgb(0xfafafa).into(),
            stripe: hsla(0., 0., 0.5, 0.03),
            success: rgb(0x15803d).into(),
            danger: rgb(0xdc2626).into(),
            danger_foreground: rgb(0xffffff).into(),
            popover: rgb(0xffffff).into(),
            radius: px(6.),
        }
    }

    fn dark() -> Self {
        Self {
            background: rgb(0x0a0a0a).into(),
            foreground: rgb(0xfafafa).into(),
            muted: rgb(0x1c1c1f).into(),
            muted_foreground: rgb(0xa1a1aa).into(),
            border: rgb(0x27272a).into(),
            sidebar: rgb(0x111113).into(),
            sidebar_accent: rgb(0x232326).into(),
            accent: rgb(0x1c1c1f).into(),
            primary: rgb(0xfafafa).into(),
            primary_foreground: rgb(0x18181b).into(),
            stripe: hsla(0., 0., 0.5, 0.05),
            success: rgb(0x4ade80).into(),
            danger: rgb(0xdc2626).into(),
            danger_foreground: rgb(0xffffff).into(),
            popover: rgb(0x18181b).into(),
            radius: px(6.),
        }
    }

    /// Sets the theme for the window's appearance, and keeps it following it.
    pub fn follow(window: &mut Window, cx: &mut App) {
        cx.set_global(Self::for_appearance(window.appearance()));
    }

    pub fn for_appearance(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::dark(),
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::light(),
        }
    }
}

/// The theme, for elements to read their colors from.
pub fn theme(cx: &App) -> &Theme {
    cx.global::<Theme>()
}
