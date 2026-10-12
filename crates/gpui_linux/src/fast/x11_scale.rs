//! The X11 scale factor from `GDK_SCALE`.
//!
//! A Wayland compositor that leaves XWayland clients unscaled (Hyprland's
//! `xwayland:force_zero_scaling`) shows them in physical pixels and expects
//! each to scale itself. Such sessions state the scale in `GDK_SCALE`, which
//! GTK and so WebKitGTK windows embedded in a GPUI window obey, and often set
//! no `Xft.dpi`. Without either, the scale factor fell back to the monitor's
//! physical DPI: 1.4167 for a 27" 4K monitor the session scales by 2, leaving
//! GPUI content smaller than the GTK content beside it. `Xft.dpi` still wins,
//! so only sessions that would have used that DPI guess change.

/// The scale factor `GDK_SCALE` asks for, if it is set to one.
pub(crate) fn gdk_scale() -> Option<f32> {
    let scale = parse_gdk_scale(&std::env::var("GDK_SCALE").ok()?)?;
    log::info!("Using scale factor from GDK_SCALE: {}", scale);
    Some(scale)
}

/// GDK scales windows by whole numbers only. A value that is not a positive
/// whole number is left to the other sources rather than guessed at.
fn parse_gdk_scale(value: &str) -> Option<f32> {
    match value.trim().parse::<u32>() {
        Ok(scale) if scale > 0 => Some(scale as f32),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_gdk_scale;

    #[test]
    fn a_whole_gdk_scale_is_the_scale_factor() {
        assert_eq!(parse_gdk_scale("2"), Some(2.0));
        assert_eq!(parse_gdk_scale("1"), Some(1.0));
    }

    #[test]
    fn a_gdk_scale_that_is_not_a_positive_whole_number_is_ignored() {
        assert_eq!(parse_gdk_scale(""), None);
        assert_eq!(parse_gdk_scale("0"), None);
        assert_eq!(parse_gdk_scale("1.5"), None);
        assert_eq!(parse_gdk_scale("-2"), None);
        assert_eq!(parse_gdk_scale("auto"), None);
    }
}
