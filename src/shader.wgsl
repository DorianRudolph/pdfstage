struct Uniforms {
    surface_image: vec4<f32>,
    mouse_flags: vec4<f32>,
    highlight: vec4<f32>,
    zoom_pan: vec4<f32>,
    source_rect: vec4<f32>,
}

struct LaserPoints {
    points: array<vec4<f32>, 64>,
}

@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(0) @binding(1) var pdf_texture: texture_2d<f32>;
@group(0) @binding(2) var pdf_sampler: sampler;
@group(0) @binding(3) var<storage, read> laser: LaserPoints;

struct VertexOut {
    @builtin(position) pos: vec4<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOut {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -3.0),
        vec2<f32>(3.0, 1.0),
        vec2<f32>(-1.0, 1.0)
    );
    var out: VertexOut;
    out.pos = vec4<f32>(positions[vertex_index], 0.0, 1.0);
    return out;
}

fn image_rect() -> vec4<f32> {
    let surface = uniforms.surface_image.xy;
    let image = uniforms.surface_image.zw;
    let origin = floor((surface - image) * 0.5 + uniforms.zoom_pan.xy);
    return vec4<f32>(origin, image);
}

fn slide_scale() -> f32 {
    let rect = image_rect();
    return min(rect.z, rect.w);
}

fn sample_page(pixel: vec2<f32>) -> vec4<f32> {
    let rect = image_rect();
    let page_uv = (pixel - rect.xy) / rect.zw;
    if (page_uv.x < 0.0 || page_uv.y < 0.0 || page_uv.x > 1.0 || page_uv.y > 1.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    let source = uniforms.source_rect;
    if (page_uv.x < source.x || page_uv.y < source.y || page_uv.x > source.z || page_uv.y > source.w) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    let uv = (page_uv - source.xy) / max(source.zw - source.xy, vec2<f32>(0.000001, 0.000001));
    return textureSampleLevel(pdf_texture, pdf_sampler, uv, 0.0);
}

fn dist_to_segment(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>) -> f32 {
    let pa = p - a;
    let ba = b - a;
    let denom = max(dot(ba, ba), 0.0001);
    let h = clamp(dot(pa, ba) / denom, 0.0, 1.0);
    return length(pa - ba * h);
}

fn laser_overlay(pixel: vec2<f32>, color: vec4<f32>) -> vec4<f32> {
    let count = i32(min(uniforms.mouse_flags.w, 64.0));
    if (count <= 0) {
        return color;
    }
    let scale = slide_scale();
    let tail_radius = max(2.0, scale * 0.0167);
    let head_radius = max(3.0, scale * 0.025);
    var alpha = 0.0;
    for (var i = 1; i < 64; i = i + 1) {
        if (i >= count) {
            break;
        }
        let a = laser.points[i - 1].xy;
        let b = laser.points[i].xy;
        let d = dist_to_segment(pixel, a, b);
        let age = f32(i) / max(uniforms.mouse_flags.w, 1.0);
        alpha = max(alpha, smoothstep(tail_radius, 0.0, d) * age);
    }
    let head = uniforms.mouse_flags.xy;
    alpha = max(alpha, smoothstep(head_radius, 0.0, length(pixel - head)));
    let laser_color = vec4<f32>(1.0, 0.02, 0.02, 1.0);
    return mix(color, laser_color, clamp(alpha, 0.0, 0.95));
}

fn highlight_overlay(pixel: vec2<f32>, color: vec4<f32>) -> vec4<f32> {
    let a = min(uniforms.highlight.xy, uniforms.highlight.zw);
    let b = max(uniforms.highlight.xy, uniforms.highlight.zw);
    let inside = pixel.x >= a.x && pixel.x <= b.x && pixel.y >= a.y && pixel.y <= b.y;
    if (!inside) {
        return color;
    }
    return vec4<f32>(
        mix(color.rgb, vec3<f32>(1.0, 0.88, 0.05), 0.35),
        color.a
    );
}

fn magnifier_overlay(pixel: vec2<f32>, color: vec4<f32>) -> vec4<f32> {
    let center = uniforms.mouse_flags.xy;
    let delta = pixel - center;
    let scale = slide_scale();
    let radius = max(32.0, scale * 0.16);
    let shadow_width = max(6.0, scale * 0.033);
    let outline_outer = max(1.0, scale * 0.0035);
    let outline_inner = max(1.0, scale * 0.0042);
    let dist = length(delta);
    let shadow = smoothstep(radius + shadow_width, radius, dist) * 0.25;
    let scale_factor = 1.5;
    var out = vec4<f32>(color.rgb * (1.0 - shadow), color.a);
    if (dist < radius) {
        let zoomed = center + delta / scale_factor;
        out = sample_page(zoomed);
    }
    let outline = smoothstep(radius + outline_outer, radius, dist) - smoothstep(radius, radius - outline_inner, dist);
    out = mix(out, vec4<f32>(0.0, 0.0, 0.0, 1.0), clamp(outline, 0.0, 1.0));
    return out;
}

@fragment
fn fs_main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let pixel = pos.xy;
    let flags = u32(uniforms.mouse_flags.z);
    var color = sample_page(pixel);
    if ((flags & 2u) != 0u) {
        color = highlight_overlay(pixel, color);
    }
    if ((flags & 4u) != 0u) {
        color = magnifier_overlay(pixel, color);
    }
    if ((flags & 1u) != 0u) {
        color = laser_overlay(pixel, color);
    }
    return color;
}
