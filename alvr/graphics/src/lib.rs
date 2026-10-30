mod lobby;
mod panel;
mod staging;
mod stream;

pub use lobby::*;
pub use panel::*;
pub use stream::*;

use alvr_common::{
    anyhow::{anyhow, bail, Context, Result},
    glam::{Mat4, UVec2, Vec4},
    info, warn, Fov,
};
use ash::vk::{self, Handle};
use std::ffi::{c_char, CStr};
use wgpu::{
    hal::{self, api, MemoryFlags, TextureUses},
    Adapter, Device, DeviceDescriptor, Extent3d, Features, Instance, InstanceFlags, Limits,
    MemoryHints, Queue, Texture, TextureDescriptor, TextureDimension, TextureFormat,
    TextureUsages, TextureView,
};

pub const SDR_FORMAT: TextureFormat = TextureFormat::Rgba8Unorm;

// Swapchain formats are passed around as raw `VkFormat` values (what OpenXR enumerates). This
// replaces the GL enums (`SRGB8_ALPHA8`, `RGBA8`, `RGBA16F`) of the GLES backend 1:1.
pub const VK_FORMAT_R8G8B8A8_UNORM: u32 = vk::Format::R8G8B8A8_UNORM.as_raw() as u32;
pub const VK_FORMAT_R8G8B8A8_SRGB: u32 = vk::Format::R8G8B8A8_SRGB.as_raw() as u32;
pub const VK_FORMAT_R16G16B16A16_SFLOAT: u32 = vk::Format::R16G16B16A16_SFLOAT.as_raw() as u32;
pub const SDR_FORMAT_VK: u32 = VK_FORMAT_R8G8B8A8_UNORM;

pub const MAX_PUSH_CONSTANTS_SIZE: u32 = 128;

fn projection_from_fov(fov: Fov) -> Mat4 {
    const NEAR: f32 = 0.1;

    let tanl = f32::tan(fov.left);
    let tanr = f32::tan(fov.right);
    let tanu = f32::tan(fov.up);
    let tand = f32::tan(fov.down);
    let a = 2.0 / (tanr - tanl);
    let b = 2.0 / (tanu - tand);
    let c = (tanr + tanl) / (tanr - tanl);
    let d = (tanu + tand) / (tanu - tand);

    // Deviation from the GLES backend: there the b and d components were flipped, to give the
    // OpenGL runtime the bottom-left-origin image it expects (wgpu's GLES backend renders with a
    // top-left origin). OpenXR Vulkan swapchains use the Vulkan convention (row 0 is the top) and
    // wgpu's Vulkan backend already maps NDC y-up to it with a negative viewport, so the plain
    // projection is the right one. NOT VERIFIED ON DEVICE: if the picture is upside down, this is
    // the place.
    Mat4::from_cols(
        Vec4::new(a, 0.0, c, 0.0),
        Vec4::new(0.0, b, d, 0.0),
        Vec4::new(0.0, 0.0, -1.0, -NEAR),
        Vec4::new(0.0, 0.0, -1.0, 0.0),
    )
    .transpose()
}

pub fn choose_swapchain_format(supported_formats: &[u32], enable_hdr: bool) -> u32 {
    // Priority-sorted list of swapchain formats we'll accept--
    let mut app_supported_swapchain_formats = vec![VK_FORMAT_R8G8B8A8_SRGB, VK_FORMAT_R8G8B8A8_UNORM];

    // float16 is required for HDR output. However, float16 swapchains
    // have a high perf cost, so only use these if HDR is enabled.
    if enable_hdr {
        app_supported_swapchain_formats.insert(0, VK_FORMAT_R16G16B16A16_SFLOAT);
    }

    for format in app_supported_swapchain_formats {
        if supported_formats.contains(&format) {
            return format;
        }
    }

    // If we can't enumerate, default to a required format
    VK_FORMAT_R8G8B8A8_UNORM
}

pub fn vk_format_to_wgpu(format: u32) -> TextureFormat {
    match format {
        VK_FORMAT_R8G8B8A8_SRGB => TextureFormat::Rgba8UnormSrgb,
        VK_FORMAT_R8G8B8A8_UNORM => TextureFormat::Rgba8Unorm,
        VK_FORMAT_R16G16B16A16_SFLOAT => TextureFormat::Rgba16Float,
        _ => panic!("Unsupported Vulkan format: {format}"),
    }
}

