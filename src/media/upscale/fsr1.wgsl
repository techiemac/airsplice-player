// AMD FidelityFX Super Resolution 1.0: EASU (edge-adaptive spatial
// upsampling) and RCAS (robust contrast-adaptive sharpening), ported to
// WGSL from ffx_fsr1.h.
//
// Copyright (c) 2021 Advanced Micro Devices, Inc. All rights reserved.
// Released under the MIT License; the full notice is in NOTICE.
//
// Both passes work on gamma-encoded (perceptual) colour, as FSR 1 expects
// for 8-bit content. Channel order does not matter: the luma estimate
// weights red and blue equally, so BGRA bytes pass through unchanged.

struct Params {
    in_size: vec2<u32>,
    out_size: vec2<u32>,
    // RCAS strength: 2^-stops, 1.0 the sharpest.
    sharpness: f32,
    pad0: f32,
    pad1: f32,
    pad2: f32,
}

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(2) var<uniform> p: Params;

fn load(c: vec2<i32>) -> vec3<f32> {
    let last = vec2<i32>(textureDimensions(src)) - vec2<i32>(1);
    return textureLoad(src, clamp(c, vec2<i32>(0), last), 0).rgb;
}

fn luma(c: vec3<f32>) -> f32 {
    return c.b * 0.5 + (c.r * 0.5 + c.g);
}

fn rcp_or_zero(x: f32) -> f32 {
    return select(0.0, 1.0 / x, x > 0.0);
}

// Accumulates edge direction and length for one of the four bilinear
// corners around the sample point, weighted by that corner's bilinear
// weight. A is above the corner, B left, C the corner, D right, E below.
fn easu_set(dir: ptr<function, vec2<f32>>, len: ptr<function, f32>, w: f32, l_a: f32, l_b: f32, l_c: f32, l_d: f32, l_e: f32) {
    let dc = l_d - l_c;
    let cb = l_c - l_b;
    var len_x = rcp_or_zero(max(abs(dc), abs(cb)));
    let dir_x = l_d - l_b;
    (*dir).x += dir_x * w;
    len_x = clamp(abs(dir_x) * len_x, 0.0, 1.0);
    len_x *= len_x;
    *len += len_x * w;

    let ec = l_e - l_c;
    let ca = l_c - l_a;
    var len_y = rcp_or_zero(max(abs(ec), abs(ca)));
    let dir_y = l_e - l_a;
    (*dir).y += dir_y * w;
    len_y = clamp(abs(dir_y) * len_y, 0.0, 1.0);
    len_y *= len_y;
    *len += len_y * w;
}

// One tap of the approximated, edge-stretched Lanczos 2 kernel.
fn easu_tap(acc: ptr<function, vec3<f32>>, acc_w: ptr<function, f32>, off: vec2<f32>, dir: vec2<f32>, len2: vec2<f32>, lob: f32, clp: f32, c: vec3<f32>) {
    // Rotate the offset into the edge's frame, then stretch it.
    var v = vec2<f32>(off.x * dir.x + off.y * dir.y, off.x * (-dir.y) + off.y * dir.x);
    v *= len2;
    let d2 = min(v.x * v.x + v.y * v.y, clp);
    // (25/16 * (2/5 * x^2 - 1)^2 - (25/16 - 1)) * (lob * x^2 - 1)^2
    var w_b = 2.0 / 5.0 * d2 - 1.0;
    var w_a = lob * d2 - 1.0;
    w_b *= w_b;
    w_a *= w_a;
    w_b = 25.0 / 16.0 * w_b - (25.0 / 16.0 - 1.0);
    let w = w_b * w_a;
    *acc += c * w;
    *acc_w += w;
}

