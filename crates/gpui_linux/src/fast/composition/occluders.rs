//! Conservative X11 occlusion rectangles. Rounded clip group markers are
//! containers, not images: only their drawable descendants contribute bounds.

use gpui::{Bounds, ContentMask, DevicePixels, ScaledPixels, Scene};

/// Bound shape request size for text-heavy overlays, as before clip groups.
const MAX_OCCLUDER_RECTANGLES: usize = 256;

fn collect(
    scene: &Scene,
    opaque_only: bool,
    enclosing: Option<Bounds<ScaledPixels>>,
    rectangles: &mut Vec<Bounds<ScaledPixels>>,
) {
    let clipped = |bounds: Bounds<ScaledPixels>, mask: &ContentMask<ScaledPixels>| {
        let bounds = bounds.intersect(&mask.bounds);
        enclosing.map_or(bounds, |clip| bounds.intersect(&clip))
    };
    rectangles.extend(
        scene
            .quads
            .iter()
            .filter(|quad| !opaque_only || quad.background.is_opaque())
            .map(|quad| clipped(quad.bounds, &quad.content_mask)),
    );
    rectangles.extend(
        scene
            .paths
            .iter()
            .map(|path| clipped(path.bounds, &path.content_mask)),
    );
    rectangles.extend(
        scene
            .underlines
            .iter()
            .map(|line| clipped(line.bounds, &line.content_mask)),
    );
    rectangles.extend(
        scene
            .monochrome_sprites
            .iter()
            .map(|sprite| clipped(sprite.bounds, &sprite.content_mask)),
    );
    rectangles.extend(
        scene
            .subpixel_sprites
            .iter()
            .map(|sprite| clipped(sprite.bounds, &sprite.content_mask)),
    );
    rectangles.extend(
        scene
            .surfaces
            .iter()
            .map(|surface| clipped(surface.bounds, &surface.content_mask)),
    );
    for sprite in &scene.polychrome_sprites {
        let bounds = clipped(sprite.bounds, &sprite.content_mask);
        let group = sprite
            .tile
            .texture_id
            .index
            .checked_sub(0xF000_0000)
            .filter(|_| sprite.tile.texture_id.index >= 0xF100_0000)
            .and_then(|key| {
                scene
                    .layers
                    .frames
                    .iter()
                    .find(|frame| frame.key.0 == key && frame.tile_size == 0)
            });
        if let Some(group) = group {
            if let Some(child) = group.content.scene() {
                collect(child, opaque_only, Some(bounds), rectangles);
            }
        } else {
            rectangles.push(bounds);
        }
    }
}

