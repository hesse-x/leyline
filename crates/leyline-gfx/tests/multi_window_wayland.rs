use std::{
    num::NonZeroU8,
    time::{Duration, Instant},
};

use leyline_gfx::{GfxHost, GfxOptions, GfxRuntime};

#[test]
#[ignore = "requires a live Wayland compositor and Vulkan WSI"]
fn two_windows_share_host_and_reject_destroyed_surface_key() {
    if std::env::var_os("LEYLINE_RUN_WAYLAND_INTEGRATION").is_none() {
        return;
    }

    let first = GfxRuntime::new(&GfxOptions::default()).expect("create initial Wayland window");
    let (mut host, first_id) =
        GfxHost::adopt_initial(first, NonZeroU8::new(2).expect("non-zero limit"));
    let first_surface = host.surface_key(first_id).expect("initial surface key");
    let second_id = host
        .create_window(&GfxOptions {
            title: "Leyline multi-window integration".into(),
            ..GfxOptions::default()
        })
        .expect("create second window asynchronously");
    let second_surface = host.surface_key(second_id).expect("creating surface key");
    assert_ne!(first_surface, second_surface);
    assert!(host.accepts_surface(first_surface));
    assert!(host.accepts_surface(second_surface));
    assert!(host.create_window(&GfxOptions::default()).is_err());

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_second_configure = false;
    while Instant::now() < deadline && host.window(second_id).is_none() {
        let mut events = Vec::new();
        host.dispatch_pending(&mut events)
            .expect("dispatch shared Wayland queue");
        for routed in events {
            assert!(host.accepts_surface(routed.surface));
            if routed.surface == second_surface
                && matches!(routed.event, leyline_gfx::PlatformEvent::Configured { .. })
            {
                saw_second_configure = true;
            }
        }
        if host.window(second_id).is_none() {
            host.poll_wait(None, Some(Duration::from_millis(50)))
                .expect("poll shared Wayland connection");
        }
    }
    assert!(saw_second_configure, "second window was not configured");
    assert!(host.window(first_id).is_some());
    assert!(host.window(second_id).is_some());

    host.remove_window(second_id)
        .expect("destroy second window");
    assert!(!host.accepts_surface(second_surface));
    assert!(host.accepts_surface(first_surface));
    assert!(host.window(first_id).is_some());
}
