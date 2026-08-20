# Leyline

Leyline is a native Wayland terminal written in Rust. The current implementation opens a real
Wayland/Vulkan window, runs independent shell or `-e` command PTYs in tabs, parses terminal output into
an immutable renderer-independent snapshot, and renders that snapshot through Fontconfig,
FreeType, HarfBuzz, and a bounded Vulkan glyph atlas.

## Ubuntu 24.04 dependencies

```sh
sudo apt install build-essential pkg-config libfontconfig1 \
  libwayland-dev libxkbcommon-dev libdecor-0-dev libvulkan-dev \
  vulkan-validationlayers-dev ncurses-bin
```

FreeType and HarfBuzz are built from the source bundled by their `-sys` crates. Fontconfig is loaded
at runtime as `libfontconfig.so.1`, so their Ubuntu development packages are not required.

Rust dependencies come from crates.io and are fixed by `Cargo.lock`. This project uses the Tsinghua
TUNA sparse mirror in `.cargo/config.toml`; remove the source replacement if the default crates.io
route is preferable.

## Build and run

```sh
cargo build --release --locked
cargo run --locked --bin leyline -- terminfo install --user
cargo run --locked --bin leyline
cargo run --locked --bin leyline -- -e program arg1 arg2
```

Without `-e`, Leyline resolves the effective user's account shell and starts it interactively as a
non-login shell. `-e` preserves program arguments exactly and does not invoke a shell. PTY children
inherit the startup environment and working directory with `TERM=leyline-256color` and
`COLORTERM=truecolor`. Startup fails before creating a window or PTY if the entry is missing. Use
`leyline --term xterm-256color` as an explicit best-effort compatibility mode, and use `-v`, `-vv`,
or `-vvv` for progressively more detailed stderr logging.

Window launch overrides use `--geometry COLUMNSxLINES`, `--maximized`, or `--fullscreen`;
`--maximized` and `--fullscreen` are mutually exclusive. `--new-window` is a desktop-entry
compatible alias for starting this process with a terminal window; it does not contact an existing
Leyline process.

The canonical standalone source is `terminfo/leyline.terminfo`; it declares `RGB`, allowing modern
tmux to detect true color without a Leyline-specific override. Useful management and diagnostic
commands are:

```sh
leyline terminfo print
leyline terminfo check
leyline terminfo install --user
leyline terminfo uninstall --user
leyline doctor terminfo
leyline doctor ssh HOST
```

SSH forwards TERM but not the database. Install the printed source explicitly on a remote host with
`leyline terminfo print | ssh HOST 'tic -x -o ~/.terminfo /dev/stdin'`, run the read-only doctor first,
or select compatibility mode before entering an environment where the entry cannot be installed.
Leyline never modifies remote hosts or tmux configuration automatically. In tmux, keep pane identity
owned by tmux (normally `set -g default-terminal tmux-256color`). See
`doc/term-terminfo-tmux-truecolor.md` for the complete identity and per-hop contract.

The stage 0 hardware and integration probes remain available:

```sh
cargo run --locked --bin leyline-probe -- environment
cargo run --locked --bin leyline-probe -- terminal
cargo run --locked --bin leyline-probe -- scene
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
[terminal]
identity = "leyline" # or explicit best-effort "xterm-256color"

[font]
family = "monospace"
size = 13.0
ligatures = false
hinting = "slight"         # none | slight | full | system
antialiasing = "grayscale" # grayscale | system (LCD safely falls back today)
line_spacing = 1.0          # logical pixels, 0.0..=8.0

[colors]
foreground = "#d8dcd8"
# The final byte controls opacity; the compositor shows the desktop behind the window.
background = "#2e3436f2"
cursor = "#d8dcd8"
selection_foreground = "#f4f6f4"
selection_background = "#58656dcc"
search_current_foreground = "#171a1c"
search_current_background = "#ffb454"
search_match_foreground = "#f4f6f4"
search_match_background = "#8a6532cc"
ansi = [
  "#2e3436", "#cc6666", "#6fa66f", "#c8a85f",
  "#5f87af", "#a27aa8", "#5f9ea0", "#d3d7cf",
  "#6c7375", "#e07a7a", "#87bd87", "#d8bd73",
  "#7aa2c8", "#b58abb", "#72b2b4", "#eeeeec",
]

[window]
padding_x = 0
padding_y = 5
columns = 80
lines = 24
startup_state = "normal" # normal | maximized | fullscreen
max_windows = 8

[scrolling]
history_lines = 10000
scroll_on_output = false # true forces new output to leave history and jump to the bottom

[scrollbar]
mode = "auto"              # auto | always | hidden
width = 4.0
hit_width = 12.0
min_thumb_size = 24.0
thumb = "#9aa2a680"
thumb_hover = "#c1c7caff"
track = "#00000000"

[cursor]
style = "block"

[behavior]
hold_after_exit = false
confirm_multiline_paste = false

[tabs]
visibility = "always" # always | multiple | never
max_count = 32
bar_height = 32
min_width = 80
max_width = 240
show_close_button = true
new_tab_cwd = "inherit" # inherit | fixed | home
# new_tab_fixed_cwd = "/srv/project"

[[keybindings]]
key = "PageUp"
mods = ["Shift"]
action = "ScrollPageUp"
```

