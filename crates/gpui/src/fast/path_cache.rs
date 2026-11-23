//! Paths kept from one frame to the next.
//!
//! Upstream builds a path's outline (stroking it with tiny-skia) and bins its
//! curves into tiles every time it is painted, which for a sparkline in every
//! row of a table, a chart or a window-sized ground with holes is most of
//! what painting them costs, though most are the same shape as the frame
//! before.
//!
//! [`build`] keeps a built [`Path`] keyed by the builder's events and style,
//! so the same shape built again shares its curves; [`bin`] keeps the tiles
//! a path's curves were binned into at a scale, keyed by those shared curves,
//! so a path painted again unchanged is not binned again. Both give exactly
//! what building and binning again would.

use std::{cell::RefCell, hash::Hash, rc::Rc, sync::Arc};

use anyhow::Error;
use collections::FxHashMap;
use lyon_path::Event;

use crate::{Path, PathBuilder, PathCurve, PathStyle, PathTiles};

/// How many shapes a generation keeps before the older one is let go.
const KEPT: usize = 2048;

/// How many bytes of curves and tiles a generation keeps before the older
/// one is let go. A thread keeps at most two generations.
const KEPT_BYTES: usize = 8 * 1024 * 1024;

/// Two generations of entries: those used since the last turn, and those
/// used before it. A hit in the older moves the entry to the newer; when the
/// newer outgrows the limits it becomes the older and the older is let go,
/// so an entry not used for a whole generation goes.
struct Generations<K, V> {
    newer: FxHashMap<K, (V, usize)>,
    older: FxHashMap<K, (V, usize)>,
    bytes: usize,
}

impl<K: Hash + Eq, V: Clone> Generations<K, V> {
    fn new() -> Self {
        Self {
            newer: FxHashMap::default(),
            older: FxHashMap::default(),
            bytes: 0,
        }
    }

    fn get(&mut self, key: &K) -> Option<V>
    where
        K: Clone,
    {
        if let Some((value, _)) = self.newer.get(key) {
            return Some(value.clone());
        }
        let (key, (value, bytes)) = self.older.remove_entry(key)?;
        self.insert(key, value.clone(), bytes);
        Some(value)
    }

    fn insert(&mut self, key: K, value: V, bytes: usize) {
        if self.newer.len() >= KEPT || self.bytes + bytes > KEPT_BYTES {
            self.older = std::mem::take(&mut self.newer);
            self.bytes = 0;
        }
        self.bytes += bytes;
        self.newer.insert(key, (value, bytes));
    }

    #[cfg(test)]
    fn clear(&mut self) {
        self.newer.clear();
        self.older.clear();
        self.bytes = 0;
    }
}

/// A path's curves, compared by identity: [`build`] shares the curves of
/// every path of one shape, so the same shape has the same curves.
#[derive(Clone)]
struct CurvesKey(Arc<[PathCurve]>);

impl PartialEq for CurvesKey {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for CurvesKey {}

impl Hash for CurvesKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.0) as *const PathCurve as usize).hash(state)
    }
}

thread_local! {
    static BUILT: RefCell<Generations<Rc<[u32]>, Path>> = RefCell::new(Generations::new());
    static BINNED: RefCell<Generations<(CurvesKey, bool, u32), Rc<PathTiles>>> =
        RefCell::new(Generations::new());
}

/// Builds `builder` as [`PathBuilder::build`] does, sharing the curves of the
/// path last built of the same shape.
pub(crate) fn build(builder: PathBuilder) -> Result<Path, Error> {
    let (lyon, style, dash_array) = builder.fast_into_parts();
    let key: Rc<[u32]> = shape_key(&lyon, &style, dash_array.as_deref()).into();
    if let Some(path) = BUILT.with_borrow_mut(|built| built.get(&key)) {
        return Ok(path);
    }
    let (curves, fill_rule) = PathBuilder::fast_from_parts(&lyon, style, dash_array).outline()?;
    let path = Path::from_curves(curves, fill_rule);
    let bytes = key.len() * 4 + std::mem::size_of_val(&**PathBuilder::fast_curves(&path));
    BUILT.with_borrow_mut(|built| built.insert(key, path.clone(), bytes));
    Ok(path)
}

/// `path`'s curves binned into tiles at `scale`, as [`Path::bin`] gives
/// them, shared with the last time the same curves were binned at it.
pub(crate) fn bin(path: &Path, scale: f32) -> Rc<PathTiles> {
    let key = (
        CurvesKey(PathBuilder::fast_curves(&path).clone()),
        path.even_odd(),
        scale.to_bits(),
    );
    if let Some(tiles) = BINNED.with_borrow_mut(|binned| binned.get(&key)) {
        return tiles;
    }
    let tiles = Rc::new(path.bin(scale));
    let bytes = std::mem::size_of_val(tiles.tiles()) + std::mem::size_of_val(tiles.curves());
    BINNED.with_borrow_mut(|binned| binned.insert(key, tiles.clone(), bytes));
    tiles
}

