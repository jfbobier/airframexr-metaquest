# AirFrameXR client for Meta Quest

> ## ALPHA TEST ONLY
>
> This client is an early build. The author has run it on **one Quest 3**, where the Vulkan renderer worked from the
> first launch. Nothing more is recorded: the checks below (colour levels, orientation, foveation, passthrough,
> restart) have not been written up, and **Quest Pro and Quest 2 have not been tried.** Expect rough edges, and do
> not rely on it. If you want a Quest client that is proven, use the stock
> [ALVR client](https://github.com/alvr-org/ALVR/releases).

The Quest client of [AirFrameXR](https://github.com/jfbobier/AirFrameXR), wireless PC VR for NVIDIA cards: the PC
runs the game through AirFrameXR's OpenXR runtime (frame generation, foveated encoding, NVENC), streams it with
the ALVR v20 protocol, and this app shows it on the headset. It pairs with the AirFrameXR streamer; it is built
around that one stream and is not meant for a stock ALVR server.

Targets: **Quest 3** (primary), **Quest Pro** (supported), **Quest 2** (best effort, never tuned for and never allowed
to block a change). Codec: **HEVC only**. **Meta Quest only:** the code for other headsets (Pico, YVR, HTC Vive, Lynx,
ByteDance body and face tracking, Quest 1) and their OpenXR loaders were removed, so use the stock ALVR client for those.

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
- **Quest only, lean.** The vendor OpenXR loaders (Pico, YVR, Quest 1, Lynx) are no longer fetched or packaged (the Quest 2, Pro
  and 3 use the generic Khronos loader), and about 1,400 lines of per-headset code are gone (HTC and Pico face tracking,
  HTC passthrough, BD body and motion tracking, the Pico/Vive/YVR workarounds, a per-frame view query only YVR used), plus two
  unused dependencies. The APK went from 26.6 MB to 20.5 MB. The streaming protocol and settings types shared with the
  streamer are untouched.
- **App name and icon:** the launcher entry is `AirFrameXR` with the AirFrameXR mark (an adaptive icon built from `branding/` in the
  AirFrameXR repository). The package name is `airframexr.client`, so it installs next to a stock ALVR client without touching it.
- **Hands as controllers** (like the visionOS client): with free hands the client binds `XR_EXT_hand_interaction`, sends the hand
  as that hand's controller (pose, pinch = trigger) next to the skeleton, and two pinches in a row (under 0.6 s each, the
  second within 0.9 s) raise X (left) or A (right) for 0.15 s, which the desktop mirror reads as a double click. Controllers are
  unchanged. It is off when the streamer's own `hand_tracking_interaction` or multimodal input is on. Target SDK 34.
- **Colour comes from what the stream declares.** The AirFrameXR chain is HEVC Main10 (10-bit), BT.709, full range, SDR.
  For that case the conversion is created as BT.709 full range and the GLES limited-range shader remap is gone. HDR or a
  limited-range stream keep the driver's values. The log prints what the driver suggested.

**Not done yet** (planned, nothing built): aligning the field-of-view restriction with the streamer's per-edge
settings, a richer Quest UI, and a fused single-pass renderer that would drop the staging pass. That last one is the
reason for the Vulkan port, and any speed gain from it is a **hypothesis that has not been measured**: there is no baseline
against the stock client and no profile of this one.

## Install (sideload)

The APK is signed with a debug key. Its package name is `airframexr.client`, so it installs next to a stock ALVR client and does
not replace it. Pairing is stored per package: the first time, pair it from the streamer ("Find headset...").

```
adb install -r airframexr-quest-0.1.0-alpha2.apk
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

Linux or WSL; the Android SDK platform 34, build-tools 34 and NDK r26b (26.1.10909125), `cargo-apk`, `cargo-ndk`, the Rust
`aarch64-linux-android` target and `unzip`.

```
git submodule update --init openvr
cargo xtask prepare-deps --platform android   # fetches the generic OpenXR loader
export JAVA_HOME=... ANDROID_HOME=~/Android/Sdk ANDROID_SDK_ROOT=~/Android/Sdk \
       ANDROID_NDK_HOME=~/Android/Sdk/ndk/26.1.10909125
cargo xtask build-client --release            # build/alvr_client_android/alvr_client_android.apk
```

The SPIR-V of the staging shaders is committed; `alvr/graphics/resources/compile_staging_shaders.sh` regenerates it
with the NDK's `glslc`.
