//! An application's own fonts in a font's fallback cascade.
//!
//! `Font::fallbacks` names the fonts CoreText falls back to, in order, for
//! characters the font lacks. Upstream puts each in the cascade as a
//! descriptor holding only its family name, which CoreText resolves against
//! the fonts registered with the system. The fonts an application adds with
//! `add_fonts` are never registered there: font-kit keeps them in its memory
//! source. So a fallback naming one of them resolved to nothing, and CoreText
//! drew the characters with a system font instead: an application shipping
//! Noto Sans Hebrew as the fallback of its Latin UI font got Arial Hebrew.
//!
//! The cascade now holds an application font's own descriptor, from the
//! font loaded out of the memory source, in its declared place. Fallbacks
//! naming system fonts are built as before.

use collections::HashMap;
use core_foundation::{
    array::{CFArrayAppendValue, CFMutableArrayRef},
    base::TCFType,
};
use core_text::font_descriptor::CTFontDescriptor;
use font_kit::{family_name::FamilyName, font::Font as FontKitFont, sources::mem::MemSource};
use gpui::{Font, FontFallbacks, FontId};
use smallvec::SmallVec;

use crate::text_system::FontKey;

/// The descriptors of the application's fonts among one font's fallbacks, by
/// the name the fallback list gives them.
pub(crate) struct AppFallbacks(HashMap<String, CTFontDescriptor>);

impl AppFallbacks {
    /// Loads the application's fonts that `fallbacks` names, each in the face
    /// that best matches `font`'s weight and style, as upstream's descriptors
    /// ask CoreText for the face matching the font's traits.
    pub(crate) fn new(
        memory_source: &MemSource,
        fallbacks: Option<&FontFallbacks>,
        font: &FontKitFont,
    ) -> Self {
        let mut descriptors = HashMap::default();
        let names = fallbacks.map_or(&[][..], |fallbacks| fallbacks.fallback_list());
        let mut properties = None;
        for name in names {
            if memory_source.select_family_by_name(name).is_err() {
                continue;
            }
            let properties = *properties.get_or_insert_with(|| font.properties());
            let loaded = memory_source
                .select_best_match(&[FamilyName::Title(name.clone())], &properties)
                .ok()
                .and_then(|handle| handle.load().ok());
            if let Some(loaded) = loaded {
                descriptors.insert(name.clone(), loaded.native_font().copy_descriptor());
            }
        }
        Self(descriptors)
    }

    /// Appends the descriptor of the application font named `name` to the
    /// cascade being built, returning whether `name` is one.
    pub(crate) fn append(this: &Self, fallback_array: CFMutableArrayRef, name: &str) -> bool {
        let Some(descriptor) = this.0.get(name) else {
            return false;
        };
        // SAFETY: the array retains the descriptor (it is created with
        // kCFTypeArrayCallBacks), which `this` keeps alive until then.
        unsafe { CFArrayAppendValue(fallback_array, descriptor.as_concrete_TypeRef() as _) };
        true
    }
}

