//! Hover held still while a wheel scrolls content under a still pointer.
//!
//! A wheel moves what is under the pointer without the pointer moving: every
//! frame of the scroll finds another row hovered, renders the row it leaves
//! and the row it enters again, and each may start an animation (a hover
//! card fading in, a toolbar showing) that keeps the list off its layer. A
//! frame of a long transcript scrolled by the wheel then costs about what it
//! costs without layers.
//!
//! So for [`HOVER_FREEZE`] after a wheel event, while the pointer stays where
//! the event was, hover styling is held still: while a frame is drawn,
//! [`crate::HitboxId::is_hovered`] is false, hover listeners of either
//! [`HoverListenerMode`] hear nothing, and the hit test a list foretells for
//! its rows is empty. Only drawing is held. The frame's hit test stays the
//! geometry under the pointer, so the cursor follows it, and events dispatched
//! between frames read it: a click at the spot the wheel scrolled under
//! reaches its handler. When the wheel stops the window is drawn again, and
//! hover follows the pointer as before. A pointer that moves ends the freeze
//! at once.
//!
//! A listener that was hovered may not be painted again: a wheel can scroll its
//! row out of a list while it hears nothing, and only painting asks it. So the
//! listeners that last heard `true` are tracked, and the first frame drawn
//! without the freeze tells those no longer under the pointer. See
//! [`HoveredListener`].

use scheduler::Instant;
use std::{cell::RefCell, rc::Rc, time::Duration};

use crate::{App, Hitbox, HoverListenerMode, MouseDownEvent, Pixels, Point, Window};

/// How long after the last wheel event hover stays held still.
pub(crate) const HOVER_FREEZE: Duration = Duration::from_millis(100);

/// When the window last got a wheel event, and where the pointer was.
pub(crate) struct HoverFreeze {
    last_wheel: Option<(Instant, Point<Pixels>)>,
    /// Off in the crate's own tests, which scroll under a still pointer to
    /// test what hover does as rows move; see [`set_enabled`].
    enabled: bool,
    /// The hover listeners that last heard `true`.
    hovered: Vec<HoveredListener>,
}

/// A hover listener, with what it takes to tell it hover has ended without
/// painting it.
#[derive(Clone)]
struct HoveredListener {
    was_hovered: Rc<RefCell<bool>>,
    has_mouse_down: Rc<RefCell<Option<MouseDownEvent>>>,
    listener: Rc<dyn Fn(&bool, &mut Window, &mut App)>,
    /// The hitbox it was last painted with.
    hitbox: Hitbox,
    mode: HoverListenerMode,
}

impl HoveredListener {
    /// Tells the listener a change of hover, tracking it while it is hovered.
    fn update(&self, is_hovered: bool, window: &mut Window, cx: &mut App) {
        if is_hovered == *self.was_hovered.borrow() {
            return;
        }
        *self.was_hovered.borrow_mut() = is_hovered;
        self.heard(is_hovered, window);
        (self.listener)(&is_hovered, window, cx);
    }

    /// Tracks the listener while it is hovered, as it is told `is_hovered`.
    fn heard(&self, is_hovered: bool, window: &mut Window) {
        if is_hovered {
            note_hovered(window, self);
        } else {
            let hovered = &mut window.fast_layers.hover_freeze.hovered;
            hovered.retain(|listener| !Rc::ptr_eq(&listener.was_hovered, &self.was_hovered));
        }
    }

    /// Whether a frame drawn now would leave the listener hovered. A press
    /// keeps it hovered only while its element is still drawn: one removed
    /// before the press is released no longer sees the release, so the press
    /// is never cleared.
    fn still_hovered(&self, window: &Window, cx: &App) -> bool {
        (self.has_mouse_down.borrow().is_some()
            && window
                .rendered_frame
                .hitboxes
                .iter()
                .any(|hitbox| hitbox.id == self.hitbox.id))
            || (!cx.has_active_drag() && self.mode.is_hovered(&self.hitbox, window))
    }
}

