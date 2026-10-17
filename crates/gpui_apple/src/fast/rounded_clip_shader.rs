//! Build-time injection keeps the upstream Metal shader source unchanged.
//! Each colour-producing fragment samples its scope's immutable mask. Path
//! rasterization itself is unmasked; its premultiplied result is masked when
//! the path sprite is composited, avoiding applying coverage twice.

pub fn source(source: &str) -> String {
    let mut source = source.to_owned();
    for name in [
        "quad_fragment",
        "shadow_fragment",
        "underline_fragment",
        "monochrome_sprite_fragment",
        "polychrome_sprite_fragment",
        "path_sprite_fragment",
        "surface_fragment",
    ] {
        let start = source
            .find(&format!("fragment float4 {name}("))
            .expect("upstream fragment entry point");
        let body_start = start + source[start..].find('{').unwrap();
        let signature_end = source[..body_start].rfind(')').unwrap();
        let mut depth = 1;
        let mut body_end = body_start + 1;
        for (offset, ch) in source[body_start + 1..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                body_end = body_start + 1 + offset;
                break;
            }
        }
        let body = &source[body_start + 1..body_end];
        let premultiplied = name == "path_sprite_fragment";
        let body = mask_returns(body, premultiplied);
        source.replace_range(body_start + 1..body_end, &body);
        source.insert_str(signature_end, ", texture2d<float> fast_clip_texture [[texture(15)]], constant uint &fast_clip_enabled [[buffer(29)]]");
    }
    let helper = r#"
float4 fast_rounded_clip_color(float4 color, float2 position, texture2d<float> mask, uint enabled, bool premultiplied) {
  if (enabled != 0) {
    float coverage = mask.read(uint2(position)).a;
    if (premultiplied) color.rgb *= coverage;
    color.a *= coverage;
  }
  return color;
}
"#;
    let offset = source.find("using namespace metal;").unwrap() + "using namespace metal;".len();
    source.insert_str(offset, helper);
    source
}

pub fn stage(input: impl AsRef<std::path::Path>, output: impl AsRef<std::path::Path>) {
    std::fs::write(output, source(&std::fs::read_to_string(input).unwrap())).unwrap();
}

fn mask_returns(body: &str, premultiplied: bool) -> String {
    let mut result = String::new();
    let mut remaining = body;
    while let Some(start) = remaining.find("return ") {
        let end = start
            + remaining[start..]
                .find(';')
                .expect("fragment return semicolon");
        result.push_str(&remaining[..start]);
        result.push_str(&format!("return fast_rounded_clip_color({}, input.position.xy, fast_clip_texture, fast_clip_enabled, {premultiplied});", &remaining[start + 7..end]));
        remaining = &remaining[end + 1..];
    }
    result.push_str(remaining);
    result
}
