//! Tests of how a draw records which entities its window tracks. See
//! [`crate::fast::tracked_entities`].

use std::{
    hint::black_box,
    rc::Rc,
    time::{Duration, Instant},
};

use collections::FxHashSet;

use crate::{
    AnyWindowHandle, App, AppContext as _, Empty, Entity, EntityId, TestAppContext,
    WindowInvalidator,
};

/// Upstream's `App::record_entities_accessed`, which every draw's record must
/// leave the maps as.
fn upstream_record(
    cx: &mut App,
    window_handle: AnyWindowHandle,
    invalidator: WindowInvalidator,
    entities: &FxHashSet<EntityId>,
) {
    let mut tracked_entities =
        std::mem::take(cx.tracked_entities.entry(window_handle.id).or_default());
    for entity in tracked_entities.iter() {
        cx.window_invalidators_by_entity
            .entry(*entity)
            .and_modify(|windows| {
                windows.remove(&window_handle.id);
            });
    }
    for entity in entities.iter() {
        cx.window_invalidators_by_entity
            .entry(*entity)
            .or_default()
            .insert(window_handle.id, invalidator.clone());
        cx.current_window_by_entity
            .insert(*entity, window_handle.id);
    }
    tracked_entities.clear();
    tracked_entities.extend(entities.iter().copied());
    cx.tracked_entities
        .insert(window_handle.id, tracked_entities);
}

/// Two windows and `count` entities to draw in them, which the caller drops
/// before the app quits.
fn windows_and_entities(
    cx: &mut TestAppContext,
    count: usize,
) -> (
    [(AnyWindowHandle, WindowInvalidator); 2],
    Vec<Entity<()>>,
    Vec<EntityId>,
) {
    let mut window = || {
        let handle = cx.add_window(|_, _| Empty);
        cx.update_window(handle.into(), |_, window, _| {
            (window.window_handle(), window.invalidator.clone())
        })
        .unwrap()
    };
    let windows = [window(), window()];
    let entities: Vec<Entity<()>> = cx.update(|cx| (0..count).map(|_| cx.new(|_| ())).collect());
    let ids = entities.iter().map(Entity::entity_id).collect();
    (windows, entities, ids)
}

/// The entities whose bit is set in `mask`, of the first 64.
fn subset(entities: &[EntityId], mask: u64) -> FxHashSet<EntityId> {
    entities
        .iter()
        .enumerate()
        .filter(|(ix, _)| mask & (1 << ix) != 0)
        .map(|(_, id)| *id)
        .collect()
}

/// Recording draws that keep, add and drop entities leaves each entity's
/// invalidators, and each window's tracked set, as upstream's record does:
/// one window records through gpui-fast's, the other through upstream's, the
/// same sets in the same order.
#[test]
fn recording_draws_leaves_the_maps_as_upstream_does() {
    let mut cx = TestAppContext::single();
    let ([(fast, fast_invalidator), (upstream, upstream_invalidator)], _entities, entities) =
        windows_and_entities(&mut cx, 64);
    // A deterministic walk through sets that mostly overlap the last one.
    let mut mask = 0x0f0f_0f0f_0f0f_0f0fu64;
    cx.update(|cx| {
        for draw in 0..200 {
            mask ^= 1u64.rotate_left(draw * 7) | 1u64.rotate_left(draw * 13 + 5);
            if draw % 50 == 49 {
                mask = 0;
            }
            let drawn = subset(&entities, mask);
            crate::fast::tracked_entities::record_entities_accessed(
                cx,
                fast,
                fast_invalidator.clone(),
                &drawn,
            );
            upstream_record(cx, upstream, upstream_invalidator.clone(), &drawn);

            assert_eq!(cx.tracked_entities[&fast.id], drawn, "draw {draw}");
            for entity in &entities {
                let windows = cx.window_invalidators_by_entity.get(entity);
                let invalidator = |id| windows.and_then(|windows| windows.get(&id));
                assert_eq!(
                    invalidator(fast.id).is_some(),
                    invalidator(upstream.id).is_some(),
                    "draw {draw}"
                );
                assert_eq!(invalidator(fast.id).is_some(), drawn.contains(entity));
                if let Some(invalidator) = invalidator(fast.id) {
                    assert!(Rc::ptr_eq(&invalidator.inner, &fast_invalidator.inner));
                }
                if drawn.contains(entity) {
                    // The window recorded last is each drawn entity's window.
                    assert_eq!(cx.current_window_by_entity[entity], upstream.id);
                }
            }
        }
    });
}

/// A window keeps drawing the entities it drew: only those that entered or
/// left the set are touched, so notifying one that left no longer reaches
/// the window, and one that entered does.
#[test]
fn notifying_follows_the_entities_a_window_draws() {
    let mut cx = TestAppContext::single();
    let ([(window, invalidator), _], _entities, entities) = windows_and_entities(&mut cx, 3);
    cx.update(|cx| {
        let record = |cx: &mut App, drawn: &[EntityId]| {
            let drawn = drawn.iter().copied().collect();
            crate::fast::tracked_entities::record_entities_accessed(
                cx,
                window,
                invalidator.clone(),
                &drawn,
            );
        };
        let reaches_window = |cx: &mut App, entity: EntityId| {
            invalidator.inner.borrow_mut().dirty_views.clear();
            cx.notify(entity);
            invalidator.inner.borrow().dirty_views.contains(&entity)
        };
        record(cx, &entities[..2]);
        record(cx, &entities[1..]);
        assert!(!reaches_window(cx, entities[0]));
        assert!(reaches_window(cx, entities[1]));
        assert!(reaches_window(cx, entities[2]));
    });
}

/// What recording a draw costs when the window draws what it drew last
/// frame. Ignored by default; run it with
///
/// ```text
/// cargo test -p gpui --lib --release tracked_entities_bench -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn tracked_entities_bench() {
    const DRAWS: usize = 500;
    let mut cx = TestAppContext::single();
    let ([(fast, fast_invalidator), (upstream, upstream_invalidator)], _entities, entities) =
        windows_and_entities(&mut cx, 10_000);
    cx.update(|cx| {
        for count in [100, 1_000, 10_000] {
            let drawn: FxHashSet<EntityId> = entities[..count].iter().copied().collect();
            let time = |cx: &mut App, record: &dyn Fn(&mut App)| -> Duration {
                record(cx);
                let start = Instant::now();
                for _ in 0..DRAWS {
                    record(cx);
                    black_box(&cx.tracked_entities);
                }
                start.elapsed()
            };
            let fast_time = time(cx, &|cx| {
                crate::fast::tracked_entities::record_entities_accessed(
                    cx,
                    fast,
                    fast_invalidator.clone(),
                    &drawn,
                )
            });
            let upstream_time = time(cx, &|cx| {
                upstream_record(cx, upstream, upstream_invalidator.clone(), &drawn)
            });
            let per_entity = |time: Duration| time.as_nanos() as f64 / (DRAWS * count) as f64;
            println!(
                "tracked_entities {count:>6} entities, same each draw: upstream {:>6.1} ns/entity, \
                 gpui-fast {:>6.1} ns/entity",
                per_entity(upstream_time),
                per_entity(fast_time),
            );
        }
    });
}
