#version 450

// Vulkan port of staging_fragment.glsl (GLES). `samplerExternalOES` becomes a combined image
// sampler with an immutable VkSamplerYcbcrConversion sampler (AHardwareBuffer external format).
// `#ifdef FIX_LIMITED_RANGE` becomes a specialization constant so one SPIR-V module serves both.
layout(set = 0, binding = 0) uniform sampler2D tex;

layout(constant_id = 0) const bool FIX_LIMITED_RANGE = false;

// Convert from limited colors to full
const float LIMITED_MIN = 16.0 / 255.0;
const float LIMITED_MAX = 235.0 / 255.0;

layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 out_color;

void main() {
    vec3 color = texture(tex, uv).rgb;
    if (FIX_LIMITED_RANGE) {
        color = LIMITED_MIN + ((LIMITED_MAX - LIMITED_MIN) * color);
    }
    out_color = vec4(color, 1.0);
}