/// Forgets the fonts chosen for requests with fallbacks after the
/// application adds fonts, whose cascades may name a font only now added.
/// They are loaded again with new font IDs on their next request; the old
/// IDs stay valid for lines already laid out with them.
pub(crate) fn forget_cascades(
    font_selections: &mut HashMap<Font, FontId>,
    font_ids_by_font_key: &mut HashMap<FontKey, SmallVec<[FontId; 4]>>,
) {
    font_selections.retain(|font, _| font.fallbacks.is_none());
    font_ids_by_font_key.retain(|key, _| key.font_fallbacks.is_none());
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use gpui::{FontFallbacks, FontRun, PlatformTextSystem, font, px};

    use crate::AppleTextSystem;

    const LILEX: &[u8] = include_bytes!("../../../../assets/fonts/lilex/Lilex-Regular.ttf");
    const PLEX: &[u8] =
        include_bytes!("../../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf");

    /// Lilex draws `TEXT`, a box-drawing line, and IBM Plex Sans does not.
    /// Neither is a font the system has, so a line drawn with Lilex was
    /// drawn by the application's copy of it.
    const TEXT: &str = "\u{2500}";

    fn with_fallbacks(family: &str, fallbacks: &[&str]) -> gpui::Font {
        let mut font = font(family);
        font.fallbacks = Some(FontFallbacks::from_fonts(
            fallbacks.iter().map(|name| name.to_string()).collect(),
        ));
        font
    }

    fn drawn_with(fonts: &AppleTextSystem, font: &gpui::Font) -> Vec<gpui::FontId> {
        let font_id = fonts.font_id(font).unwrap();
        let runs = [FontRun {
            font_id,
            len: TEXT.len(),
        }];
        let layout = fonts.layout_line(TEXT, px(16.), &runs);
        layout.runs.iter().map(|run| run.font_id).collect()
    }

    #[test]
    fn an_application_font_named_as_a_fallback_draws_what_the_font_lacks() {
        let fonts = AppleTextSystem::new();
        fonts
            .add_fonts(vec![Cow::Borrowed(PLEX), Cow::Borrowed(LILEX)])
            .unwrap();
        let plex = fonts.font_id(&font("IBM Plex Sans")).unwrap();
        let lilex = fonts.font_id(&font("Lilex")).unwrap();
        let ch = TEXT.chars().next().unwrap();
        assert!(fonts.glyph_for_char(plex, ch).is_none());
        assert!(fonts.glyph_for_char(lilex, ch).is_some());

        assert_eq!(
            drawn_with(&fonts, &with_fallbacks("IBM Plex Sans", &["Lilex"])),
            [lilex]
        );
    }

    #[test]
    fn application_and_system_fallbacks_keep_their_declared_order() {
        let fonts = AppleTextSystem::new();
        fonts
            .add_fonts(vec![Cow::Borrowed(PLEX), Cow::Borrowed(LILEX)])
            .unwrap();
        let lilex = fonts.font_id(&font("Lilex")).unwrap();
        let menlo = fonts.font_id(&font("Menlo")).unwrap();
        let ch = TEXT.chars().next().unwrap();
        assert!(fonts.glyph_for_char(menlo, ch).is_some());

        let system_first = with_fallbacks("IBM Plex Sans", &["Menlo", "Lilex"]);
        assert_eq!(drawn_with(&fonts, &system_first), [menlo]);
        let application_first = with_fallbacks("IBM Plex Sans", &["Lilex", "Menlo"]);
        assert_eq!(drawn_with(&fonts, &application_first), [lilex]);
    }

    #[test]
    fn a_fallback_added_after_the_font_was_chosen_joins_its_cascade() {
        let fonts = AppleTextSystem::new();
        fonts.add_fonts(vec![Cow::Borrowed(PLEX)]).unwrap();
        let request = with_fallbacks("IBM Plex Sans", &["Lilex"]);
        let before = fonts.font_id(&request).unwrap();

        fonts.add_fonts(vec![Cow::Borrowed(LILEX)]).unwrap();
        let lilex = fonts.font_id(&font("Lilex")).unwrap();
        let after = fonts.font_id(&request).unwrap();
        assert_ne!(before, after, "the font was not chosen again");
        assert_eq!(drawn_with(&fonts, &request), [lilex]);
        assert!(
            fonts.font_metrics(before).units_per_em > 0,
            "a font ID handed out before stays valid"
        );

        let unaffected = fonts.font_id(&font("IBM Plex Sans")).unwrap();
        fonts.add_fonts(vec![Cow::Borrowed(LILEX)]).unwrap();
        assert_eq!(
            fonts.font_id(&font("IBM Plex Sans")).unwrap(),
            unaffected,
            "a font without fallbacks is not chosen again"
        );
    }
}
