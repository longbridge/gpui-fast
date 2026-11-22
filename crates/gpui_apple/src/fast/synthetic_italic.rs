use collections::{HashMap, HashSet};
use core_foundation::base::TCFType;
use core_graphics::geometry::CGAffineTransform;
use core_text::{
    font::{CTFont, CTFontRef},
    font_descriptor::CTFontDescriptorRef,
};
use font_kit::{font::Font, properties::Style};
use gpui::{FontId, FontStyle};
use pathfinder_geometry::transform2d::Transform2F;

unsafe extern "C" {
    fn CTFontCreateCopyWithAttributes(
        font: CTFontRef,
        size: f64,
        matrix: *const CGAffineTransform,
        attributes: CTFontDescriptorRef,
    ) -> CTFontRef;
}

#[derive(Default)]
pub(crate) struct Italics {
    variants: HashMap<FontId, FontId>,
    synthetic: HashSet<FontId>,
}

impl Italics {
    pub(crate) fn select(&mut self, fonts: &mut Vec<Font>, base: FontId, italic: bool) -> FontId {
        if !italic || fonts[base.0].properties().style != Style::Normal {
            return base;
        }
        *self.variants.entry(base).or_insert_with(|| {
            let id = FontId(fonts.len());
            fonts.push(fonts[base.0].clone());
            self.synthetic.insert(id);
            id
        })
    }
    pub(crate) fn contains(&self, id: FontId) -> bool {
        self.synthetic.contains(&id)
    }
}

pub(crate) fn requested(style: FontStyle) -> bool {
    matches!(style, FontStyle::Italic | FontStyle::Oblique)
}

pub(crate) fn sized(font: &Font, size: f64, italic: bool) -> CTFont {
    let native = font.native_font();
    if !italic {
        return native.clone_with_font_size(size);
    }
    let matrix = CGAffineTransform::new(1., 0., 12_f64.to_radians().tan(), 1., 0., 0.);
    unsafe {
        CTFont::wrap_under_create_rule(CTFontCreateCopyWithAttributes(
            native.as_concrete_TypeRef(),
            size,
            &matrix,
            std::ptr::null(),
        ))
    }
}

pub(crate) fn raster_transform(scale: f32, italic: bool) -> Transform2F {
    if italic {
        Transform2F::row_major(scale, -scale * 12_f32.to_radians().tan(), 0., scale, 0., 0.)
    } else {
        Transform2F::from_scale(scale)
    }
}

pub(crate) fn record_range(
    ranges: &mut Vec<std::ops::Range<isize>>,
    range: std::ops::Range<isize>,
    font: &Font,
    synthetic: bool,
) {
    if synthetic || font.properties().style != Style::Normal {
        ranges.push(range);
    }
}

#[cfg(test)]
mod tests {
    use crate::AppleTextSystem;
    use gpui::{FontRun, FontStyle, PlatformTextSystem, font, px};

    #[test]
    fn pingfang_and_fallback_have_distinct_italic_glyphs() {
        let system = AppleTextSystem::new();
        for family in ["PingFang SC", "Helvetica"] {
            let shape = |style| {
                let mut request = font(family);
                request.style = style;
                let font_id = system.font_id(&request).unwrap();
                system.layout_line(
                    "中",
                    px(24.),
                    &[FontRun {
                        font_id,
                        len: "中".len(),
                    }],
                )
            };
            let upright = shape(FontStyle::Normal);
            for style in [FontStyle::Italic, FontStyle::Oblique] {
                let italic = shape(style);
                assert_ne!(upright.runs[0].font_id, italic.runs[0].font_id);
                assert_eq!(upright.runs[0].glyphs[0].id, italic.runs[0].glyphs[0].id);
                assert_eq!(upright.width, italic.width);
            }
        }
    }
}
