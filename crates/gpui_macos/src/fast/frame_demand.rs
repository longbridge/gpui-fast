//! Native damage for a frame requested while the compositor is idle.

use std::rc::Rc;

use objc2::runtime::AnyObject;
use objc2_app_kit::NSView;

use crate::display_link::MetalFrames;

pub(crate) struct FrameDemand {
    waker: Option<Rc<dyn Fn()>>,
}

impl FrameDemand {
    /// The view must remain live while the Metal source is attached.
    pub(crate) unsafe fn new(
        frames: Option<&Rc<MetalFrames>>,
        native_view: *mut AnyObject,
    ) -> Self {
        let waker = frames.map(|frames| {
            let frames = Rc::downgrade(frames);
            Rc::new(move || {
                if let Some(frames) = frames.upgrade() {
                    frames.wake();
                    if frames.is_attached() {
                        // Unpausing alone can leave an idle compositor without
                        // native damage, so no Metal callback arrives to draw.
                        // SAFETY: the caller keeps the view live while attached;
                        // MacWindow::drop detaches before closing its native view.
                        let view = unsafe { &*native_view.cast::<NSView>() };
                        view.setNeedsDisplay(true);
                    }
                }
            }) as Rc<dyn Fn()>
        });
        Self { waker }
    }

    pub(crate) fn schedule_frame(&self) {
        if let Some(waker) = &self.waker {
            waker();
        }
    }

    pub(crate) fn frame_waker(&self) -> Option<Rc<dyn Fn()>> {
        self.waker.clone()
    }
}