/// `tiles` without those lying entirely outside `mask`, shared when none do.
pub(crate) fn visible(
    tiles: Rc<PathTiles>,
    mask: crate::Bounds<crate::ScaledPixels>,
) -> Rc<PathTiles> {
    let visible = |tile: &crate::PathTile| {
        tile.top_left.x < mask.right().0
            && tile.top_left.y < mask.bottom().0
            && tile.bottom_right.x > mask.origin.x.0
            && tile.bottom_right.y > mask.origin.y.0
    };
    if tiles.tiles().iter().all(visible) {
        return tiles;
    }
    // A copy: moving by nothing keeps every coordinate.
    let mut tiles = tiles.translated(crate::point(0., 0.));
    tiles.retain_visible(mask);
    Rc::new(tiles)
}

/// Lets go of every kept path on this thread.
#[cfg(test)]
pub(crate) fn clear() {
    BUILT.with_borrow_mut(Generations::clear);
    BINNED.with_borrow_mut(Generations::clear);
}

/// The key a shape is kept by: its transformed events and how it is stroked
/// or filled, as bits, so that only the same shape matches.
fn shape_key(
    path: &lyon_path::Path,
    style: &PathStyle,
    dash_array: Option<&[crate::Pixels]>,
) -> Vec<u32> {
    let mut key = Vec::with_capacity(64);
    let point = |key: &mut Vec<u32>, point: lyon_path::math::Point| {
        key.push(point.x.to_bits());
        key.push(point.y.to_bits());
    };
    for event in path.iter() {
        match event {
            Event::Begin { at } => {
                key.push(0);
                point(&mut key, at);
            }
            Event::Line { to, .. } => {
                key.push(1);
                point(&mut key, to);
            }
            Event::Quadratic { ctrl, to, .. } => {
                key.push(2);
                point(&mut key, ctrl);
                point(&mut key, to);
            }
            Event::Cubic {
                ctrl1, ctrl2, to, ..
            } => {
                key.push(3);
                point(&mut key, ctrl1);
                point(&mut key, ctrl2);
                point(&mut key, to);
            }
            Event::End { close, .. } => key.push(4 + close as u32),
        }
    }
    key.push(u32::MAX);
    match style {
        PathStyle::Stroke(options) => {
            key.push(0);
            key.push(options.line_width.to_bits());
            key.push(options.line_cap as u32);
            key.push(options.line_join as u32);
            key.push(options.miter_limit.to_bits());
        }
        PathStyle::Fill(options) => {
            key.push(1);
            key.push(options.fill_rule as u32);
        }
    }
    if let Some(dash_array) = dash_array {
        key.push(u32::MAX - 2);
        key.extend(dash_array.iter().map(|dash| dash.0.to_bits()));
    }
    key
}

#[cfg(test)]
mod tests {
    use super::{Arc, Generations, KEPT, Path, PathBuilder, Rc, bin, build, clear};
    use crate::{point, px};

    fn sparkline(x: f32) -> PathBuilder {
        let mut builder = PathBuilder::stroke(px(1.5));
        builder.move_to(point(px(x), px(10.)));
        for step in 1..20 {
            builder.line_to(point(
                px(x + step as f32 * 1.5),
                px(10. + ((step * 7) % 11) as f32),
            ));
        }
        builder
    }

    #[test]
    fn the_same_shape_shares_its_curves_and_tiles() {
        clear();
        let a = build(sparkline(3.)).unwrap();
        let b = build(sparkline(3.)).unwrap();
        assert!(Arc::ptr_eq(
            PathBuilder::fast_curves(&a),
            PathBuilder::fast_curves(&b)
        ));
        assert!(Rc::ptr_eq(&bin(&a, 2.), &bin(&b, 2.)));
        assert!(!Rc::ptr_eq(&bin(&a, 2.), &bin(&b, 1.)));
        let c = build(sparkline(4.)).unwrap();
        assert!(!Arc::ptr_eq(
            PathBuilder::fast_curves(&a),
            PathBuilder::fast_curves(&c)
        ));
    }

    #[test]
    fn kept_paths_bin_as_paths_built_again_do() {
        clear();
        build(sparkline(3.25)).unwrap();
        let kept = build(sparkline(3.25)).unwrap();
        let fresh = sparkline(3.25).outline().unwrap();
        let fresh = Path::from_curves(fresh.0, fresh.1);
        let (kept, fresh) = (bin(&kept, 1.5), fresh.bin(1.5));
        assert_eq!(kept.tiles(), fresh.tiles());
        assert_eq!(kept.curves(), fresh.curves());
    }

    #[test]
    fn generations_let_go_of_what_was_not_used_for_a_whole_generation() {
        let mut kept = Generations::<u32, u32>::new();
        for i in 0..KEPT as u32 {
            kept.insert(i, i, 1);
        }
        assert_eq!(kept.get(&0), Some(0));
        kept.insert(u32::MAX, 0, 1);
        assert_eq!(kept.get(&1), Some(1));
        for i in 0..KEPT as u32 {
            kept.insert(KEPT as u32 + i, i, 1);
        }
        kept.insert(u32::MAX - 1, 0, 1);
        assert_eq!(kept.get(&2), None);
    }
}
