// Raw Framer viewer transform. Mirrors rf_color::view::ViewPipeline (.claude/COLOR_PIPELINE.md §3).

struct U {
    quad: vec4<f32>,      // image rect in NDC: x0, y0, x1, y1
    m0: vec4<f32>,        // gamut matrix rows (source -> display primaries)
    m1: vec4<f32>,
    m2: vec4<f32>,
    params: vec4<f32>,    // exposure gain, lut mix, lut size, lut1d size
    modes: vec4<u32>,     // input kind (0 lin, 1 film5, 2 passthrough), output (0 709, 1 srgb, 2 log), lut enabled, lut placement (0 log, 1 after)
    flags: vec4<u32>,     // tonemap, has lut1d, -, -
    dmin: vec4<f32>,      // 3D LUT domain
    dmax: vec4<f32>,
    d1min: vec4<f32>,     // 1D LUT domain
    d1max: vec4<f32>,
    ap: vec4<f32>,        // assists: peaking color rgb, edge threshold
    am: vec4<u32>,        // assists: peaking (0 off, 1 lines, 2 sharpen), false color (0 off, 1 display, 2 log), dim, -
};

@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var src: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var lut3: texture_3d<f32>;
@group(0) @binding(4) var lut1: texture_2d<f32>;

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VOut {
    var corners = array<vec2<f32>, 6>(
        vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
        vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0),
    );
    let c = corners[i];
    var o: VOut;
    let x = mix(u.quad.x, u.quad.z, c.x);
    let y = mix(u.quad.y, u.quad.w, c.y);
    o.pos = vec4(x, y, 0.0, 1.0);
    o.uv = vec2(c.x, c.y);
    return o;
}

const A: f32 = 0.08692876065491224;
const B: f32 = 0.005494072432257808;
const C: f32 = 0.5300133392291939;
const D: f32 = 8.283605932402494;
const E: f32 = 0.09246575342465753;
const LOG_CUT: f32 = 0.13388378;

fn film5_decode(y: vec3<f32>) -> vec3<f32> {
    return select(exp((y - C) / A) - B, (y - E) / D, y < vec3(LOG_CUT));
}
fn film5_encode(x: vec3<f32>) -> vec3<f32> {
    return select(A * log(max(x + B, vec3(1e-10))) + C, D * x + E, x < vec3(0.005));
}
fn srgb_encode(l: vec3<f32>) -> vec3<f32> {
    let c = max(l, vec3(0.0));
    return select(1.055 * pow(c, vec3(1.0 / 2.4)) - 0.055, 12.92 * c, c <= vec3(0.0031308));
}
fn linearize(v: vec3<f32>) -> vec3<f32> {
    if (u.modes.x == 1u) { return film5_decode(v); }
    return v;
}
fn rolloff(rgb: vec3<f32>) -> vec3<f32> {
    let k = 0.8;
    let m = max(rgb.r, max(rgb.g, rgb.b));
    if (m <= k) { return rgb; }
    let y = k + (1.0 - k) * (1.0 - exp(-(m - k) / (1.0 - k)));
    return rgb * (y / m);
}

fn lut1_apply(v: vec3<f32>) -> vec3<f32> {
    let n = u.params.w;
    let t = clamp((v - u.d1min.xyz) / (u.d1max.xyz - u.d1min.xyz), vec3(0.0), vec3(1.0)) * (n - 1.0);
    var o: vec3<f32>;
    for (var c = 0; c < 3; c++) {
        let i0 = u32(floor(t[c]));
        let i1 = min(i0 + 1u, u32(n) - 1u);
        let f = t[c] - f32(i0);
        let a = textureLoad(lut1, vec2(i0, 0u), 0)[c];
        let b = textureLoad(lut1, vec2(i1, 0u), 0)[c];
        o[c] = mix(a, b, f);
    }
    return o;
}

fn at(r: u32, g: u32, b: u32) -> vec3<f32> {
    return textureLoad(lut3, vec3(r, g, b), 0).rgb;
}

