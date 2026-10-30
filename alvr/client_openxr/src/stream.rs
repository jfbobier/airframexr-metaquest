use crate::{
    graphics::{self, ProjectionLayerAlphaConfig, ProjectionLayerBuilder},
    interaction::{self, InteractionContext, InteractionSourcesConfig},
};
use alvr_client_core::{
    video_decoder::{self, VideoDecoderConfig, VideoDecoderSource},
    ClientCoreContext,
};
use alvr_common::{
    anyhow::Result,
    diag, error,
    glam::{UVec2, Vec2},
    info,
    parking_lot::RwLock,
    Pose, RelaxedAtomic, ViewParams, HAND_LEFT_ID, HAND_RIGHT_ID, HEAD_ID,
};
use alvr_graphics::{
    compute_target_view_resolution, GraphicsContext, StreamRenderer, StreamViewParams,
};
use alvr_packets::{FaceData, RealTimeConfig, StreamConfig};
use alvr_session::{
    ClientsideFoveationConfig, ClientsideFoveationMode, ClientsidePostProcessingConfig, CodecType,
    FoveatedEncodingConfig, MediacodecProperty, PassthroughMode, UpscalingConfig,
};
use alvr_system_info::Platform;
use openxr as xr;
use std::{
    ptr,
    rc::Rc,
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const DECODER_MAX_TIMEOUT_MULTIPLIER: f32 = 0.8;

pub struct ParsedStreamConfig {
    pub view_resolution: UVec2,
    pub refresh_rate_hint: f32,
    pub use_full_range: bool,
    pub encoding_gamma: f32,
    pub enable_hdr: bool,
    pub passthrough: Option<PassthroughMode>,
    pub foveated_encoding_config: Option<FoveatedEncodingConfig>,
    pub clientside_foveation_config: Option<ClientsideFoveationConfig>,
    pub clientside_post_processing: Option<ClientsidePostProcessingConfig>,
    pub upscaling: Option<UpscalingConfig>,
    pub force_software_decoder: bool,
    pub max_buffering_frames: f32,
    pub buffering_history_weight: f32,
    pub decoder_options: Vec<(String, MediacodecProperty)>,
    pub interaction_sources: InteractionSourcesConfig,
}

impl ParsedStreamConfig {
    pub fn new(config: &StreamConfig) -> Self {
        let negotiated = &config.negotiated_config;
        let video = &config.settings.video;
        alvr_common::diag!(
            "[stream-config] preferred codec {:?}, 10-bit (setting) {}, negotiated: view {}x{} refresh {} Hz, full range {}, \
             hdr {}, encoding gamma {}, foveated encoding {}, wired {}; client: avoid_video_glitching {}, \
             enforce_server_frame_pacing {}, max_buffering_frames {}, buffering_history_weight {}, force software decoder {}, \
             clientside foveation {}, clientside post processing {:?}, upscaling {}",
            video.preferred_codec,
            video.encoder_config.use_10bit,
            negotiated.view_resolution.x,
            negotiated.view_resolution.y,
            negotiated.refresh_rate_hint,
            negotiated.use_full_range,
            negotiated.enable_hdr,
            negotiated.encoding_gamma,
            negotiated.enable_foveated_encoding,
            negotiated.wired,
            config.settings.connection.avoid_video_glitching,
            video.enforce_server_frame_pacing,
            video.max_buffering_frames,
            video.buffering_history_weight,
            video.force_software_decoder,
            match video.clientside_foveation.as_option() {
                None => "off".to_owned(),
                Some(c) => match c.mode {
                    ClientsideFoveationMode::Static { level } => {
                        format!("static level {}, vertical offset {}", level as i32, c.vertical_offset_deg)
                    }
                    ClientsideFoveationMode::Dynamic { max_level } => {
                        format!("dynamic max level {}, vertical offset {}", max_level as i32, c.vertical_offset_deg)
                    }
                },
            },
            video.clientside_post_processing.as_option(),
            video.upscaling.as_option().is_some(),
        );
        alvr_common::diag!(
            "[passthrough] stream start: {} | mediacodec options sent by the streamer: {:?}",
            alvr_graphics::passthrough_diag_string(video.passthrough.as_option()),
            video
                .mediacodec_extra_options
                .iter()
                .map(|(k, v)| format!("{k}={}", v.value))
                .collect::<Vec<_>>()
        );

        Self {
            view_resolution: config.negotiated_config.view_resolution,
            refresh_rate_hint: config.negotiated_config.refresh_rate_hint,
            use_full_range: config.negotiated_config.use_full_range,
            encoding_gamma: config.negotiated_config.encoding_gamma,
            enable_hdr: config.negotiated_config.enable_hdr,
            passthrough: config.settings.video.passthrough.as_option().cloned(),
            foveated_encoding_config: config
                .negotiated_config
                .enable_foveated_encoding
                .then(|| config.settings.video.foveated_encoding.as_option().cloned())
                .flatten(),
            clientside_foveation_config: config
                .settings
                .video
                .clientside_foveation
                .as_option()
                .cloned(),
            clientside_post_processing: config
                .settings
                .video
                .clientside_post_processing
                .as_option()
                .cloned(),
            upscaling: config.settings.video.upscaling.as_option().cloned(),
            force_software_decoder: config.settings.video.force_software_decoder,
            max_buffering_frames: config.settings.video.max_buffering_frames,
            buffering_history_weight: config.settings.video.buffering_history_weight,
            decoder_options: config.settings.video.mediacodec_extra_options.clone(),
            interaction_sources: InteractionSourcesConfig::new(config),
        }
    }
}

/// Counters behind the once-per-second `[diag]` line (see DIAG.md). Plain arithmetic per frame; the
/// only formatting and allocation happens once a second, in `emit`.
struct DiagWindow {
    started: Instant,
    last_render_end: Option<Instant>,
    last_timestamp: Option<Duration>,
    frames: u32,
    new_buffers: u32,
    repeats: u32,
    stalled_timestamps: u32,
    backward_timestamps: u32,
    intervals: u32,
    interval_sum: Duration,
    interval_min: Duration,
    interval_max: Duration,
    intervals_over: u32,
    render_sum: Duration,
    render_max: Duration,
    poll_sum: Duration,
    poll_max: Duration,
    draw_sum: Duration,
    draw_max: Duration,
}

impl DiagWindow {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            last_render_end: None,
            last_timestamp: None,
            frames: 0,
            new_buffers: 0,
            repeats: 0,
            stalled_timestamps: 0,
            backward_timestamps: 0,
            intervals: 0,
            interval_sum: Duration::ZERO,
            interval_min: Duration::MAX,
            interval_max: Duration::ZERO,
            intervals_over: 0,
            render_sum: Duration::ZERO,
            render_max: Duration::ZERO,
            poll_sum: Duration::ZERO,
            poll_max: Duration::ZERO,
            draw_sum: Duration::ZERO,
            draw_max: Duration::ZERO,
        }
    }

    fn reset(&mut self, now: Instant) {
        let last_render_end = self.last_render_end;
        let last_timestamp = self.last_timestamp;
        *self = Self::new();
        self.started = now;
        self.last_render_end = last_render_end;
        self.last_timestamp = last_timestamp;
    }

    #[allow(clippy::too_many_arguments)]
    fn frame(
        &mut self,
        frame_interval: Duration,
        new_buffer: Option<Duration>,
        poll: Duration,
        draw: Duration,
        render_start: Instant,
        render_end: Instant,
    ) {
        self.frames += 1;
        if let Some(timestamp) = new_buffer {
            self.new_buffers += 1;
            if let Some(last) = self.last_timestamp {
                if timestamp == last {
                    self.stalled_timestamps += 1;
                } else if timestamp < last {
                    self.backward_timestamps += 1;
                }
            }
            self.last_timestamp = Some(timestamp);
        } else {
            self.repeats += 1;
        }

        if let Some(last) = self.last_render_end {
            let interval = render_end.saturating_duration_since(last);
            self.intervals += 1;
            self.interval_sum += interval;
            self.interval_min = self.interval_min.min(interval);
            self.interval_max = self.interval_max.max(interval);
            if interval.as_secs_f32() > 1.5 * frame_interval.as_secs_f32() {
                self.intervals_over += 1;
            }
        }
        self.last_render_end = Some(render_end);

        let render = render_end.saturating_duration_since(render_start);
        self.render_sum += render;
        self.render_max = self.render_max.max(render);
        self.poll_sum += poll;
        self.poll_max = self.poll_max.max(poll);
        self.draw_sum += draw;
        self.draw_max = self.draw_max.max(draw);

        if render_end.saturating_duration_since(self.started) >= Duration::from_secs(1) {
            self.emit(render_end);
        }
    }

    fn emit(&mut self, now: Instant) {
        use alvr_common::diag::{
            DECODE, DECODER_QUEUE, DROP_SATURATION, DROP_WAIT_IDR, IDR_REQUESTS, PACKET_LOSS,
            PIPELINE, RENDERING, RX_GAP, RX_GAP_OVER, STAGING_FAILURES, STAGING_IMPORTS,
            VIDEO_PACKETS, VSYNC_QUEUE,
        };

        let ms = |d: Duration| d.as_secs_f32() * 1000.0;
        let avg = |sum: Duration, n: u32| if n > 0 { ms(sum) / n as f32 } else { 0.0 };
        let window = now.saturating_duration_since(self.started).as_secs_f32();

        let rx_gap = RX_GAP.take();
        let decode = DECODE.take();
        let queue = DECODER_QUEUE.take();
        let rendering = RENDERING.take();
        let pipeline = PIPELINE.take();
        let vsync = VSYNC_QUEUE.take();

        diag!(
            "[diag] {window:.1}s | frames {} new {} repeat {} ts_stalled {} ts_backward {} | submit interval ms min/avg/max \
             {:.1}/{:.1}/{:.1} over1.5x {} | render() ms avg/max {:.1}/{:.1} decoder-wait {:.1}/{:.1} draw {:.1}/{:.1} | \
             rx pkts {} gap ms avg/max {rx_gap} over1.5x {} loss {} drop_saturation {} drop_wait_idr {} idr_req {} | \
             decode ms avg/max {decode} decoder-queue {queue} compositor {rendering} pipeline {pipeline} vsync-queue {vsync} | \
             staging ok {} fail {}",
            self.frames,
            self.new_buffers,
            self.repeats,
            self.stalled_timestamps,
            self.backward_timestamps,
            if self.intervals > 0 { ms(self.interval_min) } else { 0.0 },
            avg(self.interval_sum, self.intervals),
            ms(self.interval_max),
            self.intervals_over,
            avg(self.render_sum, self.frames),
            ms(self.render_max),
            avg(self.poll_sum, self.frames),
            ms(self.poll_max),
            avg(self.draw_sum, self.frames),
            ms(self.draw_max),
            VIDEO_PACKETS.take(),
            RX_GAP_OVER.take(),
            PACKET_LOSS.take(),
            DROP_SATURATION.take(),
            DROP_WAIT_IDR.take(),
            IDR_REQUESTS.take(),
            STAGING_IMPORTS.take(),
            STAGING_FAILURES.take(),
        );

        self.reset(now);
    }
}

