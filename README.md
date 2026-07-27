# ascii-rat

A small toolkit for scripted, reproducible terminal recordings in the
[asciicast v2](https://docs.asciinema.org/manual/asciicast/v2/) (`.cast`)
format used by https://asciinema.org/.

Instead of recording live and hoping you don't fat-finger a command, capture a
session once into an editable script (or write the script by hand), then replay
it deterministically into a clean `.cast`. Replay drives a real child process
in a PTY, so full-screen TUIs render correctly and `sudo` works.

## Key features

What sets it apart from other scripted-recording tools:

- **Record to YAML** — capture a live session into an editable script, retime and trim by hand, replay deterministically.
- **Real PTY** — full-screen TUIs (`htop`, `nano`) render correctly; not a faked terminal.
- **Key-timing jitter** — any delay can be a `[low, high]` random range for human-like typing.
- **`sudo` passthrough** — answers password prompts during recording without storing the password.
- **Modifier keys** — `Ctrl-O`, `Shift-Tab`, `Alt-x` and friends drive editors and TUIs.
- **Key shorthand** — `Down: 6` repeats a keypress; `Keys: [Ctrl-O, Enter, Ctrl-X]` sends a sequence.

Also table stakes, supported here too:

- **Sync on output** — `Expect` waits for what the terminal actually prints instead of guessed `Wait`s.
- **Output filters** — regex-scrub secrets and trim to markers after recording.
- **Markers & captions** — asciicast chapter markers, plus `Comment`/`InlineComment` captions.
- **Live watch** — `--watch` mirrors the recording to your terminal as it happens.

## Demo

`ascii-rat` recording itself, one level at a time — each recording captures the
level below it. Click any thumbnail to play it on asciinema.org.

### Level 0 — recording the whole thing by hand

A live session (driven by the commands in
[`examples/level0.cmds`](examples/level0.cmds)) that runs the level 1 demo and
records it with plain `asciinema rec`.

[![asciicast](https://asciinema.org/a/1261455.svg)](https://asciinema.org/a/1261455)

### Level 1 — the demo-ception script

[`examples/level1.yaml`](examples/level1.yaml) replayed with
`ascii-rat-bard`: it drives `ascii-rat-scribe` to record a new script, edits it
in `nano`, then replays the edited script with `ascii-rat-bard`.

[![asciicast](https://asciinema.org/a/1261456.svg)](https://asciinema.org/a/1261456)

### Level 2 — the script recorded during the demo

The innermost recording — `level2.yaml` — captured by `ascii-rat-scribe` and
replayed by `ascii-rat-bard` from inside the level 1 demo.

[![asciicast](https://asciinema.org/a/1261457.svg)](https://asciinema.org/a/1261457)

## Structure

Cargo workspace, three crates:

| Crate | Role |
| --- | --- |
| [`ascii-rat-stage`](ascii-rat-stage) | Shared library: asciicast model, script model, PTY handling, output filters. |
| [`ascii-rat-scribe`](ascii-rat-scribe) | Recorder: runs a command in a PTY, turns what you type into a `demo.yaml` script. |
| [`ascii-rat-bard`](ascii-rat-bard) | Player: replays a `demo.yaml` script into a `.cast` file. |

```
you (live) ──▶ ascii-rat-scribe ──▶ demo.yaml ──(edit by hand)──▶ ascii-rat-bard ──▶ demo.cast
```

Writing `demo.yaml` from scratch also works — scribe just gives a realistic
starting point.

## Building

Needs a recent stable [Rust toolchain](https://rustup.rs/) (edition 2024).
Builds on Linux, macOS, and Windows (ConPTY); the `sudo:` feature and the
bundled examples are Linux(Unix?)-only.

```bash
cargo build --release   # binaries in target/release/
cargo test
```

## Usage

### Record: `ascii-rat-scribe`

```
ascii-rat-scribe [OPTIONS] [-- <command> [args...]]
```

Runs the command in a PTY, forwards your keystrokes, and writes the script.
No command = records `bash`. Quit the command (or `exit` the shell) to write
the script.

| Option | Default | Meaning |
| --- | --- | --- |
| `-o`, `--output <FILE>` | `demo.yaml` | Script output path. |
| `--cast <FILE>` | `demo.cast` | `output_file` recorded into the script. |
| `--cols <N>` / `--rows <N>` | current terminal | PTY size. |

Tuning knobs (`--wait-threshold-ms`, `--round-wait-ms`, `--typing-delay-ms`):
[`ascii-rat-scribe/README.md`](ascii-rat-scribe/README.md) or `--help`.

```bash
ascii-rat-scribe -o htop.yaml --cast htop.cast -- htop
```

For a clean prompt without your shell config leaking in:

```bash
ascii-rat-scribe -- env PS1='$ ' bash --norc --noprofile -i
```

### Replay: `ascii-rat-bard`

```
ascii-rat-bard [OPTIONS] <script_file>
```

Output path comes from the script's `output_file`, resolved relative to the
script.

| Option | Meaning |
| --- | --- |
| `-w`, `--watch` | Mirror the screen live while recording. |
| `-q`, `--quiet` | No per-action progress. |
| `-d`, `--dont-run` | Don't record (use with `--print-markers`). |
| `-m`, `--print-markers` | Print the cast's markers as Markdown. |

Full list: [`ascii-rat-bard/README.md`](ascii-rat-bard/README.md) or `--help`.

```bash
ascii-rat-bard --watch examples/hello-world.yaml
asciinema play examples/hello-world.cast
```

### `sudo`

Add a top-level `sudo: true` to the script and bard asks for the password once
(hidden prompt) before recording, then types it when a sudo prompt appears
(matches `assword` / `[sudo]`, case-insensitive). Custom prompts:

```yaml
sudo:
  prompts:
    - "Password:"
```

The password is never written to the script or the `.cast`. See
[`examples/sudo-command.yaml`](examples/sudo-command.yaml).

## Script format (`demo.yaml`)

Header + list of `actions`:

```yaml
output_file: "hello.cast"    # relative to this file
cols: 100
rows: 30
typing_delay_ms: 75          # or [low, high] for a human-like random range
sudo: true

actions:
  - Marker: "say hello"      # asciicast marker (chapter point)
  - "echo hello"             # bare string: typed as-is
  - Enter:                   # named key; add a count to repeat (Down: 6)
  - Wait: 1.0                # pause, keep capturing output
  - Comment: "now list files"
  - "ls -la"
  - Enter:
  - Wait: 1.5
  - END_REC:                 # stop here; rest is ignored
```

Action forms:

- **Bare string** → typed character by character.
- **`Marker:`** → asciicast marker; list with `--dont-run --print-markers`.
- **`Comment:`** / **`InlineComment:`** → caption event / note typed on screen
  then wiped with `Ctrl-U`.
- **Named key** (`Enter:`, `Esc:`, `Down:`, …) and **combos** (`Ctrl-C:`,
  `Shift-Tab:`, …) → one keypress. Full list in [`keys.md`](keys.md).
- **`Keys: [Down, Enter]`** → several keys in order.
- **`Wait: <seconds>`** → fixed pause.
- **`Expect: "<substr>"`** → block until the substring appears in output
  (case-insensitive, 30s default timeout; `{ text: ..., timeout: 60 }` to
  override). Watches only output produced after it begins — put it after the
  `Enter` that submits the command.
- **`END_REC:`** → end recording.

Header notes:

- Every delay is a number or `[low, high]`; spell in seconds (`typing_delay:`)
  or ms (`typing_delay_ms:`). Available: `start_delay`, `end_delay`,
  `typing_delay`, `pre_nl_delay`, `post_nl_delay`, `key_delay`.
- `with_comments: true` renders `Comment` captions; `comments_at_top: true`
  anchors them at the top.
- `filters:` — post-processing passes (regex scrubbing, marker trimming), see
  [`format.md`](format.md#filters).

Full reference: [`format.md`](format.md). Ready-to-run scripts:
[`examples/`](examples) (documented in [`examples/README.md`](examples/README.md)).

## A note on the name

Written to demo `snap-rat`, a Ratatui-based snap-store TUI that needs `sudo` — a combination existing recorders didn't
cover. Hence `-rat`, and the theatrical crates: the stage they share, the
scribe who records, the bard who performs.
