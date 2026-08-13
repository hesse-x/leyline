#![allow(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::{
    collections::HashSet,
    ffi::{CStr, CString},
    io::Cursor,
    mem::size_of,
    time::Duration,
};

use ash::{Entry, vk, vk::Handle as _};
use gpu_allocator::{
    AllocationSizes, AllocatorDebugSettings, MemoryLocation,
    vulkan::{Allocation, AllocationCreateDesc, AllocationScheme, Allocator, AllocatorCreateDesc},
};

use crate::wayland::WaylandWindow;
use crate::{GfxInitError, LinearColor, PixelSize, RectangleInstance, RenderScene, select};

const FRAME_SLOTS: usize = 2;
const FENCE_TIMEOUT: Duration = Duration::from_secs(2);
const RECTANGLES_PER_SLOT: usize = 4096;
const MAX_RETIRED_GENERATIONS: usize = 3;

#[repr(C)]
#[derive(Clone, Copy)]
struct GpuRectangle {
    origin_px: [f32; 2],
    size_px: [f32; 2],
    color: [f32; 4],
}

struct InstanceBuffer {
    buffer: vk::Buffer,
    allocation: Allocation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RenderStatus {
    Rendered,
    Deferred,
    OutOfDate,
    Suboptimal,
}

struct FrameSlot {
    pool: vk::CommandPool,
    command: vk::CommandBuffer,
    available: vk::Semaphore,
    finished: vk::Semaphore,
    fence: vk::Fence,
}

#[derive(Clone, Copy, Debug)]
struct AcquiredImage {
    slot: usize,
    image_index: u32,
    suboptimal: bool,
}

struct RetiredSwapchain {
    swapchain: vk::SwapchainKHR,
    views: Vec<vk::ImageView>,
    fences: Vec<vk::Fence>,
}

pub(crate) struct VulkanRenderer {
    _entry: Entry,
    instance: ash::Instance,
    surface_loader: ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    physical: vk::PhysicalDevice,
    device: ash::Device,
    allocator: Option<Allocator>,
    instances: Option<InstanceBuffer>,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    queue: vk::Queue,
    queue_family: u32,
    swapchain_loader: ash::khr::swapchain::Device,
    swapchain: vk::SwapchainKHR,
    format: vk::Format,
    extent: vk::Extent2D,
    images: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
    image_fences: Vec<vk::Fence>,
    frames: Vec<FrameSlot>,
    current_frame: usize,
    acquired: Option<AcquiredImage>,
    retired: Vec<RetiredSwapchain>,
}

impl VulkanRenderer {
    #[allow(clippy::too_many_lines)]
    pub(crate) fn new(window: &WaylandWindow, target: PixelSize) -> Result<Self, GfxInitError> {
        // SAFETY: Entry owns the dynamically loaded Vulkan library until every child is dropped.
        let entry = unsafe { Entry::load() }.map_err(|error| {
            GfxInitError::Environment(format!(
                "cannot load libvulkan.so.1: {error}; install libvulkan1"
            ))
        })?;
        let loader_version = unsafe { entry.try_enumerate_instance_version() }
            .map_err(|error| {
                GfxInitError::Device(format!("cannot query Vulkan loader version: {error:?}"))
            })?
            .unwrap_or(vk::API_VERSION_1_0);
        if loader_version < vk::API_VERSION_1_3 {
            return Err(GfxInitError::Device("Vulkan loader 1.3 is required".into()));
        }
        ensure_instance_extensions(&entry)?;
        let name = CString::new("leyline").expect("static string");
        let app = vk::ApplicationInfo::default()
            .application_name(&name)
            .engine_name(&name)
            .api_version(vk::API_VERSION_1_3);
        let extensions = [
            ash::khr::surface::NAME.as_ptr(),
            ash::khr::wayland_surface::NAME.as_ptr(),
        ];
        let create = vk::InstanceCreateInfo::default()
            .application_info(&app)
            .enabled_extension_names(&extensions);
        // SAFETY: create only borrows local immutable data for the duration of the call.
        let instance = unsafe { entry.create_instance(&create, None) }.map_err(|error| {
            GfxInitError::Device(format!("cannot create Vulkan 1.3 instance: {error:?}"))
        })?;
        let surface_loader = ash::khr::surface::Instance::new(&entry, &instance);
        let wayland_loader = ash::khr::wayland_surface::Instance::new(&entry, &instance);
        let create = vk::WaylandSurfaceCreateInfoKHR::default()
            .display(window.display_ptr().cast())
            .surface(window.surface_ptr().cast());
        // SAFETY: Wayland display and surface are UI-thread-owned and outlive this renderer.
        let surface =
            unsafe { wayland_loader.create_wayland_surface(&create, None) }.map_err(|error| {
                unsafe { instance.destroy_instance(None) };
                GfxInitError::Device(format!("cannot create Vulkan Wayland surface: {error:?}"))
            })?;
        let (physical, queue_family) = match select_device(&instance, &surface_loader, surface) {
            Ok(value) => value,
            Err(error) => {
                unsafe {
                    surface_loader.destroy_surface(surface, None);
                    instance.destroy_instance(None);
                }
                return Err(error);
            }
        };
        let priorities = [1.0];
        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities)];
        let device_extensions = [ash::khr::swapchain::NAME.as_ptr()];
        let mut features = vk::PhysicalDeviceVulkan13Features::default()
            .dynamic_rendering(true)
            .synchronization2(true);
        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_info)
            .enabled_extension_names(&device_extensions)
            .push_next(&mut features);
        // SAFETY: selected device/queue were queried from this instance; features were verified.
        let device =
            unsafe { instance.create_device(physical, &device_info, None) }.map_err(|error| {
                unsafe {
                    surface_loader.destroy_surface(surface, None);
                    instance.destroy_instance(None);
                }
                GfxInitError::Device(format!("cannot create Vulkan device: {error:?}"))
            })?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let swapchain_loader = ash::khr::swapchain::Device::new(&instance, &device);
        let mut renderer = Self {
            _entry: entry,
            instance,
            surface_loader,
            surface,
            physical,
            device,
            allocator: None,
            instances: None,
            pipeline_layout: vk::PipelineLayout::null(),
            pipeline: vk::Pipeline::null(),
            queue,
            queue_family,
            swapchain_loader,
            swapchain: vk::SwapchainKHR::null(),
            format: vk::Format::UNDEFINED,
            extent: vk::Extent2D::default(),
            images: Vec::new(),
            views: Vec::new(),
            image_fences: Vec::new(),
            frames: Vec::new(),
            current_frame: 0,
            acquired: None,
            retired: Vec::new(),
        };
        renderer.allocator = Some(
            Allocator::new(&AllocatorCreateDesc {
                instance: renderer.instance.clone(),
                device: renderer.device.clone(),
                physical_device: renderer.physical,
                debug_settings: AllocatorDebugSettings::default(),
                buffer_device_address: false,
                allocation_sizes: AllocationSizes::default(),
            })
            .map_err(|error| GfxInitError::Device(format!("create GPU allocator: {error}")))?,
        );
        renderer.instances = Some(
            renderer
                .create_instance_buffer()
                .map_err(GfxInitError::Device)?,
        );
        renderer.frames = renderer.create_frames().map_err(GfxInitError::Device)?;
        if !renderer.recreate(target).map_err(GfxInitError::Device)? {
            return Err(GfxInitError::Device(
                "initial swapchain creation was unexpectedly deferred".into(),
            ));
        }
        Ok(renderer)
    }

    fn create_frames(&self) -> Result<Vec<FrameSlot>, String> {
        let mut result = Vec::new();
        for _ in 0..FRAME_SLOTS {
            let pool = unsafe {
                self.device.create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .queue_family_index(self.queue_family)
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                    None,
                )
            }
            .map_err(vk_error("create command pool"))?;
            let command = unsafe {
                self.device.allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
            }
            .map_err(vk_error("allocate command buffer"))?[0];
            let available = unsafe {
                self.device
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }
            .map_err(vk_error("create acquire semaphore"))?;
            let finished = unsafe {
                self.device
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }
            .map_err(vk_error("create render semaphore"))?;
            let fence = unsafe {
                self.device.create_fence(
                    &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                    None,
                )
            }
            .map_err(vk_error("create frame fence"))?;
            result.push(FrameSlot {
                pool,
                command,
                available,
                finished,
                fence,
            });
        }
        Ok(result)
    }

    fn create_instance_buffer(&mut self) -> Result<InstanceBuffer, String> {
        let bytes = (FRAME_SLOTS * RECTANGLES_PER_SLOT * size_of::<GpuRectangle>()) as u64;
        let buffer = unsafe {
            self.device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(bytes)
                    .usage(vk::BufferUsageFlags::VERTEX_BUFFER)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
        }
        .map_err(vk_error("create rectangle instance buffer"))?;
        let requirements = unsafe { self.device.get_buffer_memory_requirements(buffer) };
        let allocation = self
            .allocator
            .as_mut()
            .expect("allocator initialized")
            .allocate(&AllocationCreateDesc {
                name: "rectangle instances",
                requirements,
                location: MemoryLocation::CpuToGpu,
                linear: true,
                allocation_scheme: AllocationScheme::GpuAllocatorManaged,
            })
            .map_err(|error| format!("allocate rectangle instance buffer: {error}"))?;
        unsafe {
            self.device
                .bind_buffer_memory(buffer, allocation.memory(), allocation.offset())
        }
        .map_err(vk_error("bind rectangle instance buffer"))?;
        Ok(InstanceBuffer { buffer, allocation })
    }

    #[allow(clippy::too_many_lines)]
    fn create_pipeline(
        &self,
        format: vk::Format,
    ) -> Result<(vk::PipelineLayout, vk::Pipeline), String> {
        let vertex_code = ash::util::read_spv(&mut Cursor::new(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/rectangle.vert"
        ))))
        .map_err(|error| format!("read rectangle vertex shader: {error}"))?;
        let fragment_code = ash::util::read_spv(&mut Cursor::new(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/rectangle.frag"
        ))))
        .map_err(|error| format!("read rectangle fragment shader: {error}"))?;
        let vertex = unsafe {
            self.device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&vertex_code),
                None,
            )
        }
        .map_err(vk_error("create rectangle vertex shader"))?;
        let fragment = match unsafe {
            self.device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&fragment_code),
                None,
            )
        } {
            Ok(module) => module,
            Err(error) => {
                unsafe { self.device.destroy_shader_module(vertex, None) };
                return Err(format!(
                    "create rectangle fragment shader failed: {error:?}"
                ));
            }
        };
        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX)
            .size(8)];
        let layout = unsafe {
            self.device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default().push_constant_ranges(&push),
                None,
            )
        }
        .map_err(vk_error("create rectangle pipeline layout"))?;
        let entry = c"main";
        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vertex)
                .name(entry),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(fragment)
                .name(entry),
        ];
        let binding = [vk::VertexInputBindingDescription {
            binding: 0,
            stride: size_of::<GpuRectangle>() as u32,
            input_rate: vk::VertexInputRate::INSTANCE,
        }];
        let attributes = [
            vk::VertexInputAttributeDescription {
                location: 0,
                binding: 0,
                format: vk::Format::R32G32_SFLOAT,
                offset: 0,
            },
            vk::VertexInputAttributeDescription {
                location: 1,
                binding: 0,
                format: vk::Format::R32G32_SFLOAT,
                offset: 8,
            },
            vk::VertexInputAttributeDescription {
                location: 2,
                binding: 0,
                format: vk::Format::R32G32B32A32_SFLOAT,
                offset: 16,
            },
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_binding_descriptions(&binding)
            .vertex_attribute_descriptions(&attributes);
        let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewport = vk::PipelineViewportStateCreateInfo::default()
            .viewport_count(1)
            .scissor_count(1);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let blend_attachment = [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachment);
        let dynamics = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
        let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamics);
        let mut rendering = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(std::slice::from_ref(&format));
        let info = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&assembly)
            .viewport_state(&viewport)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .color_blend_state(&blend)
            .dynamic_state(&dynamic)
            .layout(layout)
            .push_next(&mut rendering);
        let pipeline = unsafe {
            self.device
                .create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
        }
        .map_err(|(_, error)| format!("create rectangle graphics pipeline failed: {error:?}"))?[0];
        unsafe {
            self.device.destroy_shader_module(fragment, None);
            self.device.destroy_shader_module(vertex, None);
        }
        Ok((layout, pipeline))
    }

    fn destroy_pipeline(&mut self) {
        unsafe {
            if self.pipeline != vk::Pipeline::null() {
                self.device.destroy_pipeline(self.pipeline, None);
            }
            if self.pipeline_layout != vk::PipelineLayout::null() {
                self.device
                    .destroy_pipeline_layout(self.pipeline_layout, None);
            }
        }
        self.pipeline = vk::Pipeline::null();
        self.pipeline_layout = vk::PipelineLayout::null();
    }

    pub(crate) fn extent(&self) -> PixelSize {
        PixelSize {
            width: self.extent.width,
            height: self.extent.height,
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn recreate(&mut self, target: PixelSize) -> Result<bool, String> {
        if self.acquired.is_some() {
            return Err("cannot recreate the swapchain while an acquired image is pending".into());
        }
        self.collect_retired()?;
        if self.retired.len() >= MAX_RETIRED_GENERATIONS {
            return Ok(false);
        }
        let capabilities = unsafe {
            self.surface_loader
                .get_physical_device_surface_capabilities(self.physical, self.surface)
        }
        .map_err(vk_error("query surface capabilities"))?;
        if !capabilities
            .supported_usage_flags
            .contains(vk::ImageUsageFlags::COLOR_ATTACHMENT)
        {
            return Err("Vulkan surface does not support color attachment usage".into());
        }
        let formats = unsafe {
            self.surface_loader
                .get_physical_device_surface_formats(self.physical, self.surface)
        }
        .map_err(vk_error("query surface formats"))?;
        let modes = unsafe {
            self.surface_loader
                .get_physical_device_surface_present_modes(self.physical, self.surface)
        }
        .map_err(vk_error("query present modes"))?;
        let format = select::surface_format(&formats)
            .ok_or("Vulkan surface exposes no supported 8-bit sRGB format")?;
        let mode = select::present_mode(&modes)
            .ok_or("Vulkan surface exposes neither MAILBOX nor FIFO")?;
        let alpha = select::composite_alpha(capabilities.supported_composite_alpha)
            .ok_or("Vulkan surface exposes no composite alpha mode")?;
        let extent =
            select::extent(capabilities, target).ok_or("Vulkan surface has zero extent")?;
        let create = vk::SwapchainCreateInfoKHR::default()
            .surface(self.surface)
            .min_image_count(select::image_count(
                capabilities.min_image_count,
                capabilities.max_image_count,
            ))
            .image_format(format.format)
            .image_color_space(format.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(capabilities.current_transform)
            .composite_alpha(alpha)
            .present_mode(mode)
            .clipped(true)
            .old_swapchain(self.swapchain);
        let new_swapchain = unsafe { self.swapchain_loader.create_swapchain(&create, None) }
            .map_err(vk_error("create swapchain"))?;
        let images = unsafe { self.swapchain_loader.get_swapchain_images(new_swapchain) }
            .map_err(vk_error("get swapchain images"))?;
        let mut views = Vec::with_capacity(images.len());
        for image in &images {
            let view = unsafe {
                self.device.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(*image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(format.format)
                        .subresource_range(vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            level_count: 1,
                            layer_count: 1,
                            ..Default::default()
                        }),
                    None,
                )
            }
            .map_err(vk_error("create swapchain image view"))?;
            views.push(view);
        }
        if self.swapchain != vk::SwapchainKHR::null() {
            let mut fences: Vec<_> = self
                .image_fences
                .iter()
                .copied()
                .filter(|fence| *fence != vk::Fence::null())
                .collect();
            fences.sort_by_key(|fence| fence.as_raw());
            fences.dedup();
            self.retired.push(RetiredSwapchain {
                swapchain: self.swapchain,
                views: std::mem::take(&mut self.views),
                fences,
            });
        }
        self.swapchain = new_swapchain;
        self.images = images;
        self.views = views;
        self.image_fences = vec![vk::Fence::null(); self.images.len()];
        if self.format != format.format || self.pipeline == vk::Pipeline::null() {
            self.destroy_pipeline();
            (self.pipeline_layout, self.pipeline) = self.create_pipeline(format.format)?;
        }
        self.format = format.format;
        self.extent = extent;
        tracing::info!(
            category = "renderer",
            width = extent.width,
            height = extent.height,
            images = self.images.len(),
            present = mode.as_raw(),
            "Vulkan swapchain ready"
        );
        Ok(true)
    }

    pub(crate) fn render(&mut self, scene: &RenderScene<'_>) -> Result<RenderStatus, String> {
        let acquired = if let Some(acquired) = self.acquired {
            acquired
        } else {
            let slot_index = self.current_frame;
            let slot = &self.frames[slot_index];
            match unsafe { self.device.get_fence_status(slot.fence) } {
                Ok(true) => {}
                Ok(false) => return Ok(RenderStatus::Deferred),
                Err(error) => return Err(format!("query frame fence failed: {error:?}")),
            }
            let result = unsafe {
                self.swapchain_loader.acquire_next_image(
                    self.swapchain,
                    0,
                    slot.available,
                    vk::Fence::null(),
                )
            };
            let (image_index, suboptimal) = match result {
                Ok(value) => value,
                Err(vk::Result::NOT_READY | vk::Result::TIMEOUT) => {
                    return Ok(RenderStatus::Deferred);
                }
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => return Ok(RenderStatus::OutOfDate),
                Err(error) => return Err(format!("acquire swapchain image failed: {error:?}")),
            };
            let acquired = AcquiredImage {
                slot: slot_index,
                image_index,
                suboptimal,
            };
            self.acquired = Some(acquired);
            acquired
        };
        let slot_fence = self.frames[acquired.slot].fence;
        let slot_pool = self.frames[acquired.slot].pool;
        let slot_command = self.frames[acquired.slot].command;
        let slot_available = self.frames[acquired.slot].available;
        let slot_finished = self.frames[acquired.slot].finished;
        let image_index = acquired.image_index;
        let old_fence = self.image_fences[image_index as usize];
        if old_fence != vk::Fence::null()
            && !unsafe { self.device.get_fence_status(old_fence) }
                .map_err(vk_error("query image fence"))?
        {
            // The acquire semaphore and image remain owned by this slot. A later timer retry
            // resumes here without acquiring again, so the UI thread remains responsive.
            return Ok(RenderStatus::Deferred);
        }
        unsafe {
            self.device
                .reset_fences(&[slot_fence])
                .map_err(vk_error("reset frame fence"))?;
            self.device
                .reset_command_pool(slot_pool, vk::CommandPoolResetFlags::empty())
                .map_err(vk_error("reset command pool"))?;
        }
        self.upload_rectangles(acquired.slot, scene.rectangles)?;
        self.record(slot_command, acquired.slot, image_index as usize, scene)?;
        let wait = [vk::SemaphoreSubmitInfo::default()
            .semaphore(slot_available)
            .stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)];
        let commands = [vk::CommandBufferSubmitInfo::default().command_buffer(slot_command)];
        let signal = [vk::SemaphoreSubmitInfo::default()
            .semaphore(slot_finished)
            .stage_mask(vk::PipelineStageFlags2::ALL_GRAPHICS)];
        let submits = [vk::SubmitInfo2::default()
            .wait_semaphore_infos(&wait)
            .command_buffer_infos(&commands)
            .signal_semaphore_infos(&signal)];
        unsafe { self.device.queue_submit2(self.queue, &submits, slot_fence) }
            .map_err(vk_error("submit frame"))?;
        self.acquired = None;
        self.image_fences[image_index as usize] = slot_fence;
        let waits = [slot_finished];
        let swapchains = [self.swapchain];
        let indices = [image_index];
        let present = vk::PresentInfoKHR::default()
            .wait_semaphores(&waits)
            .swapchains(&swapchains)
            .image_indices(&indices);
        let present_suboptimal =
            match unsafe { self.swapchain_loader.queue_present(self.queue, &present) } {
                Ok(value) => value,
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                    self.current_frame = (acquired.slot + 1) % self.frames.len();
                    return Ok(RenderStatus::OutOfDate);
                }
                Err(error) => return Err(format!("present frame failed: {error:?}")),
            };
        self.current_frame = (acquired.slot + 1) % self.frames.len();
        if acquired.suboptimal || present_suboptimal {
            tracing::debug!(category = "renderer", "swapchain is suboptimal");
            return Ok(RenderStatus::Suboptimal);
        }
        Ok(RenderStatus::Rendered)
    }

    fn record(
        &self,
        command: vk::CommandBuffer,
        slot: usize,
        image_index: usize,
        scene: &RenderScene<'_>,
    ) -> Result<(), String> {
        unsafe {
            self.device.begin_command_buffer(
                command,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
        }
        .map_err(vk_error("begin command buffer"))?;
        transition(
            &self.device,
            command,
            self.images[image_index],
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::PipelineStageFlags2::NONE,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::empty(),
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        );
        let clear = color(scene.clear);
        let attachment = vk::RenderingAttachmentInfo::default()
            .image_view(self.views[image_index])
            .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .load_op(vk::AttachmentLoadOp::CLEAR)
            .store_op(vk::AttachmentStoreOp::STORE)
            .clear_value(vk::ClearValue { color: clear });
        unsafe {
            self.device.cmd_begin_rendering(
                command,
                &vk::RenderingInfo::default()
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D::default(),
                        extent: self.extent,
                    })
                    .layer_count(1)
                    .color_attachments(std::slice::from_ref(&attachment)),
            );
            self.device
                .cmd_bind_pipeline(command, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            let viewport = vk::Viewport {
                width: self.extent.width as f32,
                height: self.extent.height as f32,
                max_depth: 1.0,
                ..Default::default()
            };
            let scissor = vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: self.extent,
            };
            self.device.cmd_set_viewport(command, 0, &[viewport]);
            self.device.cmd_set_scissor(command, 0, &[scissor]);
            let instance_offset = (slot * RECTANGLES_PER_SLOT * size_of::<GpuRectangle>()) as u64;
            self.device.cmd_bind_vertex_buffers(
                command,
                0,
                &[self.instances.as_ref().expect("instance buffer").buffer],
                &[instance_offset],
            );
            let viewport_size = [scene.viewport.width as f32, scene.viewport.height as f32];
            self.device.cmd_push_constants(
                command,
                self.pipeline_layout,
                vk::ShaderStageFlags::VERTEX,
                0,
                as_bytes(&viewport_size),
            );
            self.device
                .cmd_draw(command, 6, scene.rectangles.len() as u32, 0, 0);
        }
        unsafe {
            self.device.cmd_end_rendering(command);
        }
        transition(
            &self.device,
            command,
            self.images[image_index],
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::PRESENT_SRC_KHR,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::PipelineStageFlags2::NONE,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::AccessFlags2::empty(),
        );
        unsafe { self.device.end_command_buffer(command) }.map_err(vk_error("end command buffer"))
    }

    fn upload_rectangles(
        &mut self,
        slot: usize,
        rectangles: &[RectangleInstance],
    ) -> Result<(), String> {
        if rectangles.len() > RECTANGLES_PER_SLOT {
            return Err(format!(
                "rectangle count {} exceeds per-frame capacity {RECTANGLES_PER_SLOT}",
                rectangles.len()
            ));
        }
        let offset = slot * RECTANGLES_PER_SLOT * size_of::<GpuRectangle>();
        let allocation = &mut self.instances.as_mut().expect("instance buffer").allocation;
        let mapped = allocation
            .mapped_slice_mut()
            .ok_or("rectangle instance memory is not mapped")?;
        for (index, rectangle) in rectangles.iter().enumerate() {
            let gpu = GpuRectangle {
                origin_px: rectangle.origin_px,
                size_px: rectangle.size_px,
                color: [
                    rectangle.color.red,
                    rectangle.color.green,
                    rectangle.color.blue,
                    rectangle.color.alpha,
                ],
            };
            let start = offset + index * size_of::<GpuRectangle>();
            mapped[start..start + size_of::<GpuRectangle>()].copy_from_slice(as_bytes(&gpu));
        }
        Ok(())
    }

    fn wait_frames(&self) -> Result<(), String> {
        let fences: Vec<_> = self.frames.iter().map(|slot| slot.fence).collect();
        if fences.is_empty() {
            return Ok(());
        }
        unsafe {
            self.device
                .wait_for_fences(&fences, true, FENCE_TIMEOUT.as_nanos() as u64)
        }
        .map_err(vk_error("wait for frame retirement"))
    }

    fn collect_retired(&mut self) -> Result<(), String> {
        let mut index = 0;
        while index < self.retired.len() {
            let ready = self.retired[index]
                .fences
                .iter()
                .try_fold(true, |ready, fence| {
                    unsafe { self.device.get_fence_status(*fence) }
                        .map(|signaled| ready && signaled)
                        .map_err(vk_error("query retired swapchain fence"))
                })?;
            if ready {
                let generation = self.retired.swap_remove(index);
                for view in generation.views {
                    unsafe { self.device.destroy_image_view(view, None) };
                }
                unsafe {
                    self.swapchain_loader
                        .destroy_swapchain(generation.swapchain, None);
                };
            } else {
                index += 1;
            }
        }
        Ok(())
    }
    fn destroy_swapchain_views(&mut self) {
        for view in self.views.drain(..) {
            unsafe { self.device.destroy_image_view(view, None) };
        }
    }
}