pub struct StreamContext {
    core_context: Arc<ClientCoreContext>,
    xr_session: xr::Session<xr::Vulkan>,
    interaction_context: Arc<RwLock<InteractionContext>>,
    stage_reference_space: Arc<xr::Space>,
    view_reference_space: Arc<xr::Space>,
    // The user's floor height offset (welcome panel), read when the stream starts and kept for its
    // whole duration: the stage space is lowered by it, see `interaction::get_offset_reference_space`
    height_offset_m: f32,
    swapchains: [xr::Swapchain<xr::Vulkan>; 2],
    last_good_view_params: [ViewParams; 2],
    input_thread: Option<JoinHandle<()>>,
    input_thread_running: Arc<RelaxedAtomic>,
    config: ParsedStreamConfig,
    target_view_resolution: UVec2,
    renderer: StreamRenderer,
    decoder: Option<(VideoDecoderConfig, VideoDecoderSource)>,
    use_custom_reprojection: bool,
    diag: DiagWindow,
}

impl StreamContext {
    pub fn new(
        core_ctx: Arc<ClientCoreContext>,
        xr_session: xr::Session<xr::Vulkan>,
        gfx_ctx: Rc<GraphicsContext>,
        interaction_ctx: Arc<RwLock<InteractionContext>>,
        platform: Platform,
        config: ParsedStreamConfig,
    ) -> StreamContext {
        interaction_ctx
            .write()
            .select_sources(&config.interaction_sources);

        let xr_exts = xr_session.instance().exts();

        if xr_exts.fb_display_refresh_rate.is_some() {
            xr_session
                .request_display_refresh_rate(config.refresh_rate_hint)
                .unwrap();
        }

        let foveation_profile = if let Some(config) = &config.clientside_foveation_config {
            if xr_exts.fb_swapchain_update_state.is_some()
                && xr_exts.fb_foveation.is_some()
                && xr_exts.fb_foveation_configuration.is_some()
            {
                let level;
                let dynamic;
                match config.mode {
                    ClientsideFoveationMode::Static { level: lvl } => {
                        level = lvl;
                        dynamic = false;
                    }
                    ClientsideFoveationMode::Dynamic { max_level } => {
                        level = max_level;
                        dynamic = true;
                    }
                };

                xr_session
                    .create_foveation_profile(Some(xr::FoveationLevelProfile {
                        level: xr::FoveationLevelFB::from_raw(level as i32),
                        vertical_offset: config.vertical_offset_deg,
                        dynamic: xr::FoveationDynamicFB::from_raw(dynamic as i32),
                    }))
                    .ok()
            } else {
                None
            }
        } else {
            None
        };

        let target_view_resolution =
            compute_target_view_resolution(config.view_resolution, &config.upscaling);
        let format = graphics::swapchain_format(&gfx_ctx, &xr_session, config.enable_hdr);

        diag!("[vulkan] {}", gfx_ctx.device_summary());
        diag!(
            "[swapchain] chosen format {format} (VK_FORMAT, 43 = R8G8B8A8_SRGB, 37 = R8G8B8A8_UNORM, 97 = R16G16B16A16_SFLOAT), \
             size {}x{} per eye, usage COLOR_ATTACHMENT|SAMPLED, hdr {}, runtime offered {:?}; \
             clientside foveation profile {}",
            target_view_resolution.x,
            target_view_resolution.y,
            config.enable_hdr,
            xr_session.enumerate_swapchain_formats().ok(),
            if foveation_profile.is_some() { "created (not applied to the swapchain: unused in the Vulkan binding, as in GLES)" } else { "none" }
        );

        let swapchains = [
            graphics::create_swapchain(
                &xr_session,
                &gfx_ctx,
                target_view_resolution,
                format,
                foveation_profile.as_ref(),
            ),
            graphics::create_swapchain(
                &xr_session,
                &gfx_ctx,
                target_view_resolution,
                format,
                foveation_profile.as_ref(),
            ),
        ];

        let renderer = StreamRenderer::new(
            gfx_ctx,
            config.view_resolution,
            target_view_resolution,
            [
                swapchains[0]
                    .enumerate_images()
                    .unwrap()
                    .iter()
                    .map(|i| *i as _)
                    .collect(),
                swapchains[1]
                    .enumerate_images()
                    .unwrap()
                    .iter()
                    .map(|i| *i as _)
                    .collect(),
            ],
            format,
            config.foveated_encoding_config.clone(),
            platform != Platform::Lynx && !((platform.is_pico()) && config.enable_hdr),
            config.use_full_range && !config.enable_hdr, // the stream declares BT.709 full range (SDR only); HDR keeps the driver's values like GLES
            config.encoding_gamma,
            config.upscaling.clone(),
        );

        core_ctx.send_active_interaction_profile(
            *HAND_LEFT_ID,
            interaction_ctx.read().hands_interaction[0].controllers_profile_id,
        );
        core_ctx.send_active_interaction_profile(
            *HAND_RIGHT_ID,
            interaction_ctx.read().hands_interaction[1].controllers_profile_id,
        );

        let input_thread_running = Arc::new(RelaxedAtomic::new(false));

        let height_offset_m = alvr_client_core::height_offset_cm() as f32 / 100.0;
        info!("Floor height offset: {height_offset_m} m");

        let stage_reference_space = Arc::new(interaction::get_offset_reference_space(
            &xr_session,
            xr::ReferenceSpaceType::STAGE,
            height_offset_m,
        ));
        let view_reference_space = Arc::new(interaction::get_reference_space(
            &xr_session,
            xr::ReferenceSpaceType::VIEW,
        ));

        let mut this = StreamContext {
            core_context: core_ctx,
            xr_session,
            interaction_context: interaction_ctx,
            stage_reference_space,
            view_reference_space,
            height_offset_m,
            swapchains,
            last_good_view_params: [ViewParams::default(); 2],
            input_thread: None,
            input_thread_running,
            config,
            target_view_resolution,
            renderer,
            decoder: None,
            use_custom_reprojection: platform.is_yvr(),
            diag: DiagWindow::new(),
        };

        this.update_reference_space();

        this
    }

