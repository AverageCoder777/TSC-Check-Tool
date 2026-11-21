struct Camera {
    view_projection: mat4x4<f32>,
};

struct Material {
    alpha_cutoff: f32,
};

@group(0) @binding(0) var<uniform> camera: Camera;
@group(1) @binding(0) var albedo: texture_2d<f32>;
@group(1) @binding(1) var albedo_sampler: sampler;
@group(2) @binding(0) var<uniform> material: Material;

const UP_DIR: vec3<f32> = vec3<f32>(0.0, 1.0, 0.0);
// Pre-normalized light directions: (-0.45, 0.72, 0.52), (0.55, 0.2, -0.65), (0.35, 0.45, -0.82)
const KEY_DIR: vec3<f32> = vec3<f32>(-0.4526992, 0.7243187, 0.5231191);
const FILL_DIR: vec3<f32> = vec3<f32>(0.6285714, 0.2285714, -0.7428571);
const RIM_DIR: vec3<f32> = vec3<f32>(0.3504383, 0.4505636, -0.8210269);

struct VertexInput {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
};

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) world_position: vec3<f32>,
};

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;
    output.position = camera.view_projection * vec4<f32>(input.position, 1.0);
    output.normal = input.normal;
    output.uv = input.uv;
    output.world_position = input.position;
    return output;
}

fn shade_sample(input: VertexOutput, color: vec4<f32>, face_normal: vec3<f32>) -> vec4<f32> {
    var normal = input.normal;
    if dot(normal, normal) < 1e-10 {
        normal = face_normal;
        if dot(normal, normal) < 1e-10 {
            normal = UP_DIR;
        }
    }
    normal = normalize(normal);

    let hemisphere = mix(0.28, 0.62, clamp(dot(normal, UP_DIR) * 0.5 + 0.5, 0.0, 1.0));
    let key = abs(dot(normal, KEY_DIR)) * 0.36;
    let fill = abs(dot(normal, FILL_DIR)) * 0.14;
    let rim_t = 1.0 - max(dot(normal, RIM_DIR), 0.0);
    let rim = rim_t * rim_t * 0.10;
    let light = clamp(hemisphere + key + fill + rim, 0.25, 1.25);
    return vec4<f32>(color.rgb * light, color.a);
}

@fragment
fn fs_opaque(input: VertexOutput) -> @location(0) vec4<f32> {
    let sample = textureSample(albedo, albedo_sampler, input.uv);
    let face_normal = cross(dpdy(input.world_position), dpdx(input.world_position));
    let color = shade_sample(input, sample, face_normal);
    return vec4<f32>(color.rgb, 1.0);
}

@fragment
fn fs_cutout(input: VertexOutput) -> @location(0) vec4<f32> {
    let sample = textureSample(albedo, albedo_sampler, input.uv);
    let face_normal = cross(dpdy(input.world_position), dpdx(input.world_position));
    if sample.a < material.alpha_cutoff {
        discard;
    }
    let color = shade_sample(input, sample, face_normal);
    return vec4<f32>(color.rgb, 1.0);
}

@fragment
fn fs_blended(input: VertexOutput) -> @location(0) vec4<f32> {
    let sample = textureSample(albedo, albedo_sampler, input.uv);
    let face_normal = cross(dpdy(input.world_position), dpdx(input.world_position));
    if sample.a < 0.02 {
        discard;
    }
    return shade_sample(input, sample, face_normal);
}