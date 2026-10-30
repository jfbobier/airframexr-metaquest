# Quest client: Vulkan port (branch `airframexr-quest`, base ALVR v20.14.1)

Status: **builds** (`cargo xtask build-client --release` on WSL, arm64-v8a APK, manifest declares
Vulkan 1.1, the `.so` no longer links GLES/EGL). **Nothing has run on a headset.** Every behaviour
below marked "unverified" is exactly that.

Target: Quest 3 first, Quest Pro supported, Quest 2 best effort. HEVC only (no AV1 work).

## What was ported (GLES -> Vulkan)

| GLES (v20.14.1) | Vulkan |
|---|---|
| `GraphicsContext::new_gl()`: wgpu on GLES, EGL display/config/context shared with OpenXR | `GraphicsContext::new_vulkan(&XrVulkanBridge)`: the OpenXR runtime picks the VkPhysicalDevice and creates VkInstance/VkDevice (`XR_KHR_vulkan_enable2`), wrapped into wgpu 24 with wgpu-hal `Instance::from_raw` / `Adapter::device_from_raw` (the bevy_mod_openxr approach). Created once before the session loop, reused by every session, like the GL context. |
| `xr::OpenGlEs`, `khr_opengl_es_enable`, `SessionCreateInfo::Android{egl..}` | `xr::Vulkan`, `khr_vulkan_enable2`, `xr::vulkan::SessionCreateInfo{instance,physical_device,device,queue family/index}` |
| `make_current()` calls | gone (no current-context concept) |
| swapchain formats `SRGB8_ALPHA8 / RGBA8 / RGBA16F` (GL enums) | `VkFormat` `R8G8B8A8_SRGB / R8G8B8A8_UNORM / R16G16B16A16_SFLOAT`, same priority order, same HDR rule, same RGBA8 fallback |
| `create_gl_swapchain`: GL texture ids -> wgpu textures (`texture_from_raw`) | `create_vk_swapchain`: `VkImage`s -> wgpu textures (`hal::vulkan::Device::texture_from_raw`, no ownership) |
| staging pass: EGLImage + `samplerExternalOES` + GL FBO per eye | `staging.rs`: AHardwareBuffer imported as a VkImage (external format), immutable YCbCr sampler, raw render pass per eye into the wgpu staging texture, **recorded inline in wgpu's command buffer** (`CommandEncoder::as_hal_mut` + `raw_handle`) so the order (staging first, then reprojection) is the GLES one and wgpu's own fence tells when the imported buffer is free |
| `staging_vertex/fragment.glsl` (ESSL 300) | `staging_vertex.vert`, `staging_fragment.frag` (GLSL 450, same maths), committed as SPIR-V (`compile_staging_shaders.sh`, glslc from the NDK). the GLES `FIX_LIMITED_RANGE` remap is removed (see Colour) |
| `view_idx` uniform | push constant |
| `eglCreateImage` / `eglDestroyImage` per frame | `vkCreateImage` + import `vkAllocateMemory` + view + descriptor set per frame, freed when wgpu reports the submission done (no caching, 1:1 with GLES) |
| manifest `opengles_version 3.2` | `android.hardware.vulkan.version` 0x401000 (1.1) |
| C API `alvr_*_opengl` | **removed** (took GL texture ids; no consumer in the repo) |

wgpu reprojection pass, WGSL shaders, foveated decode, upscaler, passthrough, chroma key,
composition layer settings, lobby, interaction, timing order: unchanged.

## Deviations from GLES, and why

1. **Projection y flip removed** (`projection_from_fov`). GLES had `-b`, `-d` to give the GL runtime a
   bottom-left-origin image. Vulkan swapchains are top-left and wgpu's Vulkan backend maps NDC y-up
   to it. Unverified: if the image is upside down, this is the line.
2. **Staging texture usage** `RENDER_ATTACHMENT | TEXTURE_BINDING` (was `COPY_DST | TEXTURE_BINDING`)
   because it is a render target of the raw pass.
3. **Per-frame import failures** (vkCreateImage, vkAllocateMemory, ...) log an error and repeat the
   previous frame instead of aborting (GLES `ck!` aborted only in debug builds). Setup failures panic
   with the Vulkan result.
4. **Vulkan 1.1 required** (YCbCr conversion, external memory are core 1.1); startup fails loudly otherwise.
5. Debug builds enable `InstanceFlags::DEBUG|VALIDATION` and the Khronos validation layer *if present*
   (not shipped on Quest by default). No debug-utils messenger is created.
6. FFR through `fb_foveation` is still ignored at swapchain creation, as before (`foveation` was unused in GLES).

## Not done

- Fused single pass (sampling the YCbCr image directly in the reprojection pass, removing the staging
  pass). **UNMEASURED hypothesis**, see the header of `staging.rs`. Needs a baseline first.
- No decoder, FOV-restriction or UI changes.

## Unverified / risks, most likely first

