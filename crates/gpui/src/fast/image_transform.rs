//! Images painted through a transformation matrix, as SVGs already are.
//!
//! A [`PolychromeSprite`](crate::PolychromeSprite) carries a
//! `fast_transformation`, applied to its window positions as a
//! [`MonochromeSprite`](crate::MonochromeSprite)'s `transformation` is
//! (`to_device_position_transformed` in the shaders). The polychrome fragment
//! shaders round the sprite's corners against its untransformed position, so
//! the corners turn with the image. They are not handed that position
//! itself but `fast_local_offset`, the untransformed position less the
//! transformed one, which they add to the pixel's position: under the unit
//! matrix the offset is zero at every vertex, so it interpolates to exactly
//! zero and the corners are rounded against the very position upstream's
//! shaders use, keeping an untransformed image's pixels byte for byte the
//! same. Both positions are affine across the sprite, so the offset
//! interpolates exactly under any matrix too.
//!
//! Upstream's `Window::paint_image` is `Window::fast_paint_image`, which takes
//! the matrix; `paint_image` and `paint_image_transformed` are defined here
//! and forward to it.

use std::sync::Arc;

use anyhow::Result;

use crate::{Bounds, Corners, Pixels, RenderImage, TransformationMatrix, Window};

impl Window {
    /// Paint an image into `bounds`, positioning and scaling it according to
    /// `image_bounds`, into the scene for the next frame at the current
    /// z-index. This method will panic if the frame_index is not valid.
    ///
    /// The visible region rendered is `bounds.intersect(&image_bounds)`, with
    /// `corner_radii` applied to `bounds`.
    ///
    /// This method should only be called as part of the paint phase of
    /// element drawing.
    pub fn paint_image(
        &mut self,
        bounds: Bounds<Pixels>,
        image_bounds: Bounds<Pixels>,
        corner_radii: Corners<Pixels>,
        data: Arc<RenderImage>,
        frame_index: usize,
        grayscale: bool,
    ) -> Result<()> {
        self.fast_paint_image(
            bounds,
            image_bounds,
            corner_radii,
            data,
            frame_index,
            grayscale,
            TransformationMatrix::unit(),
        )
    }

    /// Paint an image as [`Window::paint_image`] does, through
    /// `transformation`.
    ///
    /// `transformation` applies to the image's window positions in device
    /// pixels, as [`Window::paint_svg`]'s does: to turn an image about its
    /// centre, translate the centre to the origin, rotate, and translate it
    /// back. The content mask clips the transformed image; the corner radii
    /// round it before it is transformed, so they turn with it.
    ///
    /// This method should only be called as part of the paint phase of
    /// element drawing.
    pub fn paint_image_transformed(
        &mut self,
        bounds: Bounds<Pixels>,
        image_bounds: Bounds<Pixels>,
        corner_radii: Corners<Pixels>,
        data: Arc<RenderImage>,
        frame_index: usize,
        grayscale: bool,
        transformation: TransformationMatrix,
    ) -> Result<()> {
        self.fast_paint_image(
            bounds,
            image_bounds,
            corner_radii,
            data,
            frame_index,
            grayscale,
            transformation,
        )
    }
}
