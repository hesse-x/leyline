#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogicalSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Scale120(pub u32);

impl Scale120 {
    pub const ONE: Self = Self(120);

    /// Converts a logical size with checked, upward-rounded fixed-point arithmetic.
    ///
    /// # Errors
    /// Returns [`SizeError`] for a zero scale, zero dimension, or overflow.
    pub fn pixels(self, logical: LogicalSize) -> Result<PixelSize, SizeError> {
        if self.0 == 0 || logical.width == 0 || logical.height == 0 {
            return Err(SizeError::Zero);
        }
        Ok(PixelSize {
            width: scaled_dimension(logical.width, self.0)?,
            height: scaled_dimension(logical.height, self.0)?,
        })
    }
}

fn scaled_dimension(value: u32, scale: u32) -> Result<u32, SizeError> {
    let scaled = u64::from(value)
        .checked_mul(u64::from(scale))
        .ok_or(SizeError::Overflow)?;
    let rounded = scaled.checked_add(119).ok_or(SizeError::Overflow)? / 120;
    u32::try_from(rounded).map_err(|_| SizeError::Overflow)
}

#[derive(Clone, Copy, Debug, thiserror::Error, Eq, PartialEq)]
pub enum SizeError {
    #[error("window size and scale must be nonzero")]
    Zero,
    #[error("scaled window size exceeds the supported range")]
    Overflow,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WindowState {
    pub maximized: bool,
    pub fullscreen: bool,
    pub activated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlatformEvent {
    CloseRequested,
    Configured {
        logical_size: LogicalSize,
        scale: Scale120,
        state: WindowState,
    },
    ScaleChanged {
        scale: Scale120,
    },
    FrameReady,
    SurfaceSuspended,
    SurfaceResumed,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinearColor {
    pub red: f32,
    pub green: f32,
    pub blue: f32,
    pub alpha: f32,
}

impl LinearColor {
    #[must_use]
    pub fn from_srgba8(value: u32) -> Self {
        fn linear(byte: u8) -> f32 {
            let value = f32::from(byte) / 255.0;
            if value <= 0.040_45 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }
        Self {
            red: linear(value.to_be_bytes()[0]),
            green: linear(value.to_be_bytes()[1]),
            blue: linear(value.to_be_bytes()[2]),
            alpha: f32::from(value.to_be_bytes()[3]) / 255.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RectangleInstance {
    pub origin_px: [f32; 2],
    pub size_px: [f32; 2],
    pub color: LinearColor,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SceneData {
    pub clear: LinearColor,
    pub rectangles: Vec<RectangleInstance>,
}

pub struct RenderScene<'a> {
    pub clear: LinearColor,
    pub viewport: PixelSize,
    pub rectangles: &'a [RectangleInstance],
}

pub enum GfxCommand {
    SetTitle(String),
    SetDirty,
    SetScene(SceneData),
    RequestClose,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderOutcome {
    Rendered,
    WaitingForFrame,
    Deferred,
    Idle,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_point_scale_rounds_up_and_checks_inputs() {
        assert_eq!(
            Scale120(150).pixels(LogicalSize {
                width: 3,
                height: 7
            }),
            Ok(PixelSize {
                width: 4,
                height: 9
            })
        );
        assert_eq!(
            Scale120::ONE.pixels(LogicalSize {
                width: 0,
                height: 1
            }),
            Err(SizeError::Zero)
        );
        assert_eq!(
            Scale120(u32::MAX).pixels(LogicalSize {
                width: u32::MAX,
                height: 1
            }),
            Err(SizeError::Overflow)
        );
    }
}