    pub fn uses_passthrough(&self) -> bool {
        self.config.passthrough.is_some()
    }

    pub fn update_reference_space(&mut self) {
        self.input_thread_running.set(false);

        self.stage_reference_space = Arc::new(interaction::get_offset_reference_space(
            &self.xr_session,
            xr::ReferenceSpaceType::STAGE,
            self.height_offset_m,
        ));
        self.view_reference_space = Arc::new(interaction::get_reference_space(
            &self.xr_session,
            xr::ReferenceSpaceType::VIEW,
        ));

        self.core_context.send_playspace(
            self.xr_session
                .reference_space_bounds_rect(xr::ReferenceSpaceType::STAGE)
                .unwrap()
                .map(|a| Vec2::new(a.width, a.height)),
        );

        if let Some(running) = self.input_thread.take() {
            running.join().ok();
        }

        self.input_thread_running.set(true);

        self.input_thread = Some(thread::spawn({
            let core_ctx = Arc::clone(&self.core_context);
            let xr_session = self.xr_session.clone();
            let interaction_ctx = Arc::clone(&self.interaction_context);
            let stage_reference_space = Arc::clone(&self.stage_reference_space);
            let view_reference_space = Arc::clone(&self.view_reference_space);
            let refresh_rate = self.config.refresh_rate_hint;
            let running = Arc::clone(&self.input_thread_running);
            move || {
                stream_input_loop(
                    &core_ctx,
                    xr_session,
                    &interaction_ctx,
                    &stage_reference_space,
                    &view_reference_space,
                    refresh_rate,
                    running,
                )
            }
        }));
    }

