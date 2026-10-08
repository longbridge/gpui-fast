//! Presenting CPU frames on X11 with `PutImage`.
//!
//! X11 keeps a window's contents, so a frame uploads only its damage: large
//! rectangles through an MIT-SHM segment the size of the frame, small ones,
//! or all of them without MIT-SHM, in `PutImage` requests split to stay under
//! the connection's maximum request length.
//!
//! When the GPU presents through the Present extension the window shows the
//! presented pixmap, which holds a whole frame; uploading the damage of the
//! next frame on top of it is what the CPU path needs, once that pixmap is
//! in the window.
//!
//! It may not be yet. The swapchain presents in FIFO mode: Mesa's Vulkan
//! WSI sends `PresentPixmap` for a coming vblank, or later still from its
//! queue thread, and the server copies (or flips) the pixmap into the window
//! then. A CPU frame shown in between is uploaded at once and the GPU frame
//! lands over it; later CPU frames upload only their own damage, so what the
//! earlier ones uploaded would stay stale. The presenter therefore selects
//! `CompleteNotify` on the window with an event context of its own (Mesa
//! reads its own through a special event queue). A GPU present that
//! completes after CPU frames were shown since the GPU last presented means
//! the window shows a GPU frame again: the client refreshes the window
//! ([`on_event`]), which presents the scene again, and that CPU frame
//! uploads, besides its damage, every rectangle the CPU frames since the GPU
//! presented uploaded. They cover everything that differs from the GPU's
//! last frame, and no GPU present lands after the last one completes, so the
//! window ends right without waiting for another frame: stale pixels last
//! from a completion until the client's next turn of its event loop. How
//! many GPU presents are still queued is not known, so every completion is
//! answered, not only the first.
//!
//! The alternatives were worse. Re-uploading for a while after each GPU
//! present leaves a CPU frame stale when the GPU's present lands after the
//! last upload, with nothing to call the presenter then. Presenting CPU
//! frames through the Present extension too would order them with the GPU's,
//! but needs a whole-window pixmap kept up to date and has the compositor
//! take the whole window for every frame.
//!
//! Without a compositing manager the server discards what is covered, and
//! `Expose` events name the rectangles to draw again. The client refreshes
//! the window, which presents the same scene again, a CPU frame without
//! damage; the exposed rectangles are uploaded with it.
//!
//! Neither applies when the server has no Present extension: Mesa then puts
//! GPU frames into the window with requests ordered with ours. A driver
//! that presents some other way is not followed.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    os::fd::AsFd as _,
    rc::Rc,
};

use anyhow::Context as _;
use gpui::{Bounds, DevicePixels, point, size};
use gpui_wgpu::{CpuFrame, CpuPresenter, WgpuRenderer};
use x11rb::{
    connection::{Connection as _, DiscardMode, RequestConnection as _, RequestKind},
    protocol::{
        Event,
        present::{self, ConnectionExt as _},
        shm::{self, ConnectionExt as _},
        xproto::{self, ConnectionExt as _},
    },
    xcb_ffi::XCBConnection,
};

use super::memfd::Mapping;
use super::region::Region;

/// The size of a `PutImage` request without its pixels, plus some slack.
const PUT_IMAGE_HEADER_BYTES: usize = 64;
/// Rectangles of more pixels than this go through MIT-SHM, when available.
const SHM_MIN_PIXELS: usize = 64 * 64;

thread_local! {
    /// The windows with a presenter: its Present event context, if any, and
    /// what it owes the window.
    static WINDOWS: RefCell<HashMap<xproto::Window, (Option<present::Event>, Rc<RefCell<Owed>>)>> =
        RefCell::new(HashMap::new());
}

