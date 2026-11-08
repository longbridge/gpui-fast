//! What a frame's tab stops cost to record and to query. Ignored by default;
//! run it with
//!
//! ```text
//! cargo test -p gpui --lib --release tab_stops -- --ignored --nocapture
//! ```

use std::{
    hint::black_box,
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::RwLock;

use crate::{FocusHandle, FocusMap, TabStopMap, TabStopOperation};

/// Focusable elements in a frame: rows of a form or a list, a few groups deep.
fn handles(count: usize) -> Vec<FocusHandle> {
    let focus_map = Arc::new(RwLock::new(FocusMap::default()));
    (0..count)
        .map(|ix| {
            FocusHandle::new(&focus_map)
                .tab_stop(ix % 4 != 3)
                .tab_index((ix % 7) as isize)
        })
        .collect()
}

/// Paints `handles` into `map` the way a frame does: groups of ten, each
/// inside a section group.
fn paint(map: &mut TabStopMap, handles: &[FocusHandle]) {
    for (section, chunk) in handles.chunks(50).enumerate() {
        map.begin_group(section as isize);
        for (group, rows) in chunk.chunks(10).enumerate() {
            map.begin_group(group as isize);
            for handle in rows {
                map.insert(handle);
            }
            map.end_group();
        }
        map.end_group();
    }
}

fn per_op(total: Duration, frames: usize, ops: usize) -> f64 {
    total.as_nanos() as f64 / (frames * ops) as f64
}

#[test]
#[ignore]
fn tab_stops() {
    const FRAMES: usize = 200;
    for count in [100, 1_000, 10_000] {
        let handles = handles(count);
        let mut painted = TabStopMap::default();
        paint(&mut painted, &handles);
        let history: Vec<TabStopOperation> = painted.insertion_history.clone();
        let ops = history.len();

        // A frame painting every element.
        let mut map = TabStopMap::default();
        let start = Instant::now();
        for _ in 0..FRAMES {
            map.clear();
            paint(&mut map, &handles);
            black_box(&map);
        }
        let insert = start.elapsed();

        // A frame reusing last frame's views, which replays their insertions.
        let start = Instant::now();
        for _ in 0..FRAMES {
            map.clear();
            map.replay(&history);
            black_box(&map);
        }
        let replay = start.elapsed();

        // A frame followed by one Tab press.
        let start = Instant::now();
        for frame in 0..FRAMES {
            map.clear();
            map.replay(&history);
            black_box(map.next(Some(&handles[frame % count].id)));
        }
        let replay_and_tab = start.elapsed();

        println!(
            "tab_stops {count:>6} handles ({ops} ops): paint {:>7.1} ns/op, replay {:>7.1} ns/op, \
             replay + one Tab {:>9.1} us/frame",
            per_op(insert, FRAMES, ops),
            per_op(replay, FRAMES, ops),
            replay_and_tab.as_secs_f64() * 1e6 / FRAMES as f64,
        );
    }
}
