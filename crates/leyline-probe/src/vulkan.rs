#![allow(unsafe_code)]

use std::collections::HashSet;
use std::ffi::{CStr, CString};

use ash::{Entry, vk};

use crate::report::{ProbeError, ProbeResult, Reporter};
use crate::wayland::SurfaceHarness;

pub fn run(reporter: &mut Reporter) -> ProbeResult<()> {
    let wayland = SurfaceHarness::create(false)?;
    // SAFETY: ash owns the loader handle and all Vulkan objects are destroyed before it.
    let entry = unsafe { Entry::load() }.map_err(|error| {
        ProbeError::missing(
            "vulkan.loader",
            error.to_string(),
            "install libvulkan1 and a GPU driver",
        )
    })?;
    let version = loader_version(&entry)?;
    check_extensions(&entry)?;
    reporter.pass(
        "vulkan",
        "loader",
        format!(
            "API {}.{}.{} with required Wayland instance extensions",
            vk::api_version_major(version),
            vk::api_version_minor(version),
            vk::api_version_patch(version)
        ),
    );

    let application_name = CString::new("leyline-probe")
        .map_err(|error| ProbeError::internal("vulkan.instance", error.to_string()))?;
    let application = vk::ApplicationInfo::default()
        .application_name(&application_name)
        .application_version(vk::make_api_version(0, 0, 1, 0))
        .engine_name(&application_name)
        .engine_version(vk::make_api_version(0, 0, 1, 0))
        .api_version(vk::API_VERSION_1_3);
    let extension_names = [
        ash::khr::surface::NAME.as_ptr(),
        ash::khr::wayland_surface::NAME.as_ptr(),
    ];
    let create_info = vk::InstanceCreateInfo::default()
        .application_info(&application)
        .enabled_extension_names(&extension_names);
    // SAFETY: create_info only references values in this scope; no callbacks are installed.
    let instance = unsafe { entry.create_instance(&create_info, None) }.map_err(|error| {
        ProbeError::unsuitable(
            "vulkan.instance",
            format!("{error:?}"),
            "inspect the Vulkan ICD",
        )
    })?;
    let surface_loader = ash::khr::surface::Instance::new(&entry, &instance);
    let wayland_loader = ash::khr::wayland_surface::Instance::new(&entry, &instance);
    let wayland_create = vk::WaylandSurfaceCreateInfoKHR::default()
        .display(wayland.display_ptr().cast())
        .surface(wayland.surface_ptr().cast());
    // SAFETY: both Wayland pointers remain valid until after the Vulkan surface is destroyed.
    let surface = unsafe { wayland_loader.create_wayland_surface(&wayland_create, None) }.map_err(
        |error| {
            // SAFETY: no Vulkan children were successfully created.
            unsafe { instance.destroy_instance(None) };
            ProbeError::unsuitable(
                "vulkan.surface",
                format!("{error:?}"),
                "verify the Wayland compositor and Vulkan ICD use the same display/GPU",
            )
        },
    )?;
    let result = inspect_devices(&instance, &surface_loader, surface, reporter);
    // SAFETY: surface is the only child and is destroyed before its instance and Wayland owners.
    unsafe {
        surface_loader.destroy_surface(surface, None);
        instance.destroy_instance(None);
    }
    result
}

fn loader_version(entry: &Entry) -> ProbeResult<u32> {
    // SAFETY: the loader owns the returned metadata.
    let version = unsafe { entry.try_enumerate_instance_version() }
        .map_err(|error| {
            ProbeError::unsuitable(
                "vulkan.version",
                format!("{error:?}"),
                "update the Vulkan loader",
            )
        })?
        .unwrap_or(vk::API_VERSION_1_0);
    if version < vk::API_VERSION_1_3 {
        return Err(ProbeError::unsuitable(
            "vulkan.version",
            format!(
                "loader API {}.{}; required 1.3",
                vk::api_version_major(version),
                vk::api_version_minor(version)
            ),
            "update the Vulkan loader and GPU driver",
        ));
    }
    Ok(version)
}

