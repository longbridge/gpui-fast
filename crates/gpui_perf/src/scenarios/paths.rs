//! Window-sized paths built every frame: a ground with holes, such as a pane
//! layout draws behind cards that cross it, in a shape that changes every
//! frame while the cards move, and in a shape that does not.
//!
//! `paths-moving-holes` builds a path no frame has built before, so what is
//! kept of earlier frames is never drawn again; `paths-still-holes` builds the
//! same path every frame, with sparklines beside it that also repeat.

use gpui::{
    AnyView, App, AppContext as _, Context, Entity, Hsla, IntoElement, PathBuilder, Pixels, Render,
    Window, canvas, div, hsla, point, prelude::*, px,
};

pub fn scenarios() -> Vec<Box<dyn crate::Scenario>> {
    vec![
        Box::new(Paths {
            name: "paths-moving-holes",
            description: "A window-sized fill with 200 holes, every hole moved a little each frame, so each frame's path is a new shape.",
            moving: true,
        }),
        Box::new(Paths {
            name: "paths-still-holes",
            description: "The same window-sized fill with 200 holes, and 400 sparklines, all repainted unchanged each frame.",
            moving: false,
        }),
    ]
}

const HOLES: usize = 200;
const SPARKLINES: usize = 400;

const GROUND: Hsla = hsla(0.62, 0.15, 0.14, 1.0);
const LINE: Hsla = hsla(0.38, 0.55, 0.55, 1.0);

struct Paths {
    name: &'static str,
    description: &'static str,
    moving: bool,
}

impl crate::Scenario for Paths {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn build(&self, _: &mut Window, cx: &mut App) -> AnyView {
        let moving = self.moving;
        cx.new(|_| Scene { frame: 0, moving }).into()
    }

    fn step(&self, root: &AnyView, _: usize, _: &mut Window, cx: &mut App) {
        let scene: Entity<Scene> = root.clone().downcast().unwrap();
        scene.update(cx, |scene, cx| {
            scene.frame += 1;
            cx.notify();
        });
    }
}

struct Scene {
    frame: usize,
    moving: bool,
}

/// A ground covering `width` by `height`, with `HOLES` square holes in rows
/// of 40, shifted by `shift`.
fn ground(width: Pixels, height: Pixels, shift: f32) -> PathBuilder {
    let mut builder = PathBuilder::fill();
    builder.move_to(point(px(0.), px(0.)));
    builder.line_to(point(width, px(0.)));
    builder.line_to(point(width, height));
    builder.line_to(point(px(0.), height));
    builder.close();
    for hole in 0..HOLES {
        let x = 10. + (hole % 40) as f32 * 34. + shift;
        let y = 10. + (hole / 40) as f32 * 50.;
        builder.move_to(point(px(x), px(y)));
        builder.line_to(point(px(x), px(y + 30.)));
        builder.line_to(point(px(x + 30.), px(y + 30.)));
        builder.line_to(point(px(x + 30.), px(y)));
        builder.close();
    }
    builder
}

fn sparkline(at: usize) -> PathBuilder {
    let mut builder = PathBuilder::stroke(px(1.5));
    let (x, y) = ((at % 40) as f32 * 34., 300. + (at / 40) as f32 * 30.);
    builder.move_to(point(px(x), px(y)));
    for step in 1..20 {
        builder.line_to(point(
            px(x + step as f32 * 1.5),
            px(y + ((step * 7) % 11) as f32),
        ));
    }
    builder
}

impl Render for Scene {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let shift = if self.moving {
            self.frame as f32 * 0.002
        } else {
            0.
        };
        let sparklines = !self.moving;
        div().size_full().child(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    if let Ok(path) = ground(bounds.size.width, bounds.size.height, shift).build() {
                        window.paint_path(path, GROUND);
                    }
                    if sparklines {
                        for at in 0..SPARKLINES {
                            if let Ok(path) = sparkline(at).build() {
                                window.paint_path(path, LINE);
                            }
                        }
                    }
                },
            )
            .size_full(),
        )
    }
}
