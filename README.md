# Leyline

Leyline is a native Wayland terminal written in Rust. Stage 2 opens a real Wayland window and uses
Vulkan 1.3 dynamic rendering and synchronization2 to present a demand-driven rectangle test scene.
CLI/configuration and bounded cross-thread ingress are active; PTY, terminal text, and input are not.

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

`-e` preserves program arguments exactly and does not invoke a shell. During stage 2 the request is
validated and retained by the application coordinator, but the program is not started. Use `-v`,
`-vv`, or `-vvv` for progressively more detailed stderr logging.

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
foreground = "#d8d8d8"
background = "#181818"

[window]
padding_x = 8
padding_y = 8

[scrolling]
history_lines = 10000

[cursor]
style = "block"

[behavior]
hold_after_exit = false
confirm_multiline_paste = true

[[keybindings]]
key = "C"
mods = ["Control", "Shift"]
action = "Copy"
```

Unknown configuration fields produce warnings. Colors must be `#RRGGBB` or `#RRGGBBAA`; font size,
padding, and scrollback are bounded to prevent accidental resource exhaustion.

## Current limitations

The stage 2 executable renders only a fixed diagnostic scene. PTY sessions, terminal emulation,
text rendering, clipboard, and input handling arrive in later stages. GNOME client-side decoration
uses libdecor; compositors providing xdg-decoration use server-side decoration. Fractional scale is
enabled only when fractional-scale-v1 and viewporter are both available, with integer buffer-scale
as the fallback. The final pure Wayland/Vulkan client will not inherit an accessibility tree from a
GUI toolkit; screen-reader integration is outside the v0.1 commitment.