fn check_extensions(entry: &Entry) -> ProbeResult<()> {
    // SAFETY: a null layer name requests global properties; ash copies the result.
    let properties =
        unsafe { entry.enumerate_instance_extension_properties(None) }.map_err(|error| {
            ProbeError::unsuitable(
                "vulkan.extensions",
                format!("{error:?}"),
                "repair the Vulkan loader",
            )
        })?;
    let names: HashSet<String> = properties
        .iter()
        .map(|property| {
            // SAFETY: Vulkan guarantees NUL termination for extension_name.
            unsafe { CStr::from_ptr(property.extension_name.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    for required in ["VK_KHR_surface", "VK_KHR_wayland_surface"] {
        if !names.contains(required) {
            return Err(ProbeError::unsuitable(
                "vulkan.extensions",
                format!("missing {required}"),
                "install a Wayland-capable Vulkan ICD",
            ));
        }
    }
    Ok(())
}

fn inspect_devices(
    instance: &ash::Instance,
    surface_loader: &ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    reporter: &mut Reporter,
) -> ProbeResult<()> {
    // SAFETY: the instance remains alive throughout enumeration.
    let devices = unsafe { instance.enumerate_physical_devices() }.map_err(|error| {
        ProbeError::unsuitable(
            "vulkan.devices",
            format!("{error:?}"),
            "install a Vulkan 1.3 GPU driver",
        )
    })?;
    let mut rejected = Vec::new();
    for device in devices {
        // SAFETY: device belongs to instance and the queries copy metadata.
        let properties = unsafe { instance.get_physical_device_properties(device) };
        let queues = unsafe { instance.get_physical_device_queue_family_properties(device) };
        // SAFETY: Vulkan guarantees a NUL-terminated device_name.
        let name = unsafe { CStr::from_ptr(properties.device_name.as_ptr()) }.to_string_lossy();
        let mut queue = None;
        for (index, family) in queues.iter().enumerate() {
            let Ok(index) = u32::try_from(index) else {
                continue;
            };
            // SAFETY: the queue index is from this physical device and surface belongs to the instance.
            let present = unsafe {
                surface_loader.get_physical_device_surface_support(device, index, surface)
            }
            .map_err(|error| {
                ProbeError::unsuitable(
                    "vulkan.present-support",
                    format!("{error:?}"),
                    "inspect the Vulkan ICD",
                )
            })?;
            if family.queue_count > 0
                && family.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                && present
            {
                queue = Some(index);
                break;
            }
        }
        let hardware = properties.device_type != vk::PhysicalDeviceType::CPU;
        if properties.api_version < vk::API_VERSION_1_3 || queue.is_none() || !hardware {
            rejected.push(format!(
                "{name}: API {}.{} graphics+present={} hardware={hardware}",
                vk::api_version_major(properties.api_version),
                vk::api_version_minor(properties.api_version),
                queue.is_some()
            ));
            continue;
        }
        // SAFETY: all queries use a valid device/surface pair.
        let capabilities =
            unsafe { surface_loader.get_physical_device_surface_capabilities(device, surface) }
                .map_err(|error| {
                    ProbeError::unsuitable(
                        "vulkan.surface-capabilities",
                        format!("{error:?}"),
                        "inspect the compositor/driver",
                    )
                })?;
        let formats =
            unsafe { surface_loader.get_physical_device_surface_formats(device, surface) }
                .map_err(|error| {
                    ProbeError::unsuitable(
                        "vulkan.surface-formats",
                        format!("{error:?}"),
                        "inspect the compositor/driver",
                    )
                })?;
        let modes =
            unsafe { surface_loader.get_physical_device_surface_present_modes(device, surface) }
                .map_err(|error| {
                    ProbeError::unsuitable(
                        "vulkan.present-modes",
                        format!("{error:?}"),
                        "inspect the compositor/driver",
                    )
                })?;
        let format = select_format(&formats)?;
        let mode = select_present_mode(&modes)?;
        let alpha = select_composite_alpha(capabilities.supported_composite_alpha)?;
        reporter.pass("vulkan", "surface", format!("device={name}; queue={}; format={}/{}; present={}; alpha=0x{:x}; extent={}x{}; images={}..{}", queue.unwrap_or_default(), format.format.as_raw(), format.color_space.as_raw(), mode.as_raw(), alpha.as_raw(), capabilities.current_extent.width, capabilities.current_extent.height, capabilities.min_image_count, capabilities.max_image_count));
        return Ok(());
    }
    Err(ProbeError::unsuitable(
        "vulkan.devices",
        format!(
            "no suitable hardware Vulkan device; candidates: {}",
            rejected.join("; ")
        ),
        "install/select a Vulkan 1.3 hardware ICD visible in the desktop session",
    ))
}

fn select_format(formats: &[vk::SurfaceFormatKHR]) -> ProbeResult<vk::SurfaceFormatKHR> {
    formats
        .iter()
        .copied()
        .find(|value| {
            value.format == vk::Format::B8G8R8A8_SRGB
                && value.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
        })
        .or_else(|| {
            formats.iter().copied().find(|value| {
                matches!(
                    value.format,
                    vk::Format::R8G8B8A8_SRGB | vk::Format::B8G8R8A8_SRGB
                )
            })
        })
        .ok_or_else(|| {
            ProbeError::unsuitable(
                "vulkan.surface-formats",
                "no 8-bit sRGB surface format",
                "use a compositor/driver exposing an sRGB format",
            )
        })
}

fn select_present_mode(modes: &[vk::PresentModeKHR]) -> ProbeResult<vk::PresentModeKHR> {
    if modes.contains(&vk::PresentModeKHR::MAILBOX) {
        Ok(vk::PresentModeKHR::MAILBOX)
    } else if modes.contains(&vk::PresentModeKHR::FIFO) {
        Ok(vk::PresentModeKHR::FIFO)
    } else {
        Err(ProbeError::unsuitable(
            "vulkan.present-modes",
            "FIFO is missing",
            "repair the non-conformant Vulkan WSI implementation",
        ))
    }
}

fn select_composite_alpha(
    supported: vk::CompositeAlphaFlagsKHR,
) -> ProbeResult<vk::CompositeAlphaFlagsKHR> {
    [
        vk::CompositeAlphaFlagsKHR::OPAQUE,
        vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
        vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
        vk::CompositeAlphaFlagsKHR::INHERIT,
    ]
    .into_iter()
    .find(|value| supported.contains(*value))
    .ok_or_else(|| {
        ProbeError::unsuitable(
            "vulkan.composite-alpha",
            "no supported composite alpha mode",
            "repair the Vulkan WSI implementation",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{select_composite_alpha, select_format, select_present_mode};
    use ash::vk;

    #[test]
    fn surface_preferences_follow_product_policy() {
        let formats = [
            vk::SurfaceFormatKHR {
                format: vk::Format::R8G8B8A8_SRGB,
                color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            },
            vk::SurfaceFormatKHR {
                format: vk::Format::B8G8R8A8_SRGB,
                color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
            },
        ];
        assert!(select_format(&formats).expect("format").format == vk::Format::B8G8R8A8_SRGB);
        assert!(
            select_present_mode(&[vk::PresentModeKHR::FIFO, vk::PresentModeKHR::MAILBOX])
                .expect("mode")
                == vk::PresentModeKHR::MAILBOX
        );
        assert!(
            select_composite_alpha(
                vk::CompositeAlphaFlagsKHR::INHERIT | vk::CompositeAlphaFlagsKHR::OPAQUE
            )
            .expect("alpha")
                == vk::CompositeAlphaFlagsKHR::OPAQUE
        );
    }

    #[test]
    fn empty_surface_capabilities_are_rejected() {
        assert!(select_format(&[]).is_err());
        assert!(select_present_mode(&[]).is_err());
        assert!(select_composite_alpha(vk::CompositeAlphaFlagsKHR::empty()).is_err());
    }
}