pub(crate) fn scene_occluders(scene: &Scene, opaque_only: bool) -> Vec<Bounds<DevicePixels>> {
    let mut rectangles = Vec::new();
    collect(scene, opaque_only, None, &mut rectangles);

    let mut rectangles = rectangles
        .into_iter()
        .map(|bounds| {
            let origin = bounds
                .origin
                .map(|value| DevicePixels(value.0.floor() as i32));
            let corner = bounds
                .bottom_right()
                .map(|value| DevicePixels(value.0.ceil() as i32));
            Bounds::from_corners(origin, corner)
        })
        .filter(|bounds| !bounds.is_empty())
        .collect::<Vec<_>>();

    // Content usually sits on a background quad, so most rectangles are
    // covered by a larger one.
    rectangles.sort_by_key(|bounds| std::cmp::Reverse(bounds.size.width.0 * bounds.size.height.0));
    let mut kept: Vec<Bounds<DevicePixels>> = Vec::new();
    for rectangle in rectangles {
        let covered = kept.iter().any(|kept| {
            kept.origin.x <= rectangle.origin.x
                && kept.origin.y <= rectangle.origin.y
                && kept.bottom_right().x >= rectangle.bottom_right().x
                && kept.bottom_right().y >= rectangle.bottom_right().y
        });
        if !covered {
            kept.push(rectangle);
        }
    }
    if kept.len() > MAX_OCCLUDER_RECTANGLES {
        let union = kept
            .iter()
            .copied()
            .reduce(|union, bounds| union.union(&bounds))
            .into_iter()
            .collect();
        return union;
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::scene_occluders;
    use gpui::{
        Bounds, ContentMask, DevicePixels, Quad, ScaledPixels, Scene, Shadow, black, point, size,
    };

    fn quad(bounds: Bounds<ScaledPixels>, mask: Bounds<ScaledPixels>) -> Quad {
        Quad {
            bounds,
            content_mask: ContentMask { bounds: mask },
            ..Default::default()
        }
    }

    fn scaled(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
        Bounds::new(
            point(ScaledPixels(x), ScaledPixels(y)),
            size(ScaledPixels(width), ScaledPixels(height)),
        )
    }

    fn device(x: i32, y: i32, width: i32, height: i32) -> Bounds<DevicePixels> {
        Bounds::new(
            point(DevicePixels(x), DevicePixels(y)),
            size(DevicePixels(width), DevicePixels(height)),
        )
    }

    #[test]
    fn scene_occluders_skip_shadows_and_covered_rectangles() {
        let everything = scaled(0., 0., 1000., 1000.);
        let mut scene = Scene::default();
        scene.insert_primitive(Shadow {
            order: 0,
            blur_radius: ScaledPixels(24.),
            bounds: scaled(500., 500., 100., 100.),
            corner_radii: Default::default(),
            content_mask: ContentMask { bounds: everything },
            color: Default::default(),
            element_bounds: scaled(500., 500., 100., 100.),
            element_corner_radii: Default::default(),
            inset: 0,
            pad: 0,
        });
        scene.insert_primitive(quad(scaled(10., 10., 100., 50.), everything));
        scene.insert_primitive(quad(scaled(20.5, 20.5, 10., 10.), everything));
        scene.insert_primitive(quad(
            scaled(200., 0., 100., 100.),
            scaled(250., 0., 20., 40.),
        ));
        scene.finish();

        let mut occluders = scene_occluders(&scene, false);
        occluders.sort_by_key(|bounds| bounds.origin.x);
        assert_eq!(occluders, [device(10, 10, 100, 50), device(250, 0, 20, 40)]);
    }

    #[test]
    fn opaque_scene_occluders_skip_translucent_backgrounds() {
        let everything = scaled(0., 0., 1000., 1000.);
        let mut scene = Scene::default();
        // A dialog's translucent backdrop over the whole window, and its panel.
        scene.insert_primitive(Quad {
            background: black().opacity(0.4).into(),
            ..quad(everything, everything)
        });
        scene.insert_primitive(Quad {
            background: black().into(),
            ..quad(scaled(300., 200., 400., 300.), everything)
        });
        scene.finish();

        assert_eq!(scene_occluders(&scene, true), [device(300, 200, 400, 300)]);
        assert_eq!(scene_occluders(&scene, false), [device(0, 0, 1000, 1000)]);
    }
    fn group(child: Scene, bounds: Bounds<ScaledPixels>) -> Scene {
        let key = gpui::LayerKey(0x0100_0000);
        let mut scene = Scene::default();
        scene.layers.frames.push(gpui::LayerFrame {
            key,
            generation: 0,
            background: Default::default(),
            tile_size: 0,
            content: child.into(),
            dirty_tiles: Vec::new(),
        });
        scene.insert_primitive(gpui::PolychromeSprite {
            order: 0,
            pad: 0,
            grayscale: false.into(),
            opacity: 1.,
            bounds,
            content_mask: ContentMask { bounds },
            corner_radii: Default::default(),
            tile: gpui::AtlasTile {
                texture_id: gpui::AtlasTextureId {
                    index: 0xF100_0000,
                    kind: gpui::AtlasTextureKind::Polychrome,
                },
                tile_id: gpui::TileId(0),
                padding: 0,
                bounds: Default::default(),
            },
        });
        scene.finish();
        scene
    }

    #[test]
    fn rounded_group_occludes_only_descendant_bounds_intersected_with_its_clip() {
        let mut child = Scene::default();
        let bounds = scaled(45., 45., 20., 20.);
        child.insert_primitive(Quad {
            background: black().into(),
            ..quad(bounds, bounds)
        });
        child.finish();
        let scene = group(
            group(child, scaled(0., 0., 100., 100.)),
            scaled(0., 0., 50., 50.),
        );
        assert_eq!(scene_occluders(&scene, true), [device(45, 45, 5, 5)]);
    }

    #[test]
    fn rounded_group_is_not_itself_an_opaque_image() {
        let mut child = Scene::default();
        let bounds = scaled(10., 10., 20., 20.);
        child.insert_primitive(Quad {
            background: black().opacity(0.4).into(),
            ..quad(bounds, bounds)
        });
        child.finish();
        let scene = group(child, scaled(0., 0., 100., 100.));
        assert!(scene_occluders(&scene, true).is_empty());
        assert_eq!(scene_occluders(&scene, false), [device(10, 10, 20, 20)]);
        assert!(scene_occluders(&group(Scene::default(), bounds), false).is_empty());
    }
}