Selecting text publishes the primary selection; paste it with the middle mouse button or
`Shift+Insert`. `Ctrl+Shift+C` explicitly copies the active tab's selection to the Wayland
Clipboard, and `Ctrl+Shift+V` pastes the Clipboard into the same tab as long as it remains active.

`Ctrl+F` opens a compact, centered, tab-local regular-expression search dialog containing
only the query field and previous/next buttons. Enter and Shift+Enter also move between matches,
and Escape closes the dialog whenever it is open. Clicking the terminal returns keyboard and IME
input to the PTY without closing the search; click the query field or press `Ctrl+F` to focus
it again. Drag the padding around the query and navigation controls, or the eight-pixel region just
outside the panel, to reposition it; the pointer changes to grab/grabbing and the position remains
clamped to the window. Searches are case-sensitive, span soft-wrapped lines, and do not cross hard
line breaks. Queries are limited to 256 Unicode scalars/1,024 UTF-8 bytes, stored
matches are capped at 10,000, logical lines are capped at 64 KiB, and the compiled engine has
fixed resource limits. Search text is never logged or copied into a window title. When an input
method is active, the query may be shared with the local Wayland input method as surrounding text;
terminal contents and matching context are not shared.

The tab bar follows `[tabs].visibility`: `always`, `multiple`, or `never`. `Ctrl+Shift+N` creates
and activates a tab, `Ctrl+Shift+W` closes it, `Ctrl+Shift+Left/Right` cycles, and
`Ctrl+Shift+1..9` activates an ordinal tab. `Ctrl+Shift+PageUp/PageDown` reorders the active tab.
Mouse clicks switch tabs; a close button or middle click closes one, and dragging reorders tabs in
the current window with a moving title preview and bounded edge scrolling. Closing a tab while a
foreground job is running requires `Enter`/`Y` confirmation; `Escape`/`N` keeps the tab open.
The confirmation dialog also provides clickable `Close tab` and `Cancel` buttons. By default,
tab and window titles show the current local directory: `~/` for HOME, `~/...` below HOME,
and an absolute path elsewhere. An explicit OSC 0/2 application title takes precedence until it is reset.
`Ctrl+Shift+Alt+N` creates a new window with a new session, while `F11` toggles fullscreen. New tabs
and windows repeat the startup launch request and use `[tabs].new_tab_cwd`: `inherit` prefers the
active tab's last valid OSC 7 directory, `fixed`
uses `new_tab_fixed_cwd`, and `home` uses the HOME captured at startup. Every unavailable candidate
falls back to Leyline's startup directory. `MoveTabToNewWindow` moves the same live PTY into a new
window without restarting it; bind it explicitly if desired. User `[[keybindings]]` entries can
override the actions
`CopyClipboard`, `PasteClipboard`, `PastePrimary`, `Search`, `SearchNext`, `SearchPrevious`,
`CancelSearch`, `NewTab`, `NewWindow`, `CloseTab`, `PreviousTab`, `NextTab`, `MoveTabLeft`,
`MoveTabRight`, `MoveTabToNewWindow`, `ToggleFullscreen`, `ToggleMaximized`, `RestoreWindow`, and
`ActivateTab1` through `ActivateTab9`.