// Tetrahedral interpolation, identical to rf_color::Lut3D::apply.
fn lut3_apply(v: vec3<f32>) -> vec3<f32> {
    let n = u.params.z;
    let t = clamp((v - u.dmin.xyz) / (u.dmax.xyz - u.dmin.xyz), vec3(0.0), vec3(1.0)) * (n - 1.0);
    let i = min(vec3<u32>(floor(t)), vec3(u32(n) - 2u));
    let f = t - vec3<f32>(i);
    let r = i.x; let g = i.y; let b = i.z;
    let c000 = at(r, g, b);
    let c111 = at(r + 1u, g + 1u, b + 1u);
    var p1: vec3<f32>; var p2: vec3<f32>; var w: vec3<f32>;
    if (f.x > f.y) {
        if (f.y > f.z) { p1 = at(r + 1u, g, b); p2 = at(r + 1u, g + 1u, b); w = vec3(f.x, f.y, f.z); }
        else if (f.x > f.z) { p1 = at(r + 1u, g, b); p2 = at(r + 1u, g, b + 1u); w = vec3(f.x, f.z, f.y); }
        else { p1 = at(r, g, b + 1u); p2 = at(r + 1u, g, b + 1u); w = vec3(f.z, f.x, f.y); }
    } else {
        if (f.z > f.y) { p1 = at(r, g, b + 1u); p2 = at(r, g + 1u, b + 1u); w = vec3(f.z, f.y, f.x); }
        else if (f.z > f.x) { p1 = at(r, g + 1u, b); p2 = at(r, g + 1u, b + 1u); w = vec3(f.y, f.z, f.x); }
        else { p1 = at(r, g + 1u, b); p2 = at(r + 1u, g + 1u, b); w = vec3(f.y, f.x, f.z); }
    }
    return c000 + w.x * (p1 - c000) + w.y * (p2 - p1) + w.z * (c111 - p2);
}

fn apply_lut(v_in: vec3<f32>) -> vec3<f32> {
    // Guard: a NaN input or output must never reach the screen (it would be written as 0).
    let v = select(v_in, vec3(0.0), v_in != v_in);
    var o = v;
    if (u.flags.y == 1u) { o = lut1_apply(o); }
    if (u.params.z >= 2.0) { o = lut3_apply(o); }
    o = select(o, v, o != o);
    return mix(v, o, u.params.y);
}

// The viewer transform for one decoded pixel (no assists). Mirrors ViewPipeline::apply.
fn display(v: vec3<f32>) -> vec3<f32> {
    let gain = u.params.x;
    let kind = u.modes.x;
    let out_mode = u.modes.y;
    let lut_on = u.modes.z == 1u;
    var o: vec3<f32>;
    if (lut_on && u.modes.w == 0u) {
        var logv = v;
        if (kind != 2u && !(kind == 1u && gain == 1.0)) {
            logv = film5_encode(linearize(v) * gain);
        }
        o = apply_lut(logv);
    } else {
        if (kind == 2u) {
            o = v;
        } else if (out_mode == 2u) {
            o = film5_encode(linearize(v) * gain);
        } else {
            let l = linearize(v) * gain;
            var d = vec3(dot(u.m0.xyz, l), dot(u.m1.xyz, l), dot(u.m2.xyz, l));
            if (u.flags.x == 1u) { d = rolloff(d); }
            if (out_mode == 1u) { o = srgb_encode(d); } else { o = pow(max(d, vec3(0.0)), vec3(1.0 / 2.4)); }
        }
        if (lut_on && u.modes.w == 1u) {
            o = apply_lut(clamp(o, vec3(0.0), vec3(1.0)));
        }
    }
    return clamp(o, vec3(0.0), vec3(1.0));
}

fn luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3(0.2126, 0.7152, 0.0722));
}

// Camera log luma (BMD Film Gen 5) of a decoded pixel: the signal focus detection and
// "camera log" false color measure, independent of the viewer's CST/LUT.
fn log_luma(v: vec3<f32>) -> f32 {
    if (u.modes.x == 0u) { return luma(film5_encode(max(v, vec3(0.0)))); }
    return luma(v);
}

