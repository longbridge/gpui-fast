//! Scene damage: where a finished scene can draw different pixels than the
//! scene the window drew before it, for renderers that redraw only that.
//!
//! See `docs/superpowers/specs/2026-10-08-damage-and-adaptive-cpu-design.md`.
//!
//! A pixel's colour is a function of the primitives covering it, taken in
//! drawing order: by `(order, kind)`, then by position in the kind's vector,
//! which [`Scene::finish`] sorted. So each kind's two vectors are compared on
//! their own, one draw order at a time: an order whose primitives are equal
//! in both scenes, byte for byte, damages nothing; otherwise its primitives
//! are matched by value (exact equality, a hash only finds candidates), and
//! the unmatched ones of either scene are damaged, as are matched ones whose
//! position relative to the other matched ones changed (the paint order of
//! the primitives of one `paint_layer`, which share an order). Every pair
//! kept is equal in value, order included, and the kept pairs are in the
//! same relative order in both scenes, so a pixel no damaged primitive
//! covers is covered by the same primitives, in the same order, in both.
//!
//! What lies outside the primitives — the atlas texels a sprite samples, the
//! surfaces' video frames — is not in the scene: surfaces are always damaged,
//! scroll layer tiles by their layer's `dirty_tiles`, and the renderer adds
//! the sprites over atlas tiles it wrote.

use crate::{
    Bounds, DevicePixels, MonochromeSprite, Path, PolychromeSprite, Quad, ScaledPixels, Scene,
    Shadow, Size, SubpixelSprite, Underline, Window,
    fast::layers::scene::{LayerKey, decode_layer_tile},
    point,
    scene::{PathVertex, TransformationMatrix},
    size,
};
use std::time::Duration;

/// Where a finished scene can draw different pixels than an earlier scene of
/// its window, in device pixels.
#[derive(Clone, Debug, Default)]
pub struct SceneDamage {
    /// This scene's number among its window's scenes, counting from 1.
    pub frame: u64,
    /// The number of the scene `rects` compare this one with. Zero when
    /// there is none: every pixel may differ.
    pub since: u64,
    /// Where this scene can draw different pixels than scene `since`. Empty
    /// when the two draw the same pixels. One rectangle over the whole window
    /// when `since` is zero.
    pub rects: Vec<Bounds<DevicePixels>>,
    /// Primitives of either scene not drawn the same in the other.
    pub changed_primitives: usize,
}

impl SceneDamage {
    /// Whether every pixel may differ, as nothing is known to compare with.
    pub fn is_full(&self) -> bool {
        self.since == 0
    }

    /// The pixels `rects` cover, counting overlaps once per rectangle.
    pub fn area(&self) -> i64 {
        self.rects
            .iter()
            .map(|rect| rect.size.width.0 as i64 * rect.size.height.0 as i64)
            .sum()
    }
}

/// Rectangles closer than this, in device pixels, are merged into one.
const MERGE_DISTANCE: i32 = 8;
/// The most rectangles a damage keeps: past it, the two whose union adds the
/// fewest pixels are merged.
const MAX_RECTS: usize = 16;

/// What a window keeps from one frame's damage to the next: the room the
/// diff works in, the window's size, and the statistics.
#[derive(Default)]
pub(crate) struct DamageState {
    scratch: Scratch,
    /// The window's size in device pixels when its last scene was finished.
    window_size: Size<DevicePixels>,
    stats: DamageStats,
    /// Whether `damage_time` is kept: once the stats have been reset.
    timed: bool,
}

/// What the damage of the frames so far added up to, for
/// [`crate::fast::stats::LayoutStats`].
#[derive(Clone, Copy, Default)]
pub(crate) struct DamageStats {
    pub(crate) frames: u64,
    pub(crate) full_frames: u64,
    pub(crate) damaged_pixels: u64,
    pub(crate) window_pixels: u64,
    pub(crate) small_frames: u64,
    pub(crate) changed_primitives: u64,
    pub(crate) time: Duration,
}

impl DamageState {
    /// The damage the frames have added up to since [`Self::reset_stats`].
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn stats(&self) -> DamageStats {
        self.stats
    }

    /// Zeroes the statistics, and keeps the time from then on.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn reset_stats(&mut self) {
        self.stats = DamageStats::default();
        self.timed = true;
    }
}

