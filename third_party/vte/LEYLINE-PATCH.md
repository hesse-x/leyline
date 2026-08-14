# Leyline vte patch

This directory vendors `vte 0.15.0` from crates.io (registry checksum
`a5924018406ce0063cd67f8e008104968b74b563ee1b85dde3ed1f7cb87d3dbd`).

Leyline's patch is deliberately limited to the parser security boundary:

- use the const-generic `ArrayVec` OSC buffer under both `std` and `no_std`;
- raise the product OSC limit to 16 KiB;
- discard an entire overflowing OSC until BEL/ST and report metadata-only counters;
- expose truncation, rejection, and unknown-sequence counters through `ansi::Processor`;
- remove unhandled-sequence logs that included untrusted payload bytes.

The original Apache-2.0/MIT license files are retained. Replace this patch with
an upstream release only after the bounded-storage, chunking, resynchronization,
and no-partial-dispatch contract tests pass unchanged.
