use crate::{
    graphics::{self, ProjectionLayerAlphaConfig, ProjectionLayerBuilder},
    interaction::{self, InteractionContext},
};
use alvr_client_core::HEIGHT_OFFSET_RANGE_CM;
use alvr_common::{
    glam::{UVec2, Vec3},
    parking_lot::RwLock,
    Pose, LEFT_TRIGGER_VALUE_ID, RIGHT_TRIGGER_VALUE_ID,
};
use alvr_graphics::{
    panel_hit, BodyTrackingType, GraphicsContext, LobbyPointer, LobbyRenderer, LobbyViewParams,
    PanelButton, PanelState, SDR_FORMAT_VK,
};
use alvr_system_info::Platform;
use openxr as xr;
use std::{
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

// Hand tracking has no trigger: a pinch (thumb tip to index tip) is the click.
const HAND_JOINT_THUMB_TIP: usize = 5;
const HAND_JOINT_INDEX_TIP: usize = 10;
const PINCH_DOWN_DISTANCE: f32 = 0.02;
const PINCH_UP_DISTANCE: f32 = 0.035;
// The pinch ray starts at an estimated shoulder and goes through the pinch point
const SHOULDER_DROP: f32 = 0.20;
const SHOULDER_SIDE: f32 = 0.17;

const TRIGGER_DOWN: f32 = 0.7;
const TRIGGER_UP: f32 = 0.4;

// Holding minus or plus repeats it
const REPEAT_DELAY: Duration = Duration::from_millis(450);
const REPEAT_INTERVAL: Duration = Duration::from_millis(70);
// The headset height readout is redrawn at most this often
const HEIGHT_READOUT_INTERVAL: Duration = Duration::from_millis(250);

struct Pointer {
    origin: Vec3,
    hit_point: Vec3,
    button: Option<PanelButton>,
    pressed: bool,
}

// todo: add interaction?
pub struct Lobby {
    xr_session: xr::Session<xr::Vulkan>,
    interaction_ctx: Arc<RwLock<InteractionContext>>,
    platform: Platform,
    reference_space: xr::Space,
    swapchains: [xr::Swapchain<xr::Vulkan>; 2],
    view_resolution: UVec2,
    reference_space_type: xr::ReferenceSpaceType,
    renderer: LobbyRenderer,

    // Welcome panel
    panel_state: PanelState,
    drawn_panel_state: PanelState,
    offset_unsaved: bool,
    active_button: Option<PanelButton>,
    press_start: Instant,
    last_repeat: Instant,
    last_height_readout: Option<Instant>,
    trigger_down: [bool; 2],
    pinch_down: [bool; 2],
}

impl Lobby {
    pub fn new(
        xr_session: xr::Session<xr::Vulkan>,
        gfx_ctx: Rc<GraphicsContext>,
        interaction_ctx: Arc<RwLock<InteractionContext>>,
        platform: Platform,
        view_resolution: UVec2,
        initial_hud_message: &str,
    ) -> Self {
        let reference_space_type = if xr_session.instance().exts().ext_local_floor.is_some() {
            xr::ReferenceSpaceType::LOCAL_FLOOR_EXT
        } else {
            // The Quest 1 doesn't support LOCAL_FLOOR_EXT, recentering is required for AppLab, but
            // the Quest 1 is excluded from AppLab anyway.
            xr::ReferenceSpaceType::STAGE
        };

        let reference_space = interaction::get_reference_space(&xr_session, reference_space_type);

        let swapchains = [
            graphics::create_swapchain(&xr_session, &gfx_ctx, view_resolution, SDR_FORMAT_VK, None),
            graphics::create_swapchain(&xr_session, &gfx_ctx, view_resolution, SDR_FORMAT_VK, None),
        ];

        let mut panel_state = PanelState {
            offset_cm: alvr_client_core::height_offset_cm(),
            ..Default::default()
        };
        panel_state.set_hud_message(initial_hud_message);

        let renderer = LobbyRenderer::new(
            gfx_ctx,
            view_resolution,
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
            &panel_state,
        );

        Self {
            xr_session,
            interaction_ctx,
            platform,
            reference_space,
            swapchains,
            view_resolution,
            reference_space_type,
            renderer,
            drawn_panel_state: panel_state.clone(),
            panel_state,
            offset_unsaved: false,
            active_button: None,
            press_start: Instant::now(),
            last_repeat: Instant::now(),
            last_height_readout: None,
            trigger_down: [false; 2],
            pinch_down: [false; 2],
        }
    }

    pub fn update_reference_space(&mut self) {
        self.reference_space =
            interaction::get_reference_space(&self.xr_session, self.reference_space_type);
    }

    pub fn update_hud_message(&mut self, message: &str) {
        self.panel_state.set_hud_message(message);
    }

    // Whether the controller trigger (with a little hysteresis) is held
    fn trigger_pressed(&self, hand_idx: usize) -> bool {
        let id = if hand_idx == 0 {
            *LEFT_TRIGGER_VALUE_ID
        } else {
            *RIGHT_TRIGGER_VALUE_ID
        };

        let value = match self.interaction_ctx.read().button_actions.get(&id) {
            Some(interaction::ButtonAction::Scalar(action)) => action
                .state(&self.xr_session, xr::Path::NULL)
                .map(|state| state.current_state)
                .unwrap_or(0.0),
            _ => 0.0,
        };

        if self.trigger_down[hand_idx] {
            value > TRIGGER_UP
        } else {
            value > TRIGGER_DOWN
        }
    }

    // Moves the height offset by one step or resets it; true if something changed
    fn apply_button(&mut self, button: PanelButton) {
        let new_offset = match button {
            PanelButton::Minus => self.panel_state.offset_cm - 1,
            PanelButton::Plus => self.panel_state.offset_cm + 1,
            PanelButton::Reset => 0,
        }
        .clamp(
            *HEIGHT_OFFSET_RANGE_CM.start(),
            *HEIGHT_OFFSET_RANGE_CM.end(),
        );

        if new_offset != self.panel_state.offset_cm {
            self.panel_state.offset_cm = new_offset;
            self.offset_unsaved = true;
        }
    }

    // Pointing rays of the controllers (aim pose, trigger) and of the tracked hands (pinch), each
    // with the panel button it lands on. `head` is the eye position, used to place the shoulders.
    fn pointers(
        &mut self,
        vsync_time: Duration,
        head: Pose,
        hand_data: &[(Option<alvr_common::DeviceMotion>, Option<[Pose; 26]>); 2],
    ) -> Vec<Pointer> {
        let mut pointers = vec![];

        for hand_idx in 0..2 {
            // Controller
            let aim_pose = interaction::get_aim_pose(
                &self.xr_session,
                &self.reference_space,
                vsync_time,
                &self.interaction_ctx.read().hands_interaction[hand_idx],
            );
            let trigger = self.trigger_pressed(hand_idx);
            self.trigger_down[hand_idx] = trigger;
            // A controller that is not held (multimodal) still has a pose but the hand is the pointer
            if let (Some(pose), true) = (aim_pose, hand_data[hand_idx].1.is_none()) {
                let dir = pose.orientation * -Vec3::Z;
                if let Some(hit) = panel_hit(pose.position, dir) {
                    pointers.push(Pointer {
                        origin: pose.position,
                        hit_point: hit.point,
                        button: hit.button,
                        pressed: trigger,
                    });
                }
            }

            // Hand
            if let Some(joints) = &hand_data[hand_idx].1 {
                let thumb = joints[HAND_JOINT_THUMB_TIP].position;
                let index = joints[HAND_JOINT_INDEX_TIP].position;
                let distance = thumb.distance(index);
                let pinching = if self.pinch_down[hand_idx] {
                    distance < PINCH_UP_DISTANCE
                } else {
                    distance < PINCH_DOWN_DISTANCE
                };
                self.pinch_down[hand_idx] = pinching;

                let pinch_point = (thumb + index) / 2.0;
                let mut right = head.orientation * Vec3::X;
                right.y = 0.0;
                let side = if hand_idx == 0 {
                    -SHOULDER_SIDE
                } else {
                    SHOULDER_SIDE
                };
                let shoulder =
                    head.position + Vec3::new(0.0, -SHOULDER_DROP, 0.0) + right.normalize_or_zero() * side;

                if let Some(hit) = panel_hit(pinch_point, pinch_point - shoulder) {
                    pointers.push(Pointer {
                        origin: pinch_point,
                        hit_point: hit.point,
                        button: hit.button,
                        pressed: pinching,
                    });
                }
            } else {
                self.pinch_down[hand_idx] = false;
            }
        }

        pointers
    }

    pub fn render(&mut self, vsync_time: Duration) -> ProjectionLayerBuilder {
        let xr_vsync_time = xr::Time::from_nanos(vsync_time.as_nanos() as _);

        let (flags, maybe_views) = self
            .xr_session
            .locate_views(
                xr::ViewConfigurationType::PRIMARY_STEREO,
                xr_vsync_time,
                &self.reference_space,
            )
            .unwrap();

        let views = if flags.contains(xr::ViewStateFlags::ORIENTATION_VALID) {
            maybe_views
        } else {
            vec![crate::default_view(), crate::default_view()]
        };

        self.xr_session
            .sync_actions(&[(&self.interaction_ctx.read().action_set).into()])
            .ok();

        // future_time doesn't have to be any particular value, just something after vsync_time
        let future_time = vsync_time + Duration::from_millis(80);
        let left_hand_data = interaction::get_hand_data(
            &self.xr_session,
            self.platform,
            &self.reference_space,
            vsync_time,
            future_time,
            &self.interaction_ctx.read().hands_interaction[0],
            &mut Pose::default(),
            &mut Pose::default(),
        );
        let right_hand_data = interaction::get_hand_data(
            &self.xr_session,
            self.platform,
            &self.reference_space,
            vsync_time,
            future_time,
            &self.interaction_ctx.read().hands_interaction[1],
            &mut Pose::default(),
            &mut Pose::default(),
        );

        let additional_motions: Option<Vec<alvr_common::DeviceMotion>> = None;

        let body_skeleton_fb = self
            .interaction_ctx
            .read()
            .body_sources
            .body_tracker_fb
            .as_ref()
            .and_then(|(tracker, joint_count)| {
                interaction::get_fb_body_skeleton(
                    &self.reference_space,
                    xr_vsync_time,
                    tracker,
                    *joint_count,
                )
            });

        let (body_skeleton, body_tracking_type) = match body_skeleton_fb {
            Some(skeleton) => (Some(skeleton), Some(BodyTrackingType::Meta)),
            None => (None, None),
        };

        // Welcome panel: pointing, clicking, the height offset
        let head = Pose {
            position: (crate::from_xr_pose(views[0].pose).position
                + crate::from_xr_pose(views[1].pose).position)
                / 2.0,
            orientation: crate::from_xr_pose(views[0].pose).orientation,
        };
        let pointers = self.pointers(vsync_time, head, &[left_hand_data.clone(), right_hand_data.clone()]);

        // The pointer that presses a button wins, then the first one on the panel
        let chosen = pointers
            .iter()
            .find(|p| p.pressed && p.button.is_some())
            .or_else(|| pointers.iter().find(|p| p.button.is_some()))
            .or(pointers.first());

        let pressed_button = chosen.and_then(|p| p.pressed.then_some(p.button).flatten());
        let now = Instant::now();
        match pressed_button {
            Some(button) if self.active_button != Some(button) => {
                self.active_button = Some(button);
                self.press_start = now;
                self.last_repeat = now;
                self.apply_button(button);
            }
            Some(button) => {
                if button != PanelButton::Reset
                    && now.duration_since(self.press_start) > REPEAT_DELAY
                    && now.duration_since(self.last_repeat) > REPEAT_INTERVAL
                {
                    self.last_repeat = now;
                    self.apply_button(button);
                }
            }
            None => self.active_button = None,
        }

        // Saved when the button is let go, so the next game start reads it
        if self.offset_unsaved && self.active_button.is_none() {
            alvr_client_core::set_height_offset_cm(self.panel_state.offset_cm);
            self.offset_unsaved = false;
        }

        if flags.contains(xr::ViewStateFlags::POSITION_VALID | xr::ViewStateFlags::ORIENTATION_VALID)
            && self
                .last_height_readout
                .is_none_or(|t| now.duration_since(t) > HEIGHT_READOUT_INTERVAL)
        {
            self.last_height_readout = Some(now);
            self.panel_state.headset_height_cm = Some((head.position.y * 100.0).round() as i32);
        }
        self.panel_state.hovered = chosen.and_then(|p| p.button);
        self.panel_state.pressed = pressed_button;

        if self.panel_state != self.drawn_panel_state {
            self.renderer.update_panel(&self.panel_state);
            self.drawn_panel_state = self.panel_state.clone();
        }

        let lobby_pointers = pointers
            .iter()
            .map(|p| LobbyPointer {
                origin: p.origin,
                end: p.hit_point,
                pressed: p.pressed,
            })
            .collect::<Vec<_>>();

        let left_swapchain_idx = self.swapchains[0].acquire_image().unwrap();
        let right_swapchain_idx = self.swapchains[1].acquire_image().unwrap();

        self.swapchains[0]
            .wait_image(xr::Duration::INFINITE)
            .unwrap();
        self.swapchains[1]
            .wait_image(xr::Duration::INFINITE)
            .unwrap();

        self.renderer.render(
            [
                LobbyViewParams {
                    pose: crate::from_xr_pose(views[0].pose),
                    fov: crate::from_xr_fov(views[0].fov),
                    swapchain_index: left_swapchain_idx,
                },
                LobbyViewParams {
                    pose: crate::from_xr_pose(views[1].pose),
                    fov: crate::from_xr_fov(views[1].fov),
                    swapchain_index: right_swapchain_idx,
                },
            ],
            [left_hand_data, right_hand_data],
            additional_motions,
            body_skeleton,
            body_tracking_type,
            &lobby_pointers,
            false,
            cfg!(debug_assertions),
        );

        self.swapchains[0].release_image().unwrap();
        self.swapchains[1].release_image().unwrap();

        let rect = xr::Rect2Di {
            offset: xr::Offset2Di { x: 0, y: 0 },
            extent: xr::Extent2Di {
                width: self.view_resolution.x as _,
                height: self.view_resolution.y as _,
            },
        };

        ProjectionLayerBuilder::new(
            &self.reference_space,
            [
                xr::CompositionLayerProjectionView::new()
                    .pose(views[0].pose)
                    .fov(views[0].fov)
                    .sub_image(
                        xr::SwapchainSubImage::new()
                            .swapchain(&self.swapchains[0])
                            .image_array_index(0)
                            .image_rect(rect),
                    ),
                xr::CompositionLayerProjectionView::new()
                    .pose(views[1].pose)
                    .fov(views[1].fov)
                    .sub_image(
                        xr::SwapchainSubImage::new()
                            .swapchain(&self.swapchains[1])
                            .image_array_index(0)
                            .image_rect(rect),
                    ),
            ],
            Some(ProjectionLayerAlphaConfig {
                premultiplied: true,
            }),
            None,
        )
    }
}