1. **Colour range and matrix.** GLES let the driver convert; Vulkan uses the buffer's *suggested*
   model/range, except for SDR full-range streams where BT.709 full range is forced (see Colour). Check black and white levels.
2. **Upside-down or mirrored picture** (deviation 1; also the staging orientation, derived on paper).
3. **Layout / sync with wgpu** for the staging texture (contract in the `staging.rs` header). Symptoms:
   flicker, one black frame at stream start, validation errors.
4. **AHardwareBuffer import** on Qualcomm: foreign-queue acquire barrier, memory type choice.
5. Quest 2 (older driver): Vulkan 1.1 only, chroma filter feature bits.
6. Frame resource lifetime (max 6 buffers in flight, then the CPU waits).

## First things to test on a device

1. `adb logcat`: "Vulkan: instance version ..." and "Staging: decoder buffer format ..." lines, no panic.
2. Lobby renders upright, text readable.
3. Stream: upright, not mirrored, correct colours and black level (grey ramp), no flicker.
4. Foveated encoding on/off, upscaling on/off, passthrough blend and chroma key.
5. Lobby <-> stream <-> lobby, headset doff/don, stream restart with a different codec (format change path).
6. Repeat on Quest 3, Quest Pro, Quest 2. A debug build with the validation layer, if it can be side-loaded.

## Colour (changed after the first port commit)

The streamer declares BT.709, full range, sRGB transfer in the VUI (`nvenc/upstream/platform/win32/VideoEncoderNVENC.cpp`,
`use_full_range` is forced on by `tools/host/src/nvidia_profile.rs`). The host's own colour-fit measurement of NVENC's
reconstruction against its input is BT.709 full range, gain 0.996-0.998, offset about 0 (`docs/BENCHMARK_DESIGN.md`), so the
declaration and the conversion agree. The bench's reconstruction is P010, i.e. the same 10-bit path the live stream uses, but it is the encoder's own reconstruction, not what
the Quest's decoder outputs.

**The AirFrameXR chain** (`tools/host/src/nvidia_profile.rs`): HEVC, `use_10bit_encoder` 1 (ABGR10 into NVENC, Main10 out), `use_full_range` 1,
`enable_hdr` 0, so the Quest decodes 10-bit SDR BT.709 full range and the override below is the case that always applies. A 10-bit decoder
output is a P010-style AHardwareBuffer with an external format; the conversion handles the bit depth itself. Staging and swapchain are
still 8-bit (sRGB or RGBA8), as in GLES, so the 10-bit gain is in the codec (less banding at a given bitrate), not on the display.
Not verified: Main10 HEVC hardware decode on each of Quest 3, Pro and Quest 2.

- SDR and `use_full_range`: the YCbCr conversion is created with `YCBCR_709` + `ITU_FULL` instead of the driver's suggestion, and the
  GLES `FIX_LIMITED_RANGE` shader remap is deleted. GLES squeezed RGB into 16-235 to undo the driver's limited-range guess; that
  remap also scaled chroma by 219/224 (about 2 % less saturation than a true full-range decode), so the output can differ slightly.
- HDR, or a limited-range stream: the driver's suggested model and range are kept, as GLES did (GLES needed no fix there).
- The log line `Staging: decoder buffer format ...` now also prints what the driver suggested. First device check: if the suggested
  range is already `ITU_FULL` and model `YCBCR_709`, the override is a no-op; otherwise compare a grey ramp.
- Untested risk: some drivers may reject or ignore an application-chosen model/range on an external-format conversion.

## Quest-only lean-up (after the port)

The client now targets Meta Quest only. Removed, each in its own commit: the vendor OpenXR loaders and their download
(`alvr/xtask`, `libopenxr_loader` is the only one packaged and loaded); HTC facial tracking and passthrough, Pico face tracking
and configuration, BD (ByteDance) body and motion tracking (`extra_extensions/*`, `interaction.rs`, `stream.rs`, `lobby.rs`);
the Pico / Vive / YVR / Lynx / Quest 1 branches (controller offsets and profiles, the combined eye-gaze source used only off
Quest, velocity workarounds, YVR custom reprojection with its per-frame `locate_views`, the YVR passthrough start, the sRGB
condition for Pico/Lynx which is simply on for Quest); two unused dependencies (`pollster`, `mdns-sd`); the Pico Store xtask
flavour. Kept on purpose: `alvr_system_info::Platform` (shared with other crates, the client still uses it to tell Quest models
apart), the shared `alvr_packets` / `alvr_session` types (the HTC/BD fields and settings stay in the protocol, the client sends
`None` / disabled for them), the graphics lobby's BD skeleton drawing (dead on Quest, in `alvr_graphics`, touches the approved
panel code path), `Cargo.toml`'s `supportedDevices=all` metadata, and the AV1 capability flag for Quest 3/3S (the
streamer chooses HEVC). Unverified: everything above only builds; nothing was run on a headset.
