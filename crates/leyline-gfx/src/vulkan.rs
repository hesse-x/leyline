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

use crate::atlas::{ATLAS_PAGE_SIZE, AtlasRect, MAX_ATLAS_PAGES};
use crate::wayland::WaylandWindow;
use crate::{
    GfxInitError, GlyphInstance, LinearColor, PixelSize, RectangleInstance, RenderScene, select,
};
use leyline_text::GlyphAsset;

const FRAME_SLOTS: usize = 2;
const FENCE_TIMEOUT: Duration = Duration::from_secs(2);
const RECTANGLES_PER_SLOT: usize = 524_304;
const GLYPHS_PER_SLOT: usize = 524_288;
const GLYPH_STAGING_PER_SLOT: usize = 32 * 1024 * 1024;
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

struct AtlasPage {
    image: vk::Image,
    view: vk::ImageView,
    allocation: Allocation,
    initialized: bool,
}
struct GlyphResources {
    instances: InstanceBuffer,
    staging: InstanceBuffer,
    pages: Vec<AtlasPage>,
    sampler: vk::Sampler,
    descriptor_pool: vk::DescriptorPool,
    descriptor_layout: vk::DescriptorSetLayout,
    descriptors: Vec<vk::DescriptorSet>,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    pending: Vec<(AtlasRect, GlyphAsset)>,
    pending_repack: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GpuGlyph {
    origin_px: [f32; 2],
    size_px: [f32; 2],
    uv_min: [f32; 2],
    uv_max: [f32; 2],
    color: [f32; 4],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RenderStatus {
    Rendered,
    Deferred,
    OutOfDate,
    SubmittedOutOfDate,
    Suboptimal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RendererOperation {
    Upload,
    Fence,
    Acquire,
    Recreate,
    Submit,
    Present,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RendererFault {
    #[error("swapchain is out of date")]
    OutOfDate,
    #[error("swapchain is suboptimal")]
    Suboptimal,
    #[error("renderer is not ready")]
    NotReady,
    #[error("surface was lost during {operation:?}")]
    SurfaceLost { operation: RendererOperation },
    #[error("device was lost during {operation:?}")]
    DeviceLost { operation: RendererOperation },
    #[error("host memory was exhausted during {operation:?}")]
    OutOfHostMemory { operation: RendererOperation },
    #[error("device memory was exhausted during {operation:?}")]
    OutOfDeviceMemory { operation: RendererOperation },
    #[error("Vulkan operation timed out during {operation:?}")]
    Timeout { operation: RendererOperation },
    #[error("Vulkan {operation:?} failed with code {code}")]
    Fatal {
        operation: RendererOperation,
        code: i32,
    },
    #[error("renderer invariant failed: {0}")]
    Invariant(String),
}

#[derive(Default)]
struct RendererHealth {
    poisoned: Option<RendererFault>,
}

impl RendererHealth {
    fn guard(&self) -> Result<(), RendererFault> {
        self.poisoned.clone().map_or(Ok(()), Err)
    }

    fn observe<T>(&mut self, result: Result<T, RendererFault>) -> Result<T, RendererFault> {
        if let Err(fault @ RendererFault::DeviceLost { .. }) = &result
            && self.poisoned.is_none()
        {
            self.poisoned = Some(fault.clone());
        }
        result
    }

    const fn is_poisoned(&self) -> bool {
        self.poisoned.is_some()
    }
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
    glyph: Option<GlyphResources>,
    queue: vk::Queue,
    queue_family: u32,
    swapchain_loader: ash::khr::swapchain::Device,
    swapchain: vk::SwapchainKHR,
    composite_alpha: vk::CompositeAlphaFlagsKHR,
    format: vk::Format,
    extent: vk::Extent2D,
    images: Vec<vk::Image>,
    views: Vec<vk::ImageView>,
    image_fences: Vec<vk::Fence>,
    frames: Vec<FrameSlot>,
    current_frame: usize,
    acquired: Option<AcquiredImage>,
    retired: Vec<RetiredSwapchain>,
    health: RendererHealth,
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
            glyph: None,
            queue,
            queue_family,
            swapchain_loader,
            swapchain: vk::SwapchainKHR::null(),
            composite_alpha: vk::CompositeAlphaFlagsKHR::OPAQUE,
            format: vk::Format::UNDEFINED,
            extent: vk::Extent2D::default(),
            images: Vec::new(),
            views: Vec::new(),
            image_fences: Vec::new(),
            frames: Vec::new(),
            current_frame: 0,
            acquired: None,
            retired: Vec::new(),
            health: RendererHealth::default(),
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
        if !renderer
            .recreate(target)
            .map_err(|error| GfxInitError::Device(error.to_string()))?
        {
            return Err(GfxInitError::Device(
                "initial swapchain creation was unexpectedly deferred".into(),
            ));
        }
        renderer.glyph = Some(
            renderer
                .create_glyph_resources()
                .map_err(GfxInitError::Device)?,
        );
        Ok(renderer)
    }

    pub(crate) fn upload_glyphs(
        &mut self,
        uploads: &[(AtlasRect, GlyphAsset)],
        replace_pending: bool,
    ) -> Result<(), RendererFault> {
        self.health.guard()?;
        let retained_bytes = if replace_pending {
            0
        } else {
            self.glyph
                .as_ref()
                .expect("glyph resources")
                .pending
                .iter()
                .try_fold(0usize, |total, (_, asset)| {
                    total.checked_add(asset.bitmap.coverage.len())
                })
                .ok_or_else(|| RendererFault::Invariant("glyph upload size overflow".into()))?
        };
        let bytes = uploads
            .iter()
            .try_fold(retained_bytes, |total, (_, asset)| {
                total.checked_add(asset.bitmap.coverage.len())
            })
            .ok_or_else(|| RendererFault::Invariant("glyph upload size overflow".into()))?;
        if bytes > GLYPH_STAGING_PER_SLOT {
            return Err(RendererFault::Invariant(format!(
                "glyph upload byte count {bytes} exceeds its bounded staging partition"
            )));
        }
        let glyphs = self.glyph.as_mut().expect("glyph resources");
        if replace_pending {
            glyphs.pending.clear();
            glyphs.pending_repack = true;
        }
        glyphs.pending.extend_from_slice(uploads);
        Ok(())
    }

    pub(crate) fn discard_pending_glyphs(&mut self) {
        if let Some(glyphs) = self.glyph.as_mut() {
            glyphs.pending.clear();
            glyphs.pending_repack = false;
        }
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

    fn create_buffer(
        &mut self,
        bytes: u64,
        usage: vk::BufferUsageFlags,
        name: &'static str,
    ) -> Result<InstanceBuffer, String> {
        let buffer = unsafe {
            self.device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(bytes)
                    .usage(usage)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
        }
        .map_err(vk_error("create buffer"))?;
        let requirements = unsafe { self.device.get_buffer_memory_requirements(buffer) };
        let allocation = self
            .allocator
            .as_mut()
            .expect("allocator initialized")
            .allocate(&AllocationCreateDesc {
                name,
                requirements,
                location: MemoryLocation::CpuToGpu,
                linear: true,
                allocation_scheme: AllocationScheme::GpuAllocatorManaged,
            })
            .map_err(|error| format!("allocate {name}: {error}"))?;
        unsafe {
            self.device
                .bind_buffer_memory(buffer, allocation.memory(), allocation.offset())
        }
        .map_err(vk_error("bind buffer memory"))?;
        Ok(InstanceBuffer { buffer, allocation })
    }

    #[allow(clippy::too_many_lines)]
    fn create_glyph_resources(&mut self) -> Result<GlyphResources, String> {
        let instances = self.create_buffer(
            (FRAME_SLOTS * GLYPHS_PER_SLOT * size_of::<GpuGlyph>()) as u64,
            vk::BufferUsageFlags::VERTEX_BUFFER,
            "glyph instances",
        )?;
        let staging = self.create_buffer(
            (FRAME_SLOTS * GLYPH_STAGING_PER_SLOT) as u64,
            vk::BufferUsageFlags::TRANSFER_SRC,
            "glyph staging",
        )?;
        let properties = unsafe { self.instance.get_physical_device_properties(self.physical) };
        if properties.limits.max_image_dimension2_d < u32::from(ATLAS_PAGE_SIZE) {
            return Err("device cannot create 2048x2048 glyph atlas".into());
        }
        let mut pages = Vec::new();
        for _ in 0..MAX_ATLAS_PAGES {
            let image = unsafe {
                self.device.create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(vk::Format::R8_UNORM)
                        .extent(vk::Extent3D {
                            width: u32::from(ATLAS_PAGE_SIZE),
                            height: u32::from(ATLAS_PAGE_SIZE),
                            depth: 1,
                        })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::OPTIMAL)
                        .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE)
                        .initial_layout(vk::ImageLayout::UNDEFINED),
                    None,
                )
            }
            .map_err(vk_error("create glyph atlas image"))?;
            let requirements = unsafe { self.device.get_image_memory_requirements(image) };
            let allocation = self
                .allocator
                .as_mut()
                .expect("allocator initialized")
                .allocate(&AllocationCreateDesc {
                    name: "glyph atlas",
                    requirements,
                    location: MemoryLocation::GpuOnly,
                    linear: false,
                    allocation_scheme: AllocationScheme::GpuAllocatorManaged,
                })
                .map_err(|error| format!("allocate glyph atlas: {error}"))?;
            unsafe {
                self.device
                    .bind_image_memory(image, allocation.memory(), allocation.offset())
            }
            .map_err(vk_error("bind glyph atlas image"))?;
            let view = unsafe {
                self.device.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(vk::Format::R8_UNORM)
                        .subresource_range(vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            level_count: 1,
                            layer_count: 1,
                            ..Default::default()
                        }),
                    None,
                )
            }
            .map_err(vk_error("create glyph atlas view"))?;
            pages.push(AtlasPage {
                image,
                view,
                allocation,
                initialized: false,
            });
        }
        let sampler = unsafe {
            self.device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                None,
            )
        }
        .map_err(vk_error("create glyph sampler"))?;
        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
        let descriptor_layout = unsafe {
            self.device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }
        .map_err(vk_error("create glyph descriptor layout"))?;
        let sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
            descriptor_count: MAX_ATLAS_PAGES as u32,
        }];
        let descriptor_pool = unsafe {
            self.device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(MAX_ATLAS_PAGES as u32)
                    .pool_sizes(&sizes),
                None,
            )
        }
        .map_err(vk_error("create glyph descriptor pool"))?;
        let layouts = vec![descriptor_layout; MAX_ATLAS_PAGES];
        let descriptors = unsafe {
            self.device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(descriptor_pool)
                    .set_layouts(&layouts),
            )
        }
        .map_err(vk_error("allocate glyph descriptors"))?;
        for (descriptor, page) in descriptors.iter().zip(&pages) {
            let image = [vk::DescriptorImageInfo::default()
                .sampler(sampler)
                .image_view(page.view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let write = [vk::WriteDescriptorSet::default()
                .dst_set(*descriptor)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&image)];
            unsafe { self.device.update_descriptor_sets(&write, &[]) };
        }
        let (pipeline_layout, pipeline) =
            self.create_glyph_pipeline(self.format, descriptor_layout)?;
        Ok(GlyphResources {
            instances,
            staging,
            pages,
            sampler,
            descriptor_pool,
            descriptor_layout,
            descriptors,
            pipeline_layout,
            pipeline,
            pending: Vec::new(),
            pending_repack: false,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn create_glyph_pipeline(
        &self,
        format: vk::Format,
        descriptor_layout: vk::DescriptorSetLayout,
    ) -> Result<(vk::PipelineLayout, vk::Pipeline), String> {
        let vertex_code = ash::util::read_spv(&mut Cursor::new(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/glyph.vert"
        ))))
        .map_err(|error| format!("read glyph vertex shader: {error}"))?;
        let fragment_code = ash::util::read_spv(&mut Cursor::new(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/glyph.frag"
        ))))
        .map_err(|error| format!("read glyph fragment shader: {error}"))?;
        let vertex = unsafe {
            self.device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&vertex_code),
                None,
            )
        }
        .map_err(vk_error("create glyph vertex shader"))?;
        let fragment = unsafe {
            self.device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&fragment_code),
                None,
            )
        }
        .map_err(vk_error("create glyph fragment shader"))?;
        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX)
            .size(8)];
        let layouts = [descriptor_layout];
        let layout = unsafe {
            self.device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&layouts)
                    .push_constant_ranges(&push),
                None,
            )
        }
        .map_err(vk_error("create glyph pipeline layout"))?;
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
            stride: size_of::<GpuGlyph>() as u32,
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
                format: vk::Format::R32G32_SFLOAT,
                offset: 16,
            },
            vk::VertexInputAttributeDescription {
                location: 3,
                binding: 0,
                format: vk::Format::R32G32_SFLOAT,
                offset: 24,
            },
            vk::VertexInputAttributeDescription {
                location: 4,
                binding: 0,
                format: vk::Format::R32G32B32A32_SFLOAT,
                offset: 32,
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
        let attachments = [vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::ONE)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(vk::ColorComponentFlags::RGBA)];
        let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&attachments);
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
        .map_err(|(_, error)| format!("create glyph pipeline: {error:?}"))?[0];
        unsafe {
            self.device.destroy_shader_module(fragment, None);
            self.device.destroy_shader_module(vertex, None);
        }
        Ok((layout, pipeline))
    }

    #[allow(clippy::too_many_lines)]
    fn create_pipeline(
        &self,
        format: vk::Format,
    ) -> Result<(vk::PipelineLayout, vk::Pipeline), RendererFault> {
        let vertex_code = ash::util::read_spv(&mut Cursor::new(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/rectangle.vert"
        ))))
        .map_err(|error| {
            RendererFault::Invariant(format!("read rectangle vertex shader: {error}"))
        })?;
        let fragment_code = ash::util::read_spv(&mut Cursor::new(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/rectangle.frag"
        ))))
        .map_err(|error| {
            RendererFault::Invariant(format!("read rectangle fragment shader: {error}"))
        })?;
        let vertex = unsafe {
            self.device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&vertex_code),
                None,
            )
        }
        .map_err(|error| classify_vk(RendererOperation::Recreate, error))?;
        let fragment = match unsafe {
            self.device.create_shader_module(
                &vk::ShaderModuleCreateInfo::default().code(&fragment_code),
                None,
            )
        } {
            Ok(module) => module,
            Err(error) => {
                unsafe { self.device.destroy_shader_module(vertex, None) };
                return Err(classify_vk(RendererOperation::Recreate, error));
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
        .map_err(|error| classify_vk(RendererOperation::Recreate, error))?;
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
        .map_err(|(_, error)| classify_vk(RendererOperation::Recreate, error))?[0];
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

    pub(crate) fn recreate(&mut self, target: PixelSize) -> Result<bool, RendererFault> {
        self.health.guard()?;
        let result = self.recreate_inner(target);
        self.latch_device_lost(result)
    }

    #[allow(clippy::too_many_lines)]
    fn recreate_inner(&mut self, target: PixelSize) -> Result<bool, RendererFault> {
        if self.acquired.is_some() {
            return Err(RendererFault::Invariant(
                "cannot recreate the swapchain while an acquired image is pending".into(),
            ));
        }
        self.collect_retired()?;
        if self.retired.len() >= MAX_RETIRED_GENERATIONS {
            return Ok(false);
        }
        let capabilities = unsafe {
            self.surface_loader
                .get_physical_device_surface_capabilities(self.physical, self.surface)
        }
        .map_err(|error| classify_vk(RendererOperation::Recreate, error))?;
        if !capabilities
            .supported_usage_flags
            .contains(vk::ImageUsageFlags::COLOR_ATTACHMENT)
        {
            return Err(RendererFault::Invariant(
                "Vulkan surface does not support color attachment usage".into(),
            ));
        }
        let formats = unsafe {
            self.surface_loader
                .get_physical_device_surface_formats(self.physical, self.surface)
        }
        .map_err(|error| classify_vk(RendererOperation::Recreate, error))?;
        let modes = unsafe {
            self.surface_loader
                .get_physical_device_surface_present_modes(self.physical, self.surface)
        }
        .map_err(|error| classify_vk(RendererOperation::Recreate, error))?;
        let format = select::surface_format(&formats)
            .ok_or_else(|| RendererFault::Invariant("no supported 8-bit sRGB format".into()))?;
        let mode = select::present_mode(&modes)
            .ok_or_else(|| RendererFault::Invariant("no supported present mode".into()))?;
        let alpha = select::composite_alpha(capabilities.supported_composite_alpha)
            .ok_or_else(|| RendererFault::Invariant("no supported composite alpha mode".into()))?;
        let extent = select::extent(capabilities, target)
            .ok_or_else(|| RendererFault::Invariant("Vulkan surface has zero extent".into()))?;
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
            .map_err(|error| classify_vk(RendererOperation::Recreate, error))?;
        let images = unsafe { self.swapchain_loader.get_swapchain_images(new_swapchain) }
            .map_err(|error| classify_vk(RendererOperation::Recreate, error))?;
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
            .map_err(|error| classify_vk(RendererOperation::Recreate, error))?;
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
        self.composite_alpha = alpha;
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

    pub(crate) fn render(
        &mut self,
        scene: &RenderScene<'_>,
    ) -> Result<RenderStatus, RendererFault> {
        self.health.guard()?;
        let result = self.render_inner(scene);
        self.latch_device_lost(result)
    }

    #[allow(clippy::too_many_lines)]
    fn render_inner(&mut self, scene: &RenderScene<'_>) -> Result<RenderStatus, RendererFault> {
        if self
            .glyph
            .as_ref()
            .is_some_and(|glyphs| glyphs.pending_repack)
            && !self.all_frame_fences_ready()?
        {
            return Ok(RenderStatus::Deferred);
        }
        let acquired = if let Some(acquired) = self.acquired {
            acquired
        } else {
            let slot_index = self.current_frame;
            let slot_fence = self.frames[slot_index].fence;
            let slot_available = self.frames[slot_index].available;
            match unsafe { self.device.get_fence_status(slot_fence) } {
                Ok(true) => {}
                Ok(false) => return Ok(RenderStatus::Deferred),
                Err(error) => return Err(classify_vk(RendererOperation::Fence, error)),
            }
            // Retired generations referencing this signaled fence must be destroyed before the
            // frame slot resets it for unrelated work.
            self.collect_retired()?;
            let result = unsafe {
                self.swapchain_loader.acquire_next_image(
                    self.swapchain,
                    0,
                    slot_available,
                    vk::Fence::null(),
                )
            };
            let (image_index, suboptimal) = match result {
                Ok(value) => value,
                Err(vk::Result::NOT_READY | vk::Result::TIMEOUT) => {
                    return Ok(RenderStatus::Deferred);
                }
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => return Ok(RenderStatus::OutOfDate),
                Err(error) => return Err(classify_vk(RendererOperation::Acquire, error)),
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
                .map_err(|error| classify_vk(RendererOperation::Fence, error))?
        {
            // The acquire semaphore and image remain owned by this slot. A later timer retry
            // resumes here without acquiring again, so the UI thread remains responsive.
            return Ok(RenderStatus::Deferred);
        }
        unsafe {
            self.device
                .reset_fences(&[slot_fence])
                .map_err(|error| classify_vk(RendererOperation::Submit, error))?;
            self.device
                .reset_command_pool(slot_pool, vk::CommandPoolResetFlags::empty())
                .map_err(|error| classify_vk(RendererOperation::Submit, error))?;
        }
        self.upload_rectangles(acquired.slot, scene.rectangles)
            .map_err(RendererFault::Invariant)?;
        self.upload_glyph_instances(acquired.slot, scene.glyphs)
            .map_err(RendererFault::Invariant)?;
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
            .map_err(|error| classify_vk(RendererOperation::Submit, error))?;
        if let Some(glyphs) = self.glyph.as_mut() {
            for (rect, _) in &glyphs.pending {
                glyphs.pages[usize::from(rect.page)].initialized = true;
            }
            glyphs.pending.clear();
            glyphs.pending_repack = false;
        }
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
                    return Ok(RenderStatus::SubmittedOutOfDate);
                }
                Err(error) => return Err(classify_vk(RendererOperation::Present, error)),
            };
        self.current_frame = (acquired.slot + 1) % self.frames.len();
        if acquired.suboptimal || present_suboptimal {
            tracing::debug!(category = "renderer", "swapchain is suboptimal");
            return Ok(RenderStatus::Suboptimal);
        }
        Ok(RenderStatus::Rendered)
    }

    fn all_frame_fences_ready(&self) -> Result<bool, RendererFault> {
        self.frames.iter().try_fold(true, |ready, slot| {
            unsafe { self.device.get_fence_status(slot.fence) }
                .map(|signaled| ready && signaled)
                .map_err(|error| classify_vk(RendererOperation::Fence, error))
        })
    }

    fn latch_device_lost<T>(
        &mut self,
        result: Result<T, RendererFault>,
    ) -> Result<T, RendererFault> {
        self.health.observe(result)
    }

    #[allow(clippy::too_many_lines)]
    fn record(
        &mut self,
        command: vk::CommandBuffer,
        slot: usize,
        image_index: usize,
        scene: &RenderScene<'_>,
    ) -> Result<(), RendererFault> {
        unsafe {
            self.device.begin_command_buffer(
                command,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
        }
        .map_err(|error| classify_vk(RendererOperation::Submit, error))?;
        self.upload_glyph_data(slot, command)
            .map_err(RendererFault::Invariant)?;
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
        let clear = if self.composite_alpha == vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED {
            premultiplied_color(scene.clear)
        } else {
            let mut opaque = scene.clear;
            opaque.alpha = 1.0;
            color(opaque)
        };
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
            if !scene.glyphs.is_empty() {
                let glyph = self.glyph.as_ref().expect("glyph resources");
                self.device.cmd_bind_pipeline(
                    command,
                    vk::PipelineBindPoint::GRAPHICS,
                    glyph.pipeline,
                );
                let offset = (slot * GLYPHS_PER_SLOT * size_of::<GpuGlyph>()) as u64;
                self.device.cmd_bind_vertex_buffers(
                    command,
                    0,
                    &[glyph.instances.buffer],
                    &[offset],
                );
                self.device.cmd_push_constants(
                    command,
                    glyph.pipeline_layout,
                    vk::ShaderStageFlags::VERTEX,
                    0,
                    as_bytes(&viewport_size),
                );
                let mut start = 0;
                while start < scene.glyphs.len() {
                    let page = scene.glyphs[start].atlas_page;
                    let mut end = start + 1;
                    while end < scene.glyphs.len() && scene.glyphs[end].atlas_page == page {
                        end += 1;
                    }
                    self.device.cmd_bind_descriptor_sets(
                        command,
                        vk::PipelineBindPoint::GRAPHICS,
                        glyph.pipeline_layout,
                        0,
                        &[glyph.descriptors[usize::from(page)]],
                        &[],
                    );
                    self.device
                        .cmd_draw(command, 6, (end - start) as u32, 0, start as u32);
                    start = end;
                }
            }
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
        unsafe { self.device.end_command_buffer(command) }
            .map_err(|error| classify_vk(RendererOperation::Submit, error))
    }

    #[allow(clippy::too_many_lines)]
    fn upload_glyph_data(&mut self, slot: usize, command: vk::CommandBuffer) -> Result<(), String> {
        let glyphs = self.glyph.as_mut().expect("glyph resources");
        let mut staging_offset = slot * GLYPH_STAGING_PER_SLOT;
        let start_offset = staging_offset;
        let mapped = glyphs
            .staging
            .allocation
            .mapped_slice_mut()
            .ok_or("glyph staging memory is not mapped")?;
        let mut copies = Vec::new();
        for (rect, asset) in &glyphs.pending {
            let bytes = asset.bitmap.coverage.as_ref();
            let end = staging_offset
                .checked_add(bytes.len())
                .ok_or("glyph staging offset overflow")?;
            if end > start_offset + GLYPH_STAGING_PER_SLOT {
                return Err("glyph uploads exceed staging partition".into());
            }
            mapped[staging_offset..end].copy_from_slice(bytes);
            copies.push((*rect, (staging_offset - start_offset) as u64));
            staging_offset = end;
        }
        let mut prepared_pages = HashSet::new();
        let repacking = glyphs.pending_repack;
        for (rect, offset) in copies {
            let page = &mut glyphs.pages[usize::from(rect.page)];
            let was_initialized = page.initialized || prepared_pages.contains(&rect.page);
            transition(
                &self.device,
                command,
                page.image,
                if was_initialized {
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
                } else {
                    vk::ImageLayout::UNDEFINED
                },
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                if was_initialized {
                    vk::PipelineStageFlags2::FRAGMENT_SHADER
                } else {
                    vk::PipelineStageFlags2::NONE
                },
                vk::PipelineStageFlags2::COPY,
                if was_initialized {
                    vk::AccessFlags2::SHADER_SAMPLED_READ
                } else {
                    vk::AccessFlags2::empty()
                },
                vk::AccessFlags2::TRANSFER_WRITE,
            );
            if !prepared_pages.contains(&rect.page) && (!page.initialized || repacking) {
                let range = vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    level_count: 1,
                    layer_count: 1,
                    ..Default::default()
                };
                unsafe {
                    self.device.cmd_clear_color_image(
                        command,
                        page.image,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &vk::ClearColorValue { uint32: [0; 4] },
                        &[range],
                    );
                }
            }
            let region = vk::BufferImageCopy::default()
                .buffer_offset((start_offset as u64) + offset)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    layer_count: 1,
                    ..Default::default()
                })
                .image_offset(vk::Offset3D {
                    x: i32::from(rect.x),
                    y: i32::from(rect.y),
                    z: 0,
                })
                .image_extent(vk::Extent3D {
                    width: u32::from(rect.width),
                    height: u32::from(rect.height),
                    depth: 1,
                });
            unsafe {
                self.device.cmd_copy_buffer_to_image(
                    command,
                    glyphs.staging.buffer,
                    page.image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[region],
                );
            }
            transition(
                &self.device,
                command,
                page.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::PipelineStageFlags2::COPY,
                vk::PipelineStageFlags2::FRAGMENT_SHADER,
                vk::AccessFlags2::TRANSFER_WRITE,
                vk::AccessFlags2::SHADER_SAMPLED_READ,
            );
            prepared_pages.insert(rect.page);
        }
        Ok(())
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
            let alpha = rectangle.color.alpha;
            let gpu = GpuRectangle {
                origin_px: rectangle.origin_px,
                size_px: rectangle.size_px,
                color: [
                    rectangle.color.red * alpha,
                    rectangle.color.green * alpha,
                    rectangle.color.blue * alpha,
                    alpha,
                ],
            };
            let start = offset + index * size_of::<GpuRectangle>();
            mapped[start..start + size_of::<GpuRectangle>()].copy_from_slice(as_bytes(&gpu));
        }
        Ok(())
    }

    fn upload_glyph_instances(
        &mut self,
        slot: usize,
        glyphs: &[GlyphInstance],
    ) -> Result<(), String> {
        if glyphs.len() > GLYPHS_PER_SLOT {
            return Err(format!(
                "glyph count {} exceeds per-frame capacity {GLYPHS_PER_SLOT}",
                glyphs.len()
            ));
        }
        let resources = self.glyph.as_mut().expect("glyph resources");
        let offset = slot * GLYPHS_PER_SLOT * size_of::<GpuGlyph>();
        let mapped = resources
            .instances
            .allocation
            .mapped_slice_mut()
            .ok_or("glyph instance memory is not mapped")?;
        for (index, glyph) in glyphs.iter().enumerate() {
            let gpu = GpuGlyph {
                origin_px: glyph.origin_px,
                size_px: glyph.size_px,
                uv_min: glyph.uv_min,
                uv_max: glyph.uv_max,
                color: [
                    glyph.color.red,
                    glyph.color.green,
                    glyph.color.blue,
                    glyph.color.alpha,
                ],
            };
            let start = offset + index * size_of::<GpuGlyph>();
            mapped[start..start + size_of::<GpuGlyph>()].copy_from_slice(as_bytes(&gpu));
        }
        Ok(())
    }

    fn wait_frames(&self) -> Result<(), RendererFault> {
        let fences: Vec<_> = self.frames.iter().map(|slot| slot.fence).collect();
        if fences.is_empty() {
            return Ok(());
        }
        unsafe {
            self.device
                .wait_for_fences(&fences, true, FENCE_TIMEOUT.as_nanos() as u64)
        }
        .map_err(|error| classify_vk(RendererOperation::Fence, error))
    }

    fn collect_retired(&mut self) -> Result<(), RendererFault> {
        let mut index = 0;
        while index < self.retired.len() {
            let ready = self.retired[index]
                .fences
                .iter()
                .try_fold(true, |ready, fence| {
                    unsafe { self.device.get_fence_status(*fence) }
                        .map(|signaled| ready && signaled)
                        .map_err(|error| classify_vk(RendererOperation::Fence, error))
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
        if !self.health.is_poisoned() {
            let _ = self.wait_frames();
        }
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
        if let Some(glyph) = self.glyph.take() {
            unsafe {
                self.device.destroy_pipeline(glyph.pipeline, None);
                self.device
                    .destroy_pipeline_layout(glyph.pipeline_layout, None);
                self.device
                    .destroy_descriptor_pool(glyph.descriptor_pool, None);
                self.device
                    .destroy_descriptor_set_layout(glyph.descriptor_layout, None);
                self.device.destroy_sampler(glyph.sampler, None);
                self.device.destroy_buffer(glyph.instances.buffer, None);
                self.device.destroy_buffer(glyph.staging.buffer, None);
                for page in &glyph.pages {
                    self.device.destroy_image_view(page.view, None);
                    self.device.destroy_image(page.image, None);
                }
            }
            if let Some(allocator) = self.allocator.as_mut() {
                let _ = allocator.free(glyph.instances.allocation);
                let _ = allocator.free(glyph.staging.allocation);
                for page in glyph.pages {
                    let _ = allocator.free(page.allocation);
                }
            }
        }
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

fn premultiplied_color(value: LinearColor) -> vk::ClearColorValue {
    vk::ClearColorValue {
        float32: [
            value.red * value.alpha,
            value.green * value.alpha,
            value.blue * value.alpha,
            value.alpha,
        ],
    }
}
fn vk_error(context: &'static str) -> impl FnOnce(vk::Result) -> String {
    move |error| format!("{context}: {error:?}")
}

fn classify_vk(operation: RendererOperation, error: vk::Result) -> RendererFault {
    match error {
        vk::Result::ERROR_OUT_OF_DATE_KHR => RendererFault::OutOfDate,
        vk::Result::SUBOPTIMAL_KHR => RendererFault::Suboptimal,
        vk::Result::NOT_READY => RendererFault::NotReady,
        vk::Result::TIMEOUT => RendererFault::Timeout { operation },
        vk::Result::ERROR_SURFACE_LOST_KHR => RendererFault::SurfaceLost { operation },
        vk::Result::ERROR_DEVICE_LOST => RendererFault::DeviceLost { operation },
        vk::Result::ERROR_OUT_OF_HOST_MEMORY => RendererFault::OutOfHostMemory { operation },
        vk::Result::ERROR_OUT_OF_DEVICE_MEMORY => RendererFault::OutOfDeviceMemory { operation },
        _ => RendererFault::Fatal {
            operation,
            code: error.as_raw(),
        },
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;
    use std::collections::VecDeque;

    struct ScriptedBackend {
        outcomes: VecDeque<(RendererOperation, Result<(), vk::Result>)>,
        executed: usize,
    }

    impl ScriptedBackend {
        fn execute_next(&mut self, health: &mut RendererHealth) -> Result<(), RendererFault> {
            health.guard()?;
            let (operation, result) = self.outcomes.pop_front().expect("scripted operation");
            self.executed += 1;
            health.observe(result.map_err(|error| classify_vk(operation, error)))
        }
    }

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

    #[test]
    fn vulkan_fault_mapping_preserves_recovery_categories() {
        let cases = [
            (vk::Result::ERROR_OUT_OF_DATE_KHR, RendererFault::OutOfDate),
            (vk::Result::SUBOPTIMAL_KHR, RendererFault::Suboptimal),
            (vk::Result::NOT_READY, RendererFault::NotReady),
            (
                vk::Result::TIMEOUT,
                RendererFault::Timeout {
                    operation: RendererOperation::Recreate,
                },
            ),
            (
                vk::Result::ERROR_SURFACE_LOST_KHR,
                RendererFault::SurfaceLost {
                    operation: RendererOperation::Recreate,
                },
            ),
            (
                vk::Result::ERROR_DEVICE_LOST,
                RendererFault::DeviceLost {
                    operation: RendererOperation::Recreate,
                },
            ),
            (
                vk::Result::ERROR_OUT_OF_HOST_MEMORY,
                RendererFault::OutOfHostMemory {
                    operation: RendererOperation::Recreate,
                },
            ),
            (
                vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
                RendererFault::OutOfDeviceMemory {
                    operation: RendererOperation::Recreate,
                },
            ),
            (
                vk::Result::ERROR_UNKNOWN,
                RendererFault::Fatal {
                    operation: RendererOperation::Recreate,
                    code: vk::Result::ERROR_UNKNOWN.as_raw(),
                },
            ),
        ];
        for (raw, expected) in cases {
            assert_eq!(classify_vk(RendererOperation::Recreate, raw), expected);
        }
    }

    #[test]
    fn first_device_lost_fault_poison_is_stable() {
        let first = RendererFault::DeviceLost {
            operation: RendererOperation::Submit,
        };
        let second = RendererFault::DeviceLost {
            operation: RendererOperation::Present,
        };
        let mut health = RendererHealth::default();
        assert_eq!(health.observe::<()>(Err(first.clone())), Err(first.clone()));
        let _ = health.observe::<()>(Err(second));
        assert_eq!(health.guard(), Err(first));
    }

    #[test]
    fn scripted_operations_map_faults_and_stop_after_device_lost() {
        let mut backend = ScriptedBackend {
            outcomes: VecDeque::from([
                (RendererOperation::Fence, Err(vk::Result::NOT_READY)),
                (
                    RendererOperation::Acquire,
                    Err(vk::Result::ERROR_OUT_OF_DATE_KHR),
                ),
                (
                    RendererOperation::Upload,
                    Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY),
                ),
                (
                    RendererOperation::Present,
                    Err(vk::Result::ERROR_SURFACE_LOST_KHR),
                ),
                (
                    RendererOperation::Submit,
                    Err(vk::Result::ERROR_DEVICE_LOST),
                ),
                (RendererOperation::Present, Ok(())),
            ]),
            executed: 0,
        };
        let mut health = RendererHealth::default();

        assert_eq!(
            backend.execute_next(&mut health),
            Err(RendererFault::NotReady)
        );
        assert_eq!(
            backend.execute_next(&mut health),
            Err(RendererFault::OutOfDate)
        );
        assert_eq!(
            backend.execute_next(&mut health),
            Err(RendererFault::OutOfDeviceMemory {
                operation: RendererOperation::Upload
            })
        );
        assert_eq!(
            backend.execute_next(&mut health),
            Err(RendererFault::SurfaceLost {
                operation: RendererOperation::Present
            })
        );
        let device_lost = RendererFault::DeviceLost {
            operation: RendererOperation::Submit,
        };
        assert_eq!(backend.execute_next(&mut health), Err(device_lost.clone()));
        assert_eq!(backend.execute_next(&mut health), Err(device_lost));
        assert_eq!(backend.executed, 5);
        assert_eq!(backend.outcomes.len(), 1);
    }
}
