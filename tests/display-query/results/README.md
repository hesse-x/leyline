# Display/query acceptance evidence

`run.sh --full` creates timestamped directories here. Stable `*-current` directories
are local baseline captures and may be replaced by a later run only when its metadata,
result hashes and logs are retained together.

Machine-generated evidence proves only the topology and assertions named in each
result. Scale, visual, long-soak and Vulkan validation acceptance must also include a
completed copy of `../manual-wayland-checklist.md`.
