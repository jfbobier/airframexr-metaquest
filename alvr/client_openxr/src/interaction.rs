use crate::{
    extra_extensions::{
        BodyTrackerFB, EyeTrackerSocial, FaceTracker2FB, MultimodalMeta,
        BODY_JOINT_SET_FULL_BODY_META, FULL_BODY_JOINT_COUNT_META,
        FULL_BODY_JOINT_LEFT_FOOT_BALL_META, FULL_BODY_JOINT_LEFT_LOWER_LEG_META,
        FULL_BODY_JOINT_RIGHT_FOOT_BALL_META, FULL_BODY_JOINT_RIGHT_LOWER_LEG_META,
    },
    Platform,
};
use alvr_common::{
    glam::{Quat, Vec3},
    *,
};
use alvr_packets::{ButtonEntry, ButtonValue, StreamConfig};
use alvr_session::{BodyTrackingSourcesConfig, FaceTrackingSourcesConfig};
use openxr as xr;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use xr::SpaceLocationFlags;

const IPD_CHANGE_EPS: f32 = 0.001;

// Most OpenXR runtime, including Meta's one, do not follow perfectly the specification regarding
// controller pose. The Z axis should point down through the center of the controller grip, the X
// axis should go out perpendicular from the palm, and the position should be aligned roughtly with
// the center of the palm.
// https://registry.khronos.org/OpenXR/specs/1.1/html/xrspec.html
// Note: right controller offsets are calculated from left controller offsets by mirroring along the
// Y-Z plane.
fn get_controller_offset(platform: Platform, is_right_hand: bool) -> Pose {
    const DEG_TO_RAD: f32 = std::f32::consts::PI / 180.0;

    let left_offset = match platform {
        // todo: check Quest 2
        p if p.is_quest() => Pose {
            position: Vec3::new(-0.005, -0.005, 0.00),
            orientation: Quat::from_rotation_x(-15.0 * DEG_TO_RAD),
        },
        _ => Pose::default(),
    };

    if is_right_hand {
        let p = left_offset.position;
        let q = left_offset.orientation;

        Pose {
            position: Vec3::new(-p[0], p[1], p[2]),
            orientation: Quat::from_xyzw(-q.x, q.y, q.z, -q.w),
        }
    } else {
        left_offset
    }
}

fn create_ext_object<T>(
    name: &str,
    enabled: Option<bool>,
    create_cb: impl FnOnce() -> xr::Result<T>,
) -> Option<T> {
    enabled
        .unwrap_or(false)
        .then(|| match create_cb() {
            Ok(obj) => Some(obj),
            Err(xr::sys::Result::ERROR_FEATURE_UNSUPPORTED) => {
                warn!("Cannot create unsupported {name}");
                None
            }
            Err(xr::sys::Result::ERROR_EXTENSION_NOT_PRESENT) => None,
            Err(e) => {
                warn!("Failed to create {name}: {e}");
                None
            }
        })
        .flatten()
}

pub enum ButtonAction {
    Binary(xr::Action<bool>),
    Scalar(xr::Action<f32>),
}

pub struct HandInteraction {
    pub controllers_profile_id: u64,
    pub pose_offset: Pose,

    pub grip_action: xr::Action<xr::Posef>,
    pub grip_space: xr::Space,

    pub aim_action: xr::Action<xr::Posef>,
    pub aim_space: xr::Space,

    pub vibration_action: xr::Action<xr::Haptic>,
    pub skeleton_tracker: Option<xr::HandTracker>,
}

pub struct FaceSources {
    pub eye_tracker_fb: Option<EyeTrackerSocial>,
    pub face_tracker_fb: Option<FaceTracker2FB>,
}

pub struct BodySources {
    pub body_tracker_fb: Option<(BodyTrackerFB, usize)>,
}

#[derive(Clone)]
pub struct InteractionSourcesConfig {
    pub face_tracking: Option<FaceTrackingSourcesConfig>,
    pub body_tracking: Option<BodyTrackingSourcesConfig>,
    pub prefers_multimodal_input: bool,
    pub hands_as_controllers: bool,
}

