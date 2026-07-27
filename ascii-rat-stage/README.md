# ascii-rat-stage

Shared library behind [`ascii-rat-scribe`](../ascii-rat-scribe) and
[`ascii-rat-bard`](../ascii-rat-bard). You usually want those tools, not this
crate directly.

- `cast` — the asciicast v2 model (read/write `.cast` files).
- `script` — the `demo.yaml` script model (header, actions, key parsing).
- `pty` — spawn and drive child processes in a pseudo-terminal.
- `filters` — output post-processing passes (regex scrubbing, marker trimming).
- `util` — shared helpers.

Script format: [`format.md`](../format.md).
