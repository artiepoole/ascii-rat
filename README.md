# ascii-rat

`ascii-rat` is a small toolkit for producing scripted, reproducible terminal
recordings in the [asciicast v2](https://docs.asciinema.org/manual/asciicast/v2/)
(`.cast`) format used by https://asciinema.org/. 

Instead of recording a live session and hoping you don't fat-finger a command,
you either capture a session once and turn it into an editable script, or
write the script by hand, and then replay that script deterministically to
produce a clean `.cast` file. The replay drives a real child process inside a
PTY, so full-screen TUIs render correctly and commands that need `sudo` work.

## Structure

The project is a Cargo workspace of three crates:

| Crate | Role | Kind |
| --- | --- | --- |
| [`ascii-rat-stage`](ascii-rat-stage) | **Shared library** — the asciicast model, the scripted-action model, PTY handling, output filters, and shared utilities. Both tools build on top of it. | `lib` |
| [`ascii-rat-scribe`](ascii-rat-scribe) | **Recorder of inputs** — runs a command in a PTY, watches what you type, and writes an editable `demo.yaml` script. | `bin` |
| [`ascii-rat-bard`](ascii-rat-bard) | **Player of inputs** — reads a `demo.yaml` script and re-drives the session, recording it into an asciicast `.cast` file. | `bin` |

The typical flow:

```
you (live) ──▶ ascii-rat-scribe ──▶ demo.yaml ──(edit by hand)──▶ ascii-rat-bard ──▶ demo.cast
```

You can also skip the recorder entirely and write `demo.yaml` from scratch —
`ascii-rat-scribe` just gives you a realistic starting point.

## A note on the name

`ascii-rat` was written to produce a demo of a TUI called `snap-rat`, which
manages snaps from a full-screen terminal interface and needs `sudo` for
privileged operations. `snap-rat` was so-called because it uses Ratatui as the TUI framework and is a snap store applications. Recording that demo required a tool that could handle
both a real TUI and a `sudo` prompt — a combination the existing recording
options didn't cover well — so `ascii-rat` was built to fill the gap. Hence the
`-rat` suffix and the theatrical crate names: the shared stage, the
scribe who records what happens, and the bard who performs it back.

## Building

You need a recent stable [Rust toolchain](https://rustup.rs/) (edition 2021).

Build everything in the workspace:

```bash
cargo build --release
```

The two binaries are produced at:

```
target/release/ascii-rat-scribe
target/release/ascii-rat-bard
```

You can run them straight from Cargo during development, e.g.:

```bash
cargo run --release --bin ascii-rat-bard -- demo.yaml
```

Run the test suite:

```bash
cargo test
```

## Usage

### `ascii-rat-scribe` — record a session into a script

`ascii-rat-scribe` runs a command inside a PTY, forwards your keystrokes to it
(mirroring its output to your terminal), and translates what you typed — plus
the idle gaps between keystrokes — into a `demo.yaml` script.

```
ascii-rat-scribe [OPTIONS] [-- <command> [args...]]
```

Everything after `--` is the command line to run and record. If you omit the
command entirely, `ascii-rat-scribe` records `bash`, giving you a clean terminal
you can type into.

Key options:

| Option | Default | Meaning |
| --- | --- | --- |
| `-o`, `--output <FILE>` | `demo.yaml` | Where to write the produced script. |
| `--cast <FILE>` | `demo.cast` | The `output_file` recorded into the script (the `.cast` that `ascii-rat-bard` will later produce). |
| `--wait-threshold-ms <MS>` | `500` | Idle time after which a gap becomes an explicit `Wait` action. |
| `--round-wait-ms <MS>` | `500` | Round each recorded `Wait` to the nearest this many milliseconds. `0` keeps millisecond-precise waits. |
| `--typing-delay-ms <MS>` | `75` | `typing_delay_ms` written into the script header. |
| `--cols <N>` / `--rows <N>` | current terminal | PTY size to record at. |

#### Controlling how idle gaps become `Wait` actions

Two flags control how the pauses in your recording are turned into `Wait`
actions in the script:

`--wait-threshold-ms` sets the minimum gap that is recorded at all (the
"min-timeout"). Any idle pause shorter than this is ignored, so quick pauses
between keystrokes do not clutter the script with tiny waits; only gaps of at
least this many milliseconds become a `Wait`. It defaults to `500` (matching the
default rounding). Lower it to capture shorter pauses, or raise it to record only
the longer, deliberate ones:

```bash
ascii-rat-scribe --wait-threshold-ms 1000 -- htop   # only record pauses of 1s+
```

`--round-wait-ms` snaps each recorded `Wait` to the nearest multiple of the
given number of milliseconds, so the script reads in tidy, predictable steps
rather than values like `1.732`. It defaults to `500` (round to the nearest half
second): a 1.7s pause is written as `Wait: 1.5`. Change the granularity, or pass
`0` to disable rounding and keep the exact millisecond-precise waits:

```bash
ascii-rat-scribe --round-wait-ms 1000 -- htop       # round waits to whole seconds
ascii-rat-scribe --round-wait-ms 0 -- htop          # keep exact waits, no rounding
```

Example — capture an interactive `htop` session:

```bash
ascii-rat-scribe -o htop.yaml --cast htop.cast -- htop
```

Do whatever you want to demo, quit the program normally, and `ascii-rat-scribe`
writes `htop.yaml`. Open it, trim it, add `Marker`/`Comment` lines, and adjust
timings before handing it to `ascii-rat-bard`.

#### Recording a clean terminal you can type into

`ascii-rat-scribe` records whatever command you put after `--`. To get a blank
prompt you can freely type any commands into — rather than a single fixed
program like `htop` — record a shell as the command. Because `bash` is the
default, running the recorder with no command at all does exactly this:

```bash
ascii-rat-scribe
```

which is equivalent to naming the shell explicitly:

```bash
ascii-rat-scribe -o session.yaml --cast session.cast -- bash
```

This drops you into a normal `bash` prompt inside the recorder. Type any
commands you like; everything you type (and the pauses between keystrokes) is
captured. When you are finished, exit the shell (`exit` or `Ctrl-D`) and the
script is written.

For a genuinely clean recording, start the shell without your personal
configuration so a custom prompt, aliases, or shell history do not leak into the
demo:

```bash
ascii-rat-scribe -o session.yaml --cast session.cast -- bash --norc --noprofile -i
```

`--norc --noprofile` skip your `~/.bashrc`/profile (giving a plain `$` prompt),
and `-i` forces an interactive shell. You can pin the prompt explicitly and set
a predictable size, e.g.:

```bash
ascii-rat-scribe --cols 100 --rows 30 -- env PS1='$ ' bash --norc --noprofile -i
```

Any shell works — swap `bash` for `zsh`, `fish`, `sh`, etc. If you only want to
demo one program, run it directly (as in the `htop` example above) instead of
going through a shell.

### `ascii-rat-bard` — replay a script into a `.cast`

`ascii-rat-bard` reads a script and re-drives the session inside a PTY,
recording it as an asciicast v2 `.cast` file. The output path comes from the
script's `output_file` field and is resolved relative to the script's directory.

```
ascii-rat-bard [OPTIONS] <script_file>
```

Key options:

| Option | Meaning |
| --- | --- |
| `-w`, `--watch` | Mirror the recorded screen to your terminal live while recording. |
| `-q`, `--quiet` | Don't print per-action progress. |
| `-d`, `--dont-run` | Don't record (useful together with `--print-markers`). |
| `-m`, `--print-markers` | Print the cast's markers as a Markdown list. |
| `--data-id <ID>` | HTML element id to associate with the marker list (used with `--print-markers`). |

Example — record the demo script shipped in this repo and watch it happen:

```bash
ascii-rat-bard --watch demo.yaml
```

If the script has a top-level `sudo:` block, `ascii-rat-bard` prompts once for
the sudo password (hidden) before recording starts. The password is never
stored in the script or the `.cast` — it is typed into the child only when a
sudo prompt appears.

Play the finished recording back with any asciicast player, e.g.:

```bash
asciinema play demo.cast
```

## The script format (`demo.yaml`)

A script has a small header followed by a list of `actions`. Here is a minimal
hand-written example:

```yaml
output_file: "hello.cast"    # where the .cast is written (relative to this file)
cols: 100                    # PTY width
rows: 30                     # PTY height
typing_delay_ms: 75          # per-character typing delay
sudo: true                   # prompt for a sudo password before recording

actions:
  - Marker: "say hello"      # inserts an asciicast marker
  - "echo hello"             # a bare string is typed as-is
  - Enter:                   # a named key
  - Wait: 1.0                # pause (and keep capturing output) for 1 second
  - Comment: "now list files"
  - "ls -la"
  - Enter:
  - Wait: 1.5
  - "q"
  - END_REC:                 # stop the recording exactly here
```

Notable action forms:

- **Bare string** → typed character by character (`"echo hello"`).
- **`Marker:`** → an asciicast marker (chapter point); list them with
  `ascii-rat-bard --dont-run --print-markers`.
- **`Comment:`** → a caption/comment event.
- **A named key** (`Enter:`, `Esc:`, `Down:`, `Tab:`, …) → one keypress; add a
  count to repeat it (`Down: 6`, `Esc: 2`).
- **`Keys: [Down, Enter]`** → send several named keys once each, in order.
- **`Wait: <seconds>`** → pause while still capturing the child's output.
- **`END_REC:`** → end the recording; anything after it is ignored.

Timing/header fields:

- Every delay field can be a single number or a `[low, high]` range (a random
  delay is picked in that range for a human-like feel).
- Each delay can be spelled in seconds (`typing_delay:`) or milliseconds
  (`typing_delay_ms:`) — use one, not both.
- Available delays: `start_delay`, `end_delay`, `typing_delay`, `pre_nl_delay`,
  `post_nl_delay`, `key_delay` (each also has a `_ms` form).
- `sudo:` accepts `true` (use the built-in prompt matchers) or a mapping with a
  custom `prompts:` list.

See [`demo.yaml`](demo.yaml) in the repository root for a full, real-world
example (the original `snap-rat` demo).