Leyline receives current-directory metadata but does not install shell hooks. Configure bash, zsh,
fish, or a prompt plugin to emit `OSC 7 ; file://host/absolute-path` terminated by BEL or ST, with
the path RFC 3986 percent-encoded. For a quick check in a directory containing no spaces, `%`, or
control bytes, run `printf '\033]7;file://%s%s\033\\' "$(hostname)" "$PWD"` and then open a tab.
Empty authority, `localhost`, and Leyline's startup hostname are accepted; other authorities clear
the tab's inheritable hint and are never mapped to local paths. A silent SSH/tmux/container layer
cannot be detected, so Leyline falls back only after it receives a rejected report or cannot open
the reported host-side directory.

[`config/reference.toml`](config/reference.toml) fixes the Ubuntu Sans Mono 13 opaque screenshot
baseline. [`config/legacy.toml`](config/legacy.toml) restores the previous font metrics, palette,
background alpha, and gutter-free layout without relying on an implicit runtime preset.

When paste confirmation is enabled, multiline or control-character clipboard content opens a
modal warning that shows only its source, size, line count, and risk category. Press `Enter` or
`Y` to paste; press `Escape` or `N` to reject. Any other key cancels the modal and is consumed.
Losing keyboard focus, replacing the matching Clipboard or primary-selection offer, switching tabs,
requesting another paste, or closing the session also cancels the pending paste. Changes to the
other selection target do not close the modal.

Unknown configuration fields produce warnings. General colors accept `#RRGGBB` or `#RRGGBBAA`;
the ANSI palette must contain exactly 16 opaque `#RRGGBB` entries. Font size, line spacing,
scrollbar geometry, padding, and scrollback are bounded to prevent accidental resource exhaustion.
`auto` and `always` reserve the same right-side gutter, so thumb visibility never resizes the PTY;
`hidden` removes the gutter. Gutter clicks, drags, and wheel events are consumed by window chrome and
are never encoded as terminal mouse reports.

`behavior.hold_after_exit = false` closes only the completed tab after both the child status and PTY EOF
have been observed, so trailing output is parsed first; the last completed tab closes the window. Setting it to `true` retains the final
terminal snapshot while releasing the PTY worker and file descriptor.
Closing a running session targets the child through a Linux pidfd, requests `SIGTERM`, and escalates
after a short grace period, so an uncooperative child cannot indefinitely block window shutdown.
Leyline handles `SIGHUP`, `SIGINT`, and `SIGTERM` through the same bounded shutdown path and exits
with status 129, 130, or 143 respectively after closing every window's sessions. Closing the PTY
uses traditional Unix terminal hangup semantics: interactive jobs that remain attached to the
controlling terminal are expected to exit, while programs deliberately detached with mechanisms
such as `nohup`, `disown`, `setsid`, daemonization, or a separate supervisor are allowed to survive.
`SIGKILL` and synchronous process crashes cannot run user-space cleanup; in those cases Leyline can
only rely on the kernel closing its file descriptors and producing the normal PTY hangup.

## Current limitations

Text rendering uses matched Fontconfig properties, configurable FreeType hinting, grayscale
coverage, physical-pixel placement, nearest sampling, and a four-page `2048x2048 R8_UNORM` atlas.
Startup logging reports the requested and resolved face, raster profile, scale, metrics, and atlas
filter. LCD subpixel rendering remains disabled: `antialiasing = "system"` records a diagnostic and
uses grayscale when the system requests RGB/BGR, since transparent or output-ambiguous Wayland
surfaces cannot safely use subpixel coverage. Color emoji are not supported; an outline fallback is used when one is
available. Ligatures are disabled by default and always remain constrained to terminal cell spans.
Strong RTL runs are shaped, but v0.1 does not perform full Unicode bidi visual reordering: cursor
and selection coordinates remain in logical cell order.

Keyboard, mouse, IME, interactive selection, Wayland Clipboard/primary selection, and explicitly
triggered OSC 8 links are implemented. OSC 52 clipboard requests remain deliberately rejected.
OSC 8 `Ctrl+click` opening has been manually verified on GNOME.
Broader IBus/Fcitx, fractional-scale, and hardware Vulkan acceptance coverage is still pending.
GNOME client-side decoration
uses libdecor; compositors providing xdg-decoration use server-side decoration. Fractional scale is
enabled only when fractional-scale-v1 and viewporter are both available, with integer buffer-scale
as the fallback. The final pure Wayland/Vulkan client will not inherit an accessibility tree from a
GUI toolkit; screen-reader integration is outside the v0.1 commitment.
