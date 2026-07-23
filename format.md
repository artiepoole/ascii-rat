# The `demo.yaml` script format

An `ascii-rat` script is a single YAML document with a small header of
top-level fields followed by a list of `actions`. `ascii-rat-scribe` writes one
of these for you, and `ascii-rat-bard` reads one back to produce a `.cast`. This
page is the reference for every keyword the format accepts; for the narrative
walkthrough see the [README](README.md), and for the named-key vocabulary used
inside actions see [`keys.md`](keys.md).

A minimal script:

```yaml
output_file: "hello.cast"
cols: 100
rows: 30
typing_delay_ms: 75

actions:
  - "echo hello"
  - Enter:
  - Wait: 1.0
  - END_REC:
```

## Header fields

These keys sit at the top level of the document, alongside `actions`.

| Field | Required | Meaning |
| --- | --- | --- |
| **`output_file`** | yes | Path of the `.cast` to write, resolved relative to the script file's own directory (not the current directory). |
| **`actions`** | yes | The ordered list of actions to perform (see below). |
| **`cols`** | no | PTY width in columns. Defaults to the current terminal size. |
| **`rows`** | no | PTY height in rows. Defaults to the current terminal size. |
| **`with_comments`** | no | When `true`, `Comment` actions are rendered as caption events in the cast. When `false` (the default) any `Comment` actions are stripped before saving. |
| **`comments_at_top`** | no | When `true`, rendered comment captions are anchored at the top of the screen instead of the bottom. Only has an effect together with `with_comments: true`. Defaults to `false`. |
| **`sudo`** | no | Enables sudo password handling (see [Sudo](#sudo)). Omit it for scripts that need no privileges. |
| **`filters`** | no | A list of post-processing passes over the recorded events (see [Filters](#filters)). Defaults to none. |
| Delay fields | no | Timing controls, each with a seconds and a milliseconds spelling (see [Delay fields](#delay-fields)). |

## Delay fields

Six delays control the timing of a replay. Each can be written in seconds under
its plain name or in milliseconds under the `_ms` twin — use one spelling, not
both. Each value is either a single number, or a `[low, high]` list from which a
random value is picked per use, for a human-like feel.

| Field (seconds) | Field (ms) | Controls |
| --- | --- | --- |
| **`start_delay`** | `start_delay_ms` | Pause before the first action begins. |
| **`end_delay`** | `end_delay_ms` | Pause after the last action before the child is closed. |
| **`typing_delay`** | `typing_delay_ms` | Delay between characters when typing a string. |
| **`pre_nl_delay`** | `pre_nl_delay_ms` | Delay before the newline at the end of a typed line. |
| **`post_nl_delay`** | `post_nl_delay_ms` | Delay after that newline. |
| **`key_delay`** | `key_delay_ms` | Delay after each keypress sent by a `Key`/`Keys` action. Defaults to a small built-in value when omitted. |

Examples:

```yaml
typing_delay_ms: 75              # a fixed 75 ms between characters
typing_delay_ms: [30, 70]        # a random 30-70 ms, per character
end_delay: 1.5                   # 1.5 seconds, written in seconds instead of ms
```

## Actions

Every entry in the `actions` list is one action. Most are a single-key YAML
mapping named after the action; a plain string is the exception.

| Action | Form | What it does |
| --- | --- | --- |
| **Bare string** | `"echo hello"` | Typed character by character using the script's default delays. |
| **`Input`** | `Input: { text: "/", pre_nl_delay: 0.0, post_nl_delay: 0.2 }` | Like a bare string, but with explicit per-action `pre_nl_delay` and `post_nl_delay` (both required). |
| **`Marker`** | `Marker: "chapter one"` | Inserts an asciicast marker (chapter point); list them with `ascii-rat-bard --dont-run --print-markers`. |
| **`Comment`** | `Comment: "now list files"` | Inserts a caption/comment event (rendered only when `with_comments: true`). |
| **`InlineComment`** | `InlineComment: "# a note"` | Types the note on screen, flashes it, then wipes the line with `Ctrl-U` (see [InlineComment](#inlinecomment)). |
| **A named key** | `Enter:`, `Down: 6` | One keypress; an integer value repeats the key that many times. See [`keys.md`](keys.md). |
| **A modifier combo** | `Ctrl-C:`, `Shift-Tab:`, `Ctrl-Shift-Right:` | One keypress with modifiers held; a count works too. See [`keys.md`](keys.md#modifier-combinations). |
| **`Keys`** | `Keys: [Down, Enter]` | Sends several keys once each, in order. Combos work here too (`Keys: [Ctrl-O, Enter, Ctrl-X]`). |
| **`Wait`** | `Wait: 1.5` | Pauses for the given seconds while still capturing the child's output. |
| **`Expect`** | `Expect: "substr"` | Blocks until the substring appears in the child's output, then continues (see [Expect](#expect)). |
| **`END_REC`** | `END_REC:` | Ends the recording at this point; anything after it is ignored. |

Notes:

- A bare string with no leading `-` mapping key is always typed literally, so to
  send a named key you must use its mapping form (`Enter:`, not `"Enter"`).
- `END_REC:` is spelled with the underscore suffix so it never clashes with the
  `End` key. `End:` sends the End key; `END_REC:` stops the script.
- A repeat count on a key must be at least `1`; omitting the value (`Esc:` or
  `Esc: ~`) means a single press.

### Expect

`Expect` synchronizes on real output instead of guessing a duration: it pauses
until an expected substring shows up in the child's output, then continues.
Matching is case-insensitive, and output produced while waiting is still
captured into the cast (and, under `--watch`, mirrored to your terminal live as
it arrives). If the substring never appears the script gives up after a timeout
(default 30 seconds) rather than hanging.

```yaml
- "ascii-rat-bard --watch level2.yaml; echo __DONE__"
- Enter:
- Expect: "__DONE__"                       # continue only once the marker prints
```

To override the timeout, use the mapping form:

```yaml
- Expect: { text: "Server started", timeout: 60 }
```

`Expect` only watches output produced after it begins, so it deliberately
ignores the terminal's echo of the command line that triggered it. Put the
`Expect` after the `Enter` that submits the command whose output you await.

### InlineComment

`InlineComment` is a shortcut for flashing a throwaway note on screen: it types
the text, lets it linger, then wipes the whole input line with `Ctrl-U` before
continuing — nothing is ever submitted. It collapses the common
`Text + Wait + Ctrl-U + Wait` four-action pattern into one action. Unlike
`Comment` (a cast caption that is never typed), an `InlineComment` is real
terminal output the viewer sees typed and cleared.

```yaml
- InlineComment: "# welcome to the demo"    # typed, shown, then wiped with Ctrl-U
```

The text is typed verbatim, so include your own `# ` prefix if you want it to
read like a shell comment. The note lingers for a default `0.4` seconds before
and after it is wiped; use the mapping form to change how long it shows:

```yaml
- InlineComment: { text: "# read this first", show: 1.5 }
```

## Sudo

Set the top-level `sudo` field to have `ascii-rat-bard` answer sudo password
prompts for you. It asks once, at record time, for the password with a hidden
prompt, then types it (never storing it in the script or the `.cast`) whenever a
configured prompt appears.

Use `true` to match the built-in prompts (the substrings `assword` and `[sudo]`,
case-insensitively):

```yaml
sudo: true
```

Or give a mapping with your own `prompts:` list when your prompt differs:

```yaml
sudo:
  prompts:
    - "Password:"
    - "authentication required"
```

## Filters

The optional `filters` list applies post-processing passes over the recorded
events, in order. Each filter is a mapping identified by a `filter_id` field,
with any extra fields that filter needs alongside it:

```yaml
filters:
  - filter_id: RegexReplacementFilter
    regex: '\d+'
    replacement: '***'
  - filter_id: EndMarkerFilter
    end_label: "stop"
```

| `filter_id` | Extra fields | What it does |
| --- | --- | --- |
| **`RegexReplacementFilter`** | `regex`, `replacement` | Replaces text in the recorded output matching the regex. |
| **`StartMarkerFilter`** | `start_label` | Drops everything before the marker with the given label. |
| **`EndMarkerFilter`** | `end_label` | Drops everything after the marker with the given label. |
| **`CommentFilter`** | none | Renders `Comment` actions as caption events. |

You rarely need to set `filters` by hand: `with_comments` manages comment
rendering for you (it adds the comment filter when enabled and strips stray
comments when not). The marker and regex filters are there for trimming and
scrubbing a recorded script.

See the [`examples/`](examples) directory for full, ready-to-run scripts that
use these fields.