/// Notes what `event`, which the X11 client is about to handle, means for
/// the CPU frames of its window, and adds the window to `windows_to_refresh`
/// when it has to be presented again.
pub(crate) fn on_event(event: &Event, windows_to_refresh: &mut HashSet<xproto::Window>) {
    match event {
        Event::Expose(event) => {
            let rect = Bounds {
                origin: point(DevicePixels(event.x.into()), DevicePixels(event.y.into())),
                size: size(
                    DevicePixels(event.width.into()),
                    DevicePixels(event.height.into()),
                ),
            };
            with_owed(event.window, None, |owed| owed.exposed(rect));
        }
        Event::PresentCompleteNotify(event) if event.kind == present::CompleteKind::PIXMAP => {
            if with_owed(event.window, Some(event.event), Owed::gpu_landed) == Some(true) {
                windows_to_refresh.insert(event.window);
            }
        }
        _ => {}
    }
}

/// Calls `f` with what the presenter of `window` owes it, if `window` has
/// one (whose Present event context is `eid`, when given).
fn with_owed<R>(
    window: xproto::Window,
    eid: Option<present::Event>,
    f: impl FnOnce(&mut Owed) -> R,
) -> Option<R> {
    let owed = WINDOWS.with_borrow(|windows| {
        windows
            .get(&window)
            .filter(|(selected, _)| eid.is_none() || *selected == eid)
            .map(|(_, owed)| owed.clone())
    })?;
    let mut owed = owed.borrow_mut();
    Some(f(&mut owed))
}

/// What the window shows wrong, besides what the next frame changes: what
/// the CPU frames uploaded since the GPU last presented, once a GPU present
/// landed over them, and exposed rectangles.
#[derive(Default)]
struct Owed {
    /// The rectangles the CPU frames shown since the GPU last presented
    /// uploaded.
    since_gpu: Region,
    /// A GPU present completed over CPU frames: `since_gpu` is to be
    /// uploaded again.
    landed: bool,
    exposed: Vec<Bounds<DevicePixels>>,
}

impl Owed {
    fn exposed(&mut self, rect: Bounds<DevicePixels>) {
        self.exposed.push(rect);
    }

    /// Notes that a GPU present completed, and returns whether the window
    /// is to be presented again: whether CPU frames uploaded anything since
    /// the GPU presented.
    fn gpu_landed(&mut self) -> bool {
        if self.since_gpu.rects().is_empty() {
            return false;
        }
        self.landed = true;
        true
    }

    /// The GPU is about to present a whole frame.
    fn gpu_presented(&mut self) {
        self.since_gpu.clear();
        self.landed = false;
        self.exposed.clear();
    }

    /// The rectangles to upload for a frame of `width` × `height` with
    /// `damage`.
    fn upload(&mut self, damage: &[Bounds<DevicePixels>], width: u32, height: u32) -> Region {
        let damage = Region::from_rects(damage, width, height);
        let mut upload = damage.clone();
        if std::mem::take(&mut self.landed) {
            upload.add(&Region::from_rects(self.since_gpu.rects(), width, height));
        }
        upload.add(&Region::from_rects(&self.exposed, width, height));
        self.exposed.clear();
        self.since_gpu.add(&damage);
        // Exposures repeat, and what is uploaded again often holds the
        // damage: upload each pixel once where a rectangle holds another.
        let rects = upload.rects();
        let kept = rects
            .iter()
            .enumerate()
            .filter(|&(i, rect)| {
                !rects
                    .iter()
                    .enumerate()
                    .any(|(j, other)| other.intersect(rect) == *rect && (other != rect || j < i))
            })
            .map(|(_, rect)| *rect)
            .collect::<Vec<_>>();
        Region::from_rects(&kept, width, height)
    }
}

/// Lets `renderer` present the frames it draws on the CPU in `window`, of
/// `depth` bits per pixel.
pub(crate) fn install(
    renderer: &mut WgpuRenderer,
    xcb: &Rc<XCBConnection>,
    window: xproto::Window,
    depth: u8,
) {
    match PutImagePresenter::new(xcb, window, depth) {
        Ok(presenter) => renderer.set_cpu_presenter(Box::new(presenter)),
        Err(err) => log::info!("CPU frames are not presented on this X11 window: {err:#}"),
    }
}

