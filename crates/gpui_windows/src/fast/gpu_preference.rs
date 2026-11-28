//! Orders DXGI adapters using `IDXGIFactory6::EnumAdapterByGpuPreference`.
//!
//! Upstream enumerates adapters through the legacy `EnumAdapters`, which on
//! hybrid-GPU (Optimus / SmartShift) laptops can pick or wake a discrete GPU
//! out of D3 sleep on startup.
//!
//! `EnumAdapterByGpuPreference` requests `DXGI_GPU_PREFERENCE_MINIMUM_POWER`
//! (which Windows Graphics Settings per-app preferences still override).
//! Software WARP adapters (`DXGI_ADAPTER_FLAG_SOFTWARE`) are deferred until
//! after all hardware adapters so a software driver listed early by DXGI is
//! only used when no hardware adapter can create a compatible D3D11 device.
//! If `EnumAdapterByGpuPreference` fails (either immediately or partway
//! through enumeration) or yields no adapters, legacy `EnumAdapters` is used
//! to fill in any remaining unseen adapters.

use anyhow::Result;
use windows::{
    Win32::Graphics::Dxgi::{
        DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND, DXGI_GPU_PREFERENCE,
        DXGI_GPU_PREFERENCE_MINIMUM_POWER, IDXGIAdapter1, IDXGIFactory6,
    },
    core::{Error, Interface},
};

pub(crate) fn adapters(
    dxgi_factory: &IDXGIFactory6,
) -> impl Iterator<Item = Result<IDXGIAdapter1>> {
    let (ordered, terminal_error) =
        collect_adapters(dxgi_factory, DXGI_GPU_PREFERENCE_MINIMUM_POWER);
    ordered
        .into_iter()
        .map(Ok)
        .chain(std::iter::once(Err(terminal_error.into())))
}

fn collect_adapters(
    dxgi_factory: &IDXGIFactory6,
    preference: DXGI_GPU_PREFERENCE,
) -> (Vec<IDXGIAdapter1>, Error) {
    let (preferred, preferred_err) = enumerate_by_preference(dxgi_factory, preference);
    let (merged, fallback_err) = merge_with_fallback(
        preferred,
        preferred_err,
        |error| {
            log::warn!(
                "EnumAdapterByGpuPreference failed ({error}); falling back to legacy EnumAdapters"
            );
        },
        || enumerate_legacy(dxgi_factory),
        adapter_luid,
    );

    let terminal_error = fallback_err.unwrap_or_else(|| Error::from(DXGI_ERROR_NOT_FOUND));
    (order_hardware_first(merged), terminal_error)
}

fn merge_with_fallback<T, E, K: Eq>(
    mut preferred: Vec<T>,
    preferred_err: Option<E>,
    on_preferred_err: impl FnOnce(&E),
    fallback: impl FnOnce() -> (Vec<T>, Option<E>),
    mut key: impl FnMut(&T) -> Option<K>,
) -> (Vec<T>, Option<E>) {
    let need_fallback = preferred.is_empty() || preferred_err.is_some();
    if !need_fallback {
        return (preferred, None);
    }

    if let Some(error) = preferred_err.as_ref() {
        on_preferred_err(error);
    }

    let (legacy, legacy_err) = fallback();
    if preferred.is_empty() {
        return (legacy, legacy_err);
    }

    let mut seen_keys = Vec::with_capacity(preferred.len());
    for item in &preferred {
        if let Some(id) = key(item) {
            seen_keys.push(id);
        }
    }
    for item in legacy {
        let duplicate = key(&item).is_some_and(|id| seen_keys.contains(&id));
        if !duplicate {
            if let Some(id) = key(&item) {
                seen_keys.push(id);
            }
            preferred.push(item);
        }
    }
    (preferred, legacy_err)
}

fn enumerate_by_preference(
    dxgi_factory: &IDXGIFactory6,
    preference: DXGI_GPU_PREFERENCE,
) -> (Vec<IDXGIAdapter1>, Option<Error>) {
    let mut adapters = Vec::new();
    for adapter_index in 0.. {
        match unsafe {
            dxgi_factory.EnumAdapterByGpuPreference::<IDXGIAdapter1>(adapter_index, preference)
        } {
            Ok(adapter) => adapters.push(adapter),
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => return (adapters, None),
            Err(error) => return (adapters, Some(error)),
        }
    }
    unreachable!()
}

