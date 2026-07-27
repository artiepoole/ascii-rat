# ascii-rat-bard

Replay a `demo.yaml` script (from [`ascii-rat-scribe`](../ascii-rat-scribe) or
hand-written) into an asciicast v2 `.cast` file.

```
ascii-rat-bard [OPTIONS] <script_file>
```

| Option | Meaning |
| --- | --- |
| `-w`, `--watch` | Mirror the screen live while recording. |
| `-q`, `--quiet` | No per-action progress. |
| `-d`, `--dont-run` | Don't record (use with `--print-markers`). |
| `-m`, `--print-markers` | Print the cast's markers as a Markdown list. |
| `--data-id <ID>` | HTML element id for the video element (with `--print-markers`). |

Script format: [`format.md`](../format.md). Key names: [`keys.md`](../keys.md).
