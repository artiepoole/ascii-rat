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

Script format: [`format.md`](../format.md).
