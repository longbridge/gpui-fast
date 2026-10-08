//! The regions of a frame a presenter has to copy: rectangles within the
//! frame, or the whole frame.

use gpui::{Bounds, DevicePixels, point, size};

/// The most rectangles a region keeps before it becomes their bounding box.
const MAX_RECTS: usize = 16;

/// Rectangles within a frame, possibly overlapping, or the whole frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Region {
    all: bool,
    rects: Vec<Bounds<DevicePixels>>,
}

// The X11 presenter never needs the whole frame: that is for Wayland buffers.
#[cfg_attr(not(feature = "wayland"), allow(dead_code))]
impl Region {
    /// The whole frame.
    pub(crate) fn all() -> Self {
        Self {
            all: true,
            rects: Vec::new(),
        }
    }

    /// `rects` clipped to a frame of `width` × `height`, without the empty
    /// ones.
    pub(crate) fn from_rects(rects: &[Bounds<DevicePixels>], width: u32, height: u32) -> Self {
        let frame = Bounds {
            origin: point(DevicePixels(0), DevicePixels(0)),
            size: size(DevicePixels(width as i32), DevicePixels(height as i32)),
        };
        let mut region = Self::default();
        for rect in rects {
            let rect = rect.intersect(&frame);
            if rect.size.width.0 > 0 && rect.size.height.0 > 0 {
                region.rects.push(rect);
            }
        }
        region.limit();
        region
    }

    pub(crate) fn is_all(&self) -> bool {
        self.all
    }

    /// The rectangles of a region that is not the whole frame.
    pub(crate) fn rects(&self) -> &[Bounds<DevicePixels>] {
        &self.rects
    }

    pub(crate) fn set_all(&mut self) {
        self.all = true;
        self.rects.clear();
    }

    pub(crate) fn clear(&mut self) {
        self.all = false;
        self.rects.clear();
    }

    /// Adds `other` to this region.
    pub(crate) fn add(&mut self, other: &Region) {
        if self.all {
            return;
        }
        if other.all {
            self.set_all();
            return;
        }
        self.rects.extend_from_slice(&other.rects);
        self.limit();
    }

    fn limit(&mut self) {
        if self.rects.len() > MAX_RECTS {
            let bounds = self.rects[1..]
                .iter()
                .fold(self.rects[0], |bounds, rect| bounds.union(rect));
            self.rects.clear();
            self.rects.push(bounds);
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Bounds, DevicePixels, point, size};

    use super::{MAX_RECTS, Region};

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Bounds<DevicePixels> {
        Bounds {
            origin: point(DevicePixels(x), DevicePixels(y)),
            size: size(DevicePixels(w), DevicePixels(h)),
        }
    }

    #[test]
    fn clips_rects_to_the_frame_and_drops_empty_ones() {
        let region = Region::from_rects(
            &[
                rect(-5, -5, 10, 10),
                rect(90, 40, 20, 20),
                rect(200, 0, 5, 5),
            ],
            100,
            50,
        );
        assert!(!region.is_all());
        assert_eq!(region.rects(), &[rect(0, 0, 5, 5), rect(90, 40, 10, 10)]);
    }

    #[test]
    fn too_many_rects_become_their_bounding_box() {
        let rects = (0..=MAX_RECTS as i32)
            .map(|i| rect(i * 2, i, 1, 1))
            .collect::<Vec<_>>();
        let region = Region::from_rects(&rects, 1000, 1000);
        assert_eq!(
            region.rects(),
            &[rect(0, 0, MAX_RECTS as i32 * 2 + 1, MAX_RECTS as i32 + 1)]
        );
    }

    #[test]
    fn a_buffer_misses_the_damage_of_frames_since_it_was_shown() {
        // Two buffers alternate; each misses what the other showed.
        let mut a = Region::all();
        let mut b = Region::all();
        let frames = [
            Region::from_rects(&[rect(0, 0, 10, 10)], 100, 100),
            Region::from_rects(&[rect(20, 20, 5, 5)], 100, 100),
            Region::from_rects(&[rect(50, 50, 1, 1)], 100, 100),
        ];

        // Frame 0 into a, whole because it is new.
        a.add(&frames[0]);
        b.add(&frames[0]);
        assert!(a.is_all());
        a.clear();

        // Frame 1 into b, whole because it is new.
        a.add(&frames[1]);
        b.add(&frames[1]);
        assert!(b.is_all());
        b.clear();
        assert_eq!(a.rects(), frames[1].rects());

        // Frame 2 into a: frame 1's damage and its own.
        a.add(&frames[2]);
        b.add(&frames[2]);
        assert_eq!(a.rects(), &[rect(20, 20, 5, 5), rect(50, 50, 1, 1)]);
        a.clear();
        assert_eq!(b.rects(), frames[2].rects());

        // The GPU presented: both have to be copied whole.
        a.set_all();
        b.add(&Region::all());
        assert!(a.is_all() && b.is_all());
        a.add(&frames[0]);
        assert!(a.is_all());
    }
}
