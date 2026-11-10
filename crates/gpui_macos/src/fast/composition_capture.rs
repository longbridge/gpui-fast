//! Pointer ownership for a composed window, independent of AppKit hit testing.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PointerOwner {
    Gpui,
    Native(usize),
}

#[derive(Default)]
pub(super) struct PointerCapture {
    buttons: u64,
    owner: Option<PointerOwner>,
}

impl PointerCapture {
    pub(super) fn owner(&self) -> Option<PointerOwner> {
        self.owner
    }

    /// GPUI keeps its chord, including a down consumed in capture. Native downs
    /// retain the existing policy: the latest accepted native target owns drag/up.
    /// A missing target cannot steal a native claim (including a forwarder's
    /// pre-capture claim) or add a GPUI button to its tracked native buttons.
    pub(super) fn press(&mut self, button: u64, native: Option<usize>) {
        if button == 0 || (native.is_none() && matches!(self.owner, Some(PointerOwner::Native(_))))
        {
            return;
        }
        self.buttons |= button;
        if self.owner != Some(PointerOwner::Gpui) {
            self.owner = Some(native.map_or(PointerOwner::Gpui, PointerOwner::Native));
        }
    }

    pub(super) fn release(&mut self, button: u64) {
        self.buttons &= !button;
        if self.buttons == 0 {
            self.owner = None;
        }
    }

    pub(super) fn clear(&mut self) {
        self.buttons = 0;
        self.owner = None;
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum PointerPhase {
    Down,
    Drag,
    Up,
    Other,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum PointerRoute {
    Delivered,
    AppKit(Option<usize>),
}

/// Deliver GPUI-owned button events directly: a consumed initial down never
/// established an AppKit recipient, and a secondary down could hit another view.
/// Native additional downs, wheel and hover retain fresh hit testing.
pub(super) fn route_pointer_event(
    captured: Option<PointerOwner>,
    phase: PointerPhase,
    deliver_gpui: impl FnOnce(),
    hit_test: impl FnOnce() -> Option<usize>,
) -> PointerRoute {
    match captured {
        Some(PointerOwner::Gpui) if phase != PointerPhase::Other => {
            deliver_gpui();
            PointerRoute::Delivered
        }
        Some(PointerOwner::Native(owner))
            if matches!(phase, PointerPhase::Drag | PointerPhase::Up) =>
        {
            PointerRoute::AppKit(Some(owner))
        }
        _ => PointerRoute::AppKit(hit_test()),
    }
}

#[cfg(test)]
mod tests {
    use super::{PointerCapture, PointerOwner, PointerPhase, PointerRoute, route_pointer_event};

    #[test]
    fn every_button_captures_gpui_or_native_until_release() {
        for number in 0..64 {
            let button = 1u64 << number;
            for native in [None, Some(1)] {
                let mut capture = PointerCapture::default();
                capture.press(button, native);
                assert_eq!(
                    capture.owner(),
                    Some(native.map_or(PointerOwner::Gpui, PointerOwner::Native))
                );
                capture.release(button);
                assert_eq!(capture.owner(), None);
                capture.press(button, Some(2));
                assert_eq!(capture.owner(), Some(PointerOwner::Native(2)));
            }
        }
    }

    #[test]
    fn gpui_chord_delivers_without_appkit_tracking_until_final_release() {
        for releases in [[0, 1, 2, 4], [4, 2, 1, 0]] {
            let mut capture = PointerCapture::default();
            // Also represents a down consumed over native content: no AppKit
            // delivery or implicit mouse-down recipient is needed by this seam.
            capture.press(1, None);
            capture.release(1 << 3);
            assert_eq!(capture.owner(), Some(PointerOwner::Gpui));
            for (phase, button) in [
                (PointerPhase::Drag, 1),
                (PointerPhase::Down, 1 << 1),
                (PointerPhase::Down, 1 << 2),
                (PointerPhase::Down, 1 << 4),
                (PointerPhase::Drag, 1 << 4),
                (PointerPhase::Up, 1 << releases[0]),
                (PointerPhase::Up, 1 << releases[1]),
                (PointerPhase::Up, 1 << releases[2]),
                (PointerPhase::Up, 1 << releases[3]),
            ] {
                // The final up still uses the owner snapshotted before release.
                let owner = capture.owner();
                if phase == PointerPhase::Down {
                    capture.press(button, None);
                } else if phase == PointerPhase::Up {
                    capture.release(button);
                }
                let mut gpui_calls = 0;
                assert_eq!(
                    route_pointer_event(
                        owner,
                        phase,
                        || gpui_calls += 1,
                        || panic!("captured GPUI buttons must not hit-test native content"),
                    ),
                    PointerRoute::Delivered
                );
                assert_eq!(gpui_calls, 1);
            }
            assert_eq!(capture.owner(), None);
        }
    }

    #[test]
    fn native_additional_down_reselects_recipient_without_tracking_gpui_buttons() {
        let mut capture = PointerCapture::default();
        capture.press(1, Some(1));
        assert_eq!(
            route_pointer_event(
                capture.owner(),
                PointerPhase::Down,
                || panic!("a native-owned additional down needs fresh hit testing"),
                || Some(2),
            ),
            PointerRoute::AppKit(Some(2))
        );
        capture.press(1 << 1, Some(2));
        assert_eq!(capture.owner(), Some(PointerOwner::Native(2)));
        capture.press(1 << 2, None);
        capture.release(1 << 1);
        assert_eq!(capture.owner(), Some(PointerOwner::Native(2)));
        capture.release(1);
        assert_eq!(capture.owner(), None);
    }

    #[test]
    fn native_continuations_capture_but_additional_downs_wheel_and_hover_hit_test() {
        let native = Some(PointerOwner::Native(1));
        for phase in [PointerPhase::Drag, PointerPhase::Up] {
            assert_eq!(
                route_pointer_event(
                    native,
                    phase,
                    || panic!("native continuation must stay on AppKit"),
                    || panic!("native continuation must retain its owner"),
                ),
                PointerRoute::AppKit(Some(1))
            );
        }
        for (captured, phase) in [
            (native, PointerPhase::Down),
            (native, PointerPhase::Other),
            (Some(PointerOwner::Gpui), PointerPhase::Other),
            (None, PointerPhase::Down),
        ] {
            assert_eq!(
                route_pointer_event(
                    captured,
                    phase,
                    || panic!("uncaptured event must retain normal routing"),
                    || Some(2),
                ),
                PointerRoute::AppKit(Some(2))
            );
        }
    }

    #[test]
    fn forwarder_claim_survives_popup_dismissal_capture() {
        let mut capture = PointerCapture::default();
        capture.press(1, Some(1));
        capture.press(1, None);
        assert_eq!(capture.owner(), Some(PointerOwner::Native(1)));
        capture.release(1);
        assert_eq!(capture.owner(), None);
    }

    #[test]
    fn clearing_recovers_from_lost_releases() {
        for native in [None, Some(1)] {
            let mut capture = PointerCapture::default();
            capture.press(1, native);
            capture.press(1 << 1, native);
            capture.clear();
            assert_eq!(capture.owner(), None);
            capture.press(1 << 2, Some(2));
            capture.release(1 << 2);
            assert_eq!(capture.owner(), None);
        }
    }
}
