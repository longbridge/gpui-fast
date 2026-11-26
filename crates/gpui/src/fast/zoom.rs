//! A window drawn at a zoom: everything in it, pixel sizes included, larger.
//!
//! [`Window::set_rem_size`] scales what is sized in rems, and nothing else:
//! borders, paddings, icons, images and shadows given in pixels stay as they
//! were, so a window made larger that way comes apart. A browser's page zoom
//! scales everything instead, and that is what this does for a window:
//!
//! - layout sees a viewport `zoom` times smaller, so the same elements fill
//!   less of it;
//! - the scene is rasterized at the display's scale factor times `zoom`, so
//!   text, paths and images stay sharp rather than being stretched;
//! - what the platform reports in its own coordinates (input positions, pixel
//!   scroll distances, the pointer, insets, the visual viewport) is divided by
//!   `zoom` on the way in, and what GPUI hands the platform in window
//!   coordinates (IME and accessibility bounds, input regions, the window
//!   menu's position) is multiplied by it on the way out.
//!
//! Upstream files hold only the hooks: the zoom's field on [`Window`], the
//! lines in `Window::bounds_changed` that apply it, and one-line forwards
//! where the platform's coordinates cross into GPUI's.

use std::ops::Range;

use crate::{
    App, BackGestureEvent, Bounds, FileDropEvent, LongPressEvent, MouseDownEvent, MouseExitEvent,
    MouseMoveEvent, MousePressureEvent, MouseUpEvent, PinchEvent, Pixels, PlatformInput,
    PlatformInputHandler, Point, ScrollDelta, ScrollEvent, TouchDragEvent, TouchEvent, Window,
    WindowInsets,
};

/// A window's zoom: how many times its natural size everything in it is
/// drawn. `1.0`, the natural size, by default.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Zoom(f32);

impl Default for Zoom {
    fn default() -> Self {
        Self(1.)
    }
}

impl Window {
    /// How many times its natural size everything in the window is drawn; see
    /// [`Window::set_zoom`].
    pub fn zoom(&self) -> f32 {
        self.fast_zoom.0
    }

    /// Draws everything in the window `zoom` times its natural size, as a
    /// browser's page zoom does, so that a person who needs larger text and
    /// controls can have them.
    ///
    /// Layout sees a viewport `zoom` times smaller, the scene is rasterized at
    /// the display's scale factor times `zoom` so that text and paths stay
    /// sharp, and the positions of input events are divided by it. Unlike
    /// [`Window::set_rem_size`], which scales only what is sized in rems, this
    /// scales everything: pixel sizes, borders, images and shadows. While a
    /// window is zoomed, [`Window::scale_factor`] is the display's times the
    /// zoom. `1.0` is the natural size.
    ///
    /// The window's bounds observers are told, as when the window is resized:
    /// call this outside an update of an entity that observes them.
    pub fn set_zoom(&mut self, zoom: f32, cx: &mut App) {
        assert!(
            zoom.is_finite() && zoom > 0.,
            "zoom must be a positive number, not {zoom}"
        );
        if self.fast_zoom.0 != zoom {
            self.fast_zoom = Zoom(zoom);
            self.bounds_changed(cx);
        }
    }
}

/// After `Window::bounds_changed` has read the platform's scale factor and
/// content size: rasterize at the zoom, and lay out in a viewport that much
/// smaller.
pub(crate) fn apply_to_viewport(window: &mut Window) {
    let zoom = window.fast_zoom.0;
    window.scale_factor *= zoom;
    window.viewport_size = window.viewport_size.map(|d| d / zoom);
}

/// After `Window::bounds_changed` has read the platform's pointer: where it is
/// in the zoomed window.
pub(crate) fn apply_to_mouse_position(window: &mut Window) {
    window.mouse_position = window.mouse_position / window.fast_zoom.0;
}

/// A point the platform gives, in the zoomed window's coordinates.
pub(crate) fn point_in(point: Point<Pixels>, window: &Window) -> Point<Pixels> {
    point / window.fast_zoom.0
}

/// A point in the zoomed window, in the platform's coordinates.
pub(crate) fn point_out(point: Point<Pixels>, window: &Window) -> Point<Pixels> {
    point * window.fast_zoom.0
}

/// Bounds in the zoomed window, in the platform's coordinates.
pub(crate) fn bounds_out(bounds: Bounds<Pixels>, window: &Window) -> Bounds<Pixels> {
    let zoom = window.fast_zoom.0;
    bounds.map(|d| d * zoom)
}

/// [`Window::insets`]: the platform's, in the zoomed window's coordinates.
pub(crate) fn insets(window: &Window) -> WindowInsets {
    let zoom = window.fast_zoom.0;
    let insets = window.platform_window.insets();
    WindowInsets {
        safe_area: insets.safe_area.map(|d| *d / zoom),
        ime: insets.ime.map(|d| *d / zoom),
        corner_radii: insets.corner_radii.map(|d| *d / zoom),
    }
}