/// Room the diff works in, kept from one frame to the next.
#[derive(Default)]
struct Scratch {
    rects: Vec<Rect>,
    /// Hashes of a group's primitives in the old scene.
    hashes: Vec<u64>,
    /// Those primitives by hash, see [`match_group`].
    table: Vec<u32>,
    /// Whether each primitive of a group in the old scene found its match.
    used: Vec<bool>,
    /// For each primitive of a group in the new scene, the index of its
    /// match in the old one, or `NONE`.
    matches: Vec<u32>,
    /// Longest increasing run of `matches`: the tails and predecessors.
    tails: Vec<u32>,
    predecessors: Vec<u32>,
    /// Whether each primitive of a group in the new scene keeps its place.
    kept: Vec<bool>,
    /// The pixels each primitive of a group in the new scene covers.
    pixels: Vec<Rect>,
    /// The scroll layers whose tiles changed: the key, and whether all its
    /// tiles count as changed, or only its `dirty_tiles`, which then follow.
    layers: Vec<(LayerKey, bool, usize)>,
}

const NONE: u32 = u32::MAX;

/// Fills the damage of the frame just finished (`window.next_frame`) against
/// the frame drawn before it (`window.rendered_frame`).
pub(crate) fn finish_frame(window: &mut Window) {
    let scale = window.scale_factor();
    let viewport = window.viewport_size;
    let window_size = size(
        DevicePixels((viewport.width.0 * scale).ceil() as i32),
        DevicePixels((viewport.height.0 * scale).ceil() as i32),
    );
    diff_scenes(
        &window.rendered_frame.scene,
        &mut window.next_frame.scene,
        window_size,
        &mut window.fast_layout.damage,
    );
}

/// Fills `next.damage` against `prev`, the scene drawn before it in a window
/// `window_size` device pixels large.
pub(crate) fn diff_scenes(
    prev: &Scene,
    next: &mut Scene,
    window_size: Size<DevicePixels>,
    state: &mut DamageState,
) {
    let started_at = state.timed.then(std::time::Instant::now);
    let window = Rect {
        x0: 0,
        y0: 0,
        x1: window_size.width.0.max(0),
        y1: window_size.height.0.max(0),
    };
    let window_area = window.area();
    let resized = window_size != state.window_size;
    state.window_size = window_size;

    let frame = prev.damage.frame + 1;
    let since = if prev.damage.frame == 0 || resized {
        0
    } else {
        prev.damage.frame
    };
    let mut cx = Diff {
        rects: Rects {
            rects: std::mem::take(&mut state.scratch.rects),
            area: 0,
        },
        window,
        full_area: window_area / 2,
        full: false,
        changed: 0,
        scratch: &mut state.scratch,
    };
    cx.rects.rects.clear();
    if since == 0 {
        cx.changed = primitive_count(next);
        cx.full = true;
    } else {
        diff_primitives(prev, next, &mut cx);
    }
    let full = cx.full || cx.rects.area > cx.full_area;
    let changed = cx.changed;
    let rects = cx.rects;

    let damage = &mut next.damage;
    damage.frame = frame;
    damage.since = since;
    damage.changed_primitives = changed;
    damage.rects.clear();
    if full {
        if !window.is_empty() {
            damage.rects.push(window.bounds());
        }
    } else {
        damage
            .rects
            .extend(rects.rects.iter().map(|rect| rect.bounds()));
    }
    state.scratch.rects = rects.rects;

    let area = damage.area() as u64;
    let stats = &mut state.stats;
    stats.frames += 1;
    stats.full_frames += full as u64;
    stats.damaged_pixels += area;
    stats.window_pixels += window_area as u64;
    stats.small_frames += (area * 16 <= window_area as u64) as u64;
    stats.changed_primitives += changed as u64;
    if let Some(started_at) = started_at {
        stats.time += started_at.elapsed();
    }
}

fn primitive_count(scene: &Scene) -> usize {
    scene.shadows.len()
        + scene.quads.len()
        + scene.paths.len()
        + scene.underlines.len()
        + scene.monochrome_sprites.len()
        + scene.subpixel_sprites.len()
        + scene.polychrome_sprites.len()
        + scene.surfaces.len()
}

/// The diff of two scenes under way.
struct Diff<'a> {
    rects: Rects,
    window: Rect,
    /// Damage covering more than this many pixels covers the whole window.
    full_area: i64,
    /// Whether the damage already covers the whole window, so that the rest
    /// of the scene need not be compared. `changed` stops counting then.
    full: bool,
    changed: usize,
    scratch: &'a mut Scratch,
}

impl Diff<'_> {
    fn damage(&mut self, rect: Rect) {
        let rect = rect.intersect(&self.window);
        if rect.is_empty() {
            return;
        }
        self.rects.add(rect);
        if self.rects.area > self.full_area {
            self.full = true;
        }
    }

    fn damage_primitive<T: Drawn>(&mut self, primitive: &T) {
        self.damage(primitive.pixels());
    }

    /// Counts `primitive` as changed and damages it, until the damage covers
    /// the whole window.
    fn damage_changed<T: Drawn>(&mut self, primitive: &T) {
        if self.full {
            return;
        }
        self.changed += 1;
        self.damage_primitive(primitive);
    }
}