struct PutImagePresenter {
    xcb: Rc<XCBConnection>,
    window: xproto::Window,
    depth: u8,
    /// Whether the server takes pixels most significant byte first.
    big_endian: bool,
    gc: Option<xproto::Gcontext>,
    shm: Shm,
    owed: Rc<RefCell<Owed>>,
}

enum Shm {
    Unknown,
    Unavailable,
    Available(Option<Segment>),
}

/// An MIT-SHM segment the size of a frame, and the request after the last
/// `ShmPutImage` reading it: once that request's reply arrived, the server
/// read the segment and it can be written again.
struct Segment {
    seg: shm::Seg,
    map: Mapping,
    width: u32,
    height: u32,
    pending: Option<x11rb::connection::SequenceNumber>,
}

impl PutImagePresenter {
    fn new(xcb: &Rc<XCBConnection>, window: xproto::Window, depth: u8) -> anyhow::Result<Self> {
        anyhow::ensure!(depth == 24 || depth == 32, "depth {depth}");
        let setup = xcb.setup();
        let format = setup
            .pixmap_formats
            .iter()
            .find(|format| format.depth == depth)
            .context("no pixmap format for the window's depth")?;
        anyhow::ensure!(
            format.bits_per_pixel == 32,
            "{} bits per pixel",
            format.bits_per_pixel
        );

        let visual = xcb
            .get_window_attributes(window)?
            .reply()
            .context("GetWindowAttributes")?
            .visual;
        let visual_type = setup
            .roots
            .iter()
            .flat_map(|screen| &screen.allowed_depths)
            .flat_map(|depth| &depth.visuals)
            .find(|visual_type| visual_type.visual_id == visual)
            .context("the window's visual is not on any screen")?;
        anyhow::ensure!(
            visual_type.class == xproto::VisualClass::TRUE_COLOR
                && visual_type.red_mask == 0xff0000
                && visual_type.green_mask == 0xff00
                && visual_type.blue_mask == 0xff,
            "visual of class {:?} and masks {:#x} {:#x} {:#x}",
            visual_type.class,
            visual_type.red_mask,
            visual_type.green_mask,
            visual_type.blue_mask,
        );

        let eid = select_present_events(xcb, window)
            .inspect_err(|err| {
                log::info!("GPU presents are not followed on this X11 window: {err:#}")
            })
            .ok()
            .flatten();
        let owed = Rc::new(RefCell::new(Owed::default()));
        WINDOWS.with_borrow_mut(|windows| windows.insert(window, (eid, owed.clone())));
        Ok(Self {
            xcb: xcb.clone(),
            window,
            depth,
            big_endian: setup.image_byte_order == xproto::ImageOrder::MSB_FIRST,
            gc: None,
            shm: Shm::Unknown,
            owed,
        })
    }

    fn gc(&mut self) -> anyhow::Result<xproto::Gcontext> {
        if let Some(gc) = self.gc {
            return Ok(gc);
        }
        let gc = self.xcb.generate_id()?;
        self.xcb
            .create_gc(
                gc,
                self.window,
                &xproto::CreateGCAux::new().graphics_exposures(0),
            )?
            .check()
            .context("CreateGC")?;
        self.gc = Some(gc);
        Ok(gc)
    }

    /// Whether the server has MIT-SHM 1.2 or later, which takes a file
    /// descriptor for a segment.
    fn has_shm(&mut self) -> bool {
        if let Shm::Unknown = self.shm {
            let available = self
                .xcb
                .extension_information(shm::X11_EXTENSION_NAME)
                .ok()
                .flatten()
                .is_some()
                && self
                    .xcb
                    .shm_query_version()
                    .ok()
                    .and_then(|cookie| cookie.reply().ok())
                    .is_some_and(|version| {
                        (version.major_version, version.minor_version) >= (1, 2)
                    });
            self.shm = if available {
                Shm::Available(None)
            } else {
                Shm::Unavailable
            };
        }
        matches!(self.shm, Shm::Available(_))
    }

