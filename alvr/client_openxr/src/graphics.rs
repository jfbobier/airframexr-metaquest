use alvr_common::{
    anyhow::anyhow,
    glam::UVec2,
};
use alvr_graphics::{GraphicsContext, VulkanSessionHandles, XrVulkanBridge};
use alvr_session::ClientsidePostProcessingConfig;
use ash::vk::{self, Handle};
use openxr as xr;
use std::mem;

fn to_vk_version(version: xr::Version) -> u32 {
    vk::make_api_version(0, version.major() as _, version.minor() as _, version.patch())
}

/// Creates the Vulkan instance and device through the OpenXR runtime (XR_KHR_vulkan_enable2).
/// `Instance::graphics_requirements::<Vulkan>()` must have been called first (mandatory call).
pub fn create_graphics_context(
    xr_instance: &xr::Instance,
    xr_system: xr::SystemId,
    requirements: &xr::vulkan::Requirements,
) -> GraphicsContext {
    // SAFETY: the function pointer types only differ by the handle newtypes, which are
    // pointer sized (same approach as bevy_mod_openxr)
    let to_xr_proc_addr = |f: vk::PFN_vkGetInstanceProcAddr| unsafe {
        mem::transmute::<vk::PFN_vkGetInstanceProcAddr, xr::sys::platform::VkGetInstanceProcAddr>(f)
    };

    GraphicsContext::new_vulkan(&XrVulkanBridge {
        min_api_version: to_vk_version(requirements.min_api_version_supported),
        max_api_version: to_vk_version(requirements.max_api_version_supported),
        create_instance: &|get_instance_proc_addr, create_info| unsafe {
            let instance = xr_instance
                .create_vulkan_instance(
                    xr_system,
                    to_xr_proc_addr(get_instance_proc_addr),
                    std::ptr::from_ref(create_info).cast(),
                )
                .map_err(|e| anyhow!("{e}"))?
                .map_err(|e| anyhow!("vkCreateInstance returned {e}"))?;

            Ok(vk::Instance::from_raw(instance as u64))
        },
        get_physical_device: &|instance| unsafe {
            let physical_device = xr_instance
                .vulkan_graphics_device(xr_system, instance.as_raw() as _)
                .map_err(|e| anyhow!("{e}"))?;

            Ok(vk::PhysicalDevice::from_raw(physical_device as u64))
        },
        create_device: &|get_instance_proc_addr, physical_device, create_info| unsafe {
            let device = xr_instance
                .create_vulkan_device(
                    xr_system,
                    to_xr_proc_addr(get_instance_proc_addr),
                    physical_device.as_raw() as _,
                    std::ptr::from_ref(create_info).cast(),
                )
                .map_err(|e| anyhow!("{e}"))?
                .map_err(|e| anyhow!("vkCreateDevice returned {e}"))?;

            Ok(vk::Device::from_raw(device as u64))
        },
    })
    .unwrap_or_else(|e: alvr_common::anyhow::Error| panic!("Vulkan initialization failed: {e:#}"))
}

pub fn session_create_info(ctx: &GraphicsContext) -> xr::vulkan::SessionCreateInfo {
    let VulkanSessionHandles {
        instance,
        physical_device,
        device,
        queue_family_index,
        queue_index,
    } = ctx.vulkan_handles();

    xr::vulkan::SessionCreateInfo {
        instance: instance.as_raw() as _,
        physical_device: physical_device.as_raw() as _,
        device: device.as_raw() as _,
        queue_family_index,
        queue_index,
    }
}

pub fn swapchain_format(
    _gfx_ctx: &GraphicsContext,
    session: &xr::Session<xr::Vulkan>,
    enable_hdr: bool,
) -> u32 {
    let formats = session.enumerate_swapchain_formats().unwrap();
    alvr_graphics::choose_swapchain_format(&formats, enable_hdr)
}

#[allow(unused_variables)]
pub fn create_swapchain(
    session: &xr::Session<xr::Vulkan>,
    gfx_ctx: &GraphicsContext,
    resolution: UVec2,
    format: u32,
    foveation: Option<&xr::FoveationProfileFB>,
) -> xr::Swapchain<xr::Vulkan> {
    let swapchain_info = xr::SwapchainCreateInfo {
        create_flags: xr::SwapchainCreateFlags::EMPTY,
        usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT | xr::SwapchainUsageFlags::SAMPLED,
        format,
        sample_count: 1,
        width: resolution.x,
        height: resolution.y,
        face_count: 1,
        array_size: 1,
        mip_count: 1,
    };

    session.create_swapchain(&swapchain_info).unwrap()
}

pub struct ProjectionLayerAlphaConfig {
    pub premultiplied: bool,
}

// This is needed to work around lifetime limitations. Deref cannot be implemented because there are
// nested references, and in a way or the other I would get `cannot return reference to temporary
// value`
pub struct ProjectionLayerBuilder<'a> {
    reference_space: &'a xr::Space,
    layers: [xr::CompositionLayerProjectionView<'a, xr::Vulkan>; 2],
    alpha: Option<ProjectionLayerAlphaConfig>,
    composition_layer_settings: Option<xr::sys::CompositionLayerSettingsFB>,
}

impl<'a> ProjectionLayerBuilder<'a> {
    pub fn new(
        reference_space: &'a xr::Space,
        layers: [xr::CompositionLayerProjectionView<'a, xr::Vulkan>; 2],
        alpha: Option<ProjectionLayerAlphaConfig>,
        clientside_post_processing_config: Option<ClientsidePostProcessingConfig>,
    ) -> Self {
        let composition_layer_settings = clientside_post_processing_config
            .map(|post_processing| {
                xr::CompositionLayerSettingsFlagsFB::from_raw(
                    (post_processing.sharpening as u64) | (post_processing.super_sampling as u64),
                )
            })
            .filter(|&flags| flags != xr::CompositionLayerSettingsFlagsFB::EMPTY)
            .map(|flags| xr::sys::CompositionLayerSettingsFB {
                ty: xr::StructureType::COMPOSITION_LAYER_SETTINGS_FB,
                next: std::ptr::null(),
                layer_flags: flags,
            });
        Self {
            reference_space,
            layers,
            alpha,
            composition_layer_settings,
        }
    }

    pub fn build(&self) -> xr::CompositionLayerProjection<xr::Vulkan> {
        let mut flags = xr::CompositionLayerFlags::EMPTY;

        if let Some(alpha) = &self.alpha {
            flags |= xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA;

            if !alpha.premultiplied {
                flags |= xr::CompositionLayerFlags::UNPREMULTIPLIED_ALPHA;
            }
        }

        let layer = xr::CompositionLayerProjection::new()
            .layer_flags(flags)
            .space(self.reference_space)
            .views(&self.layers);

        if let Some(composition_layer_settings) = self.composition_layer_settings.as_ref() {
            unsafe {
                xr::CompositionLayerProjection::from_raw(xr::sys::CompositionLayerProjection {
                    next: std::ptr::from_ref(composition_layer_settings).cast(),
                    ..layer.into_raw()
                })
            }
        } else {
            layer
        }
    }
}
