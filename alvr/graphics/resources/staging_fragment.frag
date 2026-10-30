#version 450

// Vulkan port of staging_fragment.glsl (GLES). `samplerExternalOES` becomes a combined image
// sampler with an immutable VkSamplerYcbcrConversion sampler (AHardwareBuffer external format).
// The GLES FIX_LIMITED_RANGE remap is gone: the sampler's conversion is set from the range and
// matrix the stream declares (see staging.rs), so the colour comes out right.
layout(set = 0, binding = 0) uniform sampler2D tex;

layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 out_color;

void main() {
    out_color = vec4(texture(tex, uv).rgb, 1.0);
}