    /// The segment for a frame of `width` × `height`, once the server has
    /// read what the last frame wrote into it.
    fn segment(&mut self, width: u32, height: u32) -> anyhow::Result<&mut Segment> {
        let Shm::Available(segment) = &mut self.shm else {
            anyhow::bail!("no MIT-SHM");
        };
        if segment
            .as_ref()
            .is_some_and(|segment| segment.width != width || segment.height != height)
        {
            if let Some(old) = segment.take() {
                old.free(&self.xcb);
            }
        }
        if segment.is_none() {
            let map = Mapping::new(width as usize * height as usize * 4)?;
            let seg = self.xcb.generate_id()?;
            let fd = map.fd().as_fd().try_clone_to_owned()?;
            self.xcb
                .shm_attach_fd(seg, fd, true)?
                .check()
                .context("ShmAttachFd")?;
            *segment = Some(Segment {
                seg,
                map,
                width,
                height,
                pending: None,
            });
        }
        let segment = segment.as_mut().unwrap();
        if let Some(pending) = segment.pending.take() {
            self.xcb
                .wait_for_reply_or_error(pending)
                .context("waiting for the server to read the MIT-SHM segment")?;
        }
        Ok(segment)
    }

    fn put_image(
        &self,
        gc: xproto::Gcontext,
        pixels: &[u32],
        width: u32,
        rect: Bounds<DevicePixels>,
        alpha: u32,
    ) -> anyhow::Result<()> {
        let max_pixels = self
            .xcb
            .maximum_request_bytes()
            .saturating_sub(PUT_IMAGE_HEADER_BYTES)
            / 4;
        let mut data = Vec::new();
        for part in split_rect(rect, max_pixels.max(1)) {
            data.clear();
            data.reserve(part.size.width.0 as usize * part.size.height.0 as usize * 4);
            for y in part.origin.y.0..part.origin.y.0 + part.size.height.0 {
                let start = y as usize * width as usize + part.origin.x.0 as usize;
                for &pixel in &pixels[start..start + part.size.width.0 as usize] {
                    let pixel = pixel | alpha;
                    data.extend_from_slice(&if self.big_endian {
                        pixel.to_be_bytes()
                    } else {
                        pixel.to_le_bytes()
                    });
                }
            }
            self.xcb
                .put_image(
                    xproto::ImageFormat::Z_PIXMAP,
                    self.window,
                    gc,
                    part.size.width.0 as u16,
                    part.size.height.0 as u16,
                    part.origin.x.0 as i16,
                    part.origin.y.0 as i16,
                    0,
                    self.depth,
                    &data,
                )?
                .ignore_error();
        }
        Ok(())
    }

    fn shm_put_images(
        &mut self,
        gc: xproto::Gcontext,
        frame: &CpuFrame<'_>,
        rects: &[Bounds<DevicePixels>],
        alpha: u32,
    ) -> anyhow::Result<()> {
        let (width, height) = (frame.width, frame.height);
        let (window, depth, big_endian) = (self.window, self.depth, self.big_endian);
        let segment = self.segment(width, height)?;
        let seg = segment.seg;
        let dst = segment.map.pixels_mut();
        for rect in rects {
            for y in rect.origin.y.0..rect.origin.y.0 + rect.size.height.0 {
                let start = y as usize * width as usize + rect.origin.x.0 as usize;
                let end = start + rect.size.width.0 as usize;
                for (dst, &src) in dst[start..end].iter_mut().zip(&frame.pixels[start..end]) {
                    let pixel = src | alpha;
                    *dst = if big_endian {
                        pixel.to_be()
                    } else {
                        pixel.to_le()
                    };
                }
            }
        }
        for rect in rects {
            self.xcb
                .shm_put_image(
                    window,
                    gc,
                    width as u16,
                    height as u16,
                    rect.origin.x.0 as u16,
                    rect.origin.y.0 as u16,
                    rect.size.width.0 as u16,
                    rect.size.height.0 as u16,
                    rect.origin.x.0 as i16,
                    rect.origin.y.0 as i16,
                    depth,
                    xproto::ImageFormat::Z_PIXMAP.into(),
                    false,
                    seg,
                    0,
                )?
                .ignore_error();
        }
        let cookie = self.xcb.get_input_focus()?;
        let pending = cookie.sequence_number();
        // The reply is waited for before the segment is written again.
        std::mem::forget(cookie);
        if let Shm::Available(Some(segment)) = &mut self.shm {
            segment.pending = Some(pending);
        }
        Ok(())
    }
}

