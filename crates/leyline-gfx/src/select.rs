use ash::vk;

use crate::PixelSize;

pub(crate) fn surface_format(formats: &[vk::SurfaceFormatKHR]) -> Option<vk::SurfaceFormatKHR> {
    formats
        .iter()
        .copied()
        .find(|item| {
            item.format == vk::Format::B8G8R8A8_SRGB
                && item.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
        })
        .or_else(|| {
            formats.iter().copied().find(|item| {
                item.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
                    && matches!(
                        item.format,
                        vk::Format::R8G8B8A8_SRGB | vk::Format::B8G8R8A8_SRGB
                    )
            })
        })
}

pub(crate) fn present_mode(modes: &[vk::PresentModeKHR]) -> Option<vk::PresentModeKHR> {
    [vk::PresentModeKHR::MAILBOX, vk::PresentModeKHR::FIFO]
        .into_iter()
        .find(|candidate| modes.contains(candidate))
}

pub(crate) fn composite_alpha(
    supported: vk::CompositeAlphaFlagsKHR,
) -> Option<vk::CompositeAlphaFlagsKHR> {
    [
        vk::CompositeAlphaFlagsKHR::OPAQUE,
        vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
        vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
        vk::CompositeAlphaFlagsKHR::INHERIT,
    ]
    .into_iter()
    .find(|candidate| supported.contains(*candidate))
}

pub(crate) fn image_count(minimum: u32, maximum: u32) -> u32 {
    let wanted = minimum.saturating_add(1);
    if maximum == 0 {
        wanted
    } else {
        wanted.min(maximum)
    }
}

pub(crate) fn extent(
    capabilities: vk::SurfaceCapabilitiesKHR,
    target: PixelSize,
) -> Option<vk::Extent2D> {
    if capabilities.current_extent.width != u32::MAX {
        let fixed = capabilities.current_extent;
        return (fixed.width != 0 && fixed.height != 0).then_some(fixed);
    }
    let value = vk::Extent2D {
        width: target.width.clamp(
            capabilities.min_image_extent.width,
            capabilities.max_image_extent.width,
        ),
        height: target.height.clamp(
            capabilities.min_image_extent.height,
            capabilities.max_image_extent.height,
        ),
    };
    (value.width != 0 && value.height != 0).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_follow_the_fixed_product_policy() {
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
        assert!(surface_format(&formats).expect("format").format == vk::Format::B8G8R8A8_SRGB);
        assert!(present_mode(&[vk::PresentModeKHR::FIFO]) == Some(vk::PresentModeKHR::FIFO));
        assert!(
            composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
                == Some(vk::CompositeAlphaFlagsKHR::OPAQUE)
        );
        assert_eq!(image_count(4, 0), 5);
        assert_eq!(image_count(4, 4), 4);
    }

    #[test]
    fn variable_extent_is_clamped_and_zero_fixed_extent_suspends() {
        let capabilities = vk::SurfaceCapabilitiesKHR {
            current_extent: vk::Extent2D {
                width: u32::MAX,
                height: u32::MAX,
            },
            min_image_extent: vk::Extent2D {
                width: 10,
                height: 20,
            },
            max_image_extent: vk::Extent2D {
                width: 100,
                height: 200,
            },
            ..Default::default()
        };
        let selected = extent(
            capabilities,
            PixelSize {
                width: 1,
                height: 300,
            },
        )
        .expect("extent");
        assert!(selected.width == 10 && selected.height == 200);
        assert!(
            extent(
                vk::SurfaceCapabilitiesKHR {
                    current_extent: vk::Extent2D {
                        width: 0,
                        height: 0
                    },
                    ..Default::default()
                },
                PixelSize {
                    width: 20,
                    height: 20
                }
            )
            .is_none()
        );
    }
}