@compute @workgroup_size(8, 8)
fn easu(@builtin(global_invocation_id) id: vec3<u32>) {
    if id.x >= p.out_size.x || id.y >= p.out_size.y {
        return;
    }
    let scale = vec2<f32>(p.in_size) / vec2<f32>(p.out_size);
    var pp = (vec2<f32>(id.xy) + 0.5) * scale - 0.5;
    let fp = floor(pp);
    pp -= fp;
    let o = vec2<i32>(fp);

    // The 12-tap footprint around the sample point:
    //     b c
    //   e f g h
    //   i j k l
    //     n o
    let b = load(o + vec2<i32>(0, -1));
    let c = load(o + vec2<i32>(1, -1));
    let e = load(o + vec2<i32>(-1, 0));
    let f = load(o);
    let g = load(o + vec2<i32>(1, 0));
    let h = load(o + vec2<i32>(2, 0));
    let i = load(o + vec2<i32>(-1, 1));
    let j = load(o + vec2<i32>(0, 1));
    let k = load(o + vec2<i32>(1, 1));
    let l = load(o + vec2<i32>(2, 1));
    let n = load(o + vec2<i32>(0, 2));
    let oo = load(o + vec2<i32>(1, 2));

    let b_l = luma(b);
    let c_l = luma(c);
    let e_l = luma(e);
    let f_l = luma(f);
    let g_l = luma(g);
    let h_l = luma(h);
    let i_l = luma(i);
    let j_l = luma(j);
    let k_l = luma(k);
    let l_l = luma(l);
    let n_l = luma(n);
    let o_l = luma(oo);

    var dir = vec2<f32>(0.0);
    var len = 0.0;
    easu_set(&dir, &len, (1.0 - pp.x) * (1.0 - pp.y), b_l, e_l, f_l, g_l, j_l);
    easu_set(&dir, &len, pp.x * (1.0 - pp.y), c_l, f_l, g_l, h_l, k_l);
    easu_set(&dir, &len, (1.0 - pp.x) * pp.y, f_l, i_l, j_l, k_l, n_l);
    easu_set(&dir, &len, pp.x * pp.y, g_l, j_l, k_l, l_l, o_l);

    // Normalise the direction; no gradient at all means "horizontal".
    let dir2 = dir * dir;
    let dir_r2 = dir2.x + dir2.y;
    let flat = dir_r2 < 1.0 / 32768.0;
    let dir_r = select(inverseSqrt(dir_r2), 1.0, flat);
    dir.x = select(dir.x, 1.0, flat);
    dir *= vec2<f32>(dir_r);

    // Edge strength shapes the kernel: stretched along strong edges,
    // and with a sharper negative lobe.
    len = len * 0.5;
    len *= len;
    let stretch = (dir.x * dir.x + dir.y * dir.y) / max(abs(dir.x), abs(dir.y));
    let len2 = vec2<f32>(1.0 + (stretch - 1.0) * len, 1.0 - 0.5 * len);
    let lob = 0.5 + ((1.0 / 4.0 - 0.04) - 0.5) * len;
    let clp = 1.0 / lob;

    var acc = vec3<f32>(0.0);
    var acc_w = 0.0;
    easu_tap(&acc, &acc_w, vec2<f32>(0.0, -1.0) - pp, dir, len2, lob, clp, b);
    easu_tap(&acc, &acc_w, vec2<f32>(1.0, -1.0) - pp, dir, len2, lob, clp, c);
    easu_tap(&acc, &acc_w, vec2<f32>(-1.0, 1.0) - pp, dir, len2, lob, clp, i);
    easu_tap(&acc, &acc_w, vec2<f32>(0.0, 1.0) - pp, dir, len2, lob, clp, j);
    easu_tap(&acc, &acc_w, vec2<f32>(0.0, 0.0) - pp, dir, len2, lob, clp, f);
    easu_tap(&acc, &acc_w, vec2<f32>(-1.0, 0.0) - pp, dir, len2, lob, clp, e);
    easu_tap(&acc, &acc_w, vec2<f32>(1.0, 1.0) - pp, dir, len2, lob, clp, k);
    easu_tap(&acc, &acc_w, vec2<f32>(2.0, 1.0) - pp, dir, len2, lob, clp, l);
    easu_tap(&acc, &acc_w, vec2<f32>(2.0, 0.0) - pp, dir, len2, lob, clp, h);
    easu_tap(&acc, &acc_w, vec2<f32>(1.0, 0.0) - pp, dir, len2, lob, clp, g);
    easu_tap(&acc, &acc_w, vec2<f32>(1.0, 2.0) - pp, dir, len2, lob, clp, oo);
    easu_tap(&acc, &acc_w, vec2<f32>(0.0, 2.0) - pp, dir, len2, lob, clp, n);

    // De-ring: stay within the four nearest source pixels.
    let mn = min(min(f, g), min(j, k));
    let mx = max(max(f, g), max(j, k));
    let filtered = select(f, acc / acc_w, acc_w > 0.0);
    textureStore(dst, vec2<i32>(id.xy), vec4<f32>(clamp(filtered, mn, mx), 1.0));
}

// The most sharpening RCAS applies, so it never rings: 1/4 - 1/16.
const RCAS_LIMIT: f32 = 0.1875;

@compute @workgroup_size(8, 8)
fn rcas(@builtin(global_invocation_id) id: vec3<u32>) {
    if id.x >= p.out_size.x || id.y >= p.out_size.y {
        return;
    }
    let s = vec2<i32>(id.xy);
    //   b
    // d e f
    //   h
    let b = load(s + vec2<i32>(0, -1));
    let d = load(s + vec2<i32>(-1, 0));
    let e = load(s);
    let f = load(s + vec2<i32>(1, 0));
    let h = load(s + vec2<i32>(0, 1));

    let mn4 = min(min(b, d), min(f, h));
    let mx4 = max(max(b, d), max(f, h));
    // The strongest negative lobe that keeps the result in [0, 1] for
    // each channel; the guards stand in for FSR's infinite reciprocals.
    let hit_min = min(mn4, e) / max(4.0 * mx4, vec3<f32>(1.0 / 65536.0));
    let hit_max = (1.0 - max(mx4, e)) / min(4.0 * mn4 - 4.0, vec3<f32>(-1.0 / 65536.0));
    let lobe_rgb = max(-hit_min, hit_max);
    let lobe = max(-RCAS_LIMIT, min(max(lobe_rgb.r, max(lobe_rgb.g, lobe_rgb.b)), 0.0)) * p.sharpness;
    let pix = (lobe * (b + d + f + h) + e) / (4.0 * lobe + 1.0);
    textureStore(dst, s, vec4<f32>(clamp(pix, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0));
}
