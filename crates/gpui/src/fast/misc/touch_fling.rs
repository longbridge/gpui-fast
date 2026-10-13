//! How fast a touch scroll is flung when the finger lifts.

/// The speed a touch scroll is released with along one axis, in pixels per
/// second, from the two estimates `VelocityTracker::velocity` in
/// `gestures.rs` makes over its trailing window: `endpoint`, the window's
/// average speed, and `fitted`, the speed at the newest sample of a quadratic
/// fit (`None` when the samples are too degenerate to fit).
///
/// Upstream releases with `fitted` alone. With few, noisy samples, or a finger
/// easing off just before it lifts, the fit can come out tiny, so a flick ends
/// with no momentum, or pointing the other way, so the fling runs backwards.
/// The displacement over the window is the robust part: the direction is
/// taken from it, and a fit that disagrees with it is not trusted at all.
/// When both agree, the larger speed is taken, so a flick that accelerated
/// keeps its release speed and one that slowed at the very end keeps the
/// speed it had over the window. A slow drag leaves both small and still
/// does not fling.
pub(crate) fn release_velocity(endpoint: f32, fitted: Option<f64>) -> f32 {
    let Some(fitted) = fitted.map(|fitted| fitted as f32) else {
        return endpoint;
    };
    if endpoint == 0. {
        return fitted;
    }
    if fitted.signum() != endpoint.signum() {
        return endpoint;
    }
    endpoint.signum() * fitted.abs().max(endpoint.abs())
}
