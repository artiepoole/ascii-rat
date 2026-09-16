# ascii-rat-scribe

Record a live terminal session into a `demo.yaml` script for
[`ascii-rat-bard`](../ascii-rat-bard) to replay.

```
ascii-rat-scribe [OPTIONS] [-- <command> [args...]]
```

| Option | Default | Meaning |
| --- | --- | --- |
| `-o`, `--output <FILE>` | `demo.yaml` | Where to write the produced script. |
| `--cast <FILE>` | `demo.cast` | `output_file` recorded into the script. |
| `--wait-threshold-ms <MS>` | `500` | Minimum idle gap that becomes a `Wait` action. |
| `--round-wait-ms <MS>` | `500` | Round each `Wait` to this many ms; `0` = exact. |
| `--typing-delay-ms <MS>` | `75` | `typing_delay_ms` written into the script header. |
| `--cols <N>` / `--rows <N>` | current terminal | PTY size. |
| `--sudo` | off | Answer password prompts during the recording (see below). |
| `--sudo-prompt <SUBSTRING>` | built-in prompts | Prompt substring that triggers the password; repeatable. Implies `--sudo`. |

## Recording a program that needs `sudo`

`--sudo` asks once for the password (hidden) before recording starts, then types
it into the child whenever a configured prompt appears. Because the recorder
types it, the password never passes through the keystroke decoder and so cannot
be transcribed into the script:

```bash
ascii-rat-scribe --sudo -o demo.yaml --cast demo.cast -- sudo your-app
```

The produced script gets `sudo: true`, so `ascii-rat-bard` asks for the password
itself and does the same thing on replay. Nothing is written to disk either way.

Prompts are matched case-insensitively; the defaults (`assword` and `[sudo]`)
cover the standard `[sudo] password for user:`. For a program with its own
authentication widget, give your own:

```bash
ascii-rat-scribe --sudo-prompt 'authentication required' -- your-app
```

Things worth knowing:

- **A prompt that is never matched is not answered.** The program will just sit
  there waiting and you will type the password by hand — which *is* transcribed.
  Scribe checks the captured keystrokes for the password before writing the file
  and removes it if found, warning you to adjust `--sudo-prompt`. Rely on the
  matching, not the safety net.
- **Only the first prompt is answered on replay.** Scribe answers every prompt it
  sees, but `ascii-rat-bard` answers one per replay, so it will stall at a second
  prompt. Scribe warns when it answers more than once.
- **`--sudo` needs a real terminal** for the hidden prompt, and is Unix-only.
- Scribe measures idle gaps between *your* keystrokes, so the time spent
  prompting and authenticating becomes a `Wait` action. Bard also spends time
  typing the password there, so you may want to trim that `Wait` by hand.

### How the password is held

The password is never written to the script, the `.cast`, a file, an environment
variable, or an argument vector. It is kept in memory, in a wrapper that
overwrites the bytes when it is dropped, so the plaintext is not left behind in
freed memory.

The exposure window is nevertheless the whole recording: unlike `sudo`, which
uses a password immediately and discards it, the prompt being answered here may
not appear until minutes into the session, so the password has to be retained
until then.

What this does *not* protect against:

- **Core dumps.** If scribe crashes with core dumps enabled, the password can be
  written to disk in the dump. Consider `ulimit -c 0`.
- **Swap.** Memory holding the password may be paged out; the pages are not
  locked.
- **Anything that can read the process's memory**, such as `/proc/<pid>/mem`,
  which is open to the same user and to root.
- **Copies made before the password reached the wrapper.** `rpassword` returns a
  plain `String`, and one of its input-fixup paths reallocates, so a residual
  copy may exist that nothing can reach to wipe.

If that residual risk matters for your threat model, prefer a scoped
`NOPASSWD` sudoers rule for the command you are demonstrating and record without
`--sudo` at all — then there is no password to hold.

Script format: [`format.md`](../format.md).