impl InteractionSourcesConfig {
    pub fn new(config: &StreamConfig) -> Self {
        Self {
            face_tracking: config
                .settings
                .headset
                .face_tracking
                .as_option()
                .map(|c| c.sources.clone()),
            body_tracking: config
                .settings
                .headset
                .body_tracking
                .as_option()
                .map(|c| c.sources.clone()),
            prefers_multimodal_input: config
                .settings
                .headset
                .controllers
                .as_option()
                .is_some_and(|c| c.multimodal_tracking),
            hands_as_controllers: config
                .settings
                .headset
                .controllers
                .as_option()
                .is_some_and(|c| c.tracked && c.hand_tracking_interaction.as_option().is_none()),
        }
    }
}

pub struct InteractionContext {
    xr_session: xr::Session<xr::Vulkan>,
    platform: Platform,
    pub action_set: xr::ActionSet,
    pub button_actions: HashMap<u64, ButtonAction>,
    pub hands_interaction: [HandInteraction; 2],
    multimodal_handle: Option<MultimodalMeta>,
    pub multimodal_hands_enabled: bool,
    hand_interaction_bound: bool,
    // free hands drive controllers (pose, pinch = trigger, double pinch = A / X) instead of only sending the skeleton
    pub hands_as_controllers: bool,
    pub face_sources: FaceSources,
    pub body_sources: BodySources,
}

