# Special keys

`ascii-rat` scripts can send named keys as well as literal text. Use a named key
anywhere an action is expected, either on its own:

```yaml
- Down:
- Enter:
- PageDown:
```

or as a repeated key (an integer count) and a one-shot sequence of keys:

```yaml
- Down: 6          # press Down six times
- Keys: [Down, Down, Enter]
```

Names are matched case-insensitively (`Down`, `down`, and `DOWN` are all the
same key), and several keys accept short aliases (for example `PgDn` for
`PageDown`). Any name not in the table below is not a special key; if you need to
send a literal character, type it as text instead.

## Supported keys

The following names are recognised. The "Aliases" column lists the alternative
spellings accepted for the same key; the "Sends" column shows the byte sequence
delivered to the recorded program.

| Key | Aliases | Sends |
| --- | --- | --- |
| **`Up`** | — | SS3 up (`ESC O A`) |
| **`Down`** | — | SS3 down (`ESC O B`) |
| **`Right`** | — | SS3 right (`ESC O C`) |
| **`Left`** | — | SS3 left (`ESC O D`) |
| **`Enter`** | `Return`, `CR` | Carriage return (`\r`) |
| **`Esc`** | `Escape` | Escape (`ESC`) |
| **`Tab`** | — | Tab (`\t`) |
| **`Backspace`** | `BS` | Delete/backspace (`0x7f`) |
| **`Delete`** | `Del` | `ESC [ 3 ~` |
| **`Home`** | — | `ESC [ H` |
| **`End`** | — | `ESC [ F` |
| **`PageUp`** | `PgUp` | `ESC [ 5 ~` |
| **`PageDown`** | `PgDn`, `PgDown` | `ESC [ 6 ~` |
| **`Space`** | — | A single space (` `) |

Note that `End` the key is distinct from `END_REC:`, the action that ends a
recording. `End` sends the End key to the program; `END_REC:` stops the script.

## Arrow-key encoding

The arrow keys use the SS3 (`ESC O <letter>`) encoding rather than the more
common CSI (`ESC [ <letter>`) form, to match how the original `snap-rat` demo was
recorded. Most full-screen programs accept both, so this rarely matters; if a
program does not respond to an arrow key, its application-cursor-key mode may
expect the CSI form.

## Modifier combinations

Any key can be pressed with modifiers held. Write the modifiers as a prefix
joined to the key with `-`, for example:

```yaml
- Ctrl-C:                 # send Ctrl-C
- Ctrl-U: 1               # kill the current line (a count works too)
- Shift-Tab:              # back-tab
- Ctrl-Shift-Right:       # word-select right in many TUIs
- Keys: [Ctrl-O, Enter, Ctrl-X]   # nano: write out, confirm, then quit
```

The recognised modifier tokens are `Ctrl` (also `Control`), `Alt` (also `Meta`,
`Option`), and `Shift`. They may be combined in any order and are matched
case-insensitively, so `Ctrl-Shift-Right`, `shift-ctrl-right`, and
`CTRL-SHIFT-RIGHT` are the same key. The base key is either one of the named
keys in the table above or a single printable character (as in `Ctrl-C` or
`Alt-x`). A bare character on its own is not a key — type it as text instead.

### How each combination is encoded

The byte sequence sent to the program is computed from the base key and the
modifiers:

| Combination | Sends |
| --- | --- |
| **`Ctrl-<letter>`** | The C0 control byte, e.g. `Ctrl-C` → `0x03`, `Ctrl-U` → `0x15`, `Ctrl-A` → `0x01`. |
| **`Ctrl-Space`** | `NUL` (`0x00`). |
| **`Alt-<key>`** | `ESC` followed by the key's own bytes, e.g. `Alt-x` → `ESC x`. |
| **`Shift-Tab`** | The back-tab `ESC [ Z`. |
| **Modified cursor/nav keys** | The xterm CSI-with-parameter form (see below). |

Modified cursor and navigation keys (`Up`, `Down`, `Left`, `Right`, `Home`,
`End`, `PageUp`, `PageDown`, `Delete`) use the xterm CSI-with-parameter
encoding, where the parameter is `1 + Shift + 2·Alt + 4·Ctrl` (so `2` = Shift,
`5` = Ctrl, `6` = Ctrl-Shift, …). For example `Ctrl-Right` sends `ESC [ 1 ; 5 C`
and `Ctrl-Shift-Left` sends `ESC [ 1 ; 6 D`.

### Recording vs. playback

Playback (`ascii-rat-bard`) supports every combination above. The recorder
(`ascii-rat-scribe`) can round-trip a captured keystroke back into a named
action for `Ctrl-<letter>`, `Shift-Tab`, and the modified cursor/nav sequences.
`Alt-<key>` combos play back correctly but are not decoded by the recorder (a
captured `ESC`-prefixed run is treated as `Esc` followed by text), so add them by
hand if you need them in a recorded script.