impl CpuPresenter for PutImagePresenter {
    fn present(&mut self, frame: CpuFrame<'_>) -> anyhow::Result<()> {
        let (width, height) = (frame.width, frame.height);
        anyhow::ensure!(
            width > 0 && height > 0 && width <= u16::MAX as u32 && height <= u16::MAX as u32,
            "CPU frame of {width}×{height}"
        );
        anyhow::ensure!(
            frame.pixels.len() >= width as usize * height as usize,
            "CPU frame has fewer pixels than its size"
        );
        // Without the GPU's opaque composite alpha, a 32-bit window shows
        // the alpha channel: make it opaque.
        let alpha = if frame.opaque && self.depth == 32 {
            0xff00_0000
        } else {
            0
        };
        let gc = self.gc()?;
        let damage = self.owed.borrow_mut().upload(frame.damage, width, height);
        let (large, small): (Vec<_>, Vec<_>) = damage.rects().iter().partition(|rect| {
            rect.size.width.0 as usize * rect.size.height.0 as usize >= SHM_MIN_PIXELS
        });

        let mut put = small;
        if !large.is_empty() {
            if self.has_shm() {
                if let Err(err) = self.shm_put_images(gc, &frame, &large, alpha) {
                    log::warn!("MIT-SHM upload failed, using PutImage: {err:#}");
                    if let Shm::Available(segment) = &mut self.shm {
                        if let Some(segment) = segment.take() {
                            segment.free(&self.xcb);
                        }
                    }
                    self.shm = Shm::Unavailable;
                    put.extend(large);
                }
            } else {
                put.extend(large);
            }
        }
        for rect in put {
            self.put_image(gc, frame.pixels, width, rect, alpha)?;
        }
        self.xcb.flush().context("X11 flush")?;
        Ok(())
    }

    fn gpu_presented(&mut self) {
        self.owed.borrow_mut().gpu_presented();
    }

    fn release(&mut self) {
        if let Shm::Available(segment) = &mut self.shm {
            if let Some(segment) = segment.take() {
                segment.free(&self.xcb);
            }
        }
        if let Some(gc) = self.gc.take() {
            if let Ok(cookie) = self.xcb.free_gc(gc) {
                cookie.ignore_error();
            }
        }
        self.xcb.flush().ok();
    }
}

impl Drop for PutImagePresenter {
    fn drop(&mut self) {
        let eid = WINDOWS
            .with_borrow_mut(|windows| windows.remove(&self.window))
            .and_then(|(eid, _)| eid);
        if let Some(eid) = eid
            && let Ok(cookie) =
                self.xcb
                    .present_select_input(eid, self.window, present::EventMask::NO_EVENT)
        {
            // An empty mask frees the event context.
            cookie.ignore_error();
        }
        CpuPresenter::release(self);
    }
}

/// Selects `CompleteNotify` on `window` with a new event context and
/// returns it, or `None` when the server has no Present extension.
fn select_present_events(
    xcb: &XCBConnection,
    window: xproto::Window,
) -> anyhow::Result<Option<present::Event>> {
    if xcb
        .extension_information(present::X11_EXTENSION_NAME)?
        .is_none()
    {
        return Ok(None);
    }
    xcb.present_query_version(1, 0)?
        .reply()
        .context("PresentQueryVersion")?;
    let eid = xcb.generate_id()?;
    xcb.present_select_input(eid, window, present::EventMask::COMPLETE_NOTIFY)?
        .check()
        .context("PresentSelectInput")?;
    Ok(Some(eid))
}