fn diff_primitives(prev: &Scene, next: &Scene, cx: &mut Diff) {
    diff_kind(&prev.shadows, &next.shadows, cx);
    diff_kind(&prev.quads, &next.quads, cx);
    diff_kind(&prev.paths, &next.paths, cx);
    damage_regrouped_paths(prev, next, cx);
    diff_kind(&prev.underlines, &next.underlines, cx);
    diff_kind(&prev.monochrome_sprites, &next.monochrome_sprites, cx);
    diff_kind(&prev.subpixel_sprites, &next.subpixel_sprites, cx);
    diff_kind(&prev.polychrome_sprites, &next.polychrome_sprites, cx);
    damage_dirty_tiles(prev, next, cx);
    for surface in prev.surfaces.iter().chain(&next.surfaces) {
        if cx.full {
            return;
        }
        cx.changed += 1;
        cx.damage(Rect::clipped(
            surface.bounds,
            &surface.content_mask.bounds,
            0,
        ));
    }
}

/// Damages the paths whose batch changed. The renderer rasterizes a batch's
/// paths together and composites each path from what the batch rasterized,
/// so where two paths of a batch overlap, each is blended over the other:
/// splitting or joining a batch (a primitive of another kind drawn between
/// two paths' orders, or no longer) changes those pixels with no path
/// changing. A batch with no equal batch in the other scene has all its paths
/// damaged.
fn damage_regrouped_paths(prev: &Scene, next: &Scene, cx: &mut Diff) {
    /// Past this many batches in a scene, matching them costs more than the
    /// paths' damage saves: every path is damaged.
    const MAX_BATCHES: usize = 64;
    if prev.paths.is_empty() || next.paths.is_empty() || cx.full {
        return;
    }
    let prev_batches = path_batches(prev);
    let next_batches = path_batches(next);
    if prev_batches == next_batches && Path::same_all(&prev.paths, &next.paths) {
        return;
    }
    if prev_batches.len() > MAX_BATCHES || next_batches.len() > MAX_BATCHES {
        for path in prev.paths.iter().chain(&next.paths) {
            cx.damage_changed(path);
        }
        return;
    }
    let mut matched = vec![false; prev_batches.len()];
    for batch in &next_batches {
        let paths = &next.paths[batch.clone()];
        let found = prev_batches.iter().enumerate().find(|(index, before)| {
            !matched[*index] && Path::same_all(&prev.paths[(*before).clone()], paths)
        });
        match found {
            Some((index, _)) => matched[index] = true,
            None if batch.len() > 1 => paths.iter().for_each(|path| cx.damage_changed(path)),
            None => {}
        }
    }
    for (batch, matched) in prev_batches.iter().zip(matched) {
        if !matched && batch.len() > 1 {
            prev.paths[batch.clone()]
                .iter()
                .for_each(|path| cx.damage_changed(path));
        }
    }
}

/// The ranges of `scene.paths` the renderer draws as one batch each: paths
/// with no primitive of another kind drawn between them (`Scene::batches`).
fn path_batches(scene: &Scene) -> Vec<std::ops::Range<usize>> {
    /// Whether a primitive of another kind is drawn after a path of order
    /// `after` and before a path of order `before`. Kinds before paths
    /// (shadows, quads) are drawn after a path of a lower order; the others
    /// after a path of the same order.
    fn between(scene: &Scene, after: u32, before: u32) -> bool {
        fn any<T>(items: &[T], order: impl Fn(&T) -> u32, from: u32, until: u32) -> bool {
            // `from..until`, in orders; the vectors are sorted by order.
            let start = items.partition_point(|item| order(item) < from);
            items.get(start).is_some_and(|item| order(item) < until)
        }
        let lower = after.saturating_add(1);
        let upper = before.saturating_add(1);
        any(&scene.shadows, |p| p.order, lower, upper)
            || any(&scene.quads, |p| p.order, lower, upper)
            || any(&scene.underlines, |p| p.order, after, before)
            || any(&scene.monochrome_sprites, |p| p.order, after, before)
            || any(&scene.subpixel_sprites, |p| p.order, after, before)
            || any(&scene.polychrome_sprites, |p| p.order, after, before)
            || any(&scene.surfaces, |p| p.order, after, before)
    }
    let mut batches = Vec::new();
    let mut start = 0;
    for index in 1..scene.paths.len() {
        if between(
            scene,
            scene.paths[index - 1].order,
            scene.paths[index].order,
        ) {
            batches.push(start..index);
            start = index;
        }
    }
    batches.push(start..scene.paths.len());
    batches
}

