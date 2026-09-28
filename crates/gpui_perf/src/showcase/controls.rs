//! The few controls the showcase needs, drawn from plain GPUI elements so
//! that what is measured is GPUI itself: a segmented control, a switch and a
//! tooltip. They keep the states the design guides ask for — rest, hover,
//! pressed and selected — with the default arrow cursor, and name their
//! keyboard shortcut in their tooltip.

use gpui::{
    AnyView, App, Context, ElementId, FontWeight, IntoElement, Render, SharedString, Stateful,
    Window, div, prelude::*,
};

use super::theme::theme;

/// A tooltip naming a control's action and its shortcut.
pub struct Tooltip {
    label: SharedString,
    shortcut: Option<SharedString>,
}

impl Tooltip {
    pub fn text(
        label: impl Into<SharedString>,
        shortcut: Option<&'static str>,
    ) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
        let label = label.into();
        move |_, cx| {
            let label = label.clone();
            cx.new(|_| Tooltip {
                label,
                shortcut: shortcut.map(SharedString::from),
            })
            .into()
        }
    }
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .shadow_md()
            .text_xs()
            .text_color(theme.foreground)
            .child(self.label.clone())
            .when_some(self.shortcut.clone(), |this, shortcut| {
                this.child(key(shortcut, cx))
            })
    }
}

/// A keyboard key, as a tooltip or hint shows it.
pub fn key(label: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    let theme = theme(cx);
    div()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(theme.border)
        .bg(theme.muted)
        .text_color(theme.muted_foreground)
        .child(label.into())
}

/// One option of a segmented control.
pub fn segment(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    cx: &App,
) -> Stateful<gpui::Div> {
    let theme = theme(cx);
    div()
        .id(id)
        .flex()
        .items_center()
        .h_6()
        .px_2()
        .rounded(theme.radius - gpui::px(2.))
        .text_color(theme.muted_foreground)
        .when(selected, |this| {
            this.bg(theme.background)
                .text_color(theme.foreground)
                .font_weight(FontWeight::MEDIUM)
                .shadow_xs()
        })
        .when(!selected, |this| {
            this.hover(|this| this.text_color(theme.foreground))
        })
        .child(label.into())
}

/// The track a segmented control's options sit in, with its label ahead of
/// it.
pub fn segmented(label: &'static str, cx: &App) -> gpui::Div {
    let theme = theme(cx);
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(div().text_color(theme.muted_foreground).child(label))
}

pub fn segment_track(cx: &App) -> gpui::Div {
    let theme = theme(cx);
    div()
        .flex()
        .items_center()
        .gap_0p5()
        .p_0p5()
        .rounded(theme.radius)
        .bg(theme.muted)
}

/// A switch for a setting that takes effect at once, with its label.
pub fn switch(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    on: bool,
    cx: &App,
) -> Stateful<gpui::Div> {
    let theme = theme(cx);
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .h_6()
        .px_1()
        .rounded(theme.radius)
        .hover(|this| this.bg(theme.accent))
        .active(|this| this.bg(theme.muted))
        .child(
            div()
                .flex()
                .items_center()
                .w_7()
                .h_4()
                .p_0p5()
                .rounded_full()
                .bg(if on { theme.primary } else { theme.border })
                .when(on, |this| this.justify_end())
                .child(div().size_3().rounded_full().bg(if on {
                    theme.primary_foreground
                } else {
                    theme.background
                })),
        )
        .child(label.into())
}
