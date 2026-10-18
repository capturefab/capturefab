// DINOv2 forward-pass kernels (see gpu.rs). Activations are row-major f32
// matrices; every parameter lives in one `weights` buffer addressed by f32
// offsets. Linear weights keep PyTorch's [out, in] layout.

// ---------------------------------------------------------------------------
// C = epilogue(A W^T + b): 64 x 64 output tiles, 16 x 16 threads with 4 x 4
// outputs each, K in steps of 16 through workgroup memory.

struct Matmul {
    m: u32,
    n: u32,
    k: u32,
    // 0 bias, 1 GELU, 2 residual (c += scale * v), 3 positions (c = v + pos).
    mode: u32,
    w_off: u32,
    b_off: u32,
    // Layer-scale offset (mode 2) or position-table offset (mode 3).
    extra: u32,
    // Output rows are written starting at this row (patches skip CLS).
    row_off: u32,
}

@group(0) @binding(0) var<uniform> mm: Matmul;
@group(0) @binding(1) var<storage, read> mm_a: array<f32>;
@group(0) @binding(2) var<storage, read> mm_w: array<f32>;
@group(0) @binding(3) var<storage, read_write> mm_c: array<f32>;

var<workgroup> tile_a: array<f32, 1024>; // [16 k][64 rows]
var<workgroup> tile_w: array<f32, 1024>; // [16 k][64 columns]

// erf to f32 precision (Abramowitz and Stegun 7.1.26), for exact-erf GELU.
fn erf(x: f32) -> f32 {
    let t = 1.0 / (1.0 + 0.3275911 * abs(x));
    let y = 1.0 - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t * exp(-x * x);
    return select(-y, y, x >= 0.0);
}

fn store(row: u32, col0: u32, values: vec4<f32>) {
    if (row >= mm.m) {
        return;
    }
    for (var j = 0u; j < 4u; j++) {
        let col = col0 + 16u * j;
        if (col < mm.n) {
            let v = values[j] + mm_w[mm.b_off + col];
            let o = (row + mm.row_off) * mm.n + col;
            switch mm.mode {
                case 1u: {
                    mm_c[o] = 0.5 * v * (1.0 + erf(v * 0.70710678118654752));
                }
                case 2u: {
                    mm_c[o] = mm_c[o] + mm_w[mm.extra + col] * v;
                }
                case 3u: {
                    mm_c[o] = v + mm_w[mm.extra + o];
                }
                default: {
                    mm_c[o] = v;
                }
            }
        }
    }
}

fn load_a(row: u32, k: u32) -> f32 {
    if (row < mm.m && k < mm.k) {
        return mm_a[row * mm.k + k];
    }
    return 0.0;
}

fn load_w(col: u32, k: u32) -> f32 {
    if (col < mm.n && k < mm.k) {
        return mm_w[mm.w_off + col * mm.k + k];
    }
    return 0.0;
}

// Accumulators are four vec4 registers (rows y, y+16, y+32, y+48 by columns
// x, x+16, x+32, x+48); no dynamically indexed arrays, which some shader
// compilers move to slow thread memory.
@compute @workgroup_size(16, 16)
fn matmul(
    @builtin(workgroup_id) group: vec3<u32>,
    @builtin(local_invocation_id) local: vec3<u32>,
    @builtin(local_invocation_index) index: u32,
) {
    let row0 = group.y * 64u;
    let col0 = group.x * 64u;
    var c0 = vec4<f32>(0.0);
    var c1 = vec4<f32>(0.0);
    var c2 = vec4<f32>(0.0);
    var c3 = vec4<f32>(0.0);
    // Each thread stages four values of each tile: rows index / 16 + 16 e,
    // consecutive threads along K so global reads coalesce.
    let r = index / 16u;
    let kk = index % 16u;
    // Software pipelining: the next tile's global loads are issued into
    // registers before the current tile is multiplied out of workgroup memory.
    var na = vec4<f32>(load_a(row0 + r, kk), load_a(row0 + r + 16u, kk), load_a(row0 + r + 32u, kk), load_a(row0 + r + 48u, kk));
    var nw = vec4<f32>(load_w(col0 + r, kk), load_w(col0 + r + 16u, kk), load_w(col0 + r + 32u, kk), load_w(col0 + r + 48u, kk));
    for (var k0 = 0u; k0 < mm.k; k0 += 16u) {
        tile_a[kk * 64u + r] = na.x;
        tile_a[kk * 64u + r + 16u] = na.y;
        tile_a[kk * 64u + r + 32u] = na.z;
        tile_a[kk * 64u + r + 48u] = na.w;
        tile_w[kk * 64u + r] = nw.x;
        tile_w[kk * 64u + r + 16u] = nw.y;
        tile_w[kk * 64u + r + 32u] = nw.z;
        tile_w[kk * 64u + r + 48u] = nw.w;
        workgroupBarrier();
        let k = k0 + 16u + kk;
        na = vec4<f32>(load_a(row0 + r, k), load_a(row0 + r + 16u, k), load_a(row0 + r + 32u, k), load_a(row0 + r + 48u, k));
        nw = vec4<f32>(load_w(col0 + r, k), load_w(col0 + r + 16u, k), load_w(col0 + r + 32u, k), load_w(col0 + r + 48u, k));
        for (var step = 0u; step < 16u; step++) {
            let base = step * 64u;
            let w = vec4<f32>(
                tile_w[base + local.x],
                tile_w[base + local.x + 16u],
                tile_w[base + local.x + 32u],
                tile_w[base + local.x + 48u],
            );
            c0 = fma(vec4<f32>(tile_a[base + local.y]), w, c0);
            c1 = fma(vec4<f32>(tile_a[base + local.y + 16u]), w, c1);
            c2 = fma(vec4<f32>(tile_a[base + local.y + 32u]), w, c2);
            c3 = fma(vec4<f32>(tile_a[base + local.y + 48u]), w, c3);
        }
        workgroupBarrier();
    }
    let col = col0 + local.x;
    store(row0 + local.y, col, c0);
    store(row0 + local.y + 16u, col, c1);
    store(row0 + local.y + 32u, col, c2);
    store(row0 + local.y + 48u, col, c3);
}

