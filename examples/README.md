# Examples

Hand-written example scripts for `ascii-rat-bard`. Each is a script you can
replay into a `.cast` file. Play any of them with:

```bash
ascii-rat-bard --watch examples/<name>.yaml
```

`--watch` mirrors the recording to your terminal as it happens. Drop it for a
silent run. Each script writes its `.cast` next to itself (the `output_file`
field is resolved relative to the script), so playing them here produces
`examples/<name>.cast`.

The examples assume `ascii-rat-bard` (and, for one of them, `ascii-rat-scribe`)
are on your `PATH`. Build the workspace and add the binaries to `PATH`, e.g.:

```bash
cargo build --release
export PATH="$PWD/target/release:$PATH"
```

Alternatively run the player straight from Cargo:

```bash
cargo run --release --bin ascii-rat-bard -- examples/hello-world.yaml
```

## The examples

| Script | What it shows |
| --- | --- |
| [`hello-world.yaml`](hello-world.yaml) | The smallest useful script — type two commands into a `bash` prompt and exit. Good starting point. |
| [`sudo-command.yaml`](sudo-command.yaml) | A privileged command via `sudo: true`; `ascii-rat-bard` prompts once for the password and types it when sudo asks. |
| [`scribe-records-htop.yaml`](scribe-records-htop.yaml) | A meta-demo: the replay drives `ascii-rat-scribe` recording an `htop` session, so the resulting cast shows how to use the recorder on `htop`. |
| [`demo-ception.yaml`](demo-ception.yaml) | The flagship "demo-ception": a single cast that records a new script with `ascii-rat-scribe`, edits it in `nano`, and replays it with `ascii-rat-bard`. See below. |

## `demo-ception.yaml` in detail

This is the flagship demo — a demo of making a demo. When replayed with
`ascii-rat-bard --watch demo-ception.yaml`, it drives a shell and, on screen,
uses `ascii-rat` itself to:

1. record a brand-new script with `ascii-rat-scribe`,
2. tweak that script by hand in `nano`, and
3. replay the edited script with `ascii-rat-bard` to produce a cast.

All three steps happen inside the one top-level cast, so a viewer watches
`ascii-rat` record, edit, and play back a recording without leaving it.

Requirements: `ascii-rat-scribe`, `ascii-rat-bard`, and `nano` must all be on
`PATH` at replay time. The inner recording is written to `inner-demo.yaml` /
`inner-demo.cast` in whatever directory you run the demo from (both are safe to
delete afterwards). A ready-made [`inner-demo.yaml`](inner-demo.yaml) sits next
to this demo as a reference of what that inner recording looks like once edited.

## `scribe-records-htop.yaml` in detail

This is the example that records the user of `ascii-rat-scribe` recording
`htop`. When replayed, `ascii-rat-bard` opens a shell and runs:

```bash
ascii-rat-scribe -o htop.yaml --cast htop.cast -- htop
```

It then scrolls around `htop` for a few seconds, presses `q` to quit (which
ends the recorded command and makes `ascii-rat-scribe` write `htop.yaml`), and
exits the shell. The produced `.cast` is therefore a walkthrough of using the
recorder on `htop`.

Requirements: both `ascii-rat-scribe` and `htop` must be on `PATH`. If
`ascii-rat-scribe` is not on `PATH`, edit the command line inside the script to
use the full path to the binary (for example
`target/release/ascii-rat-scribe`).

## Editing these

These scripts are plain YAML — copy one, change the `actions`, and replay. See
the script format section in the [top-level README](../README.md) for every
action form (`Marker`, `Comment`, named keys, `Wait`, `END_REC`, timing/header
fields, and `sudo`).
