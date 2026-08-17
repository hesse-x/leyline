# Display/query manual Wayland acceptance

Record every run below under `tests/display-query/results/<case-id>/`. Do not mark the
product worklist complete from the scripted baseline alone.

## Environment record

- Leyline binary SHA-256, commit/worktree identity, compositor, GPU/driver, tmux,
  Vim/Neovim and Vulkan SDK versions.
- Output name, logical resolution, effective scale and `WAYLAND_DISPLAY`.
- Start/end timestamps, fixture byte count, peak RSS, FD count and validation-layer
  error count.

## Required matrix

- Run `bash tests/display-query/run.sh --full` at 1x, 1.25x, 1.5x and 2x.
- At each scale, resize continuously and verify CSI 14 remains exactly
  `columns * physical_cell_width` by `rows * physical_cell_height`; verify CSI 18
  matches `stty size` after the resize settles.
- Move the window between differently scaled outputs while issuing CSI 14/18 and
  confirm each reply belongs wholly to either the old or committed new layout.
- In Vim/Neovim, verify normal/insert/replace cursor shape and blinking; repeat in
  local tmux and `ssh 127.0.0.1 -> remote tmux`, including detach/attach.
- Inspect single, double, curly, dotted and dashed underline at every scale, including
  wide Unicode cells and clipped right-edge cells.
- Run with `VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation`; combine underline,
  blink, resize and tab switching, and archive zero validation errors.
- Run the live query/sync flood for 30 minutes. Sample Leyline RSS and `/proc/PID/fd`
  once per minute; require a plateau after warm-up and archive the samples. RSS is
  supporting evidence only--the automated internal-limit assertions remain the
  source of truth for the 2 MiB/64 KiB buffer limits.

## Completion rule

All matrix rows must identify their evidence files and pass without unexplained
timeouts, stale replies, partial synchronized frames, cross-tab state leakage,
unbounded RSS/FD growth or Vulkan validation errors.