/// Damages the scroll layer tiles `next` composites whose content changed
/// since `prev`: the tiles a layer lists as dirty in a generation `prev` did
/// not have, or every tile of a layer `prev` did not composite or composited
/// more than one generation ago.
fn damage_dirty_tiles(prev: &Scene, next: &Scene, cx: &mut Diff) {
    let frames = &next.layers.frames;
    if frames.is_empty() || cx.full {
        return;
    }
    cx.scratch.layers.clear();
    for (index, frame) in frames.iter().enumerate() {
        let before = prev
            .layers
            .frames
            .iter()
            .find(|before| before.key == frame.key)
            .map(|before| before.generation);
        match before {
            Some(generation) if generation == frame.generation => {}
            Some(generation) if generation + 1 == frame.generation => {
                if !frame.dirty_tiles.is_empty() {
                    cx.scratch.layers.push((frame.key, false, index));
                }
            }
            _ => cx.scratch.layers.push((frame.key, true, index)),
        }
    }
    if cx.scratch.layers.is_empty() {
        return;
    }
    for sprite in &next.polychrome_sprites {
        let Some((key, tile)) = decode_layer_tile(sprite.tile.texture_id, sprite.tile.tile_id)
        else {
            continue;
        };
        let Some(&(_, all, index)) = cx.scratch.layers.iter().find(|layer| layer.0 == key) else {
            continue;
        };
        if all || frames[index].dirty_tiles.contains(&tile) {
            if cx.full {
                return;
            }
            cx.changed += 1;
            cx.damage_primitive(sprite);
        }
    }
}

/// Compares one kind's primitives, both in drawing order, damaging those
/// that are not drawn the same in both.
fn diff_kind<T: Drawn>(prev: &[T], next: &[T], cx: &mut Diff) {
    if cx.full || T::same_all(prev, next) {
        return;
    }
    let (prev, next) = trim(prev, next);
    let (mut i, mut j) = (0, 0);
    while i < prev.len() || j < next.len() {
        if cx.full {
            return;
        }
        let prev_order = prev.get(i).map_or(u64::MAX, |p| p.order() as u64);
        let next_order = next.get(j).map_or(u64::MAX, |n| n.order() as u64);
        let order = prev_order.min(next_order);
        let prev_end = group_end(prev, i, order);
        let next_end = group_end(next, j, order);
        let (prev_group, next_group) = (&prev[i..prev_end], &next[j..next_end]);
        if !T::same_all(prev_group, next_group) {
            diff_group(prev_group, next_group, cx);
        }
        i = prev_end;
        j = next_end;
    }
}

/// The end of the run of primitives of `order` starting at `start`.
fn group_end<T: Drawn>(items: &[T], start: usize, order: u64) -> usize {
    let mut end = start;
    while end < items.len() && items[end].order() as u64 == order {
        end += 1;
    }
    end
}

/// The two slices without the primitives they begin and end with alike.
fn trim<'a, T: Drawn>(prev: &'a [T], next: &'a [T]) -> (&'a [T], &'a [T]) {
    let mut start = 0;
    while start < prev.len() && start < next.len() && prev[start].same(&next[start]) {
        start += 1;
    }
    let (prev, next) = (&prev[start..], &next[start..]);
    let mut end = 0;
    while end < prev.len()
        && end < next.len()
        && prev[prev.len() - 1 - end].same(&next[next.len() - 1 - end])
    {
        end += 1;
    }
    (&prev[..prev.len() - end], &next[..next.len() - end])
}

/// How far [`diff_group`] looks ahead in either scene for the two to agree
/// again after primitives that differ.
const LOOKAHEAD: usize = 8;

/// Compares the primitives of one draw order, which differ. A few are
/// matched by value right away ([`match_group`]). Otherwise walks the two
/// side by side while they agree; past primitives that differ, looks a few
/// ahead for the two to agree again, damaging what it skipped; failing that,
/// matches the rest by value ([`match_group`]). Pairs taken side by side
/// stay in order, as the diff needs.
fn diff_group<T: Drawn>(prev: &[T], next: &[T], cx: &mut Diff) {
    if prev.len() * next.len() <= 64 {
        match_group(prev, next, cx);
        return;
    }
    let (mut i, mut j) = (0, 0);
    while i < prev.len() && j < next.len() {
        if prev[i].same(&next[j]) {
            i += 1;
            j += 1;
            continue;
        }
        let Some((skip_prev, skip_next)) = resync(&prev[i..], &next[j..]) else {
            match_group(&prev[i..], &next[j..], cx);
            return;
        };
        for primitive in prev[i..i + skip_prev].iter().chain(&next[j..j + skip_next]) {
            if cx.full {
                return;
            }
            cx.changed += 1;
            cx.damage_primitive(primitive);
        }
        i += skip_prev;
        j += skip_next;
    }
    for primitive in prev[i..].iter().chain(&next[j..]) {
        if cx.full {
            return;
        }
        cx.changed += 1;
        cx.damage_primitive(primitive);
    }
}