pub fn create_texture(device: &Device, resolution: UVec2, format: TextureFormat) -> Texture {
    device.create_texture(&TextureDescriptor {
        label: None,
        size: Extent3d {
            width: resolution.x,
            height: resolution.y,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::COPY_DST | TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

/// The stream staging texture. In the GLES backend it was a plain wgpu texture rendered through a
/// GL framebuffer; here it is a plain wgpu texture rendered by the raw Vulkan staging pass, so it
/// needs the render attachment usage (the Vulkan image gets COLOR_ATTACHMENT | SAMPLED).
pub fn create_staging_texture(device: &Device, resolution: UVec2, format: TextureFormat) -> Texture {
    device.create_texture(&TextureDescriptor {
        label: Some("stream staging"),
        size: Extent3d {
            width: resolution.x,
            height: resolution.y,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

fn create_texture_from_vulkan(
    device: &Device,
    image: u64,
    resolution: UVec2,
    format: TextureFormat,
) -> Texture {
    let size = Extent3d {
        width: resolution.x,
        height: resolution.y,
        depth_or_array_layers: 1,
    };

    unsafe {
        let hal_texture = hal::vulkan::Device::texture_from_raw(
            vk::Image::from_raw(image),
            &hal::TextureDescriptor {
                label: None,
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format,
                usage: TextureUses::COLOR_TARGET,
                memory_flags: MemoryFlags::empty(),
                view_formats: vec![],
            },
            // The image belongs to the OpenXR swapchain: do not destroy it
            Some(Box::new(|| ())),
        );

        device.create_texture_from_hal::<api::Vulkan>(
            hal_texture,
            &TextureDescriptor {
                label: None,
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format,
                usage: TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            },
        )
    }
}

// This is used to convert OpenXR swapchains to wgpu
pub fn create_vk_swapchain(
    device: &Device,
    vk_images: &[u64],
    resolution: UVec2,
    format: TextureFormat,
) -> Vec<TextureView> {
    vk_images
        .iter()
        .map(|image| {
            create_texture_from_vulkan(device, *image, resolution, format)
                .create_view(&Default::default())
        })
        .collect()
}

/// What OpenXR has to do on our behalf with XR_KHR_vulkan_enable2: the runtime chooses the
/// physical device and creates the VkInstance and VkDevice (so that its compositor can share
/// them). `alvr_graphics` does not depend on the `openxr` crate, the client implements this.
pub struct XrVulkanBridge<'a> {
    pub min_api_version: u32,
    pub max_api_version: u32,
    /// xrCreateVulkanInstanceKHR
    pub create_instance: &'a dyn Fn(
        vk::PFN_vkGetInstanceProcAddr,
        &vk::InstanceCreateInfo<'_>,
    ) -> Result<vk::Instance>,
    /// xrGetVulkanGraphicsDevice2KHR
    pub get_physical_device: &'a dyn Fn(vk::Instance) -> Result<vk::PhysicalDevice>,
    /// xrCreateVulkanDeviceKHR
    pub create_device: &'a dyn Fn(
        vk::PFN_vkGetInstanceProcAddr,
        vk::PhysicalDevice,
        &vk::DeviceCreateInfo<'_>,
    ) -> Result<vk::Device>,
}

/// Raw handles needed by XrGraphicsBindingVulkanKHR.
#[derive(Clone, Copy)]
pub struct VulkanSessionHandles {
    pub instance: vk::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: vk::Device,
    pub queue_family_index: u32,
    pub queue_index: u32,
}

#[cfg(target_os = "android")]
fn android_sdk_version() -> u32 {
    let mut value = [0 as c_char; 92]; // PROP_VALUE_MAX
    let len = unsafe {
        libc::__system_property_get(c"ro.build.version.sdk".as_ptr(), value.as_mut_ptr())
    };
    if len > 0 {
        unsafe { CStr::from_ptr(value.as_ptr()) }
            .to_str()
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    } else {
        0
    }
}

#[cfg(not(target_os = "android"))]
fn android_sdk_version() -> u32 {
    0
}

pub struct GraphicsContext {
    // Drop order matters: device and queue before adapter and instance
    device: Device,
    queue: Queue,

    #[expect(dead_code)]
    adapter: Adapter,
    _instance: Instance,

    vulkan_handles: VulkanSessionHandles,

    // Device, driver and limits as one line, for the always-visible diagnostics (see DIAG.md)
    device_summary: String,
}

impl GraphicsContext {
    /// Replaces `new_gl()`. Creates the Vulkan instance/device through the OpenXR runtime
    /// (XR_KHR_vulkan_enable2) and wraps them into wgpu (wgpu-hal `from_raw`, same approach as
    /// bevy_mod_openxr). `Instance::graphics_requirements::<Vulkan>()` must have been called.
    pub fn new_vulkan(bridge: &XrVulkanBridge) -> Result<Self> {
        let entry = unsafe { ash::Entry::load() }.context("Failed to load the Vulkan loader")?;

        let instance_version = unsafe { entry.try_enumerate_instance_version() }
            .context("vkEnumerateInstanceVersion")?
            .unwrap_or(vk::API_VERSION_1_0);
        let api_version = instance_version.clamp(bridge.min_api_version, bridge.max_api_version);
        info!(
            "Vulkan: instance version {}.{}, OpenXR range {}.{}..{}.{}, using {}.{}",
            vk::api_version_major(instance_version),
            vk::api_version_minor(instance_version),
            vk::api_version_major(bridge.min_api_version),
            vk::api_version_minor(bridge.min_api_version),
            vk::api_version_major(bridge.max_api_version),
            vk::api_version_minor(bridge.max_api_version),
            vk::api_version_major(api_version),
            vk::api_version_minor(api_version),
        );
        // The AHardwareBuffer import (YCbCr conversion, external memory) relies on Vulkan 1.1
        // promoted functionality.
        if api_version < vk::API_VERSION_1_1 {
            bail!("Vulkan 1.1 is required, the runtime allows only up to the lower version");
        }

        let flags = if cfg!(debug_assertions) {
            InstanceFlags::DEBUG | InstanceFlags::VALIDATION
        } else {
            InstanceFlags::empty()
        };

        let extensions = hal::vulkan::Instance::desired_extensions(&entry, api_version, flags)
            .map_err(|e| anyhow!("Vulkan desired instance extensions: {e:?}"))?;
        let extension_ptrs = extensions.iter().map(|e| e.as_ptr()).collect::<Vec<_>>();

        // Validation layer only when requested and present (like wgpu's own instance creation)
        let mut layer_ptrs = vec![];
        if flags.contains(InstanceFlags::VALIDATION) {
            const VALIDATION_LAYER: &CStr = c"VK_LAYER_KHRONOS_validation";
            let available = unsafe { entry.enumerate_instance_layer_properties() }
                .unwrap_or_default()
                .iter()
                .any(|l| l.layer_name_as_c_str().ok() == Some(VALIDATION_LAYER));
            if available {
                layer_ptrs.push(VALIDATION_LAYER.as_ptr());
            } else {
                warn!("Vulkan validation requested but VK_LAYER_KHRONOS_validation is not present");
            }
        }

        let app_info = vk::ApplicationInfo::default()
            .application_name(c"ALVR Client")
            .application_version(0)
            .engine_name(c"ALVR")
            .engine_version(0)
            .api_version(api_version);
        let instance_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(&extension_ptrs)
            .enabled_layer_names(&layer_ptrs);

        let get_instance_proc_addr = entry.static_fn().get_instance_proc_addr;
        let raw_instance = (bridge.create_instance)(get_instance_proc_addr, &instance_info)
            .context("xrCreateVulkanInstanceKHR")?;
        let ash_instance = unsafe { ash::Instance::load(entry.static_fn(), raw_instance) };

        let physical_device = (bridge.get_physical_device)(raw_instance)
            .context("xrGetVulkanGraphicsDevice2KHR")?;

        let hal_instance = unsafe {
            hal::vulkan::Instance::from_raw(
                entry,
                ash_instance.clone(),
                api_version,
                android_sdk_version(),
                None, // debug utils messenger: the validation layer logs by itself
                extensions,
                flags,
                false,
                None, // wgpu-hal owns (and destroys) the instance
            )
        }
        .map_err(|e| anyhow!("wgpu-hal Vulkan instance: {e:?}"))?;

        let exposed_adapter = hal_instance.expose_adapter(physical_device)
            .context("wgpu does not support the Vulkan physical device chosen by OpenXR")?;

        let features = Features::PUSH_CONSTANTS;

        let mut device_extensions = exposed_adapter.adapter.required_device_extensions(features);

        // Needed to import the decoder's AHardwareBuffer as a sampled image. The other
        // extensions VK_ANDROID_external_memory_android_hardware_buffer depends on are part of
        // Vulkan 1.1 (checked above).
        let available_device_extensions =
            unsafe { ash_instance.enumerate_device_extension_properties(physical_device) }
                .context("vkEnumerateDeviceExtensionProperties")?;
        for required in [
            ash::android::external_memory_android_hardware_buffer::NAME,
            ash::ext::queue_family_foreign::NAME,
        ] {
            let available = available_device_extensions
                .iter()
                .any(|e| e.extension_name_as_c_str().ok() == Some(required));
            if !available {
                bail!("Vulkan device extension {required:?} is not supported");
            }
            if !device_extensions.contains(&required) {
                device_extensions.push(required);
            }
        }

        let queue_family_index = unsafe {
            ash_instance.get_physical_device_queue_family_properties(physical_device)
        }
        .iter()
        .position(|q| q.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .context("No graphics queue family")? as u32;
        let queue_index = 0;

        let queue_priorities = [1.0];
        let queue_infos = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family_index)
            .queue_priorities(&queue_priorities)];
        let device_extension_ptrs = device_extensions
            .iter()
            .map(|e| e.as_ptr())
            .collect::<Vec<*const c_char>>();

        let mut physical_device_features = exposed_adapter
            .adapter
            .physical_device_features(&device_extensions, features);
        let mut ycbcr_features =
            vk::PhysicalDeviceSamplerYcbcrConversionFeatures::default().sampler_ycbcr_conversion(true);
        let device_info = physical_device_features
            .add_to_device_create(
                vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queue_infos)
                    .enabled_extension_names(&device_extension_ptrs),
            )
            .push_next(&mut ycbcr_features);

        let raw_device = (bridge.create_device)(get_instance_proc_addr, physical_device, &device_info)
            .context("xrCreateVulkanDeviceKHR")?;
        let ash_device = unsafe { ash::Device::load(ash_instance.fp_v1_0(), raw_device) };

        let open_device = unsafe {
            exposed_adapter.adapter.device_from_raw(
                ash_device,
                None, // wgpu-hal owns (and destroys) the device
                &device_extensions,
                features,
                &MemoryHints::Performance,
                queue_family_index,
                queue_index,
            )
        }
        .map_err(|e| anyhow!("wgpu-hal Vulkan device: {e:?}"))?;

        let instance = unsafe { Instance::from_hal::<api::Vulkan>(hal_instance) };
        let adapter = unsafe { instance.create_adapter_from_hal(exposed_adapter) };
        let (device, queue) = unsafe {
            adapter.create_device_from_hal(
                open_device,
                &DeviceDescriptor {
                    label: None,
                    required_features: features,
                    required_limits: Limits {
                        max_push_constant_size: MAX_PUSH_CONSTANTS_SIZE,
                        ..adapter.limits()
                    },
                    memory_hints: MemoryHints::Performance,
                },
                None,
            )
        }
        .context("wgpu device from the OpenXR Vulkan device")?;

        let device_summary = {
            // VkPhysicalDeviceDriverProperties is core in 1.2 and an extension before
            let driver_properties_available = api_version >= vk::API_VERSION_1_2
                || available_device_extensions.iter().any(|e| {
                    e.extension_name_as_c_str().ok() == Some(ash::khr::driver_properties::NAME)
                });
            let mut driver_properties = vk::PhysicalDeviceDriverProperties::default();
            let mut properties2 = vk::PhysicalDeviceProperties2::default();
            if driver_properties_available {
                properties2 = properties2.push_next(&mut driver_properties);
            }
            unsafe { ash_instance.get_physical_device_properties2(physical_device, &mut properties2) };
            let properties = properties2.properties;
            let c_text = |chars: &[c_char]| {
                unsafe { CStr::from_ptr(chars.as_ptr()) }
                    .to_string_lossy()
                    .into_owned()
            };
            let driver = if driver_properties_available {
                format!(
                    "{} {} (id {:?})",
                    c_text(&driver_properties.driver_name),
                    c_text(&driver_properties.driver_info),
                    driver_properties.driver_id
                )
            } else {
                "driver properties not available".to_owned()
            };
            let extension_names = device_extensions
                .iter()
                .map(|e| e.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "device '{}' type {:?} vendor 0x{:x} id 0x{:x}, API {}.{}.{}, driver version 0x{:x}, {driver}; \
                 queue family {queue_family_index} queue {queue_index}; maxPushConstantsSize {}, maxImageDimension2D {}, \
                 maxMemoryAllocationCount {}; {} device extensions enabled: {extension_names}",
                c_text(&properties.device_name),
                properties.device_type,
                properties.vendor_id,
                properties.device_id,
                vk::api_version_major(properties.api_version),
                vk::api_version_minor(properties.api_version),
                vk::api_version_patch(properties.api_version),
                properties.driver_version,
                properties.limits.max_push_constants_size,
                properties.limits.max_image_dimension2_d,
                properties.limits.max_memory_allocation_count,
                device_extensions.len()
            )
        };
        alvr_common::diag!("[vulkan] instance API {api_version:#x}; {device_summary}");

        Ok(Self {
            device_summary,
            device,
            queue,
            adapter,
            _instance: instance,
            vulkan_handles: VulkanSessionHandles {
                instance: raw_instance,
                physical_device,
                device: raw_device,
                queue_family_index,
                queue_index,
            },
        })
    }

    /// Device, driver and limits in one line (diagnostics)
    pub fn device_summary(&self) -> &str {
        &self.device_summary
    }

    pub fn vulkan_handles(&self) -> VulkanSessionHandles {
        self.vulkan_handles
    }
}
