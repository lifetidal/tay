// kitty graphics 图像:实例化纹理 quad。
struct Locals {
    resolution: vec2<f32>,
}

@group(0) @binding(0) var<uniform> locals: Locals;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct VSIn {
    @location(0) pos: vec2<f32>, // 单位 quad
    @location(1) uv: vec2<f32>,
}

struct VSInst {
    @location(2) rect: vec4<f32>, // x, y, w, h(物理像素)
}

struct VSOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs(in: VSIn, inst: VSInst) -> VSOut {
    let p = in.pos * inst.rect.zw + inst.rect.xy;
    let ndc = vec2<f32>(
        p.x / locals.resolution.x * 2.0 - 1.0,
        1.0 - p.y / locals.resolution.y * 2.0,
    );
    return VSOut(vec4<f32>(ndc, 0.0, 1.0), in.uv);
}

@fragment
fn fs(in: VSOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
