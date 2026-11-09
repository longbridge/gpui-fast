//! The WebGL2 shader decodes instance records from a texture word by word,
//! stepping `instance_id * <words>` into it (`shaders_webgl.wgsl`), while the
//! renderer uploads them `size_of::<T>()` bytes apart. A record that grows on
//! the Rust side without the shader's stride following draws garbage rather
//! than failing to compile, so this compares the two.

use std::mem::size_of;

use gpui::{MonochromeSprite, PolychromeSprite, Underline};

/// The word stride `shaders_webgl.wgsl`'s `loader` steps by.
pub(crate) fn shader_stride(loader: &str) -> usize {
    let source = include_str!("../shaders_webgl.wgsl");
    let start = source
        .find(&format!("fn {loader}("))
        .unwrap_or_else(|| panic!("{loader} in shaders_webgl.wgsl"));
    let body = &source[start..];
    let call = body.find("instance_cursor(").expect("an instance cursor");
    let words = &body[call..];
    let words = &words[words.find("* ").expect("a stride") + 2..];
    let words = &words[..words.find('u').expect("an unsigned stride")];
    words.parse().expect("a numeric stride")
}

#[test]
fn sprite_records_step_by_their_size_in_the_webgl_shader() {
    for (loader, size) in [
        ("load_underline", size_of::<Underline>()),
        ("load_mono_sprite", size_of::<MonochromeSprite>()),
        ("load_poly_sprite", size_of::<PolychromeSprite>()),
    ] {
        assert_eq!(shader_stride(loader) * 4, size, "{loader}");
    }
    // `fast::image_transform`'s matrix, six words, after upstream's 24.
    assert_eq!(size_of::<PolychromeSprite>(), 30 * 4);
}
