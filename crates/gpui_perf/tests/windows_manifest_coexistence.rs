#![cfg(target_os = "windows")]

#[test]
fn fast_and_pre_platforms_link_into_one_executable() {
    let fast = gpui_platform_fast::application();
    let pre = gpui_pre_platform::application();
    drop((fast, pre));
}
