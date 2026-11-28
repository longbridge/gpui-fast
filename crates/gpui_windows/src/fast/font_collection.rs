//! Opens the DirectWrite system font collection from the Windows Font Cache
//! without forcing a synchronous scan of system font directories at startup.
//!
//! Upstream passes `checkForUpdates = true` to
//! `IDWriteFactory3::GetSystemFontCollection`, which forces the Windows Font
//! Cache service to synchronously check the registry and font directories for
//! updates before returning. On machines with many installed fonts (such as
//! CJK or developer font packs), that adds tens of milliseconds to cold
//! startup.
//!
//! Passing `checkForUpdates = false` reads the maintained font cache directly.
//! If a newly installed font is not yet in the cached collection,
//! `DirectWriteState::select_and_cache_font` already retries with
//! `checkForUpdates = true` on a lookup miss, so newly installed fonts remain
//! discoverable.

use anyhow::{Context, Result};
use windows::Win32::Graphics::DirectWrite::{IDWriteFactory5, IDWriteFontCollection1};

pub(crate) fn system_font_collection(factory: &IDWriteFactory5) -> Result<IDWriteFontCollection1> {
    let mut result = None;
    unsafe {
        factory.GetSystemFontCollection(false, &mut result, false)?;
    }
    result.context("Failed to get system font collection")
}

#[cfg(test)]
mod tests {
    use crate::direct_write::DirectWriteTextSystem;
    use crate::fast::font_collection::system_font_collection;
    use gpui::{Font, PlatformTextSystem, SharedString};
    use windows::Win32::Graphics::DirectWrite::{
        DWRITE_FACTORY_TYPE_SHARED, DWriteCreateFactory, IDWriteFactory5,
    };

    #[test]
    fn system_font_collection_loads_from_font_cache() {
        let factory: IDWriteFactory5 =
            unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.unwrap();
        let collection = system_font_collection(&factory).unwrap();
        assert!(unsafe { collection.GetFontFamilyCount() } > 0);
    }

    #[test]
    fn text_system_resolves_system_ui_font_without_startup_rescan() {
        let text_system = DirectWriteTextSystem::new().unwrap();
        let font = Font {
            family: SharedString::from(".SystemUIFont"),
            ..Font::default()
        };
        assert!(text_system.font_id(&font).is_ok());
        assert!(!text_system.all_font_names().is_empty());
    }
}
