//! State the whole application shares, as a real application has: an entity
//! held in a global, which many views read — the sidebar, the component page,
//! the table, the message list — and which changes now and then, notifying
//! every view that read it.

use gpui::{App, Entity, Global};

pub struct AppState {
    /// Unread messages, which a timer increments every few seconds.
    pub unread: usize,
    /// Whether the views lay themselves out densely.
    pub compact: bool,
}

/// The global holding the application's state.
pub struct SharedAppState(pub Entity<AppState>);

impl Global for SharedAppState {}

/// The application's state, for a view to read.
pub fn app_state(cx: &App) -> &AppState {
    cx.global::<SharedAppState>().0.read(cx)
}