impl InteractionContext {
    pub fn new(
        xr_session: xr::Session<xr::Vulkan>,
        extra_extensions: Vec<String>,
        xr_system: xr::SystemId,
        platform: Platform,
    ) -> Self {
        let xr_instance = xr_session.instance();

        let action_set = xr_instance
            .create_action_set("alvr_interaction", "ALVR interaction", 0)
            .unwrap();

        let mut bindings = vec![];

        fn binding<'a, T: xr::ActionTy>(action: &'a xr::Action<T>, path: &str) -> xr::Binding<'a> {
            xr::Binding::new(action, action.instance().string_to_path(path).unwrap())
        }

        let controllers_profile_path = QUEST_CONTROLLER_PROFILE_PATH; // todo: create new controller profile for quest pro and 3
        let controllers_profile_id = alvr_common::hash_string(controllers_profile_path);

        // Create actions:

        let mut button_actions = HashMap::new();
        for button_id in &CONTROLLER_PROFILE_INFO
            .get(&controllers_profile_id)
            .unwrap()
            .button_set
        {
            let info = BUTTON_INFO.get(button_id).unwrap();

            let name = info.path[1..].replace('/', "_");
            let display_name = format!(
                "{}{}",
                name[0..1].to_uppercase(),
                name[1..].replace('_', " ")
            );

            let action = match info.button_type {
                ButtonType::Binary => ButtonAction::Binary(
                    action_set.create_action(&name, &display_name, &[]).unwrap(),
                ),
                ButtonType::Scalar => ButtonAction::Scalar(
                    action_set.create_action(&name, &display_name, &[]).unwrap(),
                ),
            };
            button_actions.insert(*button_id, action);
        }

        let left_grip_action = action_set
            .create_action("left_grip_pose", "Left grip pose", &[])
            .unwrap();
        let right_grip_action = action_set
            .create_action("right_grip_pose", "Right grip pose", &[])
            .unwrap();

        let left_aim_action = action_set
            .create_action("left_aim_pose", "Left aim pose", &[])
            .unwrap();
        let right_aim_action = action_set
            .create_action("right_aim_pose", "Right aim pose", &[])
            .unwrap();

        let left_vibration_action = action_set
            .create_action("left_hand_vibration", "Left hand vibration", &[])
            .unwrap();
        let right_vibration_action = action_set
            .create_action("right_hand_vibration", "Right hand vibration", &[])
            .unwrap();

        // Create action bindings:

        for (id, action) in &button_actions {
            let path = &BUTTON_INFO.get(id).unwrap().path;
            match action {
                ButtonAction::Binary(action) => {
                    bindings.push(binding(action, path));
                }
                ButtonAction::Scalar(action) => {
                    bindings.push(binding(action, path));
                }
            }
        }

        bindings.push(binding(
            &left_grip_action,
            "/user/hand/left/input/grip/pose",
        ));
        bindings.push(binding(
            &right_grip_action,
            "/user/hand/right/input/grip/pose",
        ));

        bindings.push(binding(&left_aim_action, "/user/hand/left/input/aim/pose"));
        bindings.push(binding(
            &right_aim_action,
            "/user/hand/right/input/aim/pose",
        ));

        bindings.push(binding(
            &left_vibration_action,
            "/user/hand/left/output/haptic",
        ));
        bindings.push(binding(
            &right_vibration_action,
            "/user/hand/right/output/haptic",
        ));

        let multimodal_handle = create_ext_object("MultimodalMeta", Some(true), || {
            MultimodalMeta::new(xr_session.clone(), &extra_extensions, xr_system)
        });

        let left_detached_controller_pose_action;
        let right_detached_controller_pose_action;
        if multimodal_handle.is_some() {
            // Note: when multimodal input is enabled, both controllers and hands will always be active.
            // To be able to detect when controllers are actually held, we have to register detached
            // controllers pose; the controller pose will be diverted to the detached controllers when
            // they are not held. Currently the detached controllers pose is ignored
            left_detached_controller_pose_action = action_set
                .create_action::<xr::Posef>(
                    "left_detached_controller_pose",
                    "Left detached controller pose",
                    &[],
                )
                .unwrap();
            right_detached_controller_pose_action = action_set
                .create_action::<xr::Posef>(
                    "right_detached_controller_pose",
                    "Right detached controller pose",
                    &[],
                )
                .unwrap();

            bindings.push(binding(
                &left_detached_controller_pose_action,
                "/user/detached_controller_meta/left/input/grip/pose",
            ));
            bindings.push(binding(
                &right_detached_controller_pose_action,
                "/user/detached_controller_meta/right/input/grip/pose",
            ));
        }

        // Apply bindings:
        xr_instance
            .suggest_interaction_profile_bindings(
                xr_instance
                    .string_to_path(controllers_profile_path)
                    .unwrap(),
                &bindings,
            )
            .unwrap();

        // Hands as controllers: XR_EXT_hand_interaction gives a hand-driven user the same grip and aim poses, and
        // a pinch value, through the same actions as the controllers. The runtime picks the active profile:
        // the controllers' while they are held, this one with free hands.
        let hand_interaction_bound = if xr_instance.exts().ext_hand_interaction.is_some() {
            let mut hand_bindings = vec![];
            for (hand, grip, aim, trigger_click_id, trigger_value_id) in [
                (
                    "left",
                    &left_grip_action,
                    &left_aim_action,
                    *LEFT_TRIGGER_CLICK_ID,
                    *LEFT_TRIGGER_VALUE_ID,
                ),
                (
                    "right",
                    &right_grip_action,
                    &right_aim_action,
                    *RIGHT_TRIGGER_CLICK_ID,
                    *RIGHT_TRIGGER_VALUE_ID,
                ),
            ] {
                hand_bindings.push(binding(grip, &format!("/user/hand/{hand}/input/grip/pose")));
                hand_bindings.push(binding(aim, &format!("/user/hand/{hand}/input/aim/pose")));
                let pinch = format!("/user/hand/{hand}/input/pinch_ext/value");
                if let Some(ButtonAction::Binary(action)) = button_actions.get(&trigger_click_id) {
                    hand_bindings.push(binding(action, &pinch));
                }
                if let Some(ButtonAction::Scalar(action)) = button_actions.get(&trigger_value_id) {
                    hand_bindings.push(binding(action, &pinch));
                }
            }

            let res = xr_instance.suggest_interaction_profile_bindings(
                xr_instance
                    .string_to_path("/interaction_profiles/ext/hand_interaction_ext")
                    .unwrap(),
                &hand_bindings,
            );
            if let Err(e) = &res {
                warn!("Failed to register hand interaction input: {e:?}");
            }
            res.is_ok()
        } else {
            false
        };

        xr_session.attach_action_sets(&[&action_set]).unwrap();

        let left_grip_space = left_grip_action
            .create_space(xr_session.clone(), xr::Path::NULL, xr::Posef::IDENTITY)
            .unwrap();
        let right_grip_space = right_grip_action
            .create_space(xr_session.clone(), xr::Path::NULL, xr::Posef::IDENTITY)
            .unwrap();

        let left_aim_space = left_aim_action
            .create_space(xr_session.clone(), xr::Path::NULL, xr::Posef::IDENTITY)
            .unwrap();
        let right_aim_space = right_aim_action
            .create_space(xr_session.clone(), xr::Path::NULL, xr::Posef::IDENTITY)
            .unwrap();

        let left_hand_tracker = create_ext_object("HandTracker (left)", Some(true), || {
            xr_session.create_hand_tracker(xr::Hand::LEFT)
        });
        let right_hand_tracker = create_ext_object("HandTracker (right)", Some(true), || {
            xr_session.create_hand_tracker(xr::Hand::RIGHT)
        });

        Self {
            xr_session,
            platform,
            action_set,
            button_actions,
            hands_interaction: [
                HandInteraction {
                    controllers_profile_id,
                    pose_offset: get_controller_offset(platform, false),
                    grip_action: left_grip_action,
                    grip_space: left_grip_space,
                    aim_action: left_aim_action,
                    aim_space: left_aim_space,
                    vibration_action: left_vibration_action,
                    skeleton_tracker: left_hand_tracker,
                },
                HandInteraction {
                    controllers_profile_id,
                    pose_offset: get_controller_offset(platform, true),
                    grip_action: right_grip_action,
                    grip_space: right_grip_space,
                    aim_action: right_aim_action,
                    aim_space: right_aim_space,
                    vibration_action: right_vibration_action,
                    skeleton_tracker: right_hand_tracker,
                },
            ],
            multimodal_handle,
            multimodal_hands_enabled: false,
            hand_interaction_bound,
            hands_as_controllers: false,
            face_sources: FaceSources {
                eye_tracker_fb: None,
                face_tracker_fb: None,
            },
            body_sources: BodySources {
                body_tracker_fb: None,
            },
        }
    }

    pub fn select_sources(&mut self, config: &InteractionSourcesConfig) {
        // First of all, disable/delete all sources. This ensures there are no conflicts
        if let Some(handle) = &mut self.multimodal_handle {
            handle.pause().ok();
        }
        self.multimodal_hands_enabled = false;
        self.hands_as_controllers = false;
        self.face_sources.eye_tracker_fb = None;
        self.face_sources.face_tracker_fb = None;
        self.body_sources.body_tracker_fb = None;

        if let Some(config) = &config.face_tracking {
            if (config.eye_tracking_fb) && matches!(self.platform, Platform::QuestPro) {
                #[cfg(target_os = "android")]
                alvr_system_info::try_get_permission("com.oculus.permission.EYE_TRACKING")
            }
            if config.face_tracking_fb && matches!(self.platform, Platform::QuestPro) {
                #[cfg(target_os = "android")]
                {
                    alvr_system_info::try_get_permission("android.permission.RECORD_AUDIO");
                    alvr_system_info::try_get_permission("com.oculus.permission.FACE_TRACKING")
                }
            }
        }

        if let Some(config) = &config.body_tracking {
            if (config.body_tracking_fb.enabled())
                && self.platform.is_quest()
            {
                #[cfg(target_os = "android")]
                alvr_system_info::try_get_permission("com.oculus.permission.BODY_TRACKING")
            }
        }

        // Note: We cannot enable multimodal if fb body tracking is active. It would result in a
        // ERROR_RUNTIME_FAILURE crash.
        if config.body_tracking.is_none() && config.prefers_multimodal_input {
            if let Some(handle) = &mut self.multimodal_handle {
                if handle.resume().is_ok() {
                    self.multimodal_hands_enabled = true;
                }
            }
        }

        // The server's own gesture emulation (hand_tracking_interaction) and multimodal input already cover free
        // hands; only emulate here when neither does.
        self.hands_as_controllers =
            self.hand_interaction_bound && config.hands_as_controllers && !self.multimodal_hands_enabled;

        self.face_sources.eye_tracker_fb = create_ext_object(
            "EyeTrackerSocial",
            config.face_tracking.as_ref().map(|s| s.eye_tracking_fb),
            || EyeTrackerSocial::new(&self.xr_session),
        );

        self.face_sources.face_tracker_fb = create_ext_object(
            "FaceTracker2FB",
            config.face_tracking.as_ref().map(|s| s.face_tracking_fb),
            || FaceTracker2FB::new(self.xr_session.clone(), true, true),
        );

        self.body_sources.body_tracker_fb = create_ext_object(
            "BodyTrackerFB (full set)",
            config
                .body_tracking
                .as_ref()
                .and_then(|s| s.body_tracking_fb.as_option())
                .map(|c| c.full_body),
            || BodyTrackerFB::new(&self.xr_session, *BODY_JOINT_SET_FULL_BODY_META),
        )
        .map(|tracker| (tracker, FULL_BODY_JOINT_COUNT_META))
        .or_else(|| {
            create_ext_object(
                "BodyTrackerFB (default set)",
                config
                    .body_tracking
                    .as_ref()
                    .map(|s| s.body_tracking_fb.enabled()),
                || BodyTrackerFB::new(&self.xr_session, xr::BodyJointSetFB::DEFAULT),
            )
            .map(|tracker| (tracker, xr::BodyJointFB::COUNT.into_raw() as usize))
        });

    }
}