/// The fewest primitives to skip in `prev` and `next`, at most `LOOKAHEAD`
/// in each, for the two to agree again; their first primitives differ.
fn resync<T: Drawn>(prev: &[T], next: &[T]) -> Option<(usize, usize)> {
    for distance in 1..=2 * LOOKAHEAD {
        for skip_prev in distance.saturating_sub(LOOKAHEAD)..=distance.min(LOOKAHEAD) {
            let skip_next = distance - skip_prev;
            if let (Some(a), Some(b)) = (prev.get(skip_prev), next.get(skip_next))
                && a.same(b)
            {
                return Some((skip_prev, skip_next));
            }
        }
    }
    None
}

/// Matches the primitives of one draw order by value, and damages the
/// unmatched ones and the matched ones whose relative position changed.
fn match_group<T: Drawn>(prev: &[T], next: &[T], cx: &mut Diff) {
    let (prev, next) = trim(prev, next);
    if prev.is_empty() || next.is_empty() {
        for primitive in prev.iter().chain(next) {
            if cx.full {
                return;
            }
            cx.changed += 1;
            cx.damage_primitive(primitive);
        }
        return;
    }

    let scratch = &mut *cx.scratch;
    scratch.used.clear();
    scratch.used.resize(prev.len(), false);
    scratch.matches.clear();
    if prev.len() * next.len() <= 64 {
        for primitive in next {
            let found = prev
                .iter()
                .enumerate()
                .position(|(index, candidate)| !scratch.used[index] && candidate.same(primitive));
            scratch.matches.push(match found {
                Some(index) => {
                    scratch.used[index] = true;
                    index as u32
                }
                None => NONE,
            });
        }
    } else {
        // An open-addressing table of the old primitives by hash: slots hold
        // an index plus one, zero when empty.
        let slots = (prev.len() * 2).next_power_of_two();
        let mask = slots as u64 - 1;
        scratch.table.clear();
        scratch.table.resize(slots, 0);
        scratch.hashes.clear();
        for (index, primitive) in prev.iter().enumerate() {
            let hash = primitive.hash();
            scratch.hashes.push(hash);
            let mut slot = hash & mask;
            while scratch.table[slot as usize] != 0 {
                slot = (slot + 1) & mask;
            }
            scratch.table[slot as usize] = index as u32 + 1;
        }
        for primitive in next {
            let hash = primitive.hash();
            let mut slot = hash & mask;
            let mut found = NONE;
            loop {
                let entry = scratch.table[slot as usize];
                if entry == 0 {
                    break;
                }
                let index = (entry - 1) as usize;
                if scratch.hashes[index] == hash
                    && !scratch.used[index]
                    && prev[index].same(primitive)
                {
                    scratch.used[index] = true;
                    found = index as u32;
                    break;
                }
                slot = (slot + 1) & mask;
            }
            scratch.matches.push(found);
        }
    }
    keep_longest_increasing(scratch);
    keep_moved_apart(next, scratch);

    for index in 0..prev.len() {
        if !cx.scratch.used[index] {
            if cx.full {
                return;
            }
            cx.changed += 1;
            cx.damage_primitive(&prev[index]);
        }
    }
    for (index, primitive) in next.iter().enumerate() {
        if cx.scratch.kept[index] {
            continue;
        }
        if cx.full {
            return;
        }
        // A primitive that moved among the others is damaged where it is
        // drawn, which its match in `prev`, equal to it, shares.
        cx.changed += if cx.scratch.matches[index] == NONE {
            1
        } else {
            2
        };
        cx.damage_primitive(primitive);
    }
}

/// Marks in `scratch.kept` the matched primitives of the new scene that keep
/// their place: a longest run of them whose matches are in increasing order.
fn keep_longest_increasing(scratch: &mut Scratch) {
    let matches = &scratch.matches;
    scratch.kept.clear();
    scratch.kept.resize(matches.len(), false);
    scratch.tails.clear();
    scratch.predecessors.clear();
    scratch.predecessors.resize(matches.len(), NONE);
    for (index, &matched) in matches.iter().enumerate() {
        if matched == NONE {
            continue;
        }
        let length = scratch
            .tails
            .partition_point(|&tail| matches[tail as usize] < matched);
        if length > 0 {
            scratch.predecessors[index] = scratch.tails[length - 1];
        }
        if length == scratch.tails.len() {
            scratch.tails.push(index as u32);
        } else {
            scratch.tails[length] = index as u32;
        }
    }
    let mut at = scratch.tails.last().copied().unwrap_or(NONE);
    while at != NONE {
        scratch.kept[at as usize] = true;
        at = scratch.predecessors[at as usize];
    }
}

