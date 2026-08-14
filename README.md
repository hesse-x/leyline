# Leyline

Leyline is a native Wayland terminal written in Rust. The current implementation opens a real
Wayland/Vulkan window, starts one shell or `-e` command in a real PTY, parses terminal output into
an immutable renderer-independent snapshot, and renders that snapshot through Fontconfig,
FreeType, HarfBuzz, and a bounded Vulkan glyph atlas.

## Ubuntu 24.04 dependencies

```sh
sudo apt install build-essential pkg-config libfontconfig1 \
  libwayland-dev libxkbcommon-dev libdecor-0-dev libvulkan-dev \
  vulkan-validationlayers-dev
```

FreeType and HarfBuzz are built from the source bundled by their `-sys` crates. Fontconfig is loaded
at runtime as `libfontconfig.so.1`, so their Ubuntu development packages are not required.

Rust dependencies come from crates.io and are fixed by `Cargo.lock`. This project uses the Tsinghua
TUNA sparse mirror in `.cargo/config.toml`; remove the source replacement if the default crates.io
route is preferable.

## Build and run

```sh
cargo build --release --locked
cargo run --locked --bin leyline
cargo run --locked --bin leyline -- -e program arg1 arg2
```

Without `-e`, Leyline resolves the effective user's account shell and starts it interactively as a
non-login shell. `-e` preserves program arguments exactly and does not invoke a shell. PTY children
inherit the startup environment and working directory with `TERM=xterm-256color` and
`COLORTERM=truecolor`. Use `-v`, `-vv`, or `-vvv` for progressively more detailed stderr logging.

The stage 0 hardware and integration probes remain available:

```sh
cargo run --locked --bin leyline-probe -- environment
cargo run --locked --bin leyline-probe -- terminal
cargo run --locked --bin leyline-probe -- text
cargo run --locked --bin leyline-probe -- wayland
cargo run --locked --bin leyline-probe -- vulkan
```

Use `--json` for machine-readable evidence and `--verbose` for scope notes. Exit codes are 0 for
success, 2 for missing environment/dependency, 3 for unsupported capability, and 4 for probe bugs.
Use `wayland --wayland-interactive-seconds 90` for the manual libdecor resize/close acceptance
probe; resize the visible window once and then close it before the timeout.

Wayland window decoration and Vulkan presentation checks require an interactive Ubuntu 24.04 GNOME
Wayland session. Missing system packages are reported with their corresponding Ubuntu package name.

## Configuration

Leyline reads `$XDG_CONFIG_HOME/leyline/config.toml`, falling back to
`$HOME/.config/leyline/config.toml`. A missing file uses safe defaults; an invalid file stops startup
with a field-specific diagnostic. Example:

```toml
[font]
family = "monospace"
size = 11.0
ligatures = false

[colors]
foreground = "#dce7f3"
# The final byte controls opacity; the compositor shows the desktop behind the window.
background = "#101522dc"
cursor = "#8bd5ff"
selection_foreground = "#f6f9ff"
selection_background = "#526ab8cc"

[window]
padding_x = 12
padding_y = 10

[scrolling]
history_lines = 10000

[cursor]
style = "block"

[behavior]
hold_after_exit = false
confirm_multiline_paste = true

[[keybindings]]
key = "PageUp"
mods = ["Shift"]
action = "ScrollPageUp"
```

Selecting text publishes the primary selection; paste it with the middle mouse button or
`Shift+Insert`.

When paste confirmation is enabled, multiline or control-character clipboard content opens a
modal warning that shows only its source, size, line count, and risk category. Press `Enter` or
`Y` to paste; press `Escape` or `N` to reject. Any other key cancels the modal and is consumed.
Losing keyboard focus, replacing the primary-selection offer, requesting another paste, or closing
the session also cancels the pending paste.

Unknown configuration fields produce warnings. Colors must be `#RRGGBB` or `#RRGGBBAA`; font size,
padding, and scrollback are bounded to prevent accidental resource exhaustion.

`behavior.hold_after_exit = false` closes the window only after both the child status and PTY EOF
have been observed, so trailing output is parsed first. Setting it to `true` retains the final
terminal snapshot while releasing the PTY worker and file descriptor.
Closing a running session targets the child through a Linux pidfd, requests `SIGTERM`, and escalates
after a short grace period, so an uncooperative child cannot indefinitely block window shutdown.

## Current limitations

Text rendering uses system Fontconfig fallback, grayscale FreeType coverage, and a four-page
`2048x2048 R8_UNORM` atlas. Color emoji are not supported; an outline fallback is used when one is
available. Ligatures are disabled by default and always remain constrained to terminal cell spans.
Strong RTL runs are shaped, but v0.1 does not perform full Unicode bidi visual reordering: cursor
and selection coordinates remain in logical cell order.

Keyboard, mouse, IME, interactive selection, primary selection, and explicitly triggered OSC 8
links are implemented. System-clipboard shortcuts are not exposed and OSC 52 clipboard requests
remain deliberately rejected. OSC 8 `Ctrl+click` opening has been manually verified on GNOME.
Broader IBus/Fcitx, fractional-scale, and hardware Vulkan acceptance coverage is still pending.
GNOME client-side decoration
uses libdecor; compositors providing xdg-decoration use server-side decoration. Fractional scale is
enabled only when fractional-scale-v1 and viewporter are both available, with integer buffer-scale
as the fallback. The final pure Wayland/Vulkan client will not inherit an accessibility tree from a
GUI toolkit; screen-reader integration is outside the v0.1 commitment.