impl Drop for VulkanRenderer {
    fn drop(&mut self) {
        let _ = self.wait_frames();
        for generation in self.retired.drain(..) {
            for view in generation.views {
                unsafe { self.device.destroy_image_view(view, None) };
            }
            unsafe {
                self.swapchain_loader
                    .destroy_swapchain(generation.swapchain, None);
            };
        }
        self.destroy_swapchain_views();
        self.destroy_pipeline();
        if let Some(instances) = self.instances.take() {
            unsafe { self.device.destroy_buffer(instances.buffer, None) };
            if let Some(allocator) = self.allocator.as_mut() {
                let _ = allocator.free(instances.allocation);
            }
        }
        self.allocator.take();
        unsafe {
            if self.swapchain != vk::SwapchainKHR::null() {
                self.swapchain_loader
                    .destroy_swapchain(self.swapchain, None);
            }
            for slot in self.frames.drain(..) {
                self.device.destroy_fence(slot.fence, None);
                self.device.destroy_semaphore(slot.finished, None);
                self.device.destroy_semaphore(slot.available, None);
                self.device.destroy_command_pool(slot.pool, None);
            }
            self.device.destroy_device(None);
            self.surface_loader.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
    }
}

fn as_bytes<T: Sized>(value: &T) -> &[u8] {
    // SAFETY: the returned bytes borrow a fully initialized plain-data value for this call only.
    unsafe { std::slice::from_raw_parts(std::ptr::from_ref(value).cast(), size_of::<T>()) }
}

fn ensure_instance_extensions(entry: &Entry) -> Result<(), GfxInitError> {
    let properties =
        unsafe { entry.enumerate_instance_extension_properties(None) }.map_err(|error| {
            GfxInitError::Device(format!("cannot enumerate Vulkan extensions: {error:?}"))
        })?;
    let names: HashSet<_> = properties
        .iter()
        .map(|item| unsafe { CStr::from_ptr(item.extension_name.as_ptr()) }.to_bytes())
        .collect();
    for required in [
        ash::khr::surface::NAME.to_bytes(),
        ash::khr::wayland_surface::NAME.to_bytes(),
    ] {
        if !names.contains(required) {
            return Err(GfxInitError::Device(format!(
                "Vulkan ICD is missing {}",
                String::from_utf8_lossy(required)
            )));
        }
    }
    Ok(())
}

fn select_device(
    instance: &ash::Instance,
    surface_loader: &ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
) -> Result<(vk::PhysicalDevice, u32), GfxInitError> {
    let devices = unsafe { instance.enumerate_physical_devices() }.map_err(|error| {
        GfxInitError::Device(format!("cannot enumerate Vulkan devices: {error:?}"))
    })?;
    let mut rejected = Vec::new();
    for device in devices {
        let properties = unsafe { instance.get_physical_device_properties(device) };
        let name = unsafe { CStr::from_ptr(properties.device_name.as_ptr()) }.to_string_lossy();
        if properties.device_type == vk::PhysicalDeviceType::CPU
            || properties.api_version < vk::API_VERSION_1_3
        {
            rejected.push(format!("{name}: software or below Vulkan 1.3"));
            continue;
        }
        let mut features = vk::PhysicalDeviceVulkan13Features::default();
        unsafe {
            instance.get_physical_device_features2(
                device,
                &mut vk::PhysicalDeviceFeatures2::default().push_next(&mut features),
            );
        }
        if features.dynamic_rendering == 0 || features.synchronization2 == 0 {
            rejected.push(format!("{name}: missing dynamicRendering/synchronization2"));
            continue;
        }
        for (index, family) in
            unsafe { instance.get_physical_device_queue_family_properties(device) }
                .iter()
                .enumerate()
        {
            let Ok(index) = u32::try_from(index) else {
                continue;
            };
            let present = unsafe {
                surface_loader.get_physical_device_surface_support(device, index, surface)
            }
            .unwrap_or(false);
            if family.queue_count > 0
                && family.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                && present
            {
                return Ok((device, index));
            }
        }
        rejected.push(format!("{name}: no combined graphics/present queue"));
    }
    Err(GfxInitError::Device(format!(
        "no suitable hardware Vulkan device: {}",
        rejected.join("; ")
    )))
}

#[allow(clippy::too_many_arguments)]
fn transition(
    device: &ash::Device,
    command: vk::CommandBuffer,
    image: vk::Image,
    old: vk::ImageLayout,
    new: vk::ImageLayout,
    source_stage: vk::PipelineStageFlags2,
    destination_stage: vk::PipelineStageFlags2,
    source_access: vk::AccessFlags2,
    destination_access: vk::AccessFlags2,
) {
    let barrier = vk::ImageMemoryBarrier2::default()
        .src_stage_mask(source_stage)
        .src_access_mask(source_access)
        .dst_stage_mask(destination_stage)
        .dst_access_mask(destination_access)
        .old_layout(old)
        .new_layout(new)
        .image(image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            level_count: 1,
            layer_count: 1,
            ..Default::default()
        });
    unsafe {
        device.cmd_pipeline_barrier2(
            command,
            &vk::DependencyInfo::default().image_memory_barriers(std::slice::from_ref(&barrier)),
        );
    }
}