    pub fn maybe_initialize_decoder(&mut self, codec: CodecType, config_nal: Vec<u8>) {
        diag!(
            "[decoder] config NAL {} bytes, codec {codec:?}; first bytes {:02x?}",
            config_nal.len(),
            &config_nal[..config_nal.len().min(48)]
        );

        let new_config = VideoDecoderConfig {
            codec,
            force_software_decoder: self.config.force_software_decoder,
            max_buffering_frames: self.config.max_buffering_frames,
            buffering_history_weight: self.config.buffering_history_weight,
            options: self.config.decoder_options.clone(),
            config_buffer: config_nal,
        };

        let maybe_config = if let Some((config, _)) = &self.decoder {
            (new_config != *config).then_some(new_config)
        } else {
            Some(new_config)
        };

        if let Some(config) = maybe_config {
            let (mut sink, source) = video_decoder::create_decoder(config.clone(), {
                let ctx = Arc::clone(&self.core_context);
                move |maybe_timestamp: Result<Duration>| match maybe_timestamp {
                    Ok(timestamp) => ctx.report_frame_decoded(timestamp),
                    Err(e) => ctx.report_fatal_decoder_error(&e.to_string()),
                }
            });
            self.decoder = Some((config, source));

            self.core_context.set_decoder_input_callback(Box::new(
                move |timestamp, buffer| -> bool { sink.push_nal(timestamp, buffer) },
            ));
        }
    }

