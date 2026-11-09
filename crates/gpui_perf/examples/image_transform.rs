//! Images painted through a transformation matrix with
//! `Window::paint_image_transformed`.
//!
//! The same portrait image, with rounded corners, four times: painted by
//! `paint_image`, by `paint_image_transformed` with the unit matrix (the two
//! must look the same), turned a quarter about its centre, and turning
//! continuously. The rounded corners turn with the image.
//!
//! `cargo run -p gpui_perf --example image_transform`

extern crate gpui_fast as gpui;
extern crate gpui_platform_fast as gpui_platform;

use std::f32::consts::FRAC_PI_2;
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    App, Bounds, Context, Corners, Pixels, Point, Radians, RenderImage, ScaledPixels,
    TransformationMatrix, Window, WindowBounds, WindowOptions, canvas, div, prelude::*, px, rgb,
    size,
};
use image::{Frame, RgbaImage};
use smallvec::SmallVec;

const CARD: (u32, u32) = (126, 176);

/// How a card is painted.
#[derive(Clone, Copy)]
enum Paint {
    Image,
    Unit,
    QuarterTurn,
    Turning,
}

impl Paint {
    fn label(self) -> &'static str {
        match self {
            Paint::Image => "paint_image",
            Paint::Unit => "transformed, unit matrix",
            Paint::QuarterTurn => "transformed, quarter turn",
            Paint::Turning => "transformed, turning",
        }
    }
}

struct Example {
    card: Arc<RenderImage>,
    start: Instant,
}

impl Render for Example {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        window.request_animation_frame();
        let angle = self.start.elapsed().as_secs_f32() * 0.8;
        div()
            .font_family(".SystemUIFont")
            .size_full()
            .flex()
            .flex_row()
            .gap_6()
            .p_8()
            .bg(rgb(0x2a2d33))
            .text_color(rgb(0xe8e8e8))
            .text_sm()
            .justify_center()
            .items_center()
            .children(
                [
                    Paint::Image,
                    Paint::Unit,
                    Paint::QuarterTurn,
                    Paint::Turning,
                ]
                .map(|paint| self.cell(paint, angle)),
            )
    }
}

impl Example {
    fn cell(&self, paint: Paint, angle: f32) -> impl IntoElement {
        let card = self.card.clone();
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .child(
                // A square cell, so a card turned a quarter about its centre
                // stays inside it.
                div().size(px(CARD.1 as f32)).bg(rgb(0x3a3e46)).child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, _, window, _| paint_card(paint, angle, card, bounds, window),
                    )
                    .size_full(),
                ),
            )
            .child(paint.label())
    }
}

fn paint_card(
    paint: Paint,
    angle: f32,
    card: Arc<RenderImage>,
    cell: Bounds<Pixels>,
    window: &mut Window,
) {
    let card_size = size(px(CARD.0 as f32), px(CARD.1 as f32));
    let bounds = Bounds::centered_at(cell.center(), card_size);
    let radii = Corners::all(px(14.));
    let turn = |angle: f32| {
        // The matrix applies to device pixels, so the centre is scaled.
        let centre = cell.center().scale(window.scale_factor());
        let back = Point::new(ScaledPixels(-centre.x.0), ScaledPixels(-centre.y.0));
        TransformationMatrix::unit()
            .translate(centre)
            .rotate(Radians(angle))
            .translate(back)
    };
    let result = match paint {
        Paint::Image => window.paint_image(bounds, bounds, radii, card, 0, false),
        Paint::Unit => window.paint_image_transformed(
            bounds,
            bounds,
            radii,
            card,
            0,
            false,
            TransformationMatrix::unit(),
        ),
        Paint::QuarterTurn => {
            let matrix = turn(FRAC_PI_2);
            window.paint_image_transformed(bounds, bounds, radii, card, 0, false, matrix)
        }
        Paint::Turning => {
            let matrix = turn(angle);
            window.paint_image_transformed(bounds, bounds, radii, card, 0, false, matrix)
        }
    };
    if let Err(error) = result {
        eprintln!("could not paint the card: {error}");
    }
}

/// A portrait card, readable when turned: a dark title band at the top, a
/// diagonal gradient, and a light square in the bottom left corner.
fn card_image() -> Arc<RenderImage> {
    let (width, height) = CARD;
    let image = RgbaImage::from_fn(width, height, |x, y| {
        let (r, g, b) = if y < 30 {
            (40, 40, 52)
        } else if x < 40 && y > height - 50 {
            (245, 240, 220)
        } else {
            let t = (x + y) as f32 / (width + height) as f32;
            (
                (200. - 120. * t) as u8,
                (90. + 100. * t) as u8,
                (60. + 170. * t) as u8,
            )
        };
        // `RenderImage` holds BGRA pixels.
        image::Rgba([b, g, r, 255])
    });
    Arc::new(RenderImage::new(SmallVec::from_elem(Frame::new(image), 1)))
}

fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(880.), px(320.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|_| Example {
                    card: card_image(),
                    start: Instant::now(),
                })
            },
        )
        .expect("window opened");
        cx.activate(true);
    });
}
