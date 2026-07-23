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
