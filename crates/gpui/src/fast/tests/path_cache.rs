//! Paths drawn from kept triangles. See [`crate::fast::path_cache`].

use crate::{PathBuilder, Pixels, path_cache_stats, point, px};

fn sparkline(at: (f32, f32)) -> PathBuilder {
    let mut builder = PathBuilder::stroke(px(1.5));
    builder.move_to(point(px(at.0), px(at.1)));
    for step in 1..20 {
        let x = at.0 + step as f32 * 3.;
        let y = at.1 + ((step * 7) % 11) as f32;
        builder.line_to(point(px(x), px(y)));
    }
    builder
}

fn positions(path: &crate::Path<Pixels>) -> Vec<(f32, f32)> {
    path.vertices
        .iter()
        .map(|vertex| (vertex.xy_position.x.0, vertex.xy_position.y.0))
        .collect()
}

/// A shape built again gives the same triangles, whether they were kept or
/// not.
#[test]
fn a_path_built_again_is_the_same() {
    let first = sparkline((10., 20.)).build().unwrap();
    let again = sparkline((10., 20.)).build().unwrap();
    assert_eq!(positions(&first), positions(&again));
    assert_eq!(first.bounds, again.bounds);
    assert!(!first.vertices.is_empty());
}

/// The same shape built somewhere else is the same triangles moved there.
#[test]
fn a_path_of_the_same_shape_elsewhere_is_moved() {
    let here = sparkline((0., 0.)).build().unwrap();
    let there = sparkline((100., 250.)).build().unwrap();
    let moved: Vec<(f32, f32)> = positions(&here)
        .into_iter()
        .map(|(x, y)| (x + 100., y + 250.))
        .collect();
    assert_eq!(positions(&there), moved);
    assert_eq!(
        there.bounds.origin,
        here.bounds.origin + point(px(100.), px(250.))
    );
    assert_eq!(there.bounds.size, here.bounds.size);
}

/// A different shape, or the same shape stroked differently, is not taken
/// for a kept one.
#[test]
fn a_different_shape_or_stroke_is_tessellated_again() {
    let thin = sparkline((0., 0.)).build().unwrap();
    let mut wide_builder = PathBuilder::stroke(px(4.));
    wide_builder.move_to(point(px(0.), px(0.)));
    for step in 1..20 {
        wide_builder.line_to(point(px(step as f32 * 3.), px(((step * 7) % 11) as f32)));
    }
    let wide = wide_builder.build().unwrap();
    assert_ne!(positions(&thin), positions(&wide));
    assert!(wide.bounds.size.height > thin.bounds.size.height);
}

/// A window-sized ground with `holes` square holes, which move with `frame`
/// so that no two frames are the same shape.
fn ground(frame: usize, holes: usize) -> PathBuilder {
    let mut builder = PathBuilder::fill();
    builder.move_to(point(px(0.), px(0.)));
    builder.line_to(point(px(2056.), px(0.)));
    builder.line_to(point(px(2056.), px(1290.)));
    builder.line_to(point(px(0.), px(1290.)));
    builder.close();
    for hole in 0..holes {
        let x = 10. + (hole % 40) as f32 * 50. + frame as f32 * 0.37;
        let y = 10. + (hole / 40) as f32 * 50.;
        builder.move_to(point(px(x), px(y)));
        builder.line_to(point(px(x), px(y + 30.)));
        builder.line_to(point(px(x + 30.), px(y + 30.)));
        builder.line_to(point(px(x + 30.), px(y)));
        builder.close();
    }
    builder
}

/// Window-sized paths in a new shape every frame fill the kept shapes, but
/// not past the bytes they may hold.
#[test]
fn unique_window_sized_paths_are_kept_within_a_bounded_number_of_bytes() {
    for frame in 0..400 {
        ground(frame, 200).build().unwrap();
        assert!(path_cache_stats().held_bytes <= crate::fast::path_cache::MOST_HELD_BYTES);
    }
    let stats = path_cache_stats();
    assert_eq!(stats.misses, 400);
    assert!(
        stats.built_bytes > 2 * crate::fast::path_cache::MOST_HELD_BYTES as u64,
        "{} bytes built is not enough to need a bound",
        stats.built_bytes
    );
    assert!(
        stats.held_bytes > crate::fast::path_cache::MOST_HELD_BYTES / 4,
        "{} held",
        stats.held_bytes
    );
}

/// A shape let go of and built again is tessellated again, to the same
/// triangles.
#[test]
fn shapes_built_again_after_being_let_go_are_tessellated_again_the_same() {
    let first = ground(0, 200).build().unwrap();
    for frame in 1..400 {
        ground(frame, 200).build().unwrap();
    }
    let before = path_cache_stats();
    assert!(before.evictions > 0, "nothing was let go of");
    let again = ground(0, 200).build().unwrap();
    let after = path_cache_stats();
    assert_eq!(
        after.misses,
        before.misses + 1,
        "built again from kept ones"
    );
    assert_eq!(after.hits, before.hits);
    assert_eq!(positions(&again), positions(&first));
    assert_eq!(again.bounds, first.bounds);
}

/// Window-sized paths of a set drawn again every frame, weighing more than
/// half of what may be kept and less than all of it, are never tessellated
/// again after the first frame, though more than one generation of the
/// kept shapes this replaced.
#[test]
fn a_set_of_window_sized_paths_between_half_and_all_the_bytes_is_drawn_from_kept_ones() {
    let most = crate::fast::path_cache::MOST_HELD_BYTES as u64;
    for frame in 0..5 {
        for shape in 0..100 {
            ground(shape, 200).build().unwrap();
        }
        let stats = path_cache_stats();
        assert_eq!(stats.misses, 100, "after frame {frame}");
        assert_eq!(stats.hits, 100 * frame);
        assert!(stats.built_bytes > most / 2 && stats.built_bytes < most);
    }
}