impl Segment {
    fn free(self, xcb: &XCBConnection) {
        if let Some(pending) = self.pending {
            xcb.discard_reply(
                pending,
                RequestKind::HasResponse,
                DiscardMode::DiscardReplyAndError,
            );
        }
        if let Ok(cookie) = xcb.shm_detach(self.seg) {
            cookie.ignore_error();
        }
    }
}

/// Splits `rect` into rectangles of at most `max_pixels` pixels: bands of
/// whole rows, or pieces of a row when one row is longer.
fn split_rect(
    rect: Bounds<DevicePixels>,
    max_pixels: usize,
) -> impl Iterator<Item = Bounds<DevicePixels>> {
    let (x, y) = (rect.origin.x.0, rect.origin.y.0);
    let (width, height) = (rect.size.width.0.max(0), rect.size.height.0.max(0));
    let max_pixels = max_pixels.clamp(1, i32::MAX as usize) as i32;
    let (band_width, band_height) = if width <= max_pixels {
        (width, (max_pixels / width.max(1)).max(1))
    } else {
        (max_pixels, 1)
    };
    (y..y + height)
        .step_by(band_height as usize)
        .flat_map(move |top| {
            let bottom = (top + band_height).min(y + height);
            (x..x + width)
                .step_by(band_width.max(1) as usize)
                .map(move |left| {
                    let right = (left + band_width).min(x + width);
                    Bounds {
                        origin: point(DevicePixels(left), DevicePixels(top)),
                        size: size(DevicePixels(right - left), DevicePixels(bottom - top)),
                    }
                })
        })
}

#[cfg(test)]
mod tests {
    use gpui::{Bounds, DevicePixels, point, size};

    use super::{Owed, split_rect};

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Bounds<DevicePixels> {
        Bounds {
            origin: point(DevicePixels(x), DevicePixels(y)),
            size: size(DevicePixels(w), DevicePixels(h)),
        }
    }

    fn area(rects: &[Bounds<DevicePixels>]) -> i32 {
        rects
            .iter()
            .map(|rect| rect.size.width.0 * rect.size.height.0)
            .sum()
    }

    #[test]
    fn a_rect_under_the_limit_is_one_request() {
        let parts = split_rect(rect(3, 4, 10, 10), 100).collect::<Vec<_>>();
        assert_eq!(parts, vec![rect(3, 4, 10, 10)]);
    }

    #[test]
    fn a_tall_rect_splits_into_bands_of_rows() {
        let parts = split_rect(rect(5, 10, 100, 25), 1000).collect::<Vec<_>>();
        assert_eq!(
            parts,
            vec![
                rect(5, 10, 100, 10),
                rect(5, 20, 100, 10),
                rect(5, 30, 100, 5)
            ]
        );
    }

    #[test]
    fn a_row_longer_than_the_limit_splits_into_pieces() {
        let parts = split_rect(rect(0, 0, 250, 2), 100).collect::<Vec<_>>();
        assert_eq!(
            parts,
            vec![
                rect(0, 0, 100, 1),
                rect(100, 0, 100, 1),
                rect(200, 0, 50, 1),
                rect(0, 1, 100, 1),
                rect(100, 1, 100, 1),
                rect(200, 1, 50, 1),
            ]
        );
    }

    #[test]
    fn parts_stay_under_the_limit_and_cover_the_rect() {
        for (width, height, max) in [(1920, 1080, 65_535), (7, 13, 5), (4096, 3, 1000)] {
            let whole = rect(1, 2, width, height);
            let parts = split_rect(whole, max as usize).collect::<Vec<_>>();
            assert!(
                parts
                    .iter()
                    .all(|part| part.size.width.0 * part.size.height.0 <= max)
            );
            assert!(parts.iter().all(|part| whole.intersect(part) == *part));
            assert_eq!(area(&parts), width * height);
        }
    }

