//! Vulkan port of the GLES staging pass.
//!
//! GLES: the decoder's AHardwareBuffer is wrapped in an EGLImage, bound to a
//! `GL_TEXTURE_EXTERNAL_OES` texture and drawn (YUV -> RGB done by the driver) with a full screen
//! quad into one staging texture per eye (the left/right half of the stream frame). The staging
//! textures are then sampled by the wgpu reprojection pass (`stream.rs`).
//!
//! Vulkan: wgpu cannot import an AHardwareBuffer nor use a YCbCr conversion sampler, so this pass
//! is recorded with raw `ash` commands *inline in the wgpu command buffer* (`CommandEncoder::
//! as_hal_mut` + `raw_handle`), right before the wgpu passes of the same frame. This keeps the GLES
//! order of operations (staging pass first, then reprojection) and lets wgpu's own submission fence
//! tell us when the imported buffer is no longer in use.
//!
//! Layout contract with wgpu: wgpu believes a staging texture is in its RESOURCE state (shader read
//! only layout) whenever it is not sampling it; the raw render pass starts from UNDEFINED (the quad
//! overwrites every texel, like the GLES pass which did not clear) and ends in
//! SHADER_READ_ONLY_OPTIMAL, so wgpu never needs to transition it.

//! UNMEASURED performance hypothesis (not implemented): with the AHardwareBuffer now a sampled
//! Vulkan image, the reprojection pass of `stream.rs` could sample it directly through the same
//! immutable YCbCr sampler and this pass (one full-frame write and read per eye) would disappear.
//! It was not done: it needs the YCbCr sampler inside the wgpu pipeline, which wgpu cannot express
//! (raw pipeline for the whole reprojection shader), and it can only be justified by a measurement.
//! This file is the 1:1 port of the GLES two-pass structure.