pub fn get_reference_space(
    xr_session: &xr::Session<xr::Vulkan>,
    ty: xr::ReferenceSpaceType,
) -> xr::Space {
    get_offset_reference_space(xr_session, ty, 0.0)
}

/// A reference space whose origin is `height_offset_m` lower, so every pose located in it is that
/// much higher: the user's floor height offset. Views, hands and the submitted layer must all use
/// the same space, then the picture does not move and only the game sees the shifted heights.
pub fn get_offset_reference_space(
    xr_session: &xr::Session<xr::Vulkan>,
    ty: xr::ReferenceSpaceType,
    height_offset_m: f32,
) -> xr::Space {
    xr_session
        .create_reference_space(
            ty,
            xr::Posef {
                orientation: xr::Quaternionf::IDENTITY,
                position: xr::Vector3f {
                    x: 0.0,
                    y: -height_offset_m,
                    z: 0.0,
                },
            },
        )
        .unwrap()
}

/// The pointing pose of a controller (the aim pose, -Z forward), if it is tracked.
pub fn get_aim_pose(
    xr_session: &xr::Session<xr::Vulkan>,
    reference_space: &xr::Space,
    time: Duration,
    hand_source: &HandInteraction,
) -> Option<Pose> {
    if !hand_source
        .aim_action
        .is_active(xr_session, xr::Path::NULL)
        .unwrap_or(false)
    {
        return None;
    }

    let location = hand_source
        .aim_space
        .locate(reference_space, crate::to_xr_time(time))
        .ok()?;

    location
        .location_flags
        .contains(xr::SpaceLocationFlags::ORIENTATION_VALID | xr::SpaceLocationFlags::POSITION_VALID)
        .then(|| crate::from_xr_pose(location.pose))
}

