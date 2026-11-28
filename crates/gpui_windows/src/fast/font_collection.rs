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
    use std::process::Command;
    use std::time::Instant;
    use windows::Win32::Graphics::DirectWrite::{
        DWRITE_FACTORY_TYPE_SHARED, DWriteCreateFactory, IDWriteFactory5, IDWriteFontCollection1,
    };

    const BENCH_CHILD_ENV: &str = "GPUI_BENCH_DWRITE_CHILD";

    fn family_names(collection: &IDWriteFontCollection1) -> Vec<String> {
        let count = unsafe { collection.GetFontFamilyCount() };
        let mut names = Vec::with_capacity(count as usize);
        for index in 0..count {
            let family = unsafe { collection.GetFontFamily(index) }.unwrap();
            let localized = unsafe { family.GetFamilyNames() }.unwrap();
            let len = unsafe { localized.GetStringLength(0) }.unwrap();
            let mut buf = vec![0u16; (len + 1) as usize];
            unsafe { localized.GetString(0, &mut buf) }.unwrap();
            names.push(String::from_utf16_lossy(&buf[..len as usize]));
        }
        names.sort_unstable();
        names
    }

    #[test]
    fn system_font_collection_loads_from_font_cache() {
        let factory: IDWriteFactory5 =
            unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.unwrap();
        let collection = system_font_collection(&factory).unwrap();
        assert!(unsafe { collection.GetFontFamilyCount() } > 0);
    }

    #[test]
    fn system_font_collection_matches_checked_collection_families() {
        let factory: IDWriteFactory5 =
            unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.unwrap();
        let cached = system_font_collection(&factory).unwrap();
        let mut checked = None;
        unsafe {
            factory
                .GetSystemFontCollection(false, &mut checked, true)
                .unwrap();
        }
        let checked = checked.unwrap();
        assert_eq!(family_names(&cached), family_names(&checked));
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

    #[test]
    #[ignore = "manual multi-process startup benchmark"]
    #[allow(clippy::disallowed_methods)]
    fn bench_startup_font_collection() {
        if let Ok(mode) = std::env::var(BENCH_CHILD_ENV) {
            let check_for_updates = mode == "true";
            let factory: IDWriteFactory5 =
                unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.unwrap();
            let start = Instant::now();
            let mut collection = None;
            unsafe {
                factory
                    .GetSystemFontCollection(false, &mut collection, check_for_updates)
                    .unwrap();
            }
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            let count = unsafe { collection.unwrap().GetFontFamilyCount() };
            println!("RESULT={elapsed_ms:.3},{count}");
            return;
        }

        let exe = std::env::current_exe().unwrap();
        let runs = 10;
        let run_once = |mode: &str| -> (f64, u32) {
            let output = Command::new(&exe)
                .args([
                    "--exact",
                    "fast::font_collection::tests::bench_startup_font_collection",
                    "--ignored",
                    "--nocapture",
                ])
                .env(BENCH_CHILD_ENV, mode)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "child failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            let line = stdout
                .lines()
                .find_map(|line| line.strip_prefix("RESULT="))
                .expect("missing RESULT line");
            let (ms, count) = line.split_once(',').expect("expected ms,count");
            (ms.parse().unwrap(), count.parse().unwrap())
        };

        let mut checked = Vec::with_capacity(runs);
        let mut cached = Vec::with_capacity(runs);
        let mut families_checked = 0;
        let mut families_cached = 0;
        for _ in 0..runs {
            let (ms_true, c_true) = run_once("true");
            checked.push(ms_true);
            families_checked = c_true;
            let (ms_false, c_false) = run_once("false");
            cached.push(ms_false);
            families_cached = c_false;
        }
        let summarize = |values: &mut [f64]| {
            values.sort_by(f64::total_cmp);
            let avg = values.iter().sum::<f64>() / values.len() as f64;
            let median = (values[values.len() / 2 - 1] + values[values.len() / 2]) / 2.0;
            (avg, median, values[0], values[values.len() - 1])
        };
        let (t_avg, t_med, t_min, t_max) = summarize(&mut checked);
        let (f_avg, f_med, f_min, f_max) = summarize(&mut cached);
        println!(
            "GetSystemFontCollection(checkForUpdates=true)  ({runs} procs): avg={t_avg:.2} ms median={t_med:.2} ms min={t_min:.2} ms max={t_max:.2} ms families={families_checked}"
        );
        println!(
            "GetSystemFontCollection(checkForUpdates=false) ({runs} procs): avg={f_avg:.2} ms median={f_med:.2} ms min={f_min:.2} ms max={f_max:.2} ms families={families_cached}"
        );
        assert_eq!(families_checked, families_cached);
    }
}
