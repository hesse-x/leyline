//! Explicit window-scoped dependencies used by controller code.

/// A window operation cannot be constructed without naming its target window.
pub struct WindowContext<'a, W, T> {
    pub id: leyline_gfx::WindowId,
    pub window: &'a mut W,
    pub tabs: &'a mut T,
}

impl<'a, W, T> WindowContext<'a, W, T> {
    #[must_use]
    pub fn new(id: leyline_gfx::WindowId, window: &'a mut W, tabs: &'a mut T) -> Self {
        Self { id, window, tabs }
    }
}

pub struct WindowController;

impl WindowController {
    pub fn apply<W, T, R>(
        context: &mut WindowContext<'_, W, T>,
        operation: impl FnOnce(&mut W, &mut T) -> R,
    ) -> R {
        operation(context.window, context.tabs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_operates_only_on_the_named_context() {
        let mut window = 1;
        let mut tabs = 2;
        let id = leyline_gfx::WindowId::from_raw(7).unwrap();
        let mut context = WindowContext::new(id, &mut window, &mut tabs);
        WindowController::apply(&mut context, |window, tabs| {
            *window += 10;
            *tabs += 20;
        });
        assert_eq!(context.id, id);
        assert_eq!((*context.window, *context.tabs), (11, 22));
    }
}