fn color(value: LinearColor) -> vk::ClearColorValue {
    vk::ClearColorValue {
        float32: [value.red, value.green, value.blue, value.alpha],
    }
}
fn vk_error(context: &'static str) -> impl FnOnce(vk::Result) -> String {
    move |error| format!("{context}: {error:?}")
}

#[cfg(test)]
mod state_tests {
    use super::*;

    #[test]
    fn acquired_image_retains_slot_image_and_suboptimal_state() {
        let acquired = AcquiredImage {
            slot: 1,
            image_index: 7,
            suboptimal: true,
        };
        assert_eq!(acquired.slot, 1);
        assert_eq!(acquired.image_index, 7);
        assert!(acquired.suboptimal);
    }

    #[test]
    fn retired_generation_limit_is_strictly_bounded() {
        let can_recreate = |retired: usize| retired < MAX_RETIRED_GENERATIONS;
        assert!(can_recreate(MAX_RETIRED_GENERATIONS - 1));
        assert!(!can_recreate(MAX_RETIRED_GENERATIONS));
    }

    #[test]
    fn instance_capacity_matches_independent_frame_regions() {
        let region = RECTANGLES_PER_SLOT * size_of::<GpuRectangle>();
        assert_eq!(
            region * FRAME_SLOTS,
            FRAME_SLOTS * RECTANGLES_PER_SLOT * size_of::<GpuRectangle>()
        );
        assert!(region > 0);
    }
}
