# Leyline tmux baseline prototype

This disposable Stage 1 prototype freezes the contract for the future Stage 2
black-box harness. It does not enter the default build or constitute tmux support
evidence by itself. It never uses the default tmux socket, reads no user tmux
configuration, and removes only its registered temporary directory and sockets.

Local gates:

```sh
tests/tmux/harness.sh local
tests/tmux/harness.sh nested
tests/tmux/harness.sh recursive 3
bash tests/tmux/extended-keys-run.sh
bash tests/tmux/terminfo-prototype-run.sh
```

The remaining automatable real-Wayland TUI/detach and OSC 52 content checks use an
outer isolated tmux to supervise Leyline, then run another isolated tmux inside
Leyline's PTY:

```sh
bash tests/tmux/wayland-release-run.sh
```

The inner probe attaches and detaches two real tmux clients around a persistent
Vim alternate-screen scene, verifies the captured state is unchanged, then
installs Clipboard/Primary sentinels and checks after every direct and
tmux-generated OSC 52 attempt that both selections retain their baseline
content. Wayland does not expose selection-owner identity, and GNOME's clipboard
manager may asynchronously replace a `wl-copy` source, so process identity is
recorded only as diagnostics rather than treated as a release assertion.
Clipboard text is restored at cleanup, although the previous Wayland owner
identity cannot be restored. Only physical compositor keyboard, mouse, focus,
resize, and fractional-scale interaction remains a human gate.

Remote gates require an explicit target, identity, and pre-populated `known_hosts` file. SSH uses `-F /dev/null`, disables the agent, requires the supplied identity, and refuses unknown host keys:

```sh
tests/tmux/harness.sh remote user@host 22 /path/to/id_ed25519 /path/to/known_hosts
tests/tmux/harness.sh loopback localhost 22 /path/to/id_ed25519 /path/to/known_hosts
tests/tmux/harness.sh local-remote user@host 22 /path/to/id_ed25519 /path/to/known_hosts
```

`remote` is the `R1` entry and must point to a host that does not share Leyline's rootfs, HOME, terminfo database, or tmux server. A loopback target is recorded separately as `RS1`; it cannot replace `R1`. `local-remote` is the `LR1` entry. The recursive depth is a test sample, not a product limit.

Remote cases stream `remote-probe.sh` over SSH instead of assuming the remote
host shares this checkout. The remote probe creates its own temporary config and
socket, reports the remote terminfo/config hashes, and reports scoped cleanup
before the case can pass. `remote-client.sh` bounds all remote output to 64 KiB
before the harness reads it and records an explicit failure for timeout, SSH, or
oversized-output cases. RS1, R1, and LR1 also require the remote pane and kernel
winsize to converge on `97x31`, then verify that closing one session does not
terminate the case-owned server.

The local and recursive cases feed the frozen F1 fixture (`1b4f50`, SS3 `P`)
through a raw pane and verify exact byte preservation. Recursive cases attach each
inner tmux client to its parent pane, so the fixture traverses every tmux layer.
The local case also drives three rapid window sizes and requires both tmux's pane
size and the pane's kernel `stty size` to converge on the final `97x31` value. It
then closes one isolated session and verifies that a second detached session and
the case-owned server remain alive. Finally, it attaches a real pseudo-terminal
client twice, sends the default detach chord, and verifies that the session and
server survive with zero attached clients. A two-pane window must also preserve
both kernel sizes across split/zoom/unzoom, while a detached pane emits 1 MiB and
the tmux control path remains responsive.
The prototype emits shell-escaped `key=value` records and never records terminal
body or clipboard payloads. The real-Wayland release runner covers the remaining
automatable TUI lifecycle and OSC 52 content checks. Physical compositor input,
mouse, focus, resize, and fractional-scale checks remain interactive gates.

The emitted metadata deliberately reports `source_tree_hash=not-captured` and
classifies each standalone run as `exploratory`; authoritative conclusions use
the recorded evidence bundle, environment identity, case IDs, and structured
oracles. Git commit/working-tree cleanliness is not a decision or release gate.

`loopback-run.sh` is the self-contained RS1 entry. It starts a case-owned sshd
on `127.0.0.1`, creates ephemeral client/host keys and strict `known_hosts`, runs
the loopback harness, and removes only its registered PID and temporary files:

```sh
tests/tmux/loopback-run.sh
```

`isolated-remote-run.sh` builds a minimal copied rootfs, enters it through a
forced-command SSH key and bubblewrap, and runs both R1 and LR1. The container
has case-owned HOME, machine identity, terminfo database, tmux sockets, SSH
keys, and host key; it shares only the host kernel and loopback transport:

```sh
tests/tmux/isolated-remote-run.sh
```

The extended-keys gate attaches real clients with tmux `extended-keys` set to
`off`, `on`, and `always`. Because `xterm-256color` does not declare `extkeys`,
none may emit the modifyOtherKeys enable sequence, and all must preserve F1 as
`1b4f50`. The terminfo prototype separately compiles a temporary
`leyline-256color` entry, verifies its RGB/F1 declarations, and proves that a
client with the entry missing fails explicitly without terminating its server.

The loopback runner also executes negative SSH cases for host-key mismatch,
authentication failure, timeout, output overflow, missing dependencies,
malformed structured output, and interrupted-probe orphan handling. Any orphan
risk is recorded before cleanup is limited to the exact registered socket and
temporary directory.

The real GNOME Wayland L1 transport entry opens Leyline briefly, attaches a
case-owned tmux client, records TERM and size state from its pane, then exits:

```sh
cargo build --locked -p leyline
tests/tmux/wayland-run.sh
```

The GNOME Terminal/VTE reference runs the same attached pane probe with an
isolated tmux socket and config:

```sh
tests/tmux/reference-vte-run.sh
```

The non-VTE reference uses foot with an empty config and the same attached pane
probe:

```sh
tests/tmux/reference-foot-run.sh
```

The headless product snapshot gate is `cargo run --locked --bin leyline-probe -- scene`.
`scene --terminal-fixture PATH` replays a bounded tmux scene fixture through Leyline's terminal
adapter. Its fixture contract covers pane/status text, combining and CJK wide characters,
256-color attributes, focus, SGR mouse, Bracketed Paste, title, and rejected OSC 52. It checks
structured cells and interaction bytes rather than treating a screenshot as the oracle.

`scene-fixture-run.sh` captures a real isolated tmux client with two panes, pane alternate
screen, copy-mode, and a styled status line; it trims only the typescript header and detach
teardown, then replays those bytes through the same product scene oracle. Because a pseudo-terminal has no compositor focus
source and tmux consumes OSC 52 internally, the runner appends non-rendering focus and
security sentinels after the captured tmux scene. Every run records the bounded fixture size
and SHA-256 while deleting the fixture during scoped cleanup:

```sh
cargo build --locked -p leyline-probe
bash tests/tmux/scene-fixture-run.sh
```
