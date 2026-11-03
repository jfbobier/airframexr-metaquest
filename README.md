# AirFrameXR client for Meta Quest

The Meta Quest client of [AirFrameXR](https://github.com/jfbobier/AirFrameXR): wireless PC VR for NVIDIA cards. The PC runs the
game through AirFrameXR's OpenXR runtime (frame generation, foveated encoding, NVENC), streams it with the **ALVR protocol
version 20** (20.14.1), and this app shows it on the headset, and the controllers drive the game and the desktop mirror.

**Version 1.3.0. Tested on a Meta Quest 3 only** (see [What was tested](#what-was-tested)). Quest Pro and Quest 2 are the
intended targets too, but nobody has run it on them yet. **Controllers only: there is no hand pinch input** (see [Hands](#hands)).

It pairs with the AirFrameXR streamer. It is built around that one stream (HEVC, 10-bit) and is not meant for a stock ALVR
server. AirFrameXR is an independent project and is not affiliated with the ALVR project.

## Credits

This client is a fork of **[ALVR](https://github.com/alvr-org/ALVR)** at tag `v20.14.1`. The streaming protocol, the client core,
the OpenXR client, the video decoder path, the foveated-encoding decoding, the lobby, the tracking and input code, the
settings and the build system are the work of polygraphene, the alvr-org contributors and everyone listed in
[`README-ALVR.md`](README-ALVR.md) (the unchanged upstream README). Nearly everything that makes this app run is theirs, and
this fork would not exist without it. ALVR is MIT licensed and so is this fork (see `LICENSE`).

**.BarThi** pushed for this Meta Quest fork with Vulkan, and tested it. Thank you.

The base is `v20.14.1` and not upstream `master` because the AirFrameXR streamer speaks the 20.14.1 protocol, and `master`
(21.0.0-dev) does not.

## What this fork changes

Branch `airframexr-quest`, on top of `v20.14.1`. The GLES-to-Vulkan mapping, every deviation from the original code and the
device checklist are in [`QUEST_VULKAN_PORT.md`](QUEST_VULKAN_PORT.md); the welcome panel in [`QUEST_UI.md`](QUEST_UI.md).

**Modernisation**

- **Vulkan renderer, no GLES.** The renderer and the OpenXR session use `XR_KHR_vulkan_enable2`: the runtime picks the device and
  the Vulkan instance and device are wrapped into wgpu with wgpu-hal. The GLES and EGL code and the OpenGL C API are gone. The
  GLES client was the reference: each piece was ported function by function, and what differs is listed in the port notes. The
  decoder's `AHardwareBuffer` is imported as a Vulkan image with an immutable YCbCr sampler, in place of the EGL image.
- **Colour from what the stream declares.** The AirFrameXR chain is HEVC Main10, BT.709, full range, SDR. The Quest's driver
  suggests *narrow* range for that stream (this is why the old client had a limited-range shader fix), so the conversion is
  created as BT.709 full range and the shader remap is gone.
- **Android 14 target (SDK 34)** and the current Meta manifest: `horizonos.permission.HAND_TRACKING` replaces the deprecated
  `com.oculus` name.
- **Quest only.** Everything that served other headsets (Pico, YVR, Lynx, HTC and ByteDance extensions, their OpenXR loaders,
  per-platform workarounds, unused dependencies) was removed: about 1,500 lines. With debug symbols stripped, the APK went from
  26.6 MB to 3.5 MB. (Crash reports will have no symbol names.)
- **App identity.** Package `airframexr.client`, label `AirFrameXR`, the AirFrameXR launcher icon. It installs next to a stock
  ALVR client and does not replace it.
- **HSV chroma key fix.** The shader tested the wrong mode number for the HSV key, so HSV mode compared RGB values against HSV
  ranges. The same bug is in stock ALVR 20.14.1. The RGB key was not affected.

**Interface**

- **Welcome panel** in place of the text HUD: the AirFrameXR mark, the connection status ("On the PC, click Find headset..."), and a
  floor-height adjustment (-50 to +50 cm, plus, minus and reset) with the headset's own height and the height the game will see.
  Pointing works with a controller ray and trigger. The offset is saved on the headset and applied
  when the stream starts. The panel is shown while waiting and disappears when the stream starts.

**Hands**

- **No pinch input.** Hand pinch was tried (hands as controllers for the desktop mirror: hand pose, pinch as trigger, double
  pinch as X / A) and **removed**, for the same reason as on the AirFrameXR Vision Pro client: it is not reliable. The pinch signal
  is noisy, Meta's own pinch bit is often missing at the moment of a pinch, the hand's grip pose is far from the system's aim ray
  and the left hand did not register reliably. **Use the controllers** to point and click, in the welcome panel, in games and in
  the desktop mirror. The hand skeleton is still sent as the stock ALVR client does, for games that read it.

## What was tested

Tested by the author on **one Meta Quest 3**, with the AirFrameXR streamer on an RTX 5090 PC:

- Vulkan rendering of the stream at 90 Hz, HEVC Main10 (10-bit), BT.709 full range, with the AirFrameXR streamer.
- The RGB chroma key (green key colour).
- The welcome panel.
- Controllers, in several different games: all fine.
- The 1.0 build (pinch removed), on the same Quest 3: runs well, the Vulkan rendering is reactive (the author's report; no measurements were taken).

**Not tested, so do not assume it works:** Quest Pro, Quest 2 and Quest 3S; the floor-height
offset in a game; the HSV chroma key; long sessions; wired (USB) use and the streamer's USB autolaunch; the Vulkan validation
layers (no release build ran with them, so the absence of warnings is not proof that the raw Vulkan code is correct).

## Install (sideload)

The APK is signed with a debug key. Its package name is `airframexr.client`, so it installs next to a stock ALVR client. Pairing is
stored per package: the first time, pair it from the streamer ("Find headset...").

```
adb install -r airframexr-quest-1.3.0.apk
```

Then start the AirFrameXR streamer on the PC and start the app (it appears as **AirFrameXR**).

## Build

Linux or WSL: the Android SDK platform 34 and build-tools 34, NDK r26b (26.1.10909125), `cargo-apk`, `cargo-ndk`, the Rust
`aarch64-linux-android` target and `unzip`.

```
git submodule update --init openvr
cargo xtask prepare-deps --platform android
export JAVA_HOME=... ANDROID_HOME=~/Android/Sdk ANDROID_SDK_ROOT=~/Android/Sdk \
       ANDROID_NDK_HOME=~/Android/Sdk/ndk/26.1.10909125
cargo xtask build-client --release            # build/alvr_client_android/alvr_client_android.apk
```

The SPIR-V of the staging shaders is committed; `alvr/graphics/resources/compile_staging_shaders.sh` regenerates it with the NDK's
`glslc`. The app version (1.3.0, which cargo-apk turns into the Android `versionName`) is the `alvr_client_openxr` crate's version in
`alvr/client_openxr/Cargo.toml`. The ALVR protocol version the streamer checks is `alvr_common`'s, still 20.14.1.

## License

MIT, as ALVR (see `LICENSE`, which keeps the original copyright holders).