use crate::GraphicsContext;
use alvr_common::{error, glam::UVec2};
use ash::{android::external_memory_android_hardware_buffer as ahb_ext, vk};
use std::{
    cell::RefCell,
    ffi::c_void,
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use wgpu::{hal::api, CommandEncoder, Maintain, Texture};

// An import problem: same error! as before, plus the always-visible diagnostics channel and the counter
// of the 1 Hz summary (the frame is repeated, see the callers).
macro_rules! staging_error {
    ($($arg:tt)*) => {{
        let message = format!($($arg)*);
        error!("{message}");
        alvr_common::diag!("[staging-error] {message}");
        alvr_common::diag::STAGING_FAILURES.inc();
    }};
}

const VERTEX_SHADER: &[u8] = include_bytes!("../resources/staging_vertex.spv");
const FRAGMENT_SHADER: &[u8] = include_bytes!("../resources/staging_fragment.spv");

/// Frames whose imported buffers are still potentially in use by the GPU. When reached, the CPU
/// waits (the GLES driver did the equivalent implicitly).
const MAX_FRAMES_IN_FLIGHT: usize = 6;

fn ck<T>(result: ash::prelude::VkResult<T>, what: &str) -> T {
    match result {
        Ok(value) => value,
        Err(e) => {
            error!("Vulkan error in {what}: {e:?}");
            alvr_common::diag!("[vk-error] {what}: {e:?} (about to panic)");
            panic!("Vulkan error in {what}: {e:?}");
        }
    }
}

fn spirv_words(bytes: &[u8]) -> Vec<u32> {
    assert!(bytes.len() % 4 == 0);
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(target_os = "android")]
fn describe_buffer(buffer: *mut c_void) -> (u32, u32) {
    let mut desc = unsafe { std::mem::zeroed::<ndk_sys::AHardwareBuffer_Desc>() };
    unsafe { ndk_sys::AHardwareBuffer_describe(buffer.cast(), &mut desc) };
    (desc.width, desc.height)
}

/// width, height, layers, AHARDWAREBUFFER_FORMAT_*, usage flags, stride (diagnostics)
#[cfg(target_os = "android")]
fn describe_buffer_full(buffer: *mut c_void) -> (u32, u32, u32, u32, u64, u32) {
    let mut desc = unsafe { std::mem::zeroed::<ndk_sys::AHardwareBuffer_Desc>() };
    unsafe { ndk_sys::AHardwareBuffer_describe(buffer.cast(), &mut desc) };
    (desc.width, desc.height, desc.layers, desc.format, desc.usage, desc.stride)
}

#[cfg(not(target_os = "android"))]
fn describe_buffer_full(_buffer: *mut c_void) -> (u32, u32, u32, u32, u64, u32) {
    unimplemented!("AHardwareBuffer is Android only")
}

#[cfg(not(target_os = "android"))]
fn describe_buffer(_buffer: *mut c_void) -> (u32, u32) {
    unimplemented!("AHardwareBuffer is Android only")
}

/// Everything that decides the YCbCr conversion; the sampler, the descriptor layout and the
/// pipeline are rebuilt when the decoder starts producing a different format.
#[derive(Clone, Copy, PartialEq, Debug)]
struct YcbcrKey {
    format: i32,
    external_format: u64,
    model: i32,
    range: i32,
    x_chroma_offset: i32,
    y_chroma_offset: i32,
    components: [i32; 4],
    linear_chroma_filter: bool,
}

struct YcbcrState {
    key: YcbcrKey,
    conversion: vk::SamplerYcbcrConversion,
    sampler: vk::Sampler,
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    descriptor_pool: vk::DescriptorPool,
}

#[derive(Clone, Copy)]
struct YcbcrHandles {
    conversion: vk::SamplerYcbcrConversion,
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    descriptor_pool: vk::DescriptorPool,
}

/// One imported decoder buffer, alive until the GPU is done with the frame that sampled it.
struct FrameResources {
    id: u64,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    descriptor_set: vk::DescriptorSet,
}

#[derive(Default)]
struct Inner {
    ycbcr: Option<YcbcrState>,
    frames: Vec<FrameResources>,
    next_id: u64,
    last_recorded_id: Option<u64>,
}

pub struct StagingRenderer {
    context: Rc<GraphicsContext>,

    instance: ash::Instance,
    device: ash::Device,
    ahb_fn: ahb_ext::Device,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    queue_family_index: u32,

    declared_full_range_bt709: bool,
    target_format: vk::Format,
    extent: vk::Extent2D,

    render_pass: vk::RenderPass,
    vertex_module: vk::ShaderModule,
    fragment_module: vk::ShaderModule,
    staging_views: [vk::ImageView; 2],
    framebuffers: [vk::Framebuffer; 2],

    // Keep the wgpu textures alive for as long as the raw views/framebuffers exist
    _staging_textures: [Texture; 2],

    // Completed submission id, advanced by wgpu's on_submitted_work_done callback
    completed_id: Arc<AtomicU64>,
    inner: RefCell<Inner>,
}

impl StagingRenderer {
    pub fn new(
        context: Rc<GraphicsContext>,
        staging_textures: [Texture; 2],
        target_format: wgpu::TextureFormat,
        view_resolution: UVec2,
        declared_full_range_bt709: bool,
    ) -> Self {
        let (instance, device, queue_family_index, physical_device) = unsafe {
            context.device.as_hal::<api::Vulkan, _, _>(|device| {
                let device = device.expect("the wgpu device is not a Vulkan device");
                (
                    device.shared_instance().raw_instance().clone(),
                    device.raw_device().clone(),
                    device.queue_family_index(),
                    device.raw_physical_device(),
                )
            })
        };
        let memory_properties =
            unsafe { instance.get_physical_device_memory_properties(physical_device) };
        let ahb_fn = ahb_ext::Device::new(&instance, &device);

        let target_format = wgpu_format_to_vk(target_format);
        let extent = vk::Extent2D {
            width: view_resolution.x,
            height: view_resolution.y,
        };

        // Contents are fully overwritten by the quad (the GLES pass did not clear either)
        let attachments = [vk::AttachmentDescription::default()
            .format(target_format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::DONT_CARE)
            .store_op(vk::AttachmentStoreOp::STORE)
            .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
            .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let color_refs = [vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
        let subpasses = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&color_refs)];
        let dependencies = [
            // the previous frame may still be sampling the staging image
            vk::SubpassDependency::default()
                .src_subpass(vk::SUBPASS_EXTERNAL)
                .dst_subpass(0)
                .src_stage_mask(vk::PipelineStageFlags::FRAGMENT_SHADER)
                .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE),
            // make the result visible to the wgpu reprojection pass
            vk::SubpassDependency::default()
                .src_subpass(0)
                .dst_subpass(vk::SUBPASS_EXTERNAL)
                .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .dst_stage_mask(vk::PipelineStageFlags::FRAGMENT_SHADER)
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ),
        ];
        let render_pass = ck(
            unsafe {
                device.create_render_pass(
                    &vk::RenderPassCreateInfo::default()
                        .attachments(&attachments)
                        .subpasses(&subpasses)
                        .dependencies(&dependencies),
                    None,
                )
            },
            "vkCreateRenderPass",
        );

        let create_module = |bytes: &[u8]| {
            let words = spirv_words(bytes);
            ck(
                unsafe {
                    device.create_shader_module(
                        &vk::ShaderModuleCreateInfo::default().code(&words),
                        None,
                    )
                },
                "vkCreateShaderModule",
            )
        };
        let vertex_module = create_module(VERTEX_SHADER);
        let fragment_module = create_module(FRAGMENT_SHADER);

        let mut staging_views = vec![];
        let mut framebuffers = vec![];
        for texture in &staging_textures {
            let image = unsafe {
                texture.as_hal::<api::Vulkan, _, _>(|texture| {
                    texture
                        .expect("the staging texture is not a Vulkan texture")
                        .raw_handle()
                })
            };
            let view = ck(
                unsafe {
                    device.create_image_view(
                        &vk::ImageViewCreateInfo::default()
                            .image(image)
                            .view_type(vk::ImageViewType::TYPE_2D)
                            .format(target_format)
                            .subresource_range(color_range()),
                        None,
                    )
                },
                "vkCreateImageView (staging)",
            );
            let views = [view];
            let framebuffer = ck(
                unsafe {
                    device.create_framebuffer(
                        &vk::FramebufferCreateInfo::default()
                            .render_pass(render_pass)
                            .attachments(&views)
                            .width(extent.width)
                            .height(extent.height)
                            .layers(1),
                        None,
                    )
                },
                "vkCreateFramebuffer",
            );
            staging_views.push(view);
            framebuffers.push(framebuffer);
        }

        Self {
            context,
            instance,
            device,
            ahb_fn,
            memory_properties,
            queue_family_index,
            declared_full_range_bt709,
            target_format,
            extent,
            render_pass,
            vertex_module,
            fragment_module,
            staging_views: staging_views.try_into().unwrap(),
            framebuffers: framebuffers.try_into().unwrap(),
            _staging_textures: staging_textures,
            completed_id: Arc::new(AtomicU64::new(0)),
            inner: RefCell::new(Inner {
                next_id: 1,
                ..Default::default()
            }),
        }
    }

    /// Records the staging pass into the wgpu encoder, before any wgpu pass that samples the
    /// staging textures. On an import failure the error is logged and the previous staging content
    /// stays in use (the frame is repeated) instead of crashing the stream.
    ///
    /// # Safety
    /// `hardware_buffer` must be a valid AHardwareBuffer that stays valid until this call returns.
    pub unsafe fn record(&self, encoder: &mut CommandEncoder, hardware_buffer: *mut c_void) {
        if hardware_buffer.is_null() {
            return;
        }

        encoder.as_hal_mut::<api::Vulkan, _, _>(|encoder| {
            let command_buffer = encoder
                .expect("the wgpu command encoder is not a Vulkan encoder")
                .raw_handle();

            self.record_raw(command_buffer, hardware_buffer);
        });
    }

    /// Call after the queue submission that contains the recorded staging pass.
    pub fn after_submit(&self) {
        if let Some(id) = self.inner.borrow_mut().last_recorded_id.take() {
            let completed_id = Arc::clone(&self.completed_id);
            self.context.queue.on_submitted_work_done(move || {
                completed_id.fetch_max(id, Ordering::SeqCst);
            });
        }

        // runs the completion callbacks
        self.context.device.poll(Maintain::Poll);
        self.reclaim_completed_frames();
    }

    fn reclaim_completed_frames(&self) {
        let completed = self.completed_id.load(Ordering::SeqCst);
        let mut inner = self.inner.borrow_mut();

        let Inner { ycbcr, frames, .. } = &mut *inner;
        frames.retain(|frame| {
            if frame.id <= completed {
                unsafe { self.destroy_frame(ycbcr.as_ref(), frame) };
                false
            } else {
                true
            }
        });
    }

    unsafe fn destroy_frame(&self, ycbcr: Option<&YcbcrState>, frame: &FrameResources) {
        if let Some(ycbcr) = ycbcr {
            self.device
                .free_descriptor_sets(ycbcr.descriptor_pool, &[frame.descriptor_set])
                .ok();
        }
        self.device.destroy_image_view(frame.view, None);
        self.device.destroy_image(frame.image, None);
        self.device.free_memory(frame.memory, None);
    }

    unsafe fn destroy_ycbcr(&self, ycbcr: &YcbcrState) {
        // destroying the pool frees its sets
        self.device.destroy_descriptor_pool(ycbcr.descriptor_pool, None);
        self.device.destroy_pipeline(ycbcr.pipeline, None);
        self.device.destroy_pipeline_layout(ycbcr.pipeline_layout, None);
        self.device.destroy_descriptor_set_layout(ycbcr.set_layout, None);
        self.device.destroy_sampler(ycbcr.sampler, None);
        self.device.destroy_sampler_ycbcr_conversion(ycbcr.conversion, None);
    }

    unsafe fn create_ycbcr_state(&self, key: YcbcrKey) -> YcbcrState {
        let device = &self.device;

        let external_format = key.external_format != 0;
        let mut external_format_info =
            vk::ExternalFormatANDROID::default().external_format(key.external_format);
        let mut conversion_info = vk::SamplerYcbcrConversionCreateInfo::default()
            .format(vk::Format::from_raw(key.format))
            .ycbcr_model(vk::SamplerYcbcrModelConversion::from_raw(key.model))
            .ycbcr_range(vk::SamplerYcbcrRange::from_raw(key.range))
            .components(vk::ComponentMapping {
                r: vk::ComponentSwizzle::from_raw(key.components[0]),
                g: vk::ComponentSwizzle::from_raw(key.components[1]),
                b: vk::ComponentSwizzle::from_raw(key.components[2]),
                a: vk::ComponentSwizzle::from_raw(key.components[3]),
            })
            .x_chroma_offset(vk::ChromaLocation::from_raw(key.x_chroma_offset))
            .y_chroma_offset(vk::ChromaLocation::from_raw(key.y_chroma_offset))
            .chroma_filter(chroma_filter(key.linear_chroma_filter))
            .force_explicit_reconstruction(false);
        if external_format {
            conversion_info = conversion_info.push_next(&mut external_format_info);
        }
        let conversion = ck(
            device.create_sampler_ycbcr_conversion(&conversion_info, None),
            "vkCreateSamplerYcbcrConversion",
        );

        // The GLES external sampler was left at the texture defaults. Filters must match the
        // chroma filter of the conversion.
        let mut sampler_conversion_info =
            vk::SamplerYcbcrConversionInfo::default().conversion(conversion);
        let filter = chroma_filter(key.linear_chroma_filter);
        let sampler = ck(
            device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(filter)
                    .min_filter(filter)
                    .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .max_lod(0.0)
                    .unnormalized_coordinates(false)
                    .push_next(&mut sampler_conversion_info),
                None,
            ),
            "vkCreateSampler (ycbcr)",
        );

        let immutable_samplers = [sampler];
        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .immutable_samplers(&immutable_samplers)];
        let set_layout = ck(
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            ),
            "vkCreateDescriptorSetLayout",
        );

        let set_layouts = [set_layout];
        let push_constant_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX)
            .offset(0)
            .size(4)]; // view_idx
        let pipeline_layout = ck(
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&set_layouts)
                    .push_constant_ranges(&push_constant_ranges),
                None,
            ),
            "vkCreatePipelineLayout",
        );

        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(self.vertex_module)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(self.fragment_module)
                .name(c"main"),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_STRIP);
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let rasterization = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let blend_attachments = [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(false)
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let color_blend =
            vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachments);
        let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic_state =
            vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);

        let pipelines = device
            .create_graphics_pipelines(
                vk::PipelineCache::null(),
                &[vk::GraphicsPipelineCreateInfo::default()
                    .stages(&stages)
                    .vertex_input_state(&vertex_input)
                    .input_assembly_state(&input_assembly)
                    .viewport_state(&viewport_state)
                    .rasterization_state(&rasterization)
                    .multisample_state(&multisample)
                    .color_blend_state(&color_blend)
                    .dynamic_state(&dynamic_state)
                    .layout(pipeline_layout)
                    .render_pass(self.render_pass)
                    .subpass(0)],
                None,
            )
            .map_err(|(_, e)| e);
        let pipeline = ck(pipelines, "vkCreateGraphicsPipelines")[0];

        let pool_sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(MAX_FRAMES_IN_FLIGHT as u32 + 2)];
        let descriptor_pool = ck(
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
                    .max_sets(MAX_FRAMES_IN_FLIGHT as u32 + 2)
                    .pool_sizes(&pool_sizes),
                None,
            ),
            "vkCreateDescriptorPool",
        );

        YcbcrState {
            key,
            conversion,
            sampler,
            set_layout,
            pipeline_layout,
            pipeline,
            descriptor_pool,
        }
    }

    fn wait_for_gpu(&self) {
        self.context.device.poll(Maintain::Wait);
        self.reclaim_completed_frames();
    }

    unsafe fn record_raw(&self, command_buffer: vk::CommandBuffer, hardware_buffer: *mut c_void) {
        self.reclaim_completed_frames();

        // Bound the number of buffers kept alive; reaching this means the GPU is far behind.
        // submitted-but-unfinished frames are only known to wgpu, so wait on its fence.
        while self.inner.borrow().frames.len() >= MAX_FRAMES_IN_FLIGHT {
            self.wait_for_gpu();
            // after_submit() of the frames in flight already registered their callbacks
        }

        // 1. What did the decoder give us?
        let mut format_properties = vk::AndroidHardwareBufferFormatPropertiesANDROID::default();
        let mut buffer_properties =
            vk::AndroidHardwareBufferPropertiesANDROID::default().push_next(&mut format_properties);
        if let Err(e) = self.ahb_fn.get_android_hardware_buffer_properties(
            hardware_buffer.cast::<vk::AHardwareBuffer>().cast_const(),
            &mut buffer_properties,
        ) {
            staging_error!("vkGetAndroidHardwareBufferPropertiesANDROID failed: {e:?}, repeating frame");
            return;
        }
        let allocation_size = buffer_properties.allocation_size;
        let memory_type_bits = buffer_properties.memory_type_bits;

        let linear_chroma_filter = format_properties.format_features.contains(
            vk::FormatFeatureFlags::SAMPLED_IMAGE_YCBCR_CONVERSION_LINEAR_FILTER,
        );
        // The stream declares its own colour space (VUI: BT.709, full range; the NVENC conversion was
        // measured to match). GLES let the driver pick and fixed its limited-range guess in the
        // shader; here the declared values are used directly, so there is nothing to fix. Without a
        // declaration (HDR, limited range) the driver's suggestion is kept, as GLES did.
        let (model, range) = if self.declared_full_range_bt709 {
            (
                vk::SamplerYcbcrModelConversion::YCBCR_709,
                vk::SamplerYcbcrRange::ITU_FULL,
            )
        } else {
            (
                format_properties.suggested_ycbcr_model,
                format_properties.suggested_ycbcr_range,
            )
        };
        let key = YcbcrKey {
            format: format_properties.format.as_raw(),
            external_format: format_properties.external_format,
            model: model.as_raw(),
            range: range.as_raw(),
            x_chroma_offset: format_properties.suggested_x_chroma_offset.as_raw(),
            y_chroma_offset: format_properties.suggested_y_chroma_offset.as_raw(),
            components: [
                format_properties.sampler_ycbcr_conversion_components.r.as_raw(),
                format_properties.sampler_ycbcr_conversion_components.g.as_raw(),
                format_properties.sampler_ycbcr_conversion_components.b.as_raw(),
                format_properties.sampler_ycbcr_conversion_components.a.as_raw(),
            ],
            linear_chroma_filter,
        };

        // 2. (Re)build the conversion dependent objects if the format changed
        let key_changed = self
            .inner
            .borrow()
            .ycbcr
            .as_ref()
            .is_none_or(|state| state.key != key);
        if key_changed {
            alvr_common::info!(
                "Staging: decoder buffer format {key:?} (driver suggested model {:?} range {:?})",
                format_properties.suggested_ycbcr_model,
                format_properties.suggested_ycbcr_range
            );
            let (w, h, layers, format, usage, stride) = describe_buffer_full(hardware_buffer);
            alvr_common::diag!(
                "[staging] decoder buffer: {w}x{h} layers {layers} stride {stride} AHB format 0x{format:x} usage 0x{usage:x}; \
                 vk format {:?} external_format 0x{:x} features {:?}",
                format_properties.format,
                format_properties.external_format,
                format_properties.format_features
            );
            alvr_common::diag!(
                "[staging] YCbCr conversion: applied model {:?} range {:?} (driver suggested model {:?} range {:?}, forced={}); \
                 chroma offsets x {:?} y {:?}, linear chroma filter {}, components r {:?} g {:?} b {:?} a {:?}",
                vk::SamplerYcbcrModelConversion::from_raw(key.model),
                vk::SamplerYcbcrRange::from_raw(key.range),
                format_properties.suggested_ycbcr_model,
                format_properties.suggested_ycbcr_range,
                self.declared_full_range_bt709,
                format_properties.suggested_x_chroma_offset,
                format_properties.suggested_y_chroma_offset,
                key.linear_chroma_filter,
                format_properties.sampler_ycbcr_conversion_components.r,
                format_properties.sampler_ycbcr_conversion_components.g,
                format_properties.sampler_ycbcr_conversion_components.b,
                format_properties.sampler_ycbcr_conversion_components.a
            );

            // Frames in flight reference the old objects
            self.wait_for_gpu();
            let mut inner = self.inner.borrow_mut();
            if let Some(old) = inner.ycbcr.take() {
                self.destroy_ycbcr(&old);
            }
            inner.ycbcr = Some(self.create_ycbcr_state(key));
        }

        let (width, height) = describe_buffer(hardware_buffer);

        let ycbcr = {
            let inner = self.inner.borrow();
            let state = inner.ycbcr.as_ref().unwrap();
            YcbcrHandles {
                conversion: state.conversion,
                set_layout: state.set_layout,
                pipeline_layout: state.pipeline_layout,
                pipeline: state.pipeline,
                descriptor_pool: state.descriptor_pool,
            }
        };

        // 3. Import the buffer as a VkImage (what eglCreateImage + glEGLImageTargetTexture2D did)
        let mut external_memory_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::ANDROID_HARDWARE_BUFFER_ANDROID);
        let mut external_format_info =
            vk::ExternalFormatANDROID::default().external_format(key.external_format);
        let mut image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::from_raw(key.format))
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_memory_info);
        if key.external_format != 0 {
            image_info = image_info.push_next(&mut external_format_info);
        }
        let image = match self.device.create_image(&image_info, None) {
            Ok(image) => image,
            Err(e) => {
                staging_error!("vkCreateImage (AHardwareBuffer) failed: {e:?}, repeating frame");
                return;
            }
        };

        let memory_type_index = (0..self.memory_properties.memory_type_count)
            .filter(|i| memory_type_bits & (1 << i) != 0)
            .max_by_key(|&i| {
                self.memory_properties.memory_types[i as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            });
        let Some(memory_type_index) = memory_type_index else {
            staging_error!("No memory type for the AHardwareBuffer, repeating frame");
            self.device.destroy_image(image, None);
            return;
        };

        let mut dedicated_info = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut import_info = vk::ImportAndroidHardwareBufferInfoANDROID::default()
            .buffer(hardware_buffer.cast::<vk::AHardwareBuffer>());
        let memory = match self.device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(allocation_size)
                .memory_type_index(memory_type_index)
                .push_next(&mut dedicated_info)
                .push_next(&mut import_info),
            None,
        ) {
            Ok(memory) => memory,
            Err(e) => {
                staging_error!("vkAllocateMemory (AHardwareBuffer import) failed: {e:?}, repeating frame");
                self.device.destroy_image(image, None);
                return;
            }
        };
        if let Err(e) = self.device.bind_image_memory(image, memory, 0) {
            staging_error!("vkBindImageMemory failed: {e:?}, repeating frame");
            self.device.destroy_image(image, None);
            self.device.free_memory(memory, None);
            return;
        }

        let mut view_conversion_info =
            vk::SamplerYcbcrConversionInfo::default().conversion(ycbcr.conversion);
        let view = match self.device.create_image_view(
            &vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(vk::Format::from_raw(key.format))
                .subresource_range(color_range())
                .push_next(&mut view_conversion_info),
            None,
        ) {
            Ok(view) => view,
            Err(e) => {
                staging_error!("vkCreateImageView (AHardwareBuffer) failed: {e:?}, repeating frame");
                self.device.destroy_image(image, None);
                self.device.free_memory(memory, None);
                return;
            }
        };

        let set_layouts = [ycbcr.set_layout];
        let descriptor_set = match self.device.allocate_descriptor_sets(
            &vk::DescriptorSetAllocateInfo::default()
                .descriptor_pool(ycbcr.descriptor_pool)
                .set_layouts(&set_layouts),
        ) {
            Ok(sets) => sets[0],
            Err(e) => {
                staging_error!("vkAllocateDescriptorSets failed: {e:?}, repeating frame");
                self.device.destroy_image_view(view, None);
                self.device.destroy_image(image, None);
                self.device.free_memory(memory, None);
                return;
            }
        };
        // the sampler is immutable
        let image_infos = [vk::DescriptorImageInfo::default()
            .image_view(view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        self.device.update_descriptor_sets(
            &[vk::WriteDescriptorSet::default()
                .dst_set(descriptor_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&image_infos)],
            &[],
        );

        let mut inner = self.inner.borrow_mut();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.last_recorded_id = Some(id);
        alvr_common::diag::STAGING_IMPORTS.inc();
        inner.frames.push(FrameResources {
            id,
            image,
            memory,
            view,
            descriptor_set,
        });
        drop(inner);

        // 4. Record. Acquire the buffer from the producer (the decoder) keeping its contents.
        let acquire_barrier = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .dst_queue_family_index(self.queue_family_index)
            .image(image)
            .subresource_range(color_range());
        self.device.cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[acquire_barrier],
        );

        // The two eyes, same order and same source as the GLES loop over the framebuffers
        for (view_idx, framebuffer) in self.framebuffers.iter().enumerate() {
            self.device.cmd_begin_render_pass(
                command_buffer,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(self.render_pass)
                    .framebuffer(*framebuffer)
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent: self.extent,
                    }),
                vk::SubpassContents::INLINE,
            );
            self.device.cmd_bind_pipeline(
                command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                ycbcr.pipeline,
            );
            self.device.cmd_set_viewport(
                command_buffer,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: self.extent.width as f32,
                    height: self.extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            self.device.cmd_set_scissor(
                command_buffer,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: self.extent,
                }],
            );
            self.device.cmd_bind_descriptor_sets(
                command_buffer,
                vk::PipelineBindPoint::GRAPHICS,
                ycbcr.pipeline_layout,
                0,
                &[descriptor_set],
                &[],
            );
            self.device.cmd_push_constants(
                command_buffer,
                ycbcr.pipeline_layout,
                vk::ShaderStageFlags::VERTEX,
                0,
                &(view_idx as i32).to_le_bytes(),
            );
            self.device.cmd_draw(command_buffer, 4, 1, 0, 0);
            self.device.cmd_end_render_pass(command_buffer);
        }
    }
}