/// Marks as kept the matched primitives that moved among the others but
/// overlap none they changed places with: only primitives that cover a pixel
/// together need to keep their order. Skipped when the moved primitives are
/// too many to compare with the kept ones.
fn keep_moved_apart<T: Drawn>(next: &[T], scratch: &mut Scratch) {
    let matches = &scratch.matches;
    let kept = &mut scratch.kept;
    let moved = kept
        .iter()
        .zip(matches)
        .filter(|&(&kept, &matched)| !kept && matched != NONE)
        .count();
    if moved == 0 || moved * next.len() > 1 << 16 {
        return;
    }
    scratch.pixels.clear();
    scratch
        .pixels
        .extend(next.iter().map(|primitive| primitive.pixels()));
    let pixels = &scratch.pixels;
    for candidate in 0..next.len() {
        let matched = matches[candidate];
        if kept[candidate] || matched == NONE {
            continue;
        }
        let rect = pixels[candidate];
        let crosses = (0..next.len()).any(|other| {
            kept[other]
                && (candidate < other) != (matched < matches[other])
                && rect.overlaps(&pixels[other])
        });
        if !crosses {
            kept[candidate] = true;
        }
    }
}

/// A rectangle of device pixels, `x0..x1` by `y0..y1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl Rect {
    /// The whole pixels `bounds` clipped by `mask` touches, `pad` more on
    /// every side.
    #[inline]
    fn clipped(bounds: Bounds<ScaledPixels>, mask: &Bounds<ScaledPixels>, pad: i32) -> Rect {
        let max = |a: f32, b: f32| if a > b { a } else { b };
        let min = |a: f32, b: f32| if a < b { a } else { b };
        let x0 = max(bounds.origin.x.0, mask.origin.x.0);
        let y0 = max(bounds.origin.y.0, mask.origin.y.0);
        let x1 = min(
            bounds.origin.x.0 + bounds.size.width.0,
            mask.origin.x.0 + mask.size.width.0,
        );
        let y1 = min(
            bounds.origin.y.0 + bounds.size.height.0,
            mask.origin.y.0 + mask.size.height.0,
        );
        // Not `x1 <= x0`, so that NaN draws nothing too.
        if !(x1 > x0 && y1 > y0) {
            return Rect {
                x0: 0,
                y0: 0,
                x1: 0,
                y1: 0,
            };
        }
        // Rounded with casts rather than `floor` and `ceil`, which are calls
        // without SSE 4.1. Values are kept well inside `i32`, where `as`
        // truncates exactly.
        const LIMIT: f32 = (1 << 30) as f32;
        let floor = |value: f32| {
            let value = value.clamp(-LIMIT, LIMIT);
            let truncated = value as i32;
            truncated - ((truncated as f32) > value) as i32
        };
        let ceil = |value: f32| {
            let value = value.clamp(-LIMIT, LIMIT);
            let truncated = value as i32;
            truncated + ((truncated as f32) < value) as i32
        };
        Rect {
            x0: floor(x0) - pad,
            y0: floor(y0) - pad,
            x1: ceil(x1) + pad,
            y1: ceil(y1) + pad,
        }
    }

    fn is_empty(&self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }

    fn area(&self) -> i64 {
        if self.is_empty() {
            0
        } else {
            (self.x1 - self.x0) as i64 * (self.y1 - self.y0) as i64
        }
    }

    fn intersect(&self, other: &Rect) -> Rect {
        Rect {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
    }

    fn union(&self, other: &Rect) -> Rect {
        Rect {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    fn contains(&self, other: &Rect) -> bool {
        self.x0 <= other.x0 && self.y0 <= other.y0 && other.x1 <= self.x1 && other.y1 <= self.y1
    }

    fn overlaps(&self, other: &Rect) -> bool {
        self.x0 < other.x1 && other.x0 < self.x1 && self.y0 < other.y1 && other.y0 < self.y1
    }

    /// Whether the two are at most `MERGE_DISTANCE` apart on both axes.
    fn near(&self, other: &Rect) -> bool {
        self.x0 <= other.x1 + MERGE_DISTANCE
            && other.x0 <= self.x1 + MERGE_DISTANCE
            && self.y0 <= other.y1 + MERGE_DISTANCE
            && other.y0 <= self.y1 + MERGE_DISTANCE
    }

    fn bounds(&self) -> Bounds<DevicePixels> {
        Bounds {
            origin: point(DevicePixels(self.x0), DevicePixels(self.y0)),
            size: size(
                DevicePixels(self.x1 - self.x0),
                DevicePixels(self.y1 - self.y0),
            ),
        }
    }
}

/// Damaged rectangles, merged as they are added. Rectangles kept apart are
/// more than `MERGE_DISTANCE` apart on some axis, so they do not overlap.
struct Rects {
    rects: Vec<Rect>,
    /// The pixels the rectangles cover.
    area: i64,
}

impl Rects {
    fn add(&mut self, mut rect: Rect) {
        // The rectangle grown last is the likeliest to hold the next.
        if self.rects.last().is_some_and(|last| last.contains(&rect))
            || self.rects.iter().any(|kept| kept.contains(&rect))
        {
            return;
        }
        loop {
            while let Some(index) = self.rects.iter().position(|kept| kept.near(&rect)) {
                let kept = self.rects.swap_remove(index);
                rect = rect.union(&kept);
            }
            if self.rects.len() < MAX_RECTS {
                break;
            }
            // Too many: the two rectangles whose union adds the fewest pixels
            // become one, so that changes spread over the window stay apart
            // instead of becoming one rectangle over all of them.
            self.rects.push(rect);
            let mut cheapest = (0, 1, i64::MAX);
            for i in 0..self.rects.len() {
                for j in i + 1..self.rects.len() {
                    let (a, b) = (&self.rects[i], &self.rects[j]);
                    let cost = a.union(b).area() - a.area() - b.area();
                    if cost < cheapest.2 {
                        cheapest = (i, j, cost);
                    }
                }
            }
            let b = self.rects.swap_remove(cheapest.1);
            let a = self.rects.swap_remove(cheapest.0);
            rect = a.union(&b);
        }
        self.rects.push(rect);
        self.area = self.rects.iter().map(Rect::area).sum();
    }
}

/// A kind of primitive, as the diff compares it.
trait Drawn {
    fn order(&self) -> u32;
    /// Whether the two draw the same, in the same place.
    fn same(&self, other: &Self) -> bool;
    /// Whether the two runs draw the same.
    fn same_all(a: &[Self], b: &[Self]) -> bool
    where
        Self: Sized;
    /// A hash agreeing with [`Self::same`].
    fn hash(&self) -> u64;
    /// The device pixels the primitive can draw into: its rasterized
    /// geometry clipped by its content mask.
    fn pixels(&self) -> Rect;
}

/// Primitives made of 4-byte fields only, with no padding, so their bytes
/// are all initialized and equal bytes mean an equal primitive.
unsafe trait Pod {}

// The sizes are the sums of the fields' sizes: no padding.
const _: () = assert!(size_of::<crate::Background>() == 72);
const _: () = assert!(size_of::<Quad>() == 4 + 4 + 16 + 16 + 72 + 16 + 16 + 16);
const _: () = assert!(size_of::<Shadow>() == 4 + 4 + 16 + 16 + 16 + 16 + 16 + 16 + 4 + 4);
const _: () = assert!(size_of::<Underline>() == 4 + 4 + 16 + 16 + 16 + 4 + 4);
const _: () = assert!(size_of::<crate::AtlasTile>() == 32);
const _: () = assert!(size_of::<TransformationMatrix>() == 24);
const _: () = assert!(size_of::<MonochromeSprite>() == 4 + 4 + 16 + 16 + 16 + 32 + 24);
const _: () = assert!(size_of::<SubpixelSprite>() == 4 + 4 + 16 + 16 + 16 + 32 + 24);
const _: () = assert!(size_of::<PolychromeSprite>() == 4 + 4 + 4 + 4 + 16 + 16 + 16 + 32);
const _: () = assert!(size_of::<PathVertex<ScaledPixels>>() == 8 + 8 + 16);

unsafe impl Pod for Quad {}
unsafe impl Pod for Shadow {}
unsafe impl Pod for Underline {}
unsafe impl Pod for MonochromeSprite {}
unsafe impl Pod for SubpixelSprite {}
unsafe impl Pod for PolychromeSprite {}
unsafe impl Pod for crate::Background {}
unsafe impl Pod for PathVertex<ScaledPixels> {}
unsafe impl Pod for Bounds<ScaledPixels> {}
unsafe impl Pod for crate::ContentMask<ScaledPixels> {}

fn bytes<T: Pod>(items: &[T]) -> &[u8] {
    // SAFETY: `Pod` types have no padding, so every byte is initialized.
    unsafe { std::slice::from_raw_parts(items.as_ptr().cast::<u8>(), size_of_val(items)) }
}

/// FxHash over 4-byte words.
fn hash_words(bytes: &[u8], mut hash: u64) -> u64 {
    for word in bytes.chunks_exact(4) {
        let word = u32::from_ne_bytes([word[0], word[1], word[2], word[3]]);
        hash = (hash.rotate_left(5) ^ word as u64).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
    hash
}

macro_rules! pod_drawn {
    ($type:ty, $self:ident => $extent:expr) => {
        impl Drawn for $type {
            #[inline]
            fn order(&self) -> u32 {
                self.order
            }
            #[inline]
            fn same(&self, other: &Self) -> bool {
                bytes(std::slice::from_ref(self)) == bytes(std::slice::from_ref(other))
            }
            #[inline]
            fn same_all(a: &[Self], b: &[Self]) -> bool {
                bytes(a) == bytes(b)
            }

            /// Six words after the order, which hold where the primitive
            /// is: enough to tell most primitives apart.
            fn hash(&self) -> u64 {
                let bytes = bytes(std::slice::from_ref(self));
                hash_words(&bytes[4..28], 0)
            }
            fn pixels(&$self) -> Rect {
                Rect::clipped($extent, &$self.content_mask.bounds, 0)
            }
        }
    };
}

pod_drawn!(Quad, self => self.bounds);
pod_drawn!(Underline, self => self.bounds);
pod_drawn!(PolychromeSprite, self => self.bounds);
pod_drawn!(MonochromeSprite, self => transformed(self.bounds, &self.transformation));
pod_drawn!(SubpixelSprite, self => transformed(self.bounds, &self.transformation));
pod_drawn!(Shadow, self => shadow_extent(self));

/// What `vs_shadow` rasterizes: an inset shadow its element's bounds, a drop
/// shadow its bounds widened by three blur radii.
fn shadow_extent(shadow: &Shadow) -> Bounds<ScaledPixels> {
    if shadow.inset != 0 {
        return shadow.element_bounds;
    }
    let margin = 3. * shadow.blur_radius.0.max(0.);
    Bounds {
        origin: point(
            ScaledPixels(shadow.bounds.origin.x.0 - margin),
            ScaledPixels(shadow.bounds.origin.y.0 - margin),
        ),
        size: size(
            ScaledPixels(shadow.bounds.size.width.0 + 2. * margin),
            ScaledPixels(shadow.bounds.size.height.0 + 2. * margin),
        ),
    }
}

/// The smallest rectangle holding `bounds` transformed as
/// `to_device_position_transformed` does: `R·p + t`, `R` stored by rows.
fn transformed(
    bounds: Bounds<ScaledPixels>,
    matrix: &TransformationMatrix,
) -> Bounds<ScaledPixels> {
    if *matrix == TransformationMatrix::unit() {
        return bounds;
    }
    let m = &matrix.rotation_scale;
    let apply = |x: f32, y: f32| {
        (
            m[0][0] * x + m[0][1] * y + matrix.translation[0],
            m[1][0] * x + m[1][1] * y + matrix.translation[1],
        )
    };
    let (x0, y0) = (bounds.origin.x.0, bounds.origin.y.0);
    let (x1, y1) = (x0 + bounds.size.width.0, y0 + bounds.size.height.0);
    let corners = [apply(x0, y0), apply(x1, y0), apply(x0, y1), apply(x1, y1)];
    let (mut min_x, mut min_y) = (f32::INFINITY, f32::INFINITY);
    let (mut max_x, mut max_y) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
    for (x, y) in corners {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    Bounds {
        origin: point(ScaledPixels(min_x), ScaledPixels(min_y)),
        size: size(ScaledPixels(max_x - min_x), ScaledPixels(max_y - min_y)),
    }
}

impl Drawn for Path<ScaledPixels> {
    fn order(&self) -> u32 {
        self.order
    }

    /// Everything the renderer draws a path from, its `id` (its index in the
    /// scene) aside.
    fn same(&self, other: &Self) -> bool {
        self.order == other.order
            && bytes(std::slice::from_ref(&self.bounds))
                == bytes(std::slice::from_ref(&other.bounds))
            && bytes(std::slice::from_ref(&self.content_mask))
                == bytes(std::slice::from_ref(&other.content_mask))
            && bytes(std::slice::from_ref(&self.color)) == bytes(std::slice::from_ref(&other.color))
            && bytes(&self.vertices) == bytes(&other.vertices)
    }

    fn same_all(a: &[Self], b: &[Self]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.same(b))
    }

    fn hash(&self) -> u64 {
        let hash = hash_words(bytes(std::slice::from_ref(&self.color)), self.order as u64);
        let hash = hash_words(bytes(&self.vertices), hash);
        let hash = hash_words(bytes(std::slice::from_ref(&self.bounds)), hash);
        hash_words(bytes(std::slice::from_ref(&self.content_mask)), hash)
    }

    /// A pixel more on every side: the paths' intermediate texture is
    /// multisampled.
    fn pixels(&self) -> Rect {
        Rect::clipped(self.bounds, &self.content_mask.bounds, 1)
    }
}