/// [`Window::visual_viewport_bounds`]: the platform's, in the zoomed window's
/// coordinates.
pub(crate) fn visual_viewport_bounds(window: &Window) -> Bounds<Pixels> {
    let zoom = window.fast_zoom.0;
    window
        .platform_window
        .visual_viewport_bounds()
        .map(|d| d / zoom)
}

/// [`Window::set_input_region`]: the region, in the platform's coordinates.
pub(crate) fn set_input_region(window: &Window, region: Option<&[Bounds<Pixels>]>) {
    let region = region.map(|region| {
        region
            .iter()
            .map(|bounds| bounds_out(*bounds, window))
            .collect::<Vec<_>>()
    });
    window.platform_window.set_input_region(region.as_deref());
}

/// [`PlatformInputHandler::bounds_for_range`], in the platform's coordinates.
pub(crate) fn bounds_for_range(
    handler: &mut PlatformInputHandler,
    range_utf16: Range<usize>,
) -> Option<Bounds<Pixels>> {
    handler
        .cx
        .update(|window, cx| {
            let bounds = handler.handler.bounds_for_range(range_utf16, window, cx)?;
            Some(bounds_out(bounds, window))
        })
        .ok()
        .flatten()
}

/// [`PlatformInputHandler::selection_bounds_for_range`], in the platform's
/// coordinates.
pub(crate) fn selection_bounds_for_range(
    handler: &mut PlatformInputHandler,
    range_utf16: Range<usize>,
) -> Vec<Bounds<Pixels>> {
    handler
        .cx
        .update(|window, cx| {
            handler
                .handler
                .selection_bounds_for_range(range_utf16, window, cx)
                .into_iter()
                .map(|bounds| bounds_out(bounds, window))
                .collect()
        })
        .unwrap_or_default()
}

/// [`PlatformInputHandler::character_index_for_point`], for a point the
/// platform gives.
pub(crate) fn character_index_for_point(
    handler: &mut PlatformInputHandler,
    point: Point<Pixels>,
) -> Option<usize> {
    handler
        .cx
        .update(|window, cx| {
            let point = point_in(point, window);
            handler.handler.character_index_for_point(point, window, cx)
        })
        .ok()
        .flatten()
}

/// [`PlatformInputHandler::element_bounds`], in the platform's coordinates.
pub(crate) fn element_bounds(handler: &mut PlatformInputHandler) -> Option<Bounds<Pixels>> {
    handler
        .cx
        .update(|window, cx| {
            let bounds = handler.handler.element_bounds(window, cx)?;
            Some(bounds_out(bounds, window))
        })
        .ok()
        .flatten()
}

/// The event the platform sent, in the zoomed window's coordinates: its
/// positions and pixel distances divided by the zoom.
pub(crate) fn input_in(event: PlatformInput, window: &Window) -> PlatformInput {
    zoomed_out(event, window.fast_zoom.0)
}

