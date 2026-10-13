//! Tests of [`crate::fast::misc`].

use crate::fast::misc::release_velocity;

#[test]
fn accelerating_flick_keeps_its_release_speed() {
    assert_eq!(release_velocity(900., Some(1800.)), 1800.);
    assert_eq!(release_velocity(-900., Some(-1800.)), -1800.);
}

#[test]
fn flick_that_eased_off_keeps_its_window_speed() {
    assert_eq!(release_velocity(1200., Some(40.)), 1200.);
    assert_eq!(release_velocity(-1200., Some(-40.)), -1200.);
}

#[test]
fn fit_against_the_displacement_never_reverses_the_fling() {
    assert_eq!(release_velocity(1200., Some(-300.)), 1200.);
    assert_eq!(release_velocity(-1200., Some(5000.)), -1200.);
}

#[test]
fn without_a_fit_or_a_displacement_one_estimate_decides() {
    assert_eq!(release_velocity(700., None), 700.);
    assert_eq!(release_velocity(0., Some(250.)), 250.);
    assert_eq!(release_velocity(0., None), 0.);
}