fn enumerate_legacy(dxgi_factory: &IDXGIFactory6) -> (Vec<IDXGIAdapter1>, Option<Error>) {
    let mut adapters = Vec::new();
    for adapter_index in 0.. {
        match unsafe { dxgi_factory.EnumAdapters(adapter_index) } {
            Ok(adapter) => match adapter.cast::<IDXGIAdapter1>() {
                Ok(adapter) => adapters.push(adapter),
                Err(error) => return (adapters, Some(error)),
            },
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => return (adapters, None),
            Err(error) => return (adapters, Some(error)),
        }
    }
    unreachable!()
}

fn adapter_luid(adapter: &IDXGIAdapter1) -> Option<(i32, u32)> {
    unsafe { adapter.GetDesc1() }
        .ok()
        .map(|desc| (desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart))
}

fn is_software_adapter(adapter: &IDXGIAdapter1) -> bool {
    unsafe { adapter.GetDesc1() }
        .map(|desc| (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0)
        .unwrap_or(false)
}

fn order_hardware_first(adapters: Vec<IDXGIAdapter1>) -> Vec<IDXGIAdapter1> {
    partition_hardware_first(adapters, is_software_adapter)
}

fn partition_hardware_first<T>(items: Vec<T>, mut is_software: impl FnMut(&T) -> bool) -> Vec<T> {
    let mut hardware = Vec::with_capacity(items.len());
    let mut software = Vec::new();
    for item in items {
        if is_software(&item) {
            software.push(item);
        } else {
            hardware.push(item);
        }
    }
    hardware.extend(software);
    hardware
}

#[cfg(test)]
mod tests {
    use crate::directx_devices::DirectXDevices;
    use crate::fast::gpu_preference::{
        adapters, is_software_adapter, merge_with_fallback, partition_hardware_first,
    };
    use std::process::Command;
    use std::time::Instant;
    use windows::{
        Win32::{
            Foundation::HMODULE,
            Graphics::{
                Direct3D::{
                    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_1,
                    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
                },
                Direct3D11::{
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
                    ID3D11Device, ID3D11DeviceContext,
                },
                Dxgi::{
                    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter1, IDXGIFactory6,
                },
            },
        },
        core::Interface,
    };

    const BENCH_CHILD_ENV: &str = "GPUI_BENCH_DXGI_CHILD";

    #[test]
    fn partition_defers_software_adapters_after_hardware() {
        let items = vec![
            ("hw0", false),
            ("warp0", true),
            ("hw1", false),
            ("warp1", true),
        ];
        let ordered = partition_hardware_first(items, |item| item.1);
        let names: Vec<&str> = ordered.into_iter().map(|item| item.0).collect();
        assert_eq!(names, vec!["hw0", "hw1", "warp0", "warp1"]);

        let software_only = vec![("warp0", true)];
        let ordered_software = partition_hardware_first(software_only, |item| item.1);
        assert_eq!(ordered_software, vec![("warp0", true)]);
    }

    #[test]
    fn merge_with_fallback_skips_legacy_when_preference_succeeds() {
        let mut fallback_called = false;
        let (merged, err) = merge_with_fallback(
            vec![(1u32, "igpu"), (2u32, "dgpu")],
            None::<&str>,
            |_| unreachable!(),
            || {
                fallback_called = true;
                (Vec::new(), None)
            },
            |item| Some(item.0),
        );
        assert!(!fallback_called);
        assert!(err.is_none());
        assert_eq!(merged, vec![(1, "igpu"), (2, "dgpu")]);
    }

    #[test]
    fn merge_with_fallback_appends_unseen_legacy_adapters_on_partial_failure() {
        let mut warned = false;
        let (merged, err) = merge_with_fallback(
            vec![(1u32, "warp", true)],
            Some("unexpected dxgi error"),
            |_| warned = true,
            || {
                (
                    vec![
                        (1u32, "warp", true),
                        (2u32, "igpu", false),
                        (3u32, "dgpu", false),
                    ],
                    None,
                )
            },
            |item| Some(item.0),
        );
        assert!(warned);
        assert!(err.is_none());
        let ordered = partition_hardware_first(merged, |item| item.2);
        let names: Vec<&str> = ordered.into_iter().map(|item| item.1).collect();
        assert_eq!(names, vec!["igpu", "dgpu", "warp"]);
    }

    #[test]
    fn merge_with_fallback_uses_legacy_when_preference_yields_nothing() {
        let (merged, err) = merge_with_fallback(
            Vec::<(u32, &str)>::new(),
            Some("unsupported"),
            |_| {},
            || (vec![(1, "dgpu")], None),
            |item| Some(item.0),
        );
        assert!(err.is_none());
        assert_eq!(merged, vec![(1, "dgpu")]);
    }

    #[test]
    fn adapters_yields_hardware_before_software_and_creates_devices() {
        let factory: IDXGIFactory6 =
            unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS::default()) }.unwrap();
        let enumerated: Vec<_> = adapters(&factory)
            .take_while(Result::is_ok)
            .map(Result::unwrap)
            .collect();
        assert!(!enumerated.is_empty());

        let first_software = enumerated.iter().position(is_software_adapter);
        if let Some(first_sw) = first_software {
            assert!(
                enumerated[first_sw..].iter().all(is_software_adapter),
                "hardware adapter appeared after a software adapter"
            );
        }

        assert!(DirectXDevices::new().is_ok());
    }

    #[test]
    #[ignore = "manual multi-process startup benchmark"]
    #[allow(clippy::disallowed_methods)]
    fn bench_dxgi_adapter_startup() {
        if let Ok(mode) = std::env::var(BENCH_CHILD_ENV) {
            let start = Instant::now();
            let factory: IDXGIFactory6 =
                unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS::default()) }.unwrap();
            let adapter: IDXGIAdapter1 = if mode == "legacy" {
                unsafe { factory.EnumAdapters(0).unwrap().cast().unwrap() }
            } else {
                adapters(&factory).next().unwrap().unwrap()
            };
            let desc = unsafe { adapter.GetDesc1() }.unwrap();
            let gpu = String::from_utf16_lossy(&desc.Description)
                .trim_matches('\0')
                .to_string();
            let mut device: Option<ID3D11Device> = None;
            let mut ctx: Option<ID3D11DeviceContext> = None;
            let mut fl = D3D_FEATURE_LEVEL::default();
            unsafe {
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&[
                        D3D_FEATURE_LEVEL_11_1,
                        D3D_FEATURE_LEVEL_11_0,
                        D3D_FEATURE_LEVEL_10_1,
                    ]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    Some(&mut fl),
                    Some(&mut ctx),
                )
                .unwrap();
            }
            let total_ms = start.elapsed().as_secs_f64() * 1000.0;
            println!("RESULT={total_ms:.3}|{gpu}");
            return;
        }

        let exe = std::env::current_exe().unwrap();
        let runs = 10;
        let run_once = |mode: &str| -> (f64, String) {
            let output = Command::new(&exe)
                .args([
                    "--exact",
                    "fast::gpu_preference::tests::bench_dxgi_adapter_startup",
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
            let (ms, gpu) = line.split_once('|').expect("expected ms|gpu");
            (ms.parse().unwrap(), gpu.to_string())
        };

        let mut legacy = Vec::with_capacity(runs);
        let mut minimum_power = Vec::with_capacity(runs);
        let mut legacy_gpu = String::new();
        let mut min_power_gpu = String::new();
        for _ in 0..runs {
            let (ms_l, gpu_l) = run_once("legacy");
            legacy.push(ms_l);
            legacy_gpu = gpu_l;
            let (ms_m, gpu_m) = run_once("minimum_power");
            minimum_power.push(ms_m);
            min_power_gpu = gpu_m;
        }
        let summarize = |values: &mut [f64]| {
            values.sort_by(f64::total_cmp);
            let avg = values.iter().sum::<f64>() / values.len() as f64;
            let median = (values[values.len() / 2 - 1] + values[values.len() / 2]) / 2.0;
            (avg, median, values[0], values[values.len() - 1])
        };
        let (l_avg, l_med, l_min, l_max) = summarize(&mut legacy);
        let (m_avg, m_med, m_min, m_max) = summarize(&mut minimum_power);
        println!(
            "legacy        ({runs} procs, GPU={legacy_gpu:?}): avg={l_avg:.2} ms median={l_med:.2} ms min={l_min:.2} ms max={l_max:.2} ms"
        );
        println!(
            "minimum_power ({runs} procs, GPU={min_power_gpu:?}): avg={m_avg:.2} ms median={m_med:.2} ms min={m_min:.2} ms max={m_max:.2} ms"
        );
    }
}
