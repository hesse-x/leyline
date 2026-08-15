# Terminal probe fixtures

Fixtures are raw byte streams passed directly to `alacritty_terminal`; they are never interpreted
by a shell. The built-in fixture covers ASCII, UTF-8, true color, wide and combining characters,
alternate screen, bracketed paste, mouse reporting, title events, and OSC 52 interception.

Pass an additional corpus entry with:

```text
leyline-probe terminal --terminal-fixture tests/fixtures/terminal/example.bin
```

Each added `.bin` file must document its source, license, and expected grid/mode/event state here.
