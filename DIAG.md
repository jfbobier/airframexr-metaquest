# Diagnostics build: how to read it

Everything below goes to logcat under one tag, `AirFrameXR-diag`, **regardless of the streamer's log level**
(the client's normal logger drops info lines once connected, so `info!` output never shows while streaming).

```
adb logcat -c                       # clear first: the headset's log buffer is small and rolls in minutes
(launch the app, start the stream, wait 20-30 s, with the chroma key on)
adb logcat -d -s AirFrameXR-diag:V > diag.txt
```

Live: `adb logcat -s AirFrameXR-diag:V`. Our normal lines, when they pass the streamer's filter, use the tag
`[ALVR NATIVE-RUST]`; `adb logcat -d -s AirFrameXR-diag:V "[ALVR NATIVE-RUST]:V"` shows both.

## One-time lines (app start, or each stream start)

| Line | What it tells you |
|---|---|
| `[vulkan] instance API ...; device '...' ... driver ... maxPushConstantsSize ...` | GPU, Vulkan API and driver version, limits, enabled device extensions. Printed at app start and again at each stream start. Chroma constants need 128 bytes of push constants: `maxPushConstantsSize` must be >= 128. |
| `[session] state changed to ...` | OpenXR session lifecycle (READY, SYNCHRONIZED, VISIBLE, FOCUSED, STOPPING). |
| `[passthrough] layer created (...)` / `layer creation FAILED (...): <error>` | Whether the FB passthrough layer exists. The stock code swallows a failure here; without the layer a keyed stream has nothing behind it. |
| `[passthrough] PassthroughStateChangedFB flags ...` | The runtime reporting the passthrough going into an error state or needing a re-init. |
| `[stream-config] ...` | Codec requested, 10-bit setting, negotiated view size / refresh / full range / HDR / gamma, foveated encoding, `avoid_video_glitching`, buffering, clientside foveation, upscaling. |
| `[passthrough] stream start: passthrough_mode=... ck_channel0/1/2=...` | The exact numbers the shader gets. Mode 0 = blend, 1 = RGB key, 2 = HSV key. For the RGB key each channel is `[start_max, start_min, end_min, end_max]` in 0..1 around the key colour; a pixel is transparent when it is inside the inner band. Also lists the MediaCodec options the streamer sent. Printed again on every real-time config change (`[passthrough] real-time config: ...`). |
| `[decoder] config NAL N bytes, codec ...` | The decoder starts; first bytes of the parameter sets. |
| `[vulkan] ...` + `[swapchain] chosen format ...` | Swapchain VK_FORMAT (43 = R8G8B8A8_SRGB, 37 = R8G8B8A8_UNORM, 97 = R16G16B16A16_SFLOAT), size per eye, the formats the runtime offered. |
| `[stream-renderer] swapchain format ..., staging ..., srgb_correction ..., forced BT.709 full range ..., shader constants {...}` | What the render pass was built with. |
| `[staging] decoder buffer: WxH ... AHB format 0x.. usage 0x.. vk format .. external_format ..` | The AHardwareBuffer the decoder hands over (AHB format 0x36 = YCbCr P010, i.e. 10-bit; 0x23 = 8-bit YCbCr 420). |
| `[staging] YCbCr conversion: applied model/range (driver suggested model/range, forced=..)` | What the sampler conversion uses versus what the driver suggested. |
| `[layers] submitting N layer(s), in order: ... projection layer flags ... alpha config ...` | Printed whenever the set of layers changes. A keyed stream should show passthrough first, then the stream projection with `BLEND_TEXTURE_SOURCE_ALPHA` and premultiplied true. |
| `[staging-error] ...`, `[vk-error] ...` | Any import failure (the frame is repeated) or a Vulkan error right before a panic. Normally absent. |

## The `[diag]` line, once per second while streaming

```
[diag] 1.0s | frames 90 new 90 repeat 0 ts_stalled 0 ts_backward 0 | submit interval ms min/avg/max 10.9/11.1/12.0 over1.5x 0
 | render() ms avg/max 2.1/4.0 decoder-wait 0.5/2.0 draw 0.9/1.6 | rx pkts 90 gap ms avg/max 11.1/14.0 over1.5x 0 loss 0
 drop_saturation 0 drop_wait_idr 0 idr_req 0 | decode ms avg/max 3.0/5.0 decoder-queue 2.0/6.0 compositor 2.0/4.0 pipeline 38.0/45.0
 vsync-queue 8.0/10.0 | staging ok 90 fail 0
```
(wrapped here; it is one line. The numbers are an illustration of a healthy stream, not a measurement.)

| Field | Meaning |
|---|---|
| `frames` | Times the render loop submitted a frame (should match the display rate, e.g. 90). |
| `new` / `repeat` | Frames that got a new decoder buffer / frames that re-showed the previous one. `new` is the **stream's** frame rate as the client sees it; `repeat` is normal when the stream runs below the display rate (e.g. `new 30 repeat 60` is a 30 fps stream). |
| `ts_stalled` / `ts_backward` | New buffers whose timestamp equals / precedes the previous new one. Should be 0. |
| `submit interval` | Time between consecutive `render()` completions. `over1.5x` counts intervals longer than 1.5 frame periods: those are visible hitches from the headset's point of view. |
| `render() ms` | Time inside the stream render call (includes the decoder wait). `decoder-wait` is the time spent polling the decoder for a frame (up to 0.8 of a frame period when none is ready). `draw` is the CPU time of the staging + reprojection command recording and submit. |
| `rx pkts`, `gap ms` | Video frames received from the network in the second, and the time between consecutive frames' arrival (avg/max). `over1.5x` = gaps longer than 1.5 nominal frame periods. |
| `loss` | Packets flagged as lost by the socket layer. Each also triggers an IDR request. |
| `drop_saturation` | Packets the decoder refused (queue full). `drop_wait_idr` = dropped while waiting for a keyframe (only with `avoid_video_glitching`). `idr_req` = keyframes requested. |
| `decode` | Packet received -> decoded (decoder latency). |
| `decoder-queue` | Decoded -> picked up for rendering (time a decoded frame waited). This is the cheap stand-in for queue depth. |
| `compositor` | Picked up -> submitted (the client's own render time per frame). |
| `pipeline` | Pose sampled -> displayed, as the client reports to the streamer. |
| `vsync-queue` | Time between submit and the vsync it targets (reported by the runtime). |
| `staging ok/fail` | AHardwareBuffer imports that worked / failed. Failures repeat the previous frame and also print `[staging-error]`. |

### Telling the causes apart

- **Network jitter or loss:** `rx gap` max and `over1.5x` high, `loss > 0`, `decode` normal. Frames then arrive late or in bursts; `repeat` rises and `submit interval` stays regular.
- **Decoder:** `decode` max high, `drop_saturation > 0`, `decoder-queue` growing; `rx gap` normal.
- **Render path / headset GPU:** `submit interval over1.5x` with `draw` or `compositor` max high while `rx gap` and `decode` are normal; the runtime's own `VrApi ... Stale=N` lines rise too.
- **Waiting for the stream:** `decoder-wait` max close to 0.8 of a frame period together with `repeat` > 0 means the renderer waited for a frame that did not come in time.
- **A low-rate stream:** `new` far below `frames` for the whole run is the streamer's frame rate (or a cap), not a client hitch.

## Upstream noise in logcat (which lines matter)

| Line | Whose | Reading |
|---|---|---|
| `FIXME: Leaking Imagereader!` (E, tag `[ALVR NATIVE-RUST]`) | upstream ALVR, `client_core/src/video_decoder/android.rs` | Printed when a stream stops: the decoder's ImageReader is deliberately leaked (known upstream TODO). Benign for a few restarts. |
| `AudioRecord: getMinBufferSize(): Invalid channel configuration` (E, repeated about a dozen times just before `Stream starting`) | Android, from upstream audio probing | The client probing microphone channel/sample-rate combinations. Benign unless the microphone is expected to work and does not. |
| `MediaCodec discarded an unknown buffer` | Android | Appears when the decoder is torn down (stream stop). Only interesting if it appears mid-stream together with `drop_*` or `[staging-error]` lines. |
| `QC2V4l2Codec ... Ignore resolution setting when o/p port is streaming` | Qualcomm codec | Vendor message: the decoder is configured with a placeholder size (512x1024, see the comment in `android.rs`) and then runs at the stream size. Benign. |
| `VideoRenderQualityTracker: Video playback content time jumped N ms forward`, `Rendered frame is earlier than the next expected frame` | Android media framework | Compares the frame timestamps we give MediaCodec (the streamer's tracking times) with real time. A steady jump of about 33 ms means the frames' timestamps are 33 ms apart, i.e. a 30 fps stream: cross-check with `new` in the `[diag]` line. Not a client error. |
| `VrApi: FPS=.. Stale=.. Early=.. Tear=..` | Meta runtime, every second | The compositor's own view: `Stale` > 0 means it had to repeat the app's previous frame (the app was late). Good cross-check for `submit interval over1.5x`. |
| `[CT] PTApiClients: ... alvr.client.stable, PT=1, ... LCnt=a/b(rec=.., proj=.., bg=..)` | Meta runtime | Per-app layer accounting: `PT=1` = the app has an active passthrough layer; `proj` counts projection layers. Useful next to our `[layers]` line. |

## What this build does not do

It does not change behaviour except for one separate fix: the HSV chroma key branch in `stream.wgsl` tested the
mode value 3 while the code sends 2 (upstream ALVR bug, same in the GLES client; it never affected the RGB key).