    pub fn update_real_time_config(&mut self, config: &RealTimeConfig) {
        self.config.passthrough = config.passthrough.clone();
        self.config.clientside_post_processing = config.clientside_post_processing.clone();
    }

    pub fn render(
        &mut self,
        frame_interval: Duration,
        vsync_time: Duration,
    ) -> (ProjectionLayerBuilder, Duration) {
        let render_start = Instant::now();
        let xr_vsync_time = xr::Time::from_nanos(vsync_time.as_nanos() as _);
        let frame_poll_deadline = Instant::now()
            + Duration::from_secs_f32(
                frame_interval.as_secs_f32() * DECODER_MAX_TIMEOUT_MULTIPLIER,
            );
        let mut frame_result = None;
        let poll_start = Instant::now();
        if let Some((_, source)) = &mut self.decoder {
            while frame_result.is_none() && Instant::now() < frame_poll_deadline {
                frame_result = source.get_frame();
                thread::sleep(Duration::from_micros(500));
            }
        }
        let poll_time = poll_start.elapsed();
        let new_buffer_timestamp = frame_result.map(|(timestamp, _)| timestamp);

        let (timestamp, view_params, buffer_ptr) =
            if let Some((timestamp, buffer_ptr)) = frame_result {
                let view_params = self.core_context.report_compositor_start(timestamp);

                self.last_good_view_params = view_params;

                (timestamp, view_params, buffer_ptr)
            } else {
                (vsync_time, self.last_good_view_params, ptr::null_mut())
            };

        let left_swapchain_idx = self.swapchains[0].acquire_image().unwrap();
        let right_swapchain_idx = self.swapchains[1].acquire_image().unwrap();

        self.swapchains[0]
            .wait_image(xr::Duration::INFINITE)
            .unwrap();
        self.swapchains[1]
            .wait_image(xr::Duration::INFINITE)
            .unwrap();

        let (flags, maybe_views) = self
            .xr_session
            .locate_views(
                xr::ViewConfigurationType::PRIMARY_STEREO,
                xr_vsync_time,
                &self.stage_reference_space,
            )
            .unwrap();

        let current_headset_views = if flags.contains(xr::ViewStateFlags::ORIENTATION_VALID) {
            maybe_views
        } else {
            vec![crate::default_view(), crate::default_view()]
        };

        // The poses and FoVs we received from the PC runtime, which may differ and/or include
        // altered FoVs based on settings and view conversions done for canting.
        let input_view_params = view_params;
        let mut output_view_params = input_view_params;
        // Avoid passing invalid timestamp to runtime.
        // `timestamp` is generally a current vsync time, but may be repeated if frames are
        // dropped. Some runtimes dislike it if the timestamp is repeated for too long, so after
        // one second we begin presenting a lagged vsync time instead.
        let mut openxr_display_time =
            Duration::max(timestamp, vsync_time.saturating_sub(Duration::from_secs(1)));

        // (shinyquagsire23) I don't entirely trust runtimes to implement CompositionLayerProjectionView
        // correctly, but if we do trust them, avoid doing rotation ourselves. Otherwise, rerender.
        // Ex: YVR/PFDMR has issues with aspect ratio mismatches and passthrough compositing.
        if self.use_custom_reprojection {
            output_view_params = [
                ViewParams {
                    pose: crate::from_xr_pose(current_headset_views[0].pose),
                    fov: crate::from_xr_fov(current_headset_views[0].fov),
                },
                ViewParams {
                    pose: crate::from_xr_pose(current_headset_views[1].pose),
                    fov: crate::from_xr_fov(current_headset_views[1].fov),
                },
            ];

            openxr_display_time = vsync_time;
        }

        let draw_start = Instant::now();
        unsafe {
            self.renderer.render(
                buffer_ptr,
                [
                    StreamViewParams {
                        swapchain_index: left_swapchain_idx,
                        input_view_params: input_view_params[0],
                        output_view_params: output_view_params[0],
                    },
                    StreamViewParams {
                        swapchain_index: right_swapchain_idx,
                        input_view_params: input_view_params[1],
                        output_view_params: output_view_params[1],
                    },
                ],
                self.config.passthrough.as_ref(),
            )
        };
        let draw_time = draw_start.elapsed();

        self.swapchains[0].release_image().unwrap();
        self.swapchains[1].release_image().unwrap();

        if !buffer_ptr.is_null() {
            if let Some(xr_now) = crate::xr_runtime_now(self.xr_session.instance()) {
                self.core_context.report_submit(
                    timestamp,
                    vsync_time.saturating_sub(Duration::from_nanos(xr_now.as_nanos() as u64)),
                );
            }
        }

        self.diag.frame(
            frame_interval,
            new_buffer_timestamp,
            poll_time,
            draw_time,
            render_start,
            Instant::now(),
        );

        let rect = xr::Rect2Di {
            offset: xr::Offset2Di { x: 0, y: 0 },
            extent: xr::Extent2Di {
                width: self.target_view_resolution.x as _,
                height: self.target_view_resolution.y as _,
            },
        };

        let clientside_post_processing = self
            .xr_session
            .instance()
            .exts()
            .fb_composition_layer_settings
            .and(self.config.clientside_post_processing.clone());

        let layer = ProjectionLayerBuilder::new(
            &self.stage_reference_space,
            [
                xr::CompositionLayerProjectionView::new()
                    .pose(crate::to_xr_pose(output_view_params[0].pose))
                    .fov(crate::to_xr_fov(output_view_params[0].fov))
                    .sub_image(
                        xr::SwapchainSubImage::new()
                            .swapchain(&self.swapchains[0])
                            .image_array_index(0)
                            .image_rect(rect),
                    ),
                xr::CompositionLayerProjectionView::new()
                    .pose(crate::to_xr_pose(output_view_params[1].pose))
                    .fov(crate::to_xr_fov(output_view_params[1].fov))
                    .sub_image(
                        xr::SwapchainSubImage::new()
                            .swapchain(&self.swapchains[1])
                            .image_array_index(0)
                            .image_rect(rect),
                    ),
            ],
            self.config
                .passthrough
                .clone()
                .map(|mode| ProjectionLayerAlphaConfig {
                    premultiplied: matches!(
                        mode,
                        PassthroughMode::Blend {
                            premultiplied_alpha: true,
                            ..
                        } | PassthroughMode::RgbChromaKey(_)
                            | PassthroughMode::HsvChromaKey(_)
                    ),
                }),
            clientside_post_processing,
        );

        (layer, openxr_display_time)
    }
}

