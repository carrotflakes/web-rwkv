@group(0) @binding(0) var<uniform> shape: vec4<u32>;                        // [C, T, B]

@group(0) @binding(1) var<storage, read> w: array<vec2<u32>>;               // (C)
@group(0) @binding(2) var<storage, read> b: array<vec2<u32>>;               // (C)
@group(0) @binding(3) var<storage, read_write> x: array<vec4<f32>>;         // (B, T, C)

const BLOCK_SIZE: u32 = 128u;

var<workgroup> sum: array<vec4<f32>, BLOCK_SIZE>;
var<workgroup> sum_squared: array<vec4<f32>, BLOCK_SIZE>;
var<workgroup> mean: f32;
var<workgroup> deviation: f32;

fn unpack4x16float(x: vec2<u32>) -> vec4<f32> {
    return vec4<f32>(unpack2x16float(x.x), unpack2x16float(x.y));
}

fn reduce_step(index: u32, stride: u32) {
    if index < stride {
        sum[index] += sum[index + stride];
        sum_squared[index] += sum_squared[index + stride];
    }
    workgroupBarrier();
}

@compute @workgroup_size(128, 1, 1)
fn layer_norm(@builtin(global_invocation_id) invocation_id: vec3<u32>) {
    let stride = shape[0] / 4u;
    let index = invocation_id.x;
    let token = invocation_id.y;
    let batch = invocation_id.z;

    let bb = (batch * shape[1] + token) * stride;

    for (var i = index; i < stride; i += BLOCK_SIZE) {
        let value = x[bb + i];
        sum[index] += value;
        sum_squared[index] += value * value;
    }
    workgroupBarrier();

    reduce_step(index, 64u);
    reduce_step(index, 32u);
    reduce_step(index, 16u);
    reduce_step(index, 8u);
    reduce_step(index, 4u);
    reduce_step(index, 2u);
    reduce_step(index, 1u);

    if index == 0u {
        mean = dot(sum[0], vec4<f32>(1.0)) / f32(shape[0]);
        deviation = inverseSqrt(dot(sum_squared[0], vec4<f32>(1.0)) / f32(shape[0]) - mean * mean);
    }
    workgroupBarrier();

    for (var i = index; i < stride; i += BLOCK_SIZE) {
        let value = (x[bb + i] - mean) * deviation;
        x[bb + i] = fma(value, unpack4x16float(w[i]), unpack4x16float(b[i]));
    }
}