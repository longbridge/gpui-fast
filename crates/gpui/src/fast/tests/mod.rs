//! Tests of what gpui-fast adds, kept out of upstream files' test modules.

mod dependencies;
mod dispatch;
mod global_id;
#[cfg(any(feature = "inspector", debug_assertions))]
mod inspector;
mod layout;
mod number_shaping;
mod oracle;
mod path_cache;
mod retained;
mod retained_bench;
mod scene_order;
mod splice;
mod support;
mod text;
mod text_shaping;