// ---------------------------------------------------------------------------
// LayerNorm, one workgroup per row.

struct Norm {
    d: u32,
    w_off: u32,
    b_off: u32,
    eps_bits: u32,
}

@group(0) @binding(0) var<uniform> ln: Norm;
@group(0) @binding(1) var<storage, read> ln_x: array<f32>;
@group(0) @binding(2) var<storage, read> ln_w: array<f32>;
@group(0) @binding(3) var<storage, read_write> ln_out: array<f32>;

var<workgroup> partial: array<f32, 128>;

fn reduce_sum(lane: u32, value: f32) -> f32 {
    partial[lane] = value;
    workgroupBarrier();
    for (var stride = 64u; stride > 0u; stride /= 2u) {
        if (lane < stride) {
            partial[lane] += partial[lane + stride];
        }
        workgroupBarrier();
    }
    let total = partial[0];
    workgroupBarrier();
    return total;
}

@compute @workgroup_size(128)
fn layer_norm(
    @builtin(workgroup_id) group: vec3<u32>,
    @builtin(local_invocation_index) lane: u32,
) {
    let base = group.x * ln.d;
    var sum = 0.0;
    for (var i = lane; i < ln.d; i += 128u) {
        sum += ln_x[base + i];
    }
    let mean = reduce_sum(lane, sum) / f32(ln.d);
    var squares = 0.0;
    for (var i = lane; i < ln.d; i += 128u) {
        let centered = ln_x[base + i] - mean;
        squares += centered * centered;
    }
    let variance = reduce_sum(lane, squares) / f32(ln.d);
    let inverse = inverseSqrt(variance + bitcast<f32>(ln.eps_bits));
    for (var i = lane; i < ln.d; i += 128u) {
        ln_out[base + i] = (ln_x[base + i] - mean) * inverse * ln_w[ln.w_off + i] + ln_w[ln.b_off + i];
    }
}

// ---------------------------------------------------------------------------
// Attention as batched strided products, one batch per head:
// scores = scale * Q K^T, a row softmax, then out = P V. Q, K and V are views
// of the fused [T, 3D] product; strides are in f32 elements.

struct Gemm {
    m: u32,
    n: u32,
    k: u32,
    alpha_bits: u32,
    a_off: u32,
    a_rs: u32,
    a_cs: u32,
    a_bs: u32,
    b_off: u32,
    b_rs: u32,
    b_cs: u32,
    b_bs: u32,
    c_off: u32,
    c_rs: u32,
    c_bs: u32,
    _pad: u32,
}

@group(0) @binding(0) var<uniform> gm: Gemm;
@group(0) @binding(1) var<storage, read> gm_a: array<f32>;
@group(0) @binding(2) var<storage, read> gm_b: array<f32>;
@group(0) @binding(3) var<storage, read_write> gm_c: array<f32>;

fn gemm_a(batch: u32, row: u32, k: u32) -> f32 {
    if (row < gm.m && k < gm.k) {
        return gm_a[gm.a_off + batch * gm.a_bs + row * gm.a_rs + k * gm.a_cs];
    }
    return 0.0;
}

fn gemm_b(batch: u32, k: u32, col: u32) -> f32 {
    if (col < gm.n && k < gm.k) {
        return gm_b[gm.b_off + batch * gm.b_bs + k * gm.b_rs + col * gm.b_cs];
    }
    return 0.0;
}

