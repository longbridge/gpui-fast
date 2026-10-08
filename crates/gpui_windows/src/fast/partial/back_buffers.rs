//! What of the canvas a frame must copy into the swap chain's back buffer.
//!
//! A `DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL` swap chain of `BUFFER_COUNT` buffers
//! hands them out in turn: after each present the back buffer is the one
//! presented `BUFFER_COUNT` presents ago, and keeps what it held then. So it
//! differs from the canvas only where the frames presented since, and this
//! one, changed: copying the union of those rectangles makes it the canvas.
//!
//! That holds only while every present of the swap chain is one of ours and
//! went through. [`BackBuffers`] remembers the changes of the last frames it
//! presented and the swap chain's present count after them; anything else
//! (no frame of ours on every buffer yet, a frame presented elsewhere, such
//! as by `fast::composition`, a present that did not succeed outright, a new
//! canvas) makes the next frames copy the whole canvas until every buffer
//! holds one of ours again.

use std::collections::VecDeque;

use windows::Win32::Foundation::RECT;

use super::{area, disjoint};

/// Frames the back buffer is behind the canvas by.
const AGE: usize = crate::directx_renderer::BUFFER_COUNT;

#[derive(Default)]
pub(crate) struct BackBuffers {
    /// What each of the last frames presented changed, newest last, `None`
    /// for everything; at most [`AGE`] of them, since the last reset.
    changes: VecDeque<Option<Vec<RECT>>>,
    /// The swap chain's present count after the last present here.
    present_count: Option<u32>,
}

impl BackBuffers {
    /// Forgets every frame: the next [`AGE`] frames copy the whole canvas.
    pub(crate) fn reset(this: &mut Self) {
        this.changes.clear();
        this.present_count = None;
    }

    /// What of a `width` × `height` canvas to copy into the back buffer for a
    /// frame that changed `changed` (`None`: everything), when the swap
    /// chain's present count is `present_count`. `None`: all of it.
    pub(crate) fn to_copy(
        this: &mut Self,
        changed: Option<&[RECT]>,
        present_count: u32,
        width: u32,
        height: u32,
    ) -> Option<Vec<RECT>> {
        if this.present_count != Some(present_count) {
            // Presented by someone else since, or never here.
            Self::reset(this);
        }
        let changed = changed?;
        if this.changes.len() < AGE {
            // A back buffer may not hold a frame of ours yet.
            return None;
        }
        let mut rects = changed.to_vec();
        for frame in this.changes.iter().skip(this.changes.len() + 1 - AGE) {
            rects.extend_from_slice(frame.as_deref()?);
        }
        let rects = disjoint(rects);
        let copied: i64 = rects.iter().map(area).sum();
        (copied * 2 <= i64::from(width) * i64::from(height)).then_some(rects)
    }

    /// Notes that a frame that changed `changed` (`None`: everything) was
    /// presented: `succeeded` when the present returned `S_OK`, leaving the
    /// swap chain's present count at `present_count`.
    pub(crate) fn presented(
        this: &mut Self,
        changed: Option<Vec<RECT>>,
        succeeded: bool,
        present_count_before: u32,
        present_count: u32,
    ) {
        if !succeeded
            || this.present_count.is_some_and(|count| count != present_count_before)
            || present_count != present_count_before.wrapping_add(1)
        {
            Self::reset(this);
            return;
        }
        this.changes.push_back(changed);
        if this.changes.len() > AGE {
            this.changes.pop_front();
        }
        this.present_count = Some(present_count);
    }
}