// Blackmagic camera false color (see assists.rs FALSE_COLOR_BANDS; keep in sync).
fn band(c: vec3<f32>, ire: f32, lo: f32, hi: f32, col: vec3<f32>) -> vec3<f32> {
    let w = smoothstep(lo - 1.0, lo + 1.0, ire) * (1.0 - smoothstep(hi - 1.0, hi + 1.0, ire));
    return mix(c, col, w);
}
fn false_color(l: f32) -> vec3<f32> {
    let ire = l * 100.0;
    var c = vec3(l);
    c = band(c, ire, 0.0, 6.0, vec3(119.0, 22.0, 119.0) / 255.0);
    c = band(c, ire, 8.0, 15.0, vec3(31.0, 2.0, 220.0) / 255.0);
    c = band(c, ire, 33.0, 43.0, vec3(90.0, 193.0, 64.0) / 255.0);
    c = band(c, ire, 50.0, 59.0, vec3(241.0, 180.0, 189.0) / 255.0);
    c = band(c, ire, 84.0, 93.0, vec3(252.0, 249.0, 25.0) / 255.0);
    c = band(c, ire, 93.0, 1000.0, vec3(226.0, 12.0, 2.0) / 255.0);
    return c;
}

fn ll(uv: vec2<f32>) -> f32 {
    return log_luma(textureSampleLevel(src, samp, uv, 0.0).rgb);
}

// Sobel gradient of camera-log luma with sample spacing `sp` (uv units).
fn sobel(uv: vec2<f32>, sp: vec2<f32>) -> vec2<f32> {
    let ox = vec2(sp.x, 0.0);
    let oy = vec2(0.0, sp.y);
    let tl = ll(uv - ox - oy);
    let t = ll(uv - oy);
    let tr = ll(uv + ox - oy);
    let l = ll(uv - ox);
    let r = ll(uv + ox);
    let bl = ll(uv - ox + oy);
    let b = ll(uv + oy);
    let br = ll(uv + ox + oy);
    return vec2((tr + 2.0 * r + br) - (tl + 2.0 * l + bl), (bl + 2.0 * b + br) - (tl + 2.0 * t + tr));
}

@fragment
fn fs_main(in: VOut) -> @location(0) vec4<f32> {
    let v = textureSample(src, samp, in.uv).rgb;
    // One screen pixel in texture space (derivatives must be taken in uniform control flow).
    let dx = dpdx(in.uv);
    let dy = dpdy(in.uv);
    var o = display(v);

    if (u.am.y != 0u) {
        let l = select(luma(o), log_luma(v), u.am.y == 2u);
        o = false_color(clamp(l, 0.0, 1.0));
    }

    if (u.am.x != 0u) {
        // Edges are measured on camera-log luma at the image's native pixel spacing (or one
        // screen pixel when zoomed out, so focus reads as you see it), then thinned to a
        // one-screen-pixel ridge along the gradient, like the camera's colored lines. Native
        // spacing keeps edges visible when zoomed in, where screen spacing would fall inside
        // a single image pixel.
        let texel = 1.0 / vec2<f32>(textureDimensions(src));
        let px = vec2(abs(dx.x) + abs(dy.x), abs(dx.y) + abs(dy.y));
        let sp = max(px, texel);
        let g0 = sobel(in.uv, sp);
        let m0 = length(g0);
        let th = u.ap.w;
        if (u.am.x == 1u) {
            let dir = g0 / max(m0, 1e-6);
            let step = dir * px;
            let ma = length(sobel(in.uv + step, sp));
            let mb = length(sobel(in.uv - step, sp));
            let ridge = m0 >= ma && m0 >= mb;
            let k = select(0.0, smoothstep(th, th * 1.25, m0), ridge);
            if (u.am.z == 1u) {
                o = mix(vec3(luma(o)), o, 0.4) * 0.55;
            }
            o = mix(o, u.ap.rgb, k);
        } else {
            // Peaking: in-focus detail glows (brightened and sharpened) on screen only.
            let c = log_luma(v);
            let avg = (ll(in.uv - vec2(sp.x, 0.0)) + ll(in.uv + vec2(sp.x, 0.0)) + ll(in.uv - vec2(0.0, sp.y)) + ll(in.uv + vec2(0.0, sp.y))) * 0.25;
            let k = smoothstep(th, th * 1.4, m0);
            o = clamp(o + vec3((c - avg) * 4.0 * k + 0.12 * k), vec3(0.0), vec3(1.0));
        }
    }

    // The value is already display-encoded. On a Unorm target write it as-is; on an
    // *Srgb target undo the hardware's encode so the bits on screen are identical.
    if (u.flags.z == 1u) {
        o = select(pow((o + 0.055) / 1.055, vec3(2.4)), o / 12.92, o <= vec3(0.04045));
    }
    return vec4(o, 1.0);
}