pub fn get_head_data(
    xr_session: &xr::Session<xr::Vulkan>,
    stage_reference_space: &xr::Space,
    view_reference_space: &xr::Space,
    time: Duration,
    last_view_params: &[ViewParams; 2],
) -> Option<(DeviceMotion, Option<[ViewParams; 2]>)> {
    let xr_time = crate::to_xr_time(time);

    let (head_location, head_velocity) = view_reference_space
        .relate(stage_reference_space, xr_time)
        .ok()?;

    if !head_location
        .location_flags
        .contains(xr::SpaceLocationFlags::ORIENTATION_VALID)
    {
        return None;
    }

    let (view_flags, views) = xr_session
        .locate_views(
            xr::ViewConfigurationType::PRIMARY_STEREO,
            xr_time,
            stage_reference_space,
        )
        .ok()?;

    if !view_flags.contains(xr::ViewStateFlags::POSITION_VALID)
        || !view_flags.contains(xr::ViewStateFlags::ORIENTATION_VALID)
    {
        return None;
    }

    let motion = DeviceMotion {
        pose: crate::from_xr_pose(head_location.pose),
        linear_velocity: if head_velocity
            .velocity_flags
            .contains(xr::SpaceVelocityFlags::LINEAR_VALID)
        {
            crate::from_xr_vec3(head_velocity.linear_velocity)
        } else {
            Vec3::ZERO
        },
        angular_velocity: if head_velocity
            .velocity_flags
            .contains(xr::SpaceVelocityFlags::ANGULAR_VALID)
        {
            crate::from_xr_vec3(head_velocity.angular_velocity)
        } else {
            Vec3::ZERO
        },
    };

    let last_ipd_m = last_view_params[0]
        .pose
        .position
        .distance(last_view_params[1].pose.position);
    let current_ipd_m = crate::from_xr_vec3(views[1].pose.position)
        .distance(crate::from_xr_vec3(views[0].pose.position));
    let view_params = if f32::abs(current_ipd_m - last_ipd_m) > IPD_CHANGE_EPS {
        Some([
            ViewParams {
                pose: motion.pose.inverse() * crate::from_xr_pose(views[0].pose),
                fov: crate::from_xr_fov(views[0].fov),
            },
            ViewParams {
                pose: motion.pose.inverse() * crate::from_xr_pose(views[1].pose),
                fov: crate::from_xr_fov(views[1].fov),
            },
        ])
    } else {
        None
    };

    Some((motion, view_params))
}