fn zoomed_out(event: PlatformInput, zoom: f32) -> PlatformInput {
    if zoom == 1. {
        return event;
    }
    match event {
        PlatformInput::KeyDown(_)
        | PlatformInput::KeyUp(_)
        | PlatformInput::ModifiersChanged(_) => event,
        PlatformInput::MouseDown(event) => PlatformInput::MouseDown(MouseDownEvent {
            position: event.position / zoom,
            ..event
        }),
        PlatformInput::MouseUp(event) => PlatformInput::MouseUp(MouseUpEvent {
            position: event.position / zoom,
            ..event
        }),
        PlatformInput::MousePressure(event) => PlatformInput::MousePressure(MousePressureEvent {
            position: event.position / zoom,
            ..event
        }),
        PlatformInput::MouseMove(event) => PlatformInput::MouseMove(MouseMoveEvent {
            position: event.position / zoom,
            ..event
        }),
        PlatformInput::MouseExited(event) => PlatformInput::MouseExited(MouseExitEvent {
            position: event.position / zoom,
            ..event
        }),
        PlatformInput::Scroll(event) => PlatformInput::Scroll(ScrollEvent {
            position: event.position / zoom,
            delta: match event.delta {
                ScrollDelta::Pixels(delta) => ScrollDelta::Pixels(delta / zoom),
                lines @ ScrollDelta::Lines(_) => lines,
            },
            ..event
        }),
        PlatformInput::Pinch(event) => PlatformInput::Pinch(PinchEvent {
            position: event.position / zoom,
            ..event
        }),
        PlatformInput::LongPress(event) => PlatformInput::LongPress(LongPressEvent {
            start_position: event.start_position / zoom,
            position: event.position / zoom,
            ..event
        }),
        PlatformInput::TouchDrag(event) => PlatformInput::TouchDrag(TouchDragEvent {
            start_position: event.start_position / zoom,
            position: event.position / zoom,
            ..event
        }),
        PlatformInput::BackGesture(event) => PlatformInput::BackGesture(BackGestureEvent {
            start_position: event.start_position / zoom,
            ..event
        }),
        PlatformInput::FileDrop(event) => PlatformInput::FileDrop(match event {
            FileDropEvent::Entered { position, paths } => FileDropEvent::Entered {
                position: position / zoom,
                paths,
            },
            FileDropEvent::Pending { position } => FileDropEvent::Pending {
                position: position / zoom,
            },
            FileDropEvent::Submit { position } => FileDropEvent::Submit {
                position: position / zoom,
            },
            FileDropEvent::Exited | FileDropEvent::Ended => event,
        }),
        PlatformInput::Touch(event) => PlatformInput::Touch(TouchEvent {
            position: event.position / zoom,
            predicted_position: event.predicted_position.map(|p| p / zoom),
            ..event
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use crate::{
        Context, InteractiveElement as _, IntoElement, Modifiers, MouseButton, ParentElement as _,
        Pixels, PlatformInput, Point, Render, ScrollDelta, ScrollEvent, Styled as _,
        TestAppContext, Window, div, point, px,
    };

    struct Target {
        presses: Rc<RefCell<Vec<Point<Pixels>>>>,
    }

    impl Render for Target {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let presses = self.presses.clone();
            div().size_full().child(
                div()
                    .id("target")
                    .absolute()
                    .top(px(40.))
                    .left(px(40.))
                    .size(px(20.))
                    .on_mouse_down(MouseButton::Left, move |event, _, _| {
                        presses.borrow_mut().push(event.position)
                    }),
            )
        }
    }

    #[crate::test]
    fn zoom_draws_larger_and_divides_input_positions(cx: &mut TestAppContext) {
        let presses = Rc::new(RefCell::new(Vec::new()));
        let (_, cx) = cx.add_window_view(|_, _| Target {
            presses: presses.clone(),
        });
        let (viewport, scale_factor) =
            cx.update(|window, _| (window.viewport_size(), window.scale_factor()));

        cx.update(|window, cx| window.set_zoom(2., cx));
        cx.update(|window, _| {
            assert_eq!(window.zoom(), 2.);
            assert_eq!(window.viewport_size(), viewport.map(|d| d / 2.));
            assert_eq!(window.scale_factor(), scale_factor * 2.);
        });
        cx.run_until_parked();

        // The platform's (100, 100) is (50, 50) in the zoomed window: inside
        // the target, which spans 40 to 60 there.
        cx.simulate_mouse_down(
            point(px(100.), px(100.)),
            MouseButton::Left,
            Modifiers::none(),
        );
        assert_eq!(*presses.borrow(), [point(px(50.), px(50.))]);
        // The platform's (50, 50) is (25, 25): outside it, though it would be
        // inside at the natural size.
        cx.simulate_mouse_down(
            point(px(50.), px(50.)),
            MouseButton::Left,
            Modifiers::none(),
        );
        assert_eq!(presses.borrow().len(), 1);
        assert_eq!(
            cx.update(|window, _| window.mouse_position()),
            point(px(25.), px(25.))
        );

        cx.update(|window, cx| window.set_zoom(1., cx));
        cx.update(|window, _| {
            assert_eq!(window.viewport_size(), viewport);
            assert_eq!(window.scale_factor(), scale_factor);
        });
        cx.run_until_parked();
        cx.simulate_mouse_down(
            point(px(50.), px(50.)),
            MouseButton::Left,
            Modifiers::none(),
        );
        assert_eq!(presses.borrow().len(), 2);
    }

    #[test]
    fn zoom_divides_pixel_scroll_deltas_and_keeps_line_deltas() {
        let pixels = PlatformInput::Scroll(ScrollEvent {
            position: point(px(30.), px(60.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(-40.))),
            ..Default::default()
        });
        let PlatformInput::Scroll(zoomed) = super::zoomed_out(pixels, 2.) else {
            unreachable!()
        };
        assert_eq!(zoomed.position, point(px(15.), px(30.)));
        assert_eq!(zoomed.delta.pixel_delta(px(16.)), point(px(0.), px(-20.)));

        let lines = PlatformInput::Scroll(ScrollEvent {
            delta: ScrollDelta::Lines(point(0., -3.)),
            ..Default::default()
        });
        let PlatformInput::Scroll(zoomed) = super::zoomed_out(lines, 2.) else {
            unreachable!()
        };
        assert_eq!(zoomed.delta.pixel_delta(px(16.)), point(px(0.), px(-48.)));
    }
}
