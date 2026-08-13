# FastTerm technical probes

This workspace implements FastTerm v0.1 stage 0. It validates the terminal core, Ubuntu system text
libraries, a native Wayland connection, and the Vulkan 1.3 loader path; it is not the terminal app.

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
cargo run --locked --bin fastterm-probe -- environment
cargo run --locked --bin fastterm-probe -- terminal
cargo run --locked --bin fastterm-probe -- text
cargo run --locked --bin fastterm-probe -- wayland
cargo run --locked --bin fastterm-probe -- vulkan
```

Use `--json` for machine-readable evidence and `--verbose` for scope notes. Exit codes are 0 for
success, 2 for missing environment/dependency, 3 for unsupported capability, and 4 for probe bugs.
Use `wayland --wayland-interactive-seconds 90` for the manual libdecor resize/close acceptance
probe; resize the visible window once and then close it before the timeout.

Wayland window decoration and Vulkan presentation checks require an interactive Ubuntu 24.04 GNOME
Wayland session. Missing system packages are reported with their corresponding Ubuntu package name.