pub fn get_hand_data(
    xr_session: &xr::Session<xr::Vulkan>,
    reference_space: &xr::Space,
    time: Duration,
    hand_source: &HandInteraction,
    last_controller_pose: &mut Pose,
    last_palm_pose: &mut Pose,
) -> (Option<DeviceMotion>, Option<[Pose; 26]>) {
    let xr_time = crate::to_xr_time(time);

    let controller_motion = if hand_source
        .grip_action
        .is_active(xr_session, xr::Path::NULL)
        .unwrap_or(false)
    {
        if let Ok((location, velocity)) = hand_source.grip_space.relate(reference_space, xr_time) {
            let orientation_valid = location
                .location_flags
                .contains(xr::SpaceLocationFlags::ORIENTATION_VALID);
            let position_valid = location
                .location_flags
                .contains(xr::SpaceLocationFlags::POSITION_VALID);

            if orientation_valid {
                last_controller_pose.orientation = crate::from_xr_quat(location.pose.orientation);
            }

            if position_valid {
                last_controller_pose.position = crate::from_xr_vec3(location.pose.position);
            }

            let pose = *last_controller_pose * hand_source.pose_offset;

            let linear_velocity = crate::from_xr_vec3(velocity.linear_velocity);
            let angular_velocity = crate::from_xr_vec3(velocity.angular_velocity);

            Some(DeviceMotion {
                pose,
                linear_velocity,
                angular_velocity,
            })
        } else {
            None
        }
    } else {
        None
    };

    let hand_joints = if let Some(tracker) = &hand_source.skeleton_tracker {
        let xr_now = crate::xr_runtime_now(xr_session.instance()).unwrap_or(xr_time);

        if let Some(joint_locations) = reference_space
            .locate_hand_joints(tracker, xr_now)
            .ok()
            .flatten()
        {
            if joint_locations[0]
                .location_flags
                .contains(xr::SpaceLocationFlags::ORIENTATION_VALID)
            {
                last_palm_pose.orientation =
                    crate::from_xr_quat(joint_locations[0].pose.orientation);
            }

            if joint_locations[0]
                .location_flags
                .contains(xr::SpaceLocationFlags::POSITION_VALID)
            {
                last_palm_pose.position = crate::from_xr_vec3(joint_locations[0].pose.position);
            }

            let mut joints: [_; 26] = joint_locations
                .iter()
                .map(|j| crate::from_xr_pose(j.pose))
                .collect::<Vec<_>>()
                .try_into()
                .unwrap();

            joints[0] = *last_palm_pose;

            Some(joints)
        } else {
            None
        }
    } else {
        None
    };

    (controller_motion, hand_joints)
}

pub fn update_buttons(
    xr_session: &xr::Session<xr::Vulkan>,
    button_actions: &HashMap<u64, ButtonAction>,
) -> Vec<ButtonEntry> {
    let mut button_entries = Vec::with_capacity(2);
    for (id, action) in button_actions {
        match action {
            ButtonAction::Binary(action) => {
                let Ok(state) = action.state(xr_session, xr::Path::NULL) else {
                    continue;
                };

                if state.changed_since_last_sync {
                    button_entries.push(ButtonEntry {
                        path_id: *id,
                        value: ButtonValue::Binary(state.current_state),
                    });
                }
            }
            ButtonAction::Scalar(action) => {
                let Ok(state) = action.state(xr_session, xr::Path::NULL) else {
                    continue;
                };

                if state.changed_since_last_sync {
                    button_entries.push(ButtonEntry {
                        path_id: *id,
                        value: ButtonValue::Scalar(state.current_state),
                    });
                }
            }
        }
    }

    button_entries
}

