// Modified by Longbridge for gpui-fast.
#![cfg(any(target_os = "linux", target_os = "freebsd"))]
mod fast;
mod linux;

pub use linux::{current_platform, linux_platform};