    #[test]
    fn an_empty_rect_is_no_request() {
        assert_eq!(split_rect(rect(0, 0, 0, 10), 100).count(), 0);
        assert_eq!(split_rect(rect(0, 0, 10, 0), 100).count(), 0);
    }

    #[test]
    fn a_frame_uploads_its_damage() {
        let mut owed = Owed::default();
        let upload = owed.upload(&[rect(1, 2, 3, 4), rect(90, 90, 20, 20)], 100, 100);
        assert_eq!(upload.rects(), &[rect(1, 2, 3, 4), rect(90, 90, 10, 10)]);
        let upload = owed.upload(&[], 100, 100);
        assert!(upload.rects().is_empty());
    }

    #[test]
    fn a_gpu_present_landing_after_cpu_frames_has_them_uploaded_again() {
        let mut owed = Owed::default();
        owed.gpu_presented();
        // The GPU's present landed before any CPU frame: nothing to redo.
        assert!(!owed.gpu_landed());
        owed.upload(&[rect(0, 0, 5, 5)], 100, 100);
        owed.upload(&[rect(10, 10, 5, 5)], 100, 100);
        // The GPU's present lands over them: the window is presented again,
        // and that frame, without damage, uploads both.
        assert!(owed.gpu_landed());
        let upload = owed.upload(&[], 100, 100);
        assert_eq!(upload.rects(), &[rect(0, 0, 5, 5), rect(10, 10, 5, 5)]);
        // Once.
        assert!(owed.upload(&[], 100, 100).rects().is_empty());
        // Another queued GPU present lands later: again.
        assert!(owed.gpu_landed());
        let upload = owed.upload(&[rect(50, 50, 1, 1)], 100, 100);
        assert_eq!(
            upload.rects(),
            &[rect(50, 50, 1, 1), rect(0, 0, 5, 5), rect(10, 10, 5, 5)]
        );
    }

    #[test]
    fn a_gpu_present_forgets_what_cpu_frames_uploaded() {
        let mut owed = Owed::default();
        owed.upload(&[rect(0, 0, 5, 5)], 100, 100);
        owed.exposed(rect(20, 20, 5, 5));
        owed.gpu_presented();
        assert!(!owed.gpu_landed());
        assert!(owed.upload(&[], 100, 100).rects().is_empty());
    }

    #[test]
    fn exposed_rects_are_uploaded_with_the_next_frame() {
        let mut owed = Owed::default();
        owed.exposed(rect(0, 0, 50, 20));
        owed.exposed(rect(80, 0, 40, 20));
        // Exposure alone is no reason to present: the client refreshes
        // exposed windows itself.
        assert!(!owed.gpu_landed());
        let upload = owed.upload(&[], 100, 100);
        assert_eq!(upload.rects(), &[rect(0, 0, 50, 20), rect(80, 0, 20, 20)]);
        assert!(owed.upload(&[], 100, 100).rects().is_empty());
        // Exposed rectangles are not what CPU frames uploaded.
        assert!(!owed.gpu_landed());
    }

    #[test]
    fn a_rect_inside_another_is_uploaded_once() {
        let mut owed = Owed::default();
        owed.upload(&[rect(10, 10, 5, 5)], 100, 100);
        assert!(owed.gpu_landed());
        owed.exposed(rect(0, 0, 100, 100));
        owed.exposed(rect(0, 0, 100, 100));
        let upload = owed.upload(&[rect(10, 10, 5, 5), rect(20, 20, 1, 1)], 100, 100);
        assert_eq!(upload.rects(), &[rect(0, 0, 100, 100)]);
    }

    #[test]
    fn rects_uploaded_before_a_resize_are_clipped_to_the_new_size() {
        let mut owed = Owed::default();
        owed.upload(&[rect(60, 60, 30, 30)], 100, 100);
        assert!(owed.gpu_landed());
        let upload = owed.upload(&[], 70, 70);
        assert_eq!(upload.rects(), &[rect(60, 60, 10, 10)]);
    }
}