impl Drop for StagingRenderer {
    fn drop(&mut self) {
        // Everything recorded so far has to be done before destroying what it uses
        self.context.device.poll(Maintain::Wait);

        let inner = self.inner.take();
        unsafe {
            for frame in &inner.frames {
                self.destroy_frame(inner.ycbcr.as_ref(), frame);
            }
            if let Some(ycbcr) = &inner.ycbcr {
                self.destroy_ycbcr(ycbcr);
            }
            for framebuffer in &self.framebuffers {
                self.device.destroy_framebuffer(*framebuffer, None);
            }
            for view in &self.staging_views {
                self.device.destroy_image_view(*view, None);
            }
            self.device.destroy_render_pass(self.render_pass, None);
            self.device.destroy_shader_module(self.vertex_module, None);
            self.device.destroy_shader_module(self.fragment_module, None);
        }

        let _ = &self.instance;
        let _ = self.target_format;
    }
}

fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .base_mip_level(0)
        .level_count(1)
        .base_array_layer(0)
        .layer_count(1)
}

fn chroma_filter(linear: bool) -> vk::Filter {
    if linear {
        vk::Filter::LINEAR
    } else {
        vk::Filter::NEAREST
    }
}

fn wgpu_format_to_vk(format: wgpu::TextureFormat) -> vk::Format {
    match format {
        wgpu::TextureFormat::Rgba8UnormSrgb => vk::Format::R8G8B8A8_SRGB,
        wgpu::TextureFormat::Rgba8Unorm => vk::Format::R8G8B8A8_UNORM,
        wgpu::TextureFormat::Rgba16Float => vk::Format::R16G16B16A16_SFLOAT,
        _ => panic!("Unsupported staging format: {format:?}"),
    }
}