fn gemm_store(batch: u32, row: u32, col0: u32, values: vec4<f32>) {
    if (row >= gm.m) {
        return;
    }
    let alpha = bitcast<f32>(gm.alpha_bits);
    for (var j = 0u; j < 4u; j++) {
        let col = col0 + 16u * j;
        if (col < gm.n) {
            gm_c[gm.c_off + batch * gm.c_bs + row * gm.c_rs + col] = alpha * values[j];
        }
    }
}

@compute @workgroup_size(16, 16)
fn gemm(
    @builtin(workgroup_id) group: vec3<u32>,
    @builtin(local_invocation_id) local: vec3<u32>,
    @builtin(local_invocation_index) index: u32,
) {
    let batch = group.z;
    let row0 = group.y * 64u;
    let col0 = group.x * 64u;
    var c0 = vec4<f32>(0.0);
    var c1 = vec4<f32>(0.0);
    var c2 = vec4<f32>(0.0);
    var c3 = vec4<f32>(0.0);
    let r = index / 16u;
    let kk = index % 16u;
    var na = vec4<f32>(gemm_a(batch, row0 + r, kk), gemm_a(batch, row0 + r + 16u, kk), gemm_a(batch, row0 + r + 32u, kk), gemm_a(batch, row0 + r + 48u, kk));
    var nb = vec4<f32>(gemm_b(batch, kk, col0 + r), gemm_b(batch, kk, col0 + r + 16u), gemm_b(batch, kk, col0 + r + 32u), gemm_b(batch, kk, col0 + r + 48u));
    for (var k0 = 0u; k0 < gm.k; k0 += 16u) {
        tile_a[kk * 64u + r] = na.x;
        tile_a[kk * 64u + r + 16u] = na.y;
        tile_a[kk * 64u + r + 32u] = na.z;
        tile_a[kk * 64u + r + 48u] = na.w;
        tile_w[kk * 64u + r] = nb.x;
        tile_w[kk * 64u + r + 16u] = nb.y;
        tile_w[kk * 64u + r + 32u] = nb.z;
        tile_w[kk * 64u + r + 48u] = nb.w;
        workgroupBarrier();
        let k = k0 + 16u + kk;
        na = vec4<f32>(gemm_a(batch, row0 + r, k), gemm_a(batch, row0 + r + 16u, k), gemm_a(batch, row0 + r + 32u, k), gemm_a(batch, row0 + r + 48u, k));
        nb = vec4<f32>(gemm_b(batch, k, col0 + r), gemm_b(batch, k, col0 + r + 16u), gemm_b(batch, k, col0 + r + 32u), gemm_b(batch, k, col0 + r + 48u));
        for (var step = 0u; step < 16u; step++) {
            let base = step * 64u;
            let w = vec4<f32>(
                tile_w[base + local.x],
                tile_w[base + local.x + 16u],
                tile_w[base + local.x + 32u],
                tile_w[base + local.x + 48u],
            );
            c0 = fma(vec4<f32>(tile_a[base + local.y]), w, c0);
            c1 = fma(vec4<f32>(tile_a[base + local.y + 16u]), w, c1);
            c2 = fma(vec4<f32>(tile_a[base + local.y + 32u]), w, c2);
            c3 = fma(vec4<f32>(tile_a[base + local.y + 48u]), w, c3);
        }
        workgroupBarrier();
    }
    let col = col0 + local.x;
    gemm_store(batch, row0 + local.y, col, c0);
    gemm_store(batch, row0 + local.y + 16u, col, c1);
    gemm_store(batch, row0 + local.y + 32u, col, c2);
    gemm_store(batch, row0 + local.y + 48u, col, c3);
}

// Softmax of each score row in place, one workgroup per row. DINOv2 logits
// are moderate, but subtracting the row maximum keeps any model safe.

struct Softmax {
    length: u32,
}

@group(0) @binding(0) var<uniform> sm: Softmax;
@group(0) @binding(1) var<storage, read_write> sm_x: array<f32>;

var<workgroup> extreme: array<f32, 128>;

@compute @workgroup_size(128)
fn softmax(
    @builtin(workgroup_id) group: vec3<u32>,
    @builtin(local_invocation_index) lane: u32,
) {
    let base = group.x * sm.length;
    var best = -3.0e38;
    for (var i = lane; i < sm.length; i += 128u) {
        best = max(best, sm_x[base + i]);
    }
    extreme[lane] = best;
    workgroupBarrier();
    for (var stride = 64u; stride > 0u; stride /= 2u) {
        if (lane < stride) {
            extreme[lane] = max(extreme[lane], extreme[lane + stride]);
        }
        workgroupBarrier();
    }
    let maximum = extreme[0];
    workgroupBarrier();
    var total = 0.0;
    for (var i = lane; i < sm.length; i += 128u) {
        let e = exp(sm_x[base + i] - maximum);
        sm_x[base + i] = e;
        total += e;
    }
    let inverse = 1.0 / reduce_sum(lane, total);
    for (var i = lane; i < sm.length; i += 128u) {
        sm_x[base + i] *= inverse;
    }
}
