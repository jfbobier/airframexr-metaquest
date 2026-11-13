use crate::Platform;
use alvr_common::{
    glam::{Quat, Vec3},
    *,
};
use alvr_packets::{ButtonEntry, ButtonValue};
use openxr as xr;
use std::{collections::HashMap, time::Duration};

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
}

pub struct InteractionContext {
    pub action_set: xr::ActionSet,
    pub button_actions: HashMap<u64, ButtonAction>,
    pub hands_interaction: [HandInteraction; 2],
}

impl InteractionContext {
    pub fn new(
        xr_session: xr::Session<xr::Vulkan>,
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

        // Apply bindings:
        xr_instance
            .suggest_interaction_profile_bindings(
                xr_instance
                    .string_to_path(controllers_profile_path)
                    .unwrap(),
                &bindings,
            )
            .unwrap();

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

        Self {
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
                },
                HandInteraction {
                    controllers_profile_id,
                    pose_offset: get_controller_offset(platform, true),
                    grip_action: right_grip_action,
                    grip_space: right_grip_space,
                    aim_action: right_aim_action,
                    aim_space: right_aim_space,
                    vibration_action: right_vibration_action,
                },
            ],
        }
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

/// Motion of a controller (grip pose and velocities), if it is tracked.
pub fn get_controller_motion(
    xr_session: &xr::Session<xr::Vulkan>,
    reference_space: &xr::Space,
    time: Duration,
    hand_source: &HandInteraction,
    last_controller_pose: &mut Pose,
) -> Option<DeviceMotion> {
    let xr_time = crate::to_xr_time(time);

    if !hand_source
        .grip_action
        .is_active(xr_session, xr::Path::NULL)
        .unwrap_or(false)
    {
        return None;
    }

    let (location, velocity) = hand_source.grip_space.relate(reference_space, xr_time).ok()?;

    if location
        .location_flags
        .contains(xr::SpaceLocationFlags::ORIENTATION_VALID)
    {
        last_controller_pose.orientation = crate::from_xr_quat(location.pose.orientation);
    }

    if location
        .location_flags
        .contains(xr::SpaceLocationFlags::POSITION_VALID)
    {
        last_controller_pose.position = crate::from_xr_vec3(location.pose.position);
    }

    Some(DeviceMotion {
        pose: *last_controller_pose * hand_source.pose_offset,
        linear_velocity: crate::from_xr_vec3(velocity.linear_velocity),
        angular_velocity: crate::from_xr_vec3(velocity.angular_velocity),
    })
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