/// Wraps the hover listener an element paints with `hitbox`, which is told
/// of each change of hover, so that it is tracked while it is hovered.
pub(crate) fn track_listener(
    window: &mut Window,
    was_hovered: &Rc<RefCell<bool>>,
    has_mouse_down: &Rc<RefCell<Option<MouseDownEvent>>>,
    listener: impl Fn(&bool, &mut Window, &mut App) + 'static,
    hitbox: &Hitbox,
    mode: HoverListenerMode,
) -> Rc<dyn Fn(&bool, &mut Window, &mut App)> {
    let tracked = HoveredListener {
        was_hovered: was_hovered.clone(),
        has_mouse_down: has_mouse_down.clone(),
        listener: Rc::new(listener),
        hitbox: hitbox.clone(),
        mode,
    };
    // A listener hearing it is hovered is painted with this hitbox now.
    if *was_hovered.borrow() {
        note_hovered(window, &tracked);
    }
    Rc::new(
        move |is_hovered: &bool, window: &mut Window, cx: &mut App| {
            tracked.heard(*is_hovered, window);
            (tracked.listener)(is_hovered, window, cx);
        },
    )
}

/// Keeps `listener` as a hovered one, with the hitbox it was just painted with.
fn note_hovered(window: &mut Window, listener: &HoveredListener) {
    let hovered = &mut window.fast_layers.hover_freeze.hovered;
    match hovered
        .iter_mut()
        .find(|held| Rc::ptr_eq(&held.was_hovered, &listener.was_hovered))
    {
        Some(held) => *held = listener.clone(),
        None => hovered.push(listener.clone()),
    }
}

/// Tells a hovered listener that a frame just drawn without the freeze left off
/// the pointer, painted or not, that it no longer is. A listener painted in that
/// frame has been told already, or will be by what it deferred, and the telling
/// is idempotent. Runs after the frame is in place, with hit testing on it.
pub(crate) fn release_unhovered(window: &mut Window, cx: &mut App) {
    if window.fast_layers.hover_freeze.hovered.is_empty() || frozen(window) {
        return;
    }
    for index in 0..window.fast_layers.hover_freeze.hovered.len() {
        let held = &window.fast_layers.hover_freeze.hovered[index];
        if !held.still_hovered(window, cx) {
            let held = held.clone();
            window.defer(cx, move |window, cx| held.update(false, window, cx));
        }
    }
}

impl Default for HoverFreeze {
    fn default() -> Self {
        Self {
            last_wheel: None,
            enabled: !cfg!(test),
            hovered: Vec::new(),
        }
    }
}

/// Notes a wheel event at the pointer's position, and draws the window again
/// once hover is no longer held still for it.
pub(crate) fn note_wheel(window: &mut Window, cx: &mut App) {
    let freeze = &mut window.fast_layers.hover_freeze;
    if !freeze.enabled {
        return;
    }
    freeze.last_wheel = Some((Instant::now(), window.mouse_position));
    window
        .spawn(cx, async move |cx| {
            cx.background_executor()
                .timer(HOVER_FREEZE + Duration::from_millis(10))
                .await;
            cx.update(|window, _| {
                if !frozen(window) {
                    window.refresh();
                }
            })
            .ok();
        })
        .detach();
}

/// Whether hover is held still: a wheel event came less than
/// [`HOVER_FREEZE`] ago, and the pointer has not moved since.
pub(crate) fn frozen(window: &Window) -> bool {
    window
        .fast_layers
        .hover_freeze
        .last_wheel
        .is_some_and(|(at, position)| {
            at.elapsed() < HOVER_FREEZE && position == window.mouse_position
        })
}

/// Whether hover reads as nothing for a hitbox the pointer has not captured,
/// for [`crate::HitboxId::is_hovered`] and what a retained subtree records of
/// it: after keyboard input, as upstream has it, and while a frame is drawn
/// with hover held still.
///
/// Event dispatch is not drawing. A mouse listener asks `is_hovered` before it
/// runs, so holding hover there too would drop a click made under a still
/// pointer right after the wheel.
pub(crate) fn hides_hover(window: &Window) -> bool {
    window.last_input_was_keyboard() || (!window.invalidator.not_drawing() && frozen(window))
}

/// Whether a frame being drawn leaves hover listeners as they were, for any
/// input modality.
pub(crate) fn holds_listeners(window: &Window) -> bool {
    !window.invalidator.not_drawing() && frozen(window)
}

impl Window {
    /// Turns holding hover still while a wheel scrolls on or off for this
    /// window, for the crate's tests.
    #[cfg(test)]
    pub(crate) fn set_hover_freeze(&mut self, enabled: bool) {
        self.fast_layers.hover_freeze.enabled = enabled;
        self.fast_layers.hover_freeze.last_wheel = None;
    }
}

#[cfg(test)]
impl HoverFreeze {
    pub(crate) fn end_for_test(&mut self) {
        self.last_wheel = None;
    }

    /// How many hover listeners are kept as hovered.
    pub(crate) fn hovered_listeners(&self) -> usize {
        self.hovered.len()
    }
}