// Two pinches in a row on the same hand are that hand's X (left) or A (right) button: the AirFrameXR desktop
// mirror reads it as a double click, like a controller's real button. Same rule as the visionOS client: a pinch
// that begins less than 0.9 s after the end of a short (under 0.6 s) one raises the button for 0.15 s.
const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(900);
const DOUBLE_TAP_MAX_PINCH: Duration = Duration::from_millis(600);
const DOUBLE_TAP_PULSE: Duration = Duration::from_millis(150);
// A pinch is read from the runtime's pinch value (the runtime's boolean click needs a much stronger pinch than the mirror
// counts as a click, so it is not used) and sent to the host as a clean trigger: 1 while pinched, 0 otherwise. The value
// hovers between 0.15 and 0.3 when the fingers rest loosely together: with the mirror's own thresholds (down 0.3, up 0.15)
// a click then stayed down for seconds (mirror log, 2026-10-30: median press 0.8 s, 7 of 16 over 1 s, one of 7.5 s, releases
// logged at 0.12-0.15 while presses started at 0.30-0.44) and selected areas of the desktop. A deliberate squeeze starts a
// pinch and a clear opening ends it; a pinch is a click, like on the visionOS client.
const PINCH_DOWN: f32 = 0.45;
const PINCH_UP: f32 = 0.30;

#[derive(Default, Clone, Copy)]
struct HandTap {
    down: bool,
    began_at: Option<Instant>,
    ended_at: Option<Instant>,
    pulse_until: Option<Instant>,
    pulse_sent: bool,
}

#[derive(Default)]
pub struct HandTaps([HandTap; 2]);

impl HandTaps {
    /// `hand_driven`: the hand is tracked without a controller (the pinch is read from the trigger
    /// value, which the hand interaction profile drives, and sent as a clean trigger value and click);
    /// with controllers nothing is emulated and the controller's own trigger entries are sent.
    pub fn update(
        &mut self,
        xr_session: &xr::Session<xr::Vulkan>,
        button_actions: &HashMap<u64, ButtonAction>,
        hand_driven: [bool; 2],
        now: Instant,
    ) -> Vec<ButtonEntry> {
        let mut entries = vec![];

        for (h, tap) in self.0.iter_mut().enumerate() {
            let (trigger_value_id, trigger_click_id, button_id) = if h == 0 {
                (*LEFT_TRIGGER_VALUE_ID, *LEFT_TRIGGER_CLICK_ID, *LEFT_X_CLICK_ID)
            } else {
                (*RIGHT_TRIGGER_VALUE_ID, *RIGHT_TRIGGER_CLICK_ID, *RIGHT_A_CLICK_ID)
            };

            let pinch = match button_actions.get(&trigger_value_id) {
                Some(ButtonAction::Scalar(action)) if hand_driven[h] => action
                    .state(xr_session, xr::Path::NULL)
                    .ok()
                    .filter(|s| s.is_active)
                    .map(|s| s.current_state),
                _ => None,
            };
            // down at PINCH_DOWN, up at PINCH_UP (hysteresis, like the mirror)
            let down = pinch.is_some_and(|v| if tap.down { v > PINCH_UP } else { v >= PINCH_DOWN });

            if down && !tap.down {
                if let (Some(began), Some(ended)) = (tap.began_at, tap.ended_at) {
                    if ended > began
                        && now.duration_since(ended) < DOUBLE_TAP_WINDOW
                        && ended.duration_since(began) < DOUBLE_TAP_MAX_PINCH
                    {
                        tap.pulse_until = Some(now + DOUBLE_TAP_PULSE);
                    }
                }
                tap.began_at = Some(now);
            } else if !down && tap.down {
                tap.ended_at = Some(now);
            }
            // the clean trigger for a free hand (and one last release when the hand stops being hand-driven)
            if down != tap.down {
                entries.push(ButtonEntry {
                    path_id: trigger_value_id,
                    value: ButtonValue::Scalar(if down { 1.0 } else { 0.0 }),
                });
                entries.push(ButtonEntry {
                    path_id: trigger_click_id,
                    value: ButtonValue::Binary(down),
                });
            }
            tap.down = down;

            let pulse = tap.pulse_until.is_some_and(|until| now < until);
            if pulse != tap.pulse_sent {
                tap.pulse_sent = pulse;
                entries.push(ButtonEntry {
                    path_id: button_id,
                    value: ButtonValue::Binary(pulse),
                });
            }
        }

        entries
    }
}

