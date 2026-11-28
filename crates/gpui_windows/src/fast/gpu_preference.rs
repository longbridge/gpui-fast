//! Orders DXGI adapters using `IDXGIFactory6::EnumAdapterByGpuPreference`.
//!
//! Upstream enumerates adapters through the legacy `EnumAdapters`, which on
//! hybrid-GPU (Optimus / SmartShift) laptops often picks or wakes the discrete
//! GPU out of D3 sleep on startup, adding ~200-300 ms to `D3D11CreateDevice`
//! and extra idle power draw.
//!
//! `EnumAdapterByGpuPreference` defaults to `DXGI_GPU_PREFERENCE_MINIMUM_POWER`
//! (which still respects per-app overrides configured in Windows Graphics
//! Settings) and can be overridden via `GPUI_GPU_PREFERENCE`
//! (`minimum_power`, `high_performance`, or `unspecified`). Software WARP
//! adapters (`DXGI_ADAPTER_FLAG_SOFTWARE`) are deferred until after all
//! hardware adapters so a software driver listed early by DXGI is only used
//! when no hardware adapter can create a compatible D3D11 device.

use anyhow::Result;
use windows::{
    Win32::Graphics::Dxgi::{
        DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND, DXGI_GPU_PREFERENCE,
        DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE, DXGI_GPU_PREFERENCE_MINIMUM_POWER,
        DXGI_GPU_PREFERENCE_UNSPECIFIED, IDXGIAdapter1, IDXGIFactory6,
    },
    core::{Error, Interface},
};

const GPU_PREFERENCE_ENV_VAR: &str = "GPUI_GPU_PREFERENCE";

pub(crate) fn adapters(
    dxgi_factory: &IDXGIFactory6,
) -> impl Iterator<Item = Result<IDXGIAdapter1>> {
    let preference = gpu_preference_from_env();
    let (ordered, terminal_error) = collect_adapters(dxgi_factory, preference);
    ordered
        .into_iter()
        .map(Ok)
        .chain(std::iter::once(Err(terminal_error.into())))
}

fn gpu_preference_from_env() -> DXGI_GPU_PREFERENCE {
    let raw = std::env::var(GPU_PREFERENCE_ENV_VAR).ok();
    parse_gpu_preference(raw.as_deref())
}

fn parse_gpu_preference(value: Option<&str>) -> DXGI_GPU_PREFERENCE {
    let Some(trimmed) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return DXGI_GPU_PREFERENCE_MINIMUM_POWER;
    };
    if trimmed.eq_ignore_ascii_case("minimum_power") {
        DXGI_GPU_PREFERENCE_MINIMUM_POWER
    } else if trimmed.eq_ignore_ascii_case("high_performance") {
        DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE
    } else if trimmed.eq_ignore_ascii_case("unspecified") {
        DXGI_GPU_PREFERENCE_UNSPECIFIED
    } else {
        log::warn!(
            "Unknown {GPU_PREFERENCE_ENV_VAR}={trimmed:?}; falling back to minimum_power \
             (expected minimum_power, high_performance, or unspecified)"
        );
        DXGI_GPU_PREFERENCE_MINIMUM_POWER
    }
}

fn collect_adapters(
    dxgi_factory: &IDXGIFactory6,
    preference: DXGI_GPU_PREFERENCE,
) -> (Vec<IDXGIAdapter1>, Error) {
    let (preferred, preferred_err) = enumerate_by_preference(dxgi_factory, preference);
    if !preferred.is_empty() {
        return (
            order_hardware_first(preferred),
            Error::from(DXGI_ERROR_NOT_FOUND),
        );
    }

    if let Some(error) = preferred_err {
        log::warn!(
            "EnumAdapterByGpuPreference failed ({error}); falling back to legacy EnumAdapters"
        );
    }

    let (legacy, legacy_err) = enumerate_legacy(dxgi_factory);
    let terminal_error = legacy_err.unwrap_or_else(|| Error::from(DXGI_ERROR_NOT_FOUND));
    (order_hardware_first(legacy), terminal_error)
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
        adapters, is_software_adapter, parse_gpu_preference, partition_hardware_first,
    };
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
        DXGI_GPU_PREFERENCE_MINIMUM_POWER, DXGI_GPU_PREFERENCE_UNSPECIFIED, IDXGIFactory6,
    };

    #[test]
    fn parse_gpu_preference_defaults_to_minimum_power() {
        assert_eq!(
            parse_gpu_preference(None),
            DXGI_GPU_PREFERENCE_MINIMUM_POWER
        );
        assert_eq!(
            parse_gpu_preference(Some("")),
            DXGI_GPU_PREFERENCE_MINIMUM_POWER
        );
        assert_eq!(
            parse_gpu_preference(Some("   ")),
            DXGI_GPU_PREFERENCE_MINIMUM_POWER
        );
        assert_eq!(
            parse_gpu_preference(Some("minimum_power")),
            DXGI_GPU_PREFERENCE_MINIMUM_POWER
        );
        assert_eq!(
            parse_gpu_preference(Some("  MINIMUM_POWER  ")),
            DXGI_GPU_PREFERENCE_MINIMUM_POWER
        );
        assert_eq!(
            parse_gpu_preference(Some("unknown_value")),
            DXGI_GPU_PREFERENCE_MINIMUM_POWER
        );
    }

    #[test]
    fn parse_gpu_preference_handles_overrides() {
        assert_eq!(
            parse_gpu_preference(Some("high_performance")),
            DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE
        );
        assert_eq!(
            parse_gpu_preference(Some("HIGH_PERFORMANCE")),
            DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE
        );
        assert_eq!(
            parse_gpu_preference(Some("unspecified")),
            DXGI_GPU_PREFERENCE_UNSPECIFIED
        );
        assert_eq!(
            parse_gpu_preference(Some(" Unspecified ")),
            DXGI_GPU_PREFERENCE_UNSPECIFIED
        );
    }

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
}
