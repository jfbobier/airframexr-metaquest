#version 450

// Vulkan port of staging_vertex.glsl (GLES). `view_idx` was a uniform, now a push constant.
layout(push_constant) uniform PushConstants {
    int view_idx;
} pc;

layout(location = 0) out vec2 uv;

void main() {
    vec2 screen_uv = vec2(gl_VertexIndex & 1, gl_VertexIndex >> 1);
    gl_Position = vec4((screen_uv - 0.5f) * 2.f, 0, 1);
    uv = vec2((screen_uv.x + float(pc.view_idx)) / 2.f, screen_uv.y);
}