pub fn get_eye_gazes(
    sources: &FaceSources,
    reference_space: &xr::Space,
    time: Duration,
) -> [Option<Pose>; 2] {
    let xr_time = crate::to_xr_time(time);

    'fb_eyes: {
        let Some(tracker) = &sources.eye_tracker_fb else {
            break 'fb_eyes;
        };

        if let Ok(gazes) = tracker.get_eye_gazes(reference_space, xr_time) {
            return [
                gazes[0].map(crate::from_xr_pose),
                gazes[1].map(crate::from_xr_pose),
            ];
        }
    };

    [None, None]
}

pub fn get_fb_face_expression(context: &FaceSources, time: Duration) -> Option<Vec<f32>> {
    let xr_time = crate::to_xr_time(time);

    context
        .face_tracker_fb
        .as_ref()
        .and_then(|t| t.get_face_expression_weights(xr_time).ok().flatten())
        .map(|weights| weights.into_iter().collect())
}

pub fn get_fb_body_skeleton(
    reference_space: &xr::Space,
    time: xr::Time,
    body_tracker: &BodyTrackerFB,
    joint_count: usize,
) -> Option<Vec<Option<Pose>>> {
    body_tracker
        .locate_body_joints(time, reference_space, joint_count)
        .ok()
        .flatten()
        .map(|joints| {
            let valid_flags: SpaceLocationFlags =
                SpaceLocationFlags::ORIENTATION_VALID | SpaceLocationFlags::POSITION_VALID;

            joints
                .iter()
                .map(|joint| {
                    joint
                        .location_flags
                        .contains(valid_flags)
                        .then(|| crate::from_xr_pose(joint.pose))
                })
                .collect()
        })
}

pub fn get_fb_body_tracking_points(
    reference_space: &xr::Space,
    time: Duration,
    body_tracker: &BodyTrackerFB,
    joint_count: usize,
) -> Vec<(u64, DeviceMotion)> {
    let xr_time = crate::to_xr_time(time);

    if let Some(joint_locations) = body_tracker
        .locate_body_joints(xr_time, reference_space, joint_count)
        .ok()
        .flatten()
    {
        let valid_flags: SpaceLocationFlags =
            SpaceLocationFlags::ORIENTATION_VALID | SpaceLocationFlags::POSITION_VALID;

        let mut joints = Vec::<(u64, DeviceMotion)>::with_capacity(8);

        if let Some(joint) = joint_locations.get(xr::BodyJointFB::CHEST.into_raw() as usize) {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_CHEST_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) = joint_locations.get(xr::BodyJointFB::HIPS.into_raw() as usize) {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_HIPS_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) =
            joint_locations.get(xr::BodyJointFB::LEFT_ARM_LOWER.into_raw() as usize)
        {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_LEFT_ELBOW_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) =
            joint_locations.get(xr::BodyJointFB::RIGHT_ARM_LOWER.into_raw() as usize)
        {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_RIGHT_ELBOW_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) = joint_locations.get(FULL_BODY_JOINT_LEFT_LOWER_LEG_META) {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_LEFT_KNEE_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) = joint_locations.get(FULL_BODY_JOINT_LEFT_FOOT_BALL_META) {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_LEFT_FOOT_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) = joint_locations.get(FULL_BODY_JOINT_RIGHT_LOWER_LEG_META) {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_RIGHT_KNEE_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        if let Some(joint) = joint_locations.get(FULL_BODY_JOINT_RIGHT_FOOT_BALL_META) {
            if joint.location_flags.contains(valid_flags) {
                joints.push((
                    *BODY_RIGHT_FOOT_ID,
                    DeviceMotion {
                        pose: crate::from_xr_pose(joint.pose),
                        linear_velocity: Vec3::ZERO,
                        angular_velocity: Vec3::ZERO,
                    },
                ))
            }
        }

        return joints;
    }

    Vec::new()
}

