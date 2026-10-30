# AirFrameXR client for Meta Quest

> ## ALPHA TEST ONLY
>
> This client **builds, but has never been run on a headset.** Nothing in it has been seen working: not the
> lobby, not a stream, not a single frame. Expect a black screen, a crash, a wrong picture or no connection. Do
> not use it for anything you care about. If you want a Quest client that works today, use the stock
> [ALVR client](https://github.com/alvr-org/ALVR/releases).

The Quest client of [AirFrameXR](https://github.com/jfbobier/AirFrameXR), wireless PC VR for NVIDIA cards: the PC
runs the game through AirFrameXR's OpenXR runtime (frame generation, foveated encoding, NVENC), streams it with
the ALVR v20 protocol, and this app shows it on the headset. It pairs with the AirFrameXR streamer; it is built
around that one stream and is not meant for a stock ALVR server.

Targets: **Quest 3** (primary), **Quest Pro** (supported), **Quest 2** (best effort, never tuned for and never allowed
to block a change). Codec: **HEVC only**.

## Credits

This is a fork of [ALVR](https://github.com/alvr-org/ALVR) at tag `v20.14.1`, by polygraphene, the alvr-org
contributors and everyone listed in `README-ALVR.md` (the unchanged upstream README). The streaming protocol, the
client core, the OpenXR client, the decoder and the lobby are theirs. ALVR is MIT licensed and so is this fork
(see `LICENSE`). Base `v20.14.1` rather than upstream `master`, because the AirFrameXR streamer speaks the 20.14.1
protocol and `master` (21.0.0-dev) does not.

## What this fork changes

Branch `airframexr-quest`, on top of `v20.14.1`. Details, the GLES-to-Vulkan mapping, every deviation and the
device checklist: [`QUEST_VULKAN_PORT.md`](QUEST_VULKAN_PORT.md).

- **Vulkan only, no GLES.** The renderer (`alvr_graphics`) and the OpenXR session use `XR_KHR_vulkan_enable2`; the
  runtime picks the device and the Vulkan instance and device are wrapped into wgpu 24 with wgpu-hal. The GLES and EGL code and the
  OpenGL C API are removed. The manifest requires Vulkan 1.1. The GLES code was the reference: each piece was ported function
  by function, and anything that differs is listed in the notes.
- **The decoder's `AHardwareBuffer` is imported as a Vulkan image** with an immutable YCbCr sampler (the staging pass), in
  place of the EGL image and `samplerExternalOES`. The two-pass structure of the GLES client is kept.
- **Colour comes from what the stream declares.** The AirFrameXR chain is HEVC Main10 (10-bit), BT.709, full range, SDR.
  For that case the conversion is created as BT.709 full range and the GLES limited-range shader remap is gone. HDR or a
  limited-range stream keep the driver's values. The log prints what the driver suggested.

**Not done yet** (planned, nothing built): aligning the field-of-view restriction with the streamer's per-edge
settings, a richer Quest UI, and a fused single-pass renderer that would drop the staging pass. That last one is the
reason for the Vulkan port, and any speed gain from it is a **hypothesis that has not been measured**: there is no baseline
against the stock client and no profile of this one.

## Install (sideload)

The APK is signed with a debug key. It uses the stock ALVR client's package name (`alvr.client.stable`), so a stock ALVR
client already on the headset has to be uninstalled first (Android refuses a different signature).

```
adb uninstall alvr.client.stable
adb install -r airframexr-quest-0.1.0-alpha1.apk
```

Then start the AirFrameXR streamer on the PC and connect as with any ALVR client.

## What to check first (on a headset, please report)

1. `adb logcat` shows the `Vulkan: instance version ...` and `Staging: decoder buffer format ... (driver suggested ...)`
   lines and no panic or Vulkan validation error.
2. The lobby is upright and its text readable.
3. The stream is upright, not mirrored, and a grey ramp has correct black and white levels (no crushed blacks, no clipped
   whites).
4. Foveation, upscaling, passthrough blend and chroma key still work.
5. Lobby to stream and back, and a stream restart, still work.
6. Same on Quest 3, then Quest Pro, then Quest 2.

Open questions only a device can answer: whether each headset decodes HEVC Main10 in hardware, whether the Qualcomm
driver accepts the forced BT.709 full-range conversion, and the AHardwareBuffer import path.

## Build

Linux or WSL; the Android SDK platform 32, build-tools 34 and NDK r26b (26.1.10909125), `cargo-apk`, `cargo-ndk`, the Rust
`aarch64-linux-android` target and `unzip`.

```
git submodule update --init openvr
cargo xtask prepare-deps --platform android   # the last download (Lynx loader) may fail; the Quest loaders are already fetched
export JAVA_HOME=... ANDROID_HOME=~/Android/Sdk ANDROID_SDK_ROOT=~/Android/Sdk \
       ANDROID_NDK_HOME=~/Android/Sdk/ndk/26.1.10909125
cargo xtask build-client --release            # build/alvr_client_android/alvr_client_android.apk
```

The SPIR-V of the staging shaders is committed; `alvr/graphics/resources/compile_staging_shaders.sh` regenerates it
with the NDK's `glslc`.