impl Drop for StreamContext {
    fn drop(&mut self) {
        self.input_thread_running.set(false);
        self.input_thread.take().unwrap().join().ok();
    }
}

fn stream_input_loop(
    core_ctx: &ClientCoreContext,
    xr_session: xr::Session<xr::Vulkan>,
    interaction_ctx: &RwLock<InteractionContext>,
    stage_reference_space: &xr::Space,
    view_reference_space: &xr::Space,
    refresh_rate: f32,
    running: Arc<RelaxedAtomic>,
) {
    let platform = alvr_system_info::platform();

    let mut last_controller_poses = [Pose::default(); 2];
    let mut last_palm_poses = [Pose::default(); 2];
    let mut last_view_params = [ViewParams::default(); 2];

    let mut deadline = Instant::now();
    let frame_interval = Duration::from_secs_f32(1.0 / refresh_rate);
    while running.value() {
        let int_ctx = &*interaction_ctx.read();
        // Streaming related inputs are updated here. Make sure every input poll is done in this
        // thread
        if let Err(e) = xr_session.sync_actions(&[(&int_ctx.action_set).into()]) {
            error!("{e}");
            return;
        }

        let Some(now) = crate::xr_runtime_now(xr_session.instance()).map(crate::from_xr_time)
        else {
            error!("Cannot poll tracking: invalid time");
            return;
        };

        let target_time = now + core_ctx.get_total_prediction_offset();

        let Some((head_motion, local_views)) = interaction::get_head_data(
            &xr_session,
            platform,
            stage_reference_space,
            view_reference_space,
            now,
            target_time,
            &last_view_params,
        ) else {
            continue;
        };

        if let Some(views) = local_views {
            core_ctx.send_view_params(views);
            last_view_params = views;
        }

        let mut device_motions = Vec::with_capacity(3);

        device_motions.push((*HEAD_ID, head_motion));

        let (left_hand_motion, left_hand_skeleton) = crate::interaction::get_hand_data(
            &xr_session,
            platform,
            stage_reference_space,
            now,
            target_time,
            &int_ctx.hands_interaction[0],
            &mut last_controller_poses[0],
            &mut last_palm_poses[0],
        );
        let (right_hand_motion, right_hand_skeleton) = crate::interaction::get_hand_data(
            &xr_session,
            platform,
            stage_reference_space,
            now,
            target_time,
            &int_ctx.hands_interaction[1],
            &mut last_controller_poses[1],
            &mut last_palm_poses[1],
        );

        // Note: When multimodal input is enabled, we are sure that when free hands are used
        // (not holding controllers) the controller data is None.
        if int_ctx.multimodal_hands_enabled || left_hand_skeleton.is_none() {
            if let Some(motion) = left_hand_motion {
                device_motions.push((*HAND_LEFT_ID, motion));
            }
        }
        if int_ctx.multimodal_hands_enabled || right_hand_skeleton.is_none() {
            if let Some(motion) = right_hand_motion {
                device_motions.push((*HAND_RIGHT_ID, motion));
            }
        }

        let face_data = FaceData {
            eye_gazes: interaction::get_eye_gazes(
                &xr_session,
                &int_ctx.face_sources,
                stage_reference_space,
                now,
            ),
            fb_face_expression: interaction::get_fb_face_expression(&int_ctx.face_sources, now).or(
                interaction::get_pico_face_expression(&int_ctx.face_sources, now),
            ),
            htc_eye_expression: interaction::get_htc_eye_expression(&int_ctx.face_sources, now),
            htc_lip_expression: interaction::get_htc_lip_expression(&int_ctx.face_sources, now),
        };

        if let Some((tracker, joint_count)) = &int_ctx.body_sources.body_tracker_fb {
            device_motions.append(&mut interaction::get_fb_body_tracking_points(
                stage_reference_space,
                now,
                tracker,
                *joint_count,
            ));
        }

        if let Some(tracker) = &int_ctx.body_sources.body_tracker_bd {
            device_motions.append(&mut interaction::get_bd_body_tracking_points(
                stage_reference_space,
                now,
                tracker,
            ));
        }

        if let Some(tracker) = &int_ctx.body_sources.motion_tracker_bd {
            device_motions.append(&mut interaction::get_bd_motion_trackers(now, tracker));
        }

        core_ctx.send_tracking(
            Duration::from_nanos(now.as_nanos() as u64),
            device_motions,
            [left_hand_skeleton, right_hand_skeleton],
            face_data,
        );

        let button_entries = interaction::update_buttons(&xr_session, &int_ctx.button_actions);
        if !button_entries.is_empty() {
            core_ctx.send_buttons(button_entries);
        }

        deadline += frame_interval / 3;
        thread::sleep(deadline.saturating_duration_since(Instant::now()));
    }
}
