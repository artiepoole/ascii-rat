//! Scripted session model: actions, delay ranges, and the recording script.
//!
//! This is a Rust port of `script.py`. It provides the strongly-typed `Script`
//! and `Action` types plus YAML (de)serialization mirroring `from_dict`, and an
//! escape-decoding helper used before sending keystrokes to the PTY.

use crate::cast::{AsciiCast, Event, Header};
use crate::filters::Filter;
use crate::pty::{OutputChunk, PtySession};
use anyhow::{Context, Result};
use portable_pty::CommandBuilder;
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Fixed RNG seed for reproducible, human-like timings (matches the reference).
const RNG_SEED: u64 = 12345;
/// Default PTY size used when `cols`/`rows` are not set in the script.
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// Terminal type advertised to the child (via `TERM`) and stored in the cast
/// header. A full-screen TUI queries the terminfo database for this name to
/// decide which cursor/screen-control escape sequences to emit. It must be a
/// capable, widely-available type that asciinema players understand; a
/// primitive type such as `ansi`, `dumb`, or an unset value makes a TUI emit
/// crippled sequences that render as a scrambled byte stream on playback.
const DEFAULT_TERM: &str = "xterm-256color";

/// Resolve the `TERM` value to advertise to the child program.
///
/// The ambient `TERM` is honoured only when it is a full-featured type, so a
/// recording made in a restricted environment (e.g. `TERM=ansi`/`dumb`, or no
/// `TERM` at all — common under CI, cron, or a bare PTY) does not tell a TUI to
/// render for a primitive terminal (which corrupts the captured stream). In
/// those cases we fall back to [`DEFAULT_TERM`].
fn resolve_term() -> String {
    match std::env::var("TERM") {
        Ok(term) if is_capable_term(&term) => term,
        _ => DEFAULT_TERM.to_string(),
    }
}

/// Whether `term` names a terminal capable enough to render a full-screen TUI
/// without scrambling. Primitive types are rejected so we substitute a capable
/// default instead of faithfully recording garbage.
fn is_capable_term(term: &str) -> bool {
    let term = term.trim();
    !(term.is_empty()
        || term.eq_ignore_ascii_case("dumb")
        || term.eq_ignore_ascii_case("ansi")
        || term.eq_ignore_ascii_case("unknown")
        || term.eq_ignore_ascii_case("vt52"))
}

/// A single scripted action.
///
/// A bare YAML scalar becomes `Text`; a mapping with an `action_id` becomes the
/// corresponding tagged variant, mirroring `parse_action`.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Plain string typed with the script's default delays.
    Text(String),
    /// String typed with explicit per-action pre/post newline delays.
    Input {
        text: String,
        pre_nl_delay: f64,
        post_nl_delay: f64,
    },
    /// Inserts a marker event.
    Marker { label: String },
    /// Inserts a comment (caption) event.
    Comment { comment: String },
    /// Sends a sequence of named keys (e.g. `Down`, `Enter`, `Esc`) in order.
    ///
    /// A single `Key` action can queue several keypresses: either the legacy
    /// `key:` + `count:` form (repeat one key) or the `keys: [..]` list form
    /// (send each named key once, in order). Both are normalized into this
    /// flattened list of keys. The dedicated `key_delay` is slept after each
    /// keypress.
    Key { keys: Vec<KeyName> },
    /// Pauses the recording for `seconds` (capturing output during the pause).
    Wait { seconds: f64 },
    /// Ends the recording at this point (the `END_REC:` action).
    ///
    /// Any actions after it are ignored and no further child output is captured,
    /// so the cast stops exactly here. This is the simple replacement for the
    /// old `Marker` + `EndMarkerFilter` pattern. It is spelled `END_REC` (not
    /// `End`) so the `End` special key stays usable as a normal keypress.
    End,
}

impl Action {
    /// A short, single-line description of this action for the live progress
    /// display shown while recording (not written to the cast).
    pub fn progress_label(&self) -> String {
        match self {
            Action::Text(text) => format!("type {text:?}"),
            Action::Input { text, .. } => format!("type {text:?}"),
            Action::Marker { label } => format!("marker {label:?}"),
            Action::Comment { comment } => format!("comment {comment:?}"),
            Action::Key { keys } => {
                let names: Vec<&str> = keys.iter().map(|k| k.label()).collect();
                format!("key {}", names.join(" "))
            }
            Action::Wait { seconds } => format!("wait {seconds}s"),
            Action::End => "end recording".to_string(),
        }
    }
}

/// A named keyboard key that maps to a fixed byte sequence sent to the child.
///
/// Names are capitalized in YAML (e.g. `Down`, `Enter`, `Esc`); parsing is
/// case-insensitive so `down`/`DOWN` also work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyName {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Esc,
    Tab,
    Backspace,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Space,
}

impl KeyName {
    /// Parse a key name (case-insensitive), accepting a few common aliases.
    pub fn parse(name: &str) -> Option<KeyName> {
        let key = match name.to_ascii_lowercase().as_str() {
            "up" => KeyName::Up,
            "down" => KeyName::Down,
            "left" => KeyName::Left,
            "right" => KeyName::Right,
            "enter" | "return" | "cr" => KeyName::Enter,
            "esc" | "escape" => KeyName::Esc,
            "tab" => KeyName::Tab,
            "backspace" | "bs" => KeyName::Backspace,
            "delete" | "del" => KeyName::Delete,
            "home" => KeyName::Home,
            "end" => KeyName::End,
            "pageup" | "pgup" => KeyName::PageUp,
            "pagedown" | "pgdn" | "pgdown" => KeyName::PageDown,
            "space" => KeyName::Space,
            _ => return None,
        };
        Some(key)
    }

    /// A short, human-readable name for progress display (e.g. `Down`).
    pub fn label(self) -> &'static str {
        match self {
            KeyName::Up => "Up",
            KeyName::Down => "Down",
            KeyName::Left => "Left",
            KeyName::Right => "Right",
            KeyName::Enter => "Enter",
            KeyName::Esc => "Esc",
            KeyName::Tab => "Tab",
            KeyName::Backspace => "Backspace",
            KeyName::Delete => "Delete",
            KeyName::Home => "Home",
            KeyName::End => "End",
            KeyName::PageUp => "PageUp",
            KeyName::PageDown => "PageDown",
            KeyName::Space => "Space",
        }
    }

    /// The raw byte sequence sent to the child for this key.
    ///
    /// Arrow keys use the SS3 (`ESC O x`) encoding to match `demo.yaml`
    /// (e.g. Down is `\u001bOB`); the rest use standard xterm sequences.
    pub fn bytes(self) -> &'static [u8] {
        match self {
            KeyName::Up => b"\x1bOA",
            KeyName::Down => b"\x1bOB",
            KeyName::Right => b"\x1bOC",
            KeyName::Left => b"\x1bOD",
            KeyName::Enter => b"\r",
            KeyName::Esc => b"\x1b",
            KeyName::Tab => b"\t",
            KeyName::Backspace => b"\x7f",
            KeyName::Delete => b"\x1b[3~",
            KeyName::Home => b"\x1b[H",
            KeyName::End => b"\x1b[F",
            KeyName::PageUp => b"\x1b[5~",
            KeyName::PageDown => b"\x1b[6~",
            KeyName::Space => b" ",
        }
    }

    /// Every named key, in declaration order. Used to build reverse lookups.
    pub const ALL: [KeyName; 14] = [
        KeyName::Up,
        KeyName::Down,
        KeyName::Left,
        KeyName::Right,
        KeyName::Enter,
        KeyName::Esc,
        KeyName::Tab,
        KeyName::Backspace,
        KeyName::Delete,
        KeyName::Home,
        KeyName::End,
        KeyName::PageUp,
        KeyName::PageDown,
        KeyName::Space,
    ];

    /// Map a raw byte sequence back to the named key that produces it.
    ///
    /// This is the inverse of [`KeyName::bytes`]: given the exact escape/byte
    /// sequence a key emits, return the matching [`KeyName`], or `None` if no
    /// key produces that sequence. Used by the recorder to turn captured
    /// keystrokes back into canonical named keys.
    ///
    /// Note: `Esc` is `\x1b`, which is a prefix of every arrow/nav escape
    /// sequence, so callers decoding a live byte stream must prefer the longest
    /// match. This exact-match lookup only returns `Esc` for a lone `\x1b`.
    pub fn from_bytes(seq: &[u8]) -> Option<KeyName> {
        KeyName::ALL.into_iter().find(|k| k.bytes() == seq)
    }
}

impl<'de> Deserialize<'de> for Action {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ActionVisitor;

        impl<'de> Visitor<'de> for ActionVisitor {
            type Value = Action;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string or an action mapping with an `action_id`")
            }

            fn visit_str<E>(self, v: &str) -> Result<Action, E>
            where
                E: de::Error,
            {
                Ok(Action::Text(v.to_string()))
            }

            fn visit_string<E>(self, v: String) -> Result<Action, E>
            where
                E: de::Error,
            {
                Ok(Action::Text(v))
            }

            fn visit_map<M>(self, mut map: M) -> Result<Action, M::Error>
            where
                M: MapAccess<'de>,
            {
                // Every non-string action is a single-key mapping named after
                // the action, e.g. `Wait: 2`, `Marker: END`, `END_REC:`,
                // `Keys: [Down, Enter]`, `Input: {text: /, post_nl_delay: 0.2}`,
                // or a bare special key `Down: 3`. There is no `action_id`.
                let tag: String = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("an action mapping must not be empty"))?;

                let action = match tag.as_str() {
                    "Input" => {
                        let raw: InputPayload = map.next_value()?;
                        Action::Input {
                            text: raw.text,
                            pre_nl_delay: raw.pre_nl_delay,
                            post_nl_delay: raw.post_nl_delay,
                        }
                    }
                    "Marker" => Action::Marker {
                        label: map.next_value()?,
                    },
                    "Comment" => Action::Comment {
                        comment: map.next_value()?,
                    },
                    // `Keys: [Down, Down, Enter]` — a sequence of named keys sent
                    // in order (queue several distinct keys in one action).
                    "Keys" => {
                        let names: Vec<String> = map.next_value()?;
                        if names.is_empty() {
                            return Err(de::Error::custom("`Keys` must list at least one key"));
                        }
                        let keys = names
                            .iter()
                            .map(|name| {
                                KeyName::parse(name).ok_or_else(|| {
                                    de::Error::custom(format!("Invalid key name {name}"))
                                })
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        Action::Key { keys }
                    }
                    "Wait" => {
                        let seconds: f64 = map.next_value()?;
                        if seconds < 0.0 {
                            return Err(de::Error::custom("Wait seconds must be >= 0"));
                        }
                        Action::Wait { seconds }
                    }
                    // `END_REC:` takes no meaningful value (write `END_REC:` or
                    // `END_REC: ~`); it stops the recording where it appears.
                    // Named to avoid clashing with the `End` special key, which
                    // is available as a normal `End: <count>` keypress below.
                    "END_REC" => {
                        map.next_value::<de::IgnoredAny>()?;
                        Action::End
                    }
                    // A bare special key `Down: 3` / `End: 1` (repeat the key
                    // `count` times). The value is the repeat count (>= 1); it
                    // may be omitted (e.g. `Esc:` or `Esc: ~`), which is treated
                    // as a single press.
                    other => {
                        let key = KeyName::parse(other).ok_or_else(|| {
                            de::Error::custom(format!("Invalid action or key `{other}`"))
                        })?;
                        let count: u32 = map.next_value::<Option<u32>>()?.unwrap_or(1);
                        if count == 0 {
                            return Err(de::Error::custom(format!(
                                "`{other}` count must be >= 1"
                            )));
                        }
                        Action::Key {
                            keys: vec![key; count as usize],
                        }
                    }
                };

                // A single-key tag mapping: reject any extra keys so typos like
                // two tags in one mapping are caught instead of silently ignored.
                if let Some(extra) = map.next_key::<String>()? {
                    return Err(de::Error::custom(format!(
                        "an action mapping must have exactly one key; found extra `{extra}`"
                    )));
                }

                Ok(action)
            }
        }

        deserializer.deserialize_any(ActionVisitor)
    }
}

impl Serialize for Action {
    /// Serialize an action into exactly the YAML grammar that [`Action`]'s
    /// `Deserialize` accepts, so a produced script round-trips.
    ///
    /// - `Text(s)` becomes a bare scalar string.
    /// - Every other variant becomes a single-key mapping named after the
    ///   action (`Input`/`Marker`/`Comment`/`Keys`/`Wait`/`END_REC`), matching
    ///   `visit_map`. `Key` always emits the `Keys: [..]` list form (never the
    ///   `Down: 3` shorthand) so any sequence of keys round-trips.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Action::Text(text) => serializer.serialize_str(text),
            Action::Input {
                text,
                pre_nl_delay,
                post_nl_delay,
            } => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry(
                    "Input",
                    &InputPayload {
                        text: text.clone(),
                        pre_nl_delay: *pre_nl_delay,
                        post_nl_delay: *post_nl_delay,
                    },
                )?;
                map.end()
            }
            Action::Marker { label } => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("Marker", label)?;
                map.end()
            }
            Action::Comment { comment } => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("Comment", comment)?;
                map.end()
            }
            Action::Key { keys } => {
                let labels: Vec<&'static str> = keys.iter().map(|k| k.label()).collect();
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("Keys", &labels)?;
                map.end()
            }
            Action::Wait { seconds } => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("Wait", seconds)?;
                map.end()
            }
            Action::End => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("END_REC", &Option::<()>::None)?;
                map.end()
            }
        }
    }
}

/// The value of an `Input:` action mapping (`{text, pre_nl_delay, post_nl_delay}`).
#[derive(Debug, Deserialize, Serialize)]
struct InputPayload {
    text: String,
    pre_nl_delay: f64,
    post_nl_delay: f64,
}

/// Default sudo password prompts matched (case-insensitively) against the
/// child's output. Covers the standard `[sudo] password for user:` prompt.
fn default_sudo_prompts() -> Vec<String> {
    vec!["assword".to_string(), "[sudo]".to_string()]
}

/// Top-level sudo handling configuration.
///
/// When present on a [`Script`], the recorder watches each command's output for
/// one of `prompts` and, on a match, types the supplied password
/// character-by-character (once) followed by Enter.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SudoConfig {
    /// Prompt substrings that trigger typing the password. Defaults to
    /// [`default_sudo_prompts`] when omitted in the YAML.
    #[serde(default = "default_sudo_prompts")]
    pub prompts: Vec<String>,
}

impl Default for SudoConfig {
    fn default() -> Self {
        SudoConfig {
            prompts: default_sudo_prompts(),
        }
    }
}

/// Accepts either `sudo: true`/`false` (a flag enabling default prompts) or a
/// `sudo:` mapping (possibly with an explicit `prompts:` list).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum SudoField {
    Flag(bool),
    Config(SudoConfig),
}

/// Deserialize the optional top-level `sudo` field: `true`/an (empty or
/// populated) mapping enables sudo (with default or provided prompts); `false`
/// or an absent field disables it.
fn deserialize_sudo<'de, D>(deserializer: D) -> Result<Option<SudoConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    let field = Option::<SudoField>::deserialize(deserializer)?;
    Ok(match field {
        None | Some(SudoField::Flag(false)) => None,
        Some(SudoField::Flag(true)) => Some(SudoConfig::default()),
        Some(SudoField::Config(cfg)) => Some(cfg),
    })
}

/// Default per-keypress delay range when a script omits `key_delay`.
///
/// A small, slightly-jittered pause so queued keys (e.g. `keys: [Down, Down,
/// Enter]`) don't all fire in the same instant, which a TUI can drop or
/// coalesce. Matches the feel of the typing delay rather than the long
/// post-newline settle.
fn default_key_delay() -> (f64, f64) {
    (0.08, 0.12)
}

/// A delay written in a script, before unit conversion.
///
/// Every delay field accepts **either** a single number (a fixed delay, no
/// jitter) **or** a two-element `[low, high]` range (a uniform random delay).
/// A single number `x` is stored as the fixed range `(x, x)`. The numeric
/// values are unit-agnostic here — whether they are seconds or milliseconds is
/// decided by which field name was used (`<name>` vs `<name>_ms`).
#[derive(Debug, Clone, Copy)]
struct DelaySpec(f64, f64);

impl DelaySpec {
    /// Convert to a `(low, high)` pair in **seconds**, multiplying by
    /// `scale` (1.0 for a seconds field, 0.001 for a `_ms` field).
    fn to_seconds(self, scale: f64) -> (f64, f64) {
        (self.0 * scale, self.1 * scale)
    }
}

impl<'de> Deserialize<'de> for DelaySpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DelayVisitor;

        impl<'de> de::Visitor<'de> for DelayVisitor {
            type Value = DelaySpec;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a number or a [low, high] two-element list")
            }

            fn visit_f64<E: de::Error>(self, v: f64) -> Result<DelaySpec, E> {
                Ok(DelaySpec(v, v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<DelaySpec, E> {
                Ok(DelaySpec(v as f64, v as f64))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<DelaySpec, E> {
                Ok(DelaySpec(v as f64, v as f64))
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<DelaySpec, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                let low: f64 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::custom("delay range needs a low value"))?;
                let high: f64 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::custom("delay range needs a high value"))?;
                if seq.next_element::<f64>()?.is_some() {
                    return Err(de::Error::custom(
                        "delay range must have exactly two values [low, high]",
                    ));
                }
                Ok(DelaySpec(low, high))
            }
        }

        deserializer.deserialize_any(DelayVisitor)
    }
}

/// Resolve one delay field that may be spelled either in seconds (`name`) or in
/// milliseconds (`name_ms`), returning `(low, high)` in **seconds**.
///
/// The two spellings are mutually exclusive. When neither is present, `default`
/// is used (already in seconds).
fn resolve_delay(
    seconds: Option<DelaySpec>,
    millis: Option<DelaySpec>,
    field: &str,
    default: Option<(f64, f64)>,
) -> Result<(f64, f64), String> {
    match (seconds, millis) {
        (Some(_), Some(_)) => Err(format!(
            "use either `{field}` (seconds) or `{field}_ms` (milliseconds), not both"
        )),
        (Some(s), None) => Ok(s.to_seconds(1.0)),
        (None, Some(m)) => Ok(m.to_seconds(0.001)),
        (None, None) => {
            default.ok_or_else(|| format!("missing field `{field}` (or `{field}_ms`)"))
        }
    }
}

/// Raw, directly-deserialized form of [`Script`].
///
/// Each delay field is accepted in two mutually-exclusive spellings — `<name>`
/// (seconds) and `<name>_ms` (milliseconds) — and each accepts a single number
/// or a `[low, high]` range (see [`DelaySpec`]). The raw form is then folded
/// into the strongly-typed [`Script`] by [`Script::deserialize`], which applies
/// unit conversion, defaults, and the not-both validation via [`resolve_delay`].
#[derive(Debug, Deserialize)]
struct ScriptRaw {
    output_file: String,
    #[serde(default)]
    start_delay: Option<DelaySpec>,
    #[serde(default)]
    start_delay_ms: Option<DelaySpec>,
    #[serde(default)]
    end_delay: Option<DelaySpec>,
    #[serde(default)]
    end_delay_ms: Option<DelaySpec>,
    #[serde(default)]
    typing_delay: Option<DelaySpec>,
    #[serde(default)]
    typing_delay_ms: Option<DelaySpec>,
    #[serde(default)]
    pre_nl_delay: Option<DelaySpec>,
    #[serde(default)]
    pre_nl_delay_ms: Option<DelaySpec>,
    #[serde(default)]
    post_nl_delay: Option<DelaySpec>,
    #[serde(default)]
    post_nl_delay_ms: Option<DelaySpec>,
    #[serde(default)]
    key_delay: Option<DelaySpec>,
    #[serde(default)]
    key_delay_ms: Option<DelaySpec>,
    #[serde(default)]
    with_comments: bool,
    #[serde(default)]
    comments_at_top: bool,
    actions: Vec<Action>,
    #[serde(default)]
    filters: Vec<Filter>,
    #[serde(default)]
    cols: Option<u16>,
    #[serde(default)]
    rows: Option<u16>,
    #[serde(default, deserialize_with = "deserialize_sudo")]
    sudo: Option<SudoConfig>,
}

/// The complete scripted session, deserialized from the YAML file.
///
/// All delay fields are stored as `(low, high)` ranges **in seconds**. In the
/// YAML each may be written as either a single number or a `[low, high]` list,
/// and in either seconds (`key_delay:`) or milliseconds (`key_delay_ms:`); see
/// [`ScriptRaw`] and [`resolve_delay`] for the parsing.
#[derive(Debug, Clone)]
pub struct Script {
    pub output_file: String,
    /// Delay before typing begins, `(low, high)` seconds (usually fixed).
    pub start_delay: (f64, f64),
    /// Delay after the last action before closing, `(low, high)` seconds.
    pub end_delay: (f64, f64),
    /// `[low, high]` uniform range for per-character typing delay.
    pub typing_delay: (f64, f64),
    /// `[low, high]` uniform range for the delay before the newline.
    pub pre_nl_delay: (f64, f64),
    /// `[low, high]` uniform range for the delay after the newline.
    pub post_nl_delay: (f64, f64),
    /// `[low, high]` uniform range for the delay slept after each keypress sent
    /// by a `Key` action (independent of line/newline timing). Defaults to
    /// [`default_key_delay`] when omitted so existing scripts keep working.
    pub key_delay: (f64, f64),
    pub with_comments: bool,
    pub comments_at_top: bool,
    pub actions: Vec<Action>,
    pub filters: Vec<Filter>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    /// Optional top-level sudo handling. When set, the recorder auto-types the
    /// supplied password once a configured prompt appears in the child output.
    pub sudo: Option<SudoConfig>,
}

impl<'de> Deserialize<'de> for Script {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = ScriptRaw::deserialize(deserializer)?;

        // `start_delay`/`end_delay` are conceptually single fixed values but go
        // through the same scalar-or-range parsing; a fixed number yields
        // `(x, x)`. They are required (no default) like before.
        let start_delay =
            resolve_delay(raw.start_delay, raw.start_delay_ms, "start_delay", None)
                .map_err(de::Error::custom)?;
        let end_delay = resolve_delay(raw.end_delay, raw.end_delay_ms, "end_delay", None)
            .map_err(de::Error::custom)?;
        let typing_delay =
            resolve_delay(raw.typing_delay, raw.typing_delay_ms, "typing_delay", None)
                .map_err(de::Error::custom)?;
        let pre_nl_delay =
            resolve_delay(raw.pre_nl_delay, raw.pre_nl_delay_ms, "pre_nl_delay", None)
                .map_err(de::Error::custom)?;
        let post_nl_delay = resolve_delay(
            raw.post_nl_delay,
            raw.post_nl_delay_ms,
            "post_nl_delay",
            None,
        )
        .map_err(de::Error::custom)?;
        let key_delay = resolve_delay(
            raw.key_delay,
            raw.key_delay_ms,
            "key_delay",
            Some(default_key_delay()),
        )
        .map_err(de::Error::custom)?;

        Ok(Script {
            output_file: raw.output_file,
            start_delay,
            end_delay,
            typing_delay,
            pre_nl_delay,
            post_nl_delay,
            key_delay,
            with_comments: raw.with_comments,
            comments_at_top: raw.comments_at_top,
            actions: raw.actions,
            filters: raw.filters,
            cols: raw.cols,
            rows: raw.rows,
            sudo: raw.sudo,
        })
    }
}

impl Script {
    /// Load a `Script` from a YAML file.
    pub fn from_yaml<P: AsRef<Path>>(yaml_file: P) -> Result<Script> {
        let contents = std::fs::read_to_string(yaml_file.as_ref())
            .with_context(|| format!("failed to read script file {:?}", yaml_file.as_ref()))?;
        let script: Script = serde_yaml::from_str(&contents)
            .with_context(|| format!("failed to parse YAML script {:?}", yaml_file.as_ref()))?;
        Ok(script)
    }

    /// Returns `true` if the script has a top-level `sudo:` block, meaning a
    /// password must be supplied to [`Script::run`].
    pub fn sudo_enabled(&self) -> bool {
        self.sudo.is_some()
    }

    /// The filter list to apply, ensuring a `Comment` filter is present when
    /// comments are enabled (mirrors `with_comments_enabled`).
    fn effective_filters(&self) -> Vec<Filter> {
        let mut filters = self.filters.clone();
        if self.with_comments && !filters.iter().any(|f| matches!(f, Filter::Comment)) {
            filters.push(Filter::Comment);
        }
        filters
    }

    /// Record the scripted session into an `AsciiCast`.
    ///
    /// This ports `script.py::run`: it spawns a child shell in a PTY, types the
    /// scripted keystrokes with sampled delays, captures timestamped output as
    /// `o` events, merges inserted marker/comment events, and applies filters.
    /// The caller is responsible for saving the returned cast.
    ///
    /// `sudo_password`, when present, is typed character-by-character into the
    /// child once a configured `sudo.prompts` needle appears in the output. It
    /// is never stored in the script or logged (but, by design, is not redacted
    /// from the cast — the auth widget masks it with `*`).
    ///
    /// When `watch` is `true`, every captured output chunk is also mirrored
    /// verbatim to the process stdout as it is recorded, so a full-screen TUI is
    /// visible live while recording. The produced cast is unaffected. The
    /// per-action progress line (see [`print_progress`]) is suppressed while
    /// watching so it cannot corrupt the mirrored display.
    pub fn run(
        &self,
        quiet: bool,
        watch: bool,
        sudo_password: Option<&str>,
    ) -> Result<AsciiCast> {
        let cols = self.cols.unwrap_or(DEFAULT_COLS);
        let rows = self.rows.unwrap_or(DEFAULT_ROWS);

        let mut rng = StdRng::seed_from_u64(RNG_SEED);

        // Build the child command: the user's shell, fallback /bin/bash.
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let mut cmd = CommandBuilder::new(&shell);
        // Advertise a capable terminal type so full-screen TUIs render with the
        // correct escape sequences instead of a scrambled stream (see
        // `resolve_term`). The same value is stored in the cast header.
        cmd.env("TERM", resolve_term());

        let mut session = PtySession::spawn(cmd, cols, rows)?;

        let t0 = Instant::now();
        let mut output_chunks: Vec<OutputChunk> = Vec::new();
        let mut insert_events: Vec<Event> = Vec::new();
        let mut newline_delay: f64 = 0.0;
        // One-shot latch: the sudo password is typed at most once, the first
        // time a configured prompt appears in the child's output.
        let mut sudo_typed = false;
        // Set by an `End` action: stop the recording exactly there, ignoring any
        // later actions and not capturing further child output.
        let mut ended = false;

        // Initial start delay before typing begins.
        sleep(Duration::from_secs_f64(sample(&mut rng, self.start_delay)));
        mirror_and_capture(&mut output_chunks, session.drain_output(), watch);

        for action in &self.actions {
            // Show the action currently being recorded, overwriting the previous
            // line (suppressed while quiet or watching). Replaces the old dots.
            print_progress(action, quiet, watch);
            match action {
                Action::Text(text) => {
                    newline_delay = send_line(
                        &mut session,
                        text,
                        &mut rng,
                        self.typing_delay,
                        self.pre_nl_delay,
                        self.post_nl_delay,
                        &mut output_chunks,
                        watch,
                    )?;
                    // A typed string no longer submits on its own, so the sudo
                    // prompt normally appears only after a later `Key: Enter`.
                    // Do a cheap, non-blocking scan of already-captured output
                    // here (covers input that happened to submit) without
                    // stalling on the timeout when nothing has been submitted.
                    self.maybe_type_sudo_password(
                        &mut session,
                        sudo_password,
                        &mut sudo_typed,
                        false,
                        &mut rng,
                        &mut output_chunks,
                        watch,
                    )?;
                }
                Action::Input {
                    text,
                    pre_nl_delay,
                    post_nl_delay,
                } => {
                    newline_delay = send_line(
                        &mut session,
                        text,
                        &mut rng,
                        self.typing_delay,
                        (*pre_nl_delay, *pre_nl_delay),
                        (*post_nl_delay, *post_nl_delay),
                        &mut output_chunks,
                        watch,
                    )?;
                    // See the Text arm: non-blocking scan only.
                    self.maybe_type_sudo_password(
                        &mut session,
                        sudo_password,
                        &mut sudo_typed,
                        false,
                        &mut rng,
                        &mut output_chunks,
                        watch,
                    )?;
                }
                Action::Marker { label } => {
                    let rel_time = marker_time(t0, newline_delay);
                    insert_events.push(Event::Marker {
                        time: rel_time,
                        label: label.clone(),
                    });
                }
                Action::Comment { comment } => {
                    let rel_time = marker_time(t0, newline_delay);
                    insert_events.push(Event::Comment {
                        time: rel_time,
                        top: self.comments_at_top,
                        comment: comment.clone(),
                    });
                }
                Action::Key { keys } => {
                    send_keys(
                        &mut session,
                        keys,
                        &mut rng,
                        self.key_delay,
                        &mut output_chunks,
                        watch,
                    )?;
                    // A key press is not a line, so the newline shift for a
                    // following marker/comment should not apply.
                    newline_delay = 0.0;
                    // Since typed strings no longer submit on their own, the
                    // Enter that runs a `sudo ...` command is a separate `Key`
                    // action. Check for the password prompt here (blocking on
                    // the prompt), otherwise it appears only after the Text
                    // action's non-blocking check has already run and the
                    // password is never typed.
                    self.maybe_type_sudo_password(
                        &mut session,
                        sudo_password,
                        &mut sudo_typed,
                        true,
                        &mut rng,
                        &mut output_chunks,
                        watch,
                    )?;
                }
                Action::Wait { seconds } => {
                    // Pause, capturing (and mirroring) output during the pause so
                    // the launched program's startup frames are recorded.
                    sleep(Duration::from_secs_f64(*seconds));
                    mirror_and_capture(&mut output_chunks, session.drain_output(), watch);
                    // A wait is not a line, so the newline shift for a following
                    // marker/comment should not apply.
                    newline_delay = 0.0;
                }
                Action::End => {
                    // Stop recording exactly here: drop any later actions and do
                    // not capture further child output. The child is still
                    // closed below, but its trailing output is discarded.
                    ended = true;
                    break;
                }
            }
        }

        if ended {
            // Recording ended at an `End` action: close the child but discard
            // any trailing output so the cast stops exactly at the End point.
            let _ = session.close()?;
        } else {
            // End delay, then close the child and collect remaining output.
            sleep(Duration::from_secs_f64(sample(&mut rng, self.end_delay)));
            mirror_and_capture(&mut output_chunks, session.drain_output(), watch);
            let trailing = session.close()?;
            mirror_and_capture(&mut output_chunks, trailing, watch);
        }

        // Terminate the self-overwriting progress line (see `print_progress`)
        // with a newline so the following "demo saved" message starts cleanly.
        if !quiet && !watch {
            eprintln!();
        }

        // Build output events with relative, monotonically-sorted timestamps.
        let output_events = chunks_to_events(t0, output_chunks);

        // Header from cols/rows/env.
        let header = build_header(cols, rows);

        let cast = AsciiCast::new(header, output_events);
        let cast = cast.insert_events(insert_events)?;
        let cast = cast.filter_events(&self.effective_filters())?;
        Ok(cast)
    }

    /// If sudo is enabled and not yet handled, check whether a configured prompt
    /// has appeared in the recently-captured output and, if so, type the
    /// password character-by-character (once).
    ///
    /// The prompt may already be in `output_chunks` (drained by the preceding
    /// command's post-newline settle). If it is not there yet, wait for it with
    /// a bounded timeout so a slow snap cold-start does not cause typing before
    /// the widget is ready. Output is captured normally throughout — there is no
    /// redaction: the widget's masked `*` and prompt line are recorded as-is.
    ///
    /// When `wait` is `true` (used after the `Key: Enter` that actually submits
    /// a `sudo ...` command) the method blocks up to [`SUDO_PROMPT_TIMEOUT`] for
    /// the prompt to appear. When `wait` is `false` (used after a `Text`/`Input`
    /// that no longer submits on its own) it only scans output already captured,
    /// so it never stalls waiting for a prompt that a later Enter will trigger.
    #[allow(clippy::too_many_arguments)]
    fn maybe_type_sudo_password(
        &self,
        session: &mut PtySession,
        sudo_password: Option<&str>,
        sudo_typed: &mut bool,
        wait: bool,
        rng: &mut StdRng,
        output_chunks: &mut Vec<OutputChunk>,
        watch: bool,
    ) -> Result<()> {
        let cfg = match &self.sudo {
            Some(cfg) if !*sudo_typed => cfg,
            _ => return Ok(()),
        };

        let needles: Vec<&str> = cfg.prompts.iter().map(String::as_str).collect();

        // Scan the already-committed output first (fast path: the prompt was
        // drained by the preceding command's settle).
        if !chunks_contain_any(output_chunks, &needles) {
            // Not seen yet. Only block for it when asked to (i.e. right after
            // the Enter that submits the command); otherwise return without
            // typing so we don't stall on a prompt a later action will trigger.
            if !wait {
                return Ok(());
            }
            // If it never appears we simply do not type (no false send); a
            // crashed child returns early too.
            let waited =
                session.wait_for_output_matching(&needles, SUDO_PROMPT_TIMEOUT)?;
            let matched = chunks_contain_any(&waited, &needles);
            // Keep the waited output in the cast (no redaction).
            mirror_and_capture(output_chunks, waited, watch);
            if !matched {
                return Ok(());
            }
        }

        let password = sudo_password.filter(|p| !p.is_empty()).context(
            "a sudo: block requires a password, but none was supplied (or it was empty)",
        )?;
        type_password(session, password, rng, self.typing_delay, output_chunks, watch)?;
        *sudo_typed = true;
        Ok(())
    }
}

/// Compute a marker/comment time relative to `t0`, shifted to appear just
/// before the next line begins (mirrors the `-0.8 * newline_delay` reference).
fn marker_time(t0: Instant, newline_delay: f64) -> f64 {
    let rel = t0.elapsed().as_secs_f64() - 0.8 * newline_delay;
    (rel * 1000.0).round() / 1000.0
}

/// Type `content` character-by-character with sampled delays.
///
/// **No newline is appended.** A typed string is exactly the characters it
/// contains — to submit a command (or a TUI search box, etc.) add an explicit
/// `Key: Enter` action afterwards. This makes composing input predictable:
/// `- "/"` then `- "fpgad"` types `/fpgad` without the first string secretly
/// submitting the search on its own.
///
/// `pre_nl_delay` is slept after the last character (a short settle before the
/// next action), then `post_nl_delay` is slept as the "line committed" pause;
/// the sampled `post_nl_delay` is returned as the reference's `newline_delay`
/// so a following marker/comment is still positioned correctly. Output arriving
/// during typing is drained into `output_chunks`.
#[allow(clippy::too_many_arguments)]
fn send_line(
    session: &mut PtySession,
    content: &str,
    rng: &mut StdRng,
    typing_delay: (f64, f64),
    pre_nl_delay: (f64, f64),
    post_nl_delay: (f64, f64),
    output_chunks: &mut Vec<OutputChunk>,
    watch: bool,
) -> Result<f64> {
    let bytes = decode_escapes(content)?;
    // Send one decoded "character" (grapheme byte-run) at a time. To match the
    // reference (which iterates over Python str characters), we iterate over the
    // *source* characters and decode each individually so escape sequences are
    // sent as a single unit.
    for unit in split_into_units(content) {
        let unit_bytes = decode_escapes(&unit)?;
        session.write(&unit_bytes)?;
        sleep(Duration::from_secs_f64(sample(rng, typing_delay)));
        mirror_and_capture(output_chunks, session.drain_output(), watch);
    }
    // `bytes` is computed above only to validate the whole string decodes; the
    // actual sending happens per-unit.
    let _ = bytes;

    sleep(Duration::from_secs_f64(sample(rng, pre_nl_delay)));
    let final_delay = sample(rng, post_nl_delay);
    sleep(Duration::from_secs_f64(final_delay));
    mirror_and_capture(output_chunks, session.drain_output(), watch);
    Ok(final_delay)
}

/// Send each key in `keys` in order as raw bytes, sleeping the sampled
/// `key_delay` after every keypress. No trailing newline is appended.
///
/// A single `Key` action can queue several keys (either `key:`+`count:` or
/// `keys: [..]`); they were flattened into `keys` at parse time. The delay is
/// slept after *each* press — including the last — so a following action does
/// not race the terminal's handling of the final key.
fn send_keys(
    session: &mut PtySession,
    keys: &[KeyName],
    rng: &mut StdRng,
    key_delay: (f64, f64),
    output_chunks: &mut Vec<OutputChunk>,
    watch: bool,
) -> Result<()> {
    for key in keys {
        session.write(key.bytes())?;
        sleep(Duration::from_secs_f64(sample(rng, key_delay)));
        mirror_and_capture(output_chunks, session.drain_output(), watch);
    }
    Ok(())
}

/// Show the action currently being recorded on a single, self-overwriting line.
///
/// The previous line is cleared first (carriage return + erase-to-end-of-line,
/// `\r\x1b[K`) so each action replaces the last instead of scrolling — a live
/// replacement for the old per-action progress dots. Written to stderr and
/// flushed immediately. Suppressed when `quiet` (no chatter) or `watch` (the
/// child's screen is mirrored live and this line would corrupt it).
fn print_progress(action: &Action, quiet: bool, watch: bool) {
    if quiet || watch {
        return;
    }
    use std::io::Write;
    let mut stderr = std::io::stderr().lock();
    let _ = write!(stderr, "\r\x1b[K→ {}", action.progress_label());
    let _ = stderr.flush();
}

/// Mirror the freshly `drained` output chunks and capture them into `out`.
///
/// When `watch` is `true`, each chunk's bytes are written to a locked stdout and
/// flushed immediately, so a full-screen TUI is shown live as it records. The
/// chunks are always moved into `out` afterwards **unchanged**, so the recorded
/// cast is byte-for-byte identical whether or not `watch` is set.
///
/// The mirrored bytes have terminal *query* sequences removed first (see
/// [`strip_terminal_queries`]). If the child's raw cursor-position / colour
/// probes reached the user's real recording terminal, it would answer them into
/// this process's stdin, and — because the controlling tty is in cooked/echo
/// mode — those replies (`ESC ] 11 ; rgb:… ST` / `ESC [ 7 ; 1 R`) would be
/// echoed back onto the live screen as scrambled bytes. The PTY reader thread
/// already answers the child directly, so nothing else needs the queries.
fn mirror_and_capture(out: &mut Vec<OutputChunk>, drained: Vec<OutputChunk>, watch: bool) {
    if watch {
        let mut stdout = std::io::stdout().lock();
        write_chunks_to(&mut stdout, &drained);
    }
    out.extend(drained);
}

/// Write each chunk's bytes to `sink` in order and flush, stripping terminal
/// query sequences first so they cannot leak onto the live view.
///
/// Extracted from [`mirror_and_capture`] so the mirroring behaviour can be
/// exercised against an in-memory sink in tests. Write/flush errors are ignored
/// (a broken pipe on the live view must not abort the recording).
fn write_chunks_to<W: std::io::Write>(sink: &mut W, chunks: &[OutputChunk]) {
    for chunk in chunks {
        let _ = sink.write_all(&strip_terminal_queries(&chunk.bytes));
    }
    let _ = sink.flush();
}

/// How long to wait for a sudo password prompt to appear before giving up (no
/// false send). Generous because a snap cold-start can be slow to prompt.
const SUDO_PROMPT_TIMEOUT: Duration = Duration::from_secs(10);

/// Return `true` if the concatenated bytes of `chunks` contain any of `needles`
/// (case-insensitive). Used to detect a sudo prompt in the captured output.
/// The lossy decode is only for matching; the chunk bytes are untouched.
fn chunks_contain_any(chunks: &[OutputChunk], needles: &[&str]) -> bool {
    if chunks.is_empty() {
        return false;
    }
    let mut hay = String::new();
    for chunk in chunks {
        hay.push_str(&String::from_utf8_lossy(&chunk.bytes));
    }
    let hay = hay.to_ascii_lowercase();
    needles
        .iter()
        .any(|n| hay.contains(&n.to_ascii_lowercase()))
}

/// Type the sudo password character-by-character (like [`send_line`]), then send
/// Enter. Output is captured into `output_chunks` normally — there is **no**
/// redaction; the auth widget's masked `*` and prompt are recorded as-is. This
/// drives an interactive PAM widget (e.g. corporate Landscape) that consumes
/// individual keystrokes, which a bulk write does not satisfy.
fn type_password(
    session: &mut PtySession,
    password: &str,
    rng: &mut StdRng,
    typing_delay: (f64, f64),
    output_chunks: &mut Vec<OutputChunk>,
    watch: bool,
) -> Result<()> {
    for ch in password.chars() {
        let mut buf = [0u8; 4];
        session.write(ch.encode_utf8(&mut buf).as_bytes())?;
        sleep(Duration::from_secs_f64(sample(rng, typing_delay)));
        mirror_and_capture(output_chunks, session.drain_output(), watch);
    }
    // Enter as a carriage return, matching how a human/terminal submits input.
    session.write(b"\r")?;
    mirror_and_capture(output_chunks, session.drain_output(), watch);
    Ok(())
}

/// Split a scripted string into typing units. A `\uXXXX`/`\xXX`/`\r` etc.
/// escape counts as a single unit; other characters are individual units.
fn split_into_units(input: &str) -> Vec<String> {
    let mut units = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            units.push(c.to_string());
            continue;
        }
        match chars.peek().copied() {
            Some('u') => {
                let mut unit = String::from("\\u");
                chars.next();
                for _ in 0..4 {
                    if let Some(d) = chars.next() {
                        unit.push(d);
                    }
                }
                units.push(unit);
            }
            Some('x') => {
                let mut unit = String::from("\\x");
                chars.next();
                for _ in 0..2 {
                    if let Some(d) = chars.next() {
                        unit.push(d);
                    }
                }
                units.push(unit);
            }
            Some(other) => {
                chars.next();
                units.push(format!("\\{other}"));
            }
            None => units.push("\\".to_string()),
        }
    }
    units
}

/// Sample a value uniformly from the inclusive `[low, high]` range.
fn sample(rng: &mut StdRng, range: (f64, f64)) -> f64 {
    let (low, high) = range;
    if high <= low {
        low
    } else {
        rng.random_range(low..=high)
    }
}

/// Convert timestamped output chunks into `o` events with relative times.
///
/// Chunks are reassembled by the PTY reader so a multi-byte UTF-8 sequence is
/// never split across a chunk boundary; each chunk is therefore valid UTF-8 by
/// construction. We decode losslessly via `String::from_utf8` and only fall
/// back to a lossy decode for genuinely malformed bytes (never panicking),
/// which keeps the cast byte-faithful to real `asciinema` output.
fn chunks_to_events(t0: Instant, chunks: Vec<OutputChunk>) -> Vec<Event> {
    let mut events = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let rel = chunk.instant.saturating_duration_since(t0).as_secs_f64();
        // Drop terminal *query* sequences the child emitted (cursor-position,
        // device-attributes and colour probes). They carry no visible output;
        // during recording the responder answers them so the child is happy,
        // but if the query bytes themselves survive into the cast, a real
        // terminal will answer them again on playback and its replies leak onto
        // the screen as scrambled control sequences. Stripping them here keeps
        // the recorded stream clean without affecting anything the child drew.
        let cleaned = strip_terminal_queries(&chunk.bytes);
        if cleaned.is_empty() {
            // Nothing left to record once the probe bytes are removed.
            continue;
        }
        let data = match String::from_utf8(cleaned) {
            Ok(s) => s,
            // Should not happen after reassembly, but never panic on bad bytes.
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        };
        events.push(Event::Output { time: rel, data });
    }
    events
}

/// Remove terminal *query* escape sequences from `bytes`, preserving every
/// other byte exactly.
///
/// A full-screen TUI probes the terminal by emitting queries such as the
/// cursor-position request (`ESC [ 6 n`), a device-attributes request
/// (`ESC [ c`) or a colour query (`ESC ] 11 ; ? ST`). These are *questions*,
/// not drawing commands — they produce nothing on screen. They must never end
/// up in a recording: on playback the viewer's real terminal answers them and
/// its answers (e.g. `ESC ] 11 ; rgb:…` and `ESC [ 7 ; 1 R`) appear as garbage.
///
/// Only the specific query forms are removed; ordinary escape sequences
/// (colour SGR, cursor moves, mode sets, colour *sets*/*reports*) and plain
/// text pass through untouched. An incomplete sequence at the very end (no
/// final/terminator byte yet) is kept verbatim so a query split across a chunk
/// boundary is not silently corrupted.
fn strip_terminal_queries(bytes: &[u8]) -> Vec<u8> {
    const ESC: u8 = 0x1b;
    const BEL: u8 = 0x07;
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != ESC {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            // CSI sequences: ESC [ ...
            Some(b'[') => {
                let start = i + 2;
                let mut j = start;
                while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                    j += 1;
                }
                if j >= bytes.len() {
                    // Incomplete CSI at end of chunk: keep verbatim.
                    out.extend_from_slice(&bytes[i..]);
                    break;
                }
                let params = &bytes[start..j];
                let final_byte = bytes[j];
                let is_query = match final_byte {
                    // Device Status Report: only 5n/6n are queries.
                    b'n' => params == b"5" || params == b"6",
                    // Primary Device Attributes: ESC[c / ESC[0c.
                    b'c' => params.is_empty() || params == b"0",
                    _ => false,
                };
                if !is_query {
                    out.extend_from_slice(&bytes[i..=j]);
                }
                i = j + 1;
            }
            // OSC sequences: ESC ] ... terminated by BEL or ST (ESC \).
            Some(b']') => {
                let start = i + 2;
                let mut j = start;
                let mut term_len = 0;
                while j < bytes.len() {
                    if bytes[j] == BEL {
                        term_len = 1;
                        break;
                    }
                    if bytes[j] == ESC && bytes.get(j + 1) == Some(&b'\\') {
                        term_len = 2;
                        break;
                    }
                    j += 1;
                }
                if term_len == 0 {
                    // Incomplete OSC at end of chunk: keep verbatim.
                    out.extend_from_slice(&bytes[i..]);
                    break;
                }
                let body = &bytes[start..j];
                // Only colour *queries* (ending in "?") are removed; colour
                // sets and reports are legitimate output and kept.
                let is_query = body == b"10;?" || body == b"11;?";
                let end = j + term_len;
                if !is_query {
                    out.extend_from_slice(&bytes[i..end]);
                }
                i = end;
            }
            // Any other escape (or ESC at end of chunk): keep the ESC as-is.
            _ => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    out
}

/// Build an asciicast header from the resolved size and current environment.
fn build_header(cols: u16, rows: u16) -> Header {
    let mut header = Header::new(cols, rows);
    header.timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64);
    let mut env = BTreeMap::new();
    if let Ok(shell) = std::env::var("SHELL") {
        env.insert("SHELL".to_string(), shell);
    }
    // Record the same terminal type advertised to the child so a player
    // interprets the stream with the capabilities it was generated for.
    env.insert("TERM".to_string(), resolve_term());
    if !env.is_empty() {
        header.env = Some(env);
    }
    header
}

/// Decode backslash escape sequences in a scripted string into raw bytes.
///
/// Mirrors the escapes used in `demo.yaml`: `\r`, `\n`, `\t`, `\0`, `\\`,
/// `\uXXXX` (4 hex digits) and `\xXX` (2 hex digits). Unknown escapes are left
/// verbatim (backslash + following char) to be forgiving.
pub fn decode_escapes(input: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }

        match chars.next() {
            None => {
                // Trailing backslash: keep it verbatim.
                out.push(b'\\');
            }
            Some('r') => out.push(b'\r'),
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('0') => out.push(0),
            Some('\\') => out.push(b'\\'),
            Some('u') => {
                let code = read_hex(&mut chars, 4)
                    .context("invalid \\u escape: expected 4 hex digits")?;
                let ch = char::from_u32(code)
                    .with_context(|| format!("invalid unicode code point U+{code:04X}"))?;
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            Some('x') => {
                let code = read_hex(&mut chars, 2)
                    .context("invalid \\x escape: expected 2 hex digits")?;
                out.push(code as u8);
            }
            Some(other) => {
                // Unknown escape: keep backslash and the char verbatim.
                out.push(b'\\');
                let mut buf = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
        }
    }

    Ok(out)
}

/// Read exactly `n` hex digits from the iterator and parse them as a u32.
fn read_hex<I>(chars: &mut std::iter::Peekable<I>, n: usize) -> Result<u32>
where
    I: Iterator<Item = char>,
{
    let mut value: u32 = 0;
    for _ in 0..n {
        let c = chars
            .next()
            .context("unexpected end of string in hex escape")?;
        let digit = c
            .to_digit(16)
            .with_context(|| format!("invalid hex digit '{c}'"))?;
        value = value * 16 + digit;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_demo_yaml() {
        let script = Script::from_yaml("demo.yaml").expect("demo.yaml should parse");
        assert_eq!(script.output_file, "2026-06-06-snap-rat-vibes.cast");
        // All delays are written with the `_ms` suffix (milliseconds) and a
        // single number, so each becomes a fixed `(x, x)` range in seconds.
        assert_eq!(script.start_delay, (0.5, 0.5));
        assert_eq!(script.end_delay, (0.5, 0.5));
        assert_eq!(script.typing_delay, (0.075, 0.075));
        assert_eq!(script.pre_nl_delay, (0.2, 0.2));
        assert_eq!(script.post_nl_delay, (0.5, 0.5));
        // `key_delay_ms: 250` is converted from milliseconds to seconds.
        assert_eq!(script.key_delay, (0.25, 0.25));
        // The comment caption feature is unused by this script: the fields are
        // omitted from demo.yaml and default to `false` (the `Comment` filter
        // code remains available for scripts that add explicit Comment actions).
        assert!(!script.with_comments);
        assert!(!script.comments_at_top);
        assert_eq!(script.cols, Some(100));
        assert_eq!(script.rows, Some(40));

        // First action is a marker, then a bare string command...
        assert_eq!(
            script.actions[0],
            Action::Marker {
                label: "sudo for privileged operations".to_string()
            }
        );
        assert_eq!(
            script.actions[1],
            Action::Text("sudo snap-rat-vibes".to_string())
        );
        // ...followed by an explicit `Enter: 1` (typed strings no longer submit).
        assert_eq!(
            script.actions[2],
            Action::Key {
                keys: vec![KeyName::Enter],
            }
        );
        // Then a `Wait` giving the launched TUI time to paint, and the query.
        assert_eq!(script.actions[3], Action::Wait { seconds: 1.0 });
        // A bare `Esc:` (no count) parses as a single Esc press.
        assert!(script.actions.iter().any(|a| matches!(
            a,
            Action::Key { keys } if keys == &vec![KeyName::Esc]
        )));
        // A queued multi-key action (the `Keys: [..]` list form) is present.
        assert!(script.actions.iter().any(|a| matches!(
            a,
            Action::Key { keys } if keys == &vec![KeyName::Down, KeyName::Down, KeyName::Enter]
        )));
        // A `PageDown: 2` shorthand expands to two PageDown presses, and a
        // `Down: 6` shorthand expands to six Down presses.
        assert!(script.actions.iter().any(|a| matches!(
            a,
            Action::Key { keys } if keys.len() == 2 && keys.iter().all(|k| *k == KeyName::PageDown)
        )));
        assert!(script.actions.iter().any(|a| matches!(
            a,
            Action::Key { keys } if keys.len() == 6 && keys.iter().all(|k| *k == KeyName::Down)
        )));
        // The top-level `sudo:` block enables password typing with defaults.
        assert!(script.sudo_enabled());
        assert_eq!(
            script.sudo.as_ref().unwrap().prompts,
            vec!["assword".to_string(), "[sudo]".to_string()]
        );
        // The `q` quit command is present (types `q` to exit the TUI).
        assert!(script
            .actions
            .iter()
            .any(|a| matches!(a, Action::Text(t) if t == "q")));
        // Recording is stopped by an `END_REC:` action (no marker/filter now).
        assert_eq!(script.actions.last().unwrap(), &Action::End);
        assert!(script.actions.iter().any(|a| matches!(a, Action::End)));

        // No filters are used; `END_REC:` replaces the old EndMarkerFilter.
        assert!(script.filters.is_empty());
    }

    #[test]
    fn primitive_terminals_are_rejected() {
        // Types that a full-screen TUI cannot render correctly must be treated
        // as incapable so a capable default is substituted instead of recording
        // a scrambled stream.
        assert!(!is_capable_term(""));
        assert!(!is_capable_term("   "));
        assert!(!is_capable_term("dumb"));
        assert!(!is_capable_term("ansi"));
        assert!(!is_capable_term("ANSI"));
        assert!(!is_capable_term("unknown"));
        assert!(!is_capable_term("vt52"));
    }

    #[test]
    fn capable_terminals_are_accepted() {
        assert!(is_capable_term("xterm-256color"));
        assert!(is_capable_term("xterm"));
        assert!(is_capable_term("screen-256color"));
        assert!(is_capable_term("tmux-256color"));
        assert!(is_capable_term("xterm-ghostty"));
    }

    #[test]
    fn key_name_parse_and_bytes() {
        // Case-insensitive parsing and aliases.
        assert_eq!(KeyName::parse("Down"), Some(KeyName::Down));
        assert_eq!(KeyName::parse("down"), Some(KeyName::Down));
        assert_eq!(KeyName::parse("ENTER"), Some(KeyName::Enter));
        assert_eq!(KeyName::parse("return"), Some(KeyName::Enter));
        assert_eq!(KeyName::parse("escape"), Some(KeyName::Esc));
        assert_eq!(KeyName::parse("del"), Some(KeyName::Delete));
        assert_eq!(KeyName::parse("bogus"), None);

        // Byte encodings must match the sequences used in the original demo.
        assert_eq!(KeyName::Down.bytes(), b"\x1bOB");
        assert_eq!(KeyName::Up.bytes(), b"\x1bOA");
        assert_eq!(KeyName::Left.bytes(), b"\x1bOD");
        assert_eq!(KeyName::Enter.bytes(), b"\r");
        assert_eq!(KeyName::Esc.bytes(), b"\x1b");
        assert_eq!(KeyName::Delete.bytes(), b"\x1b[3~");
    }

    #[test]
    fn key_name_from_bytes_is_inverse_of_bytes() {
        // Every named key must reverse-map from its exact byte sequence back to
        // itself, so the recorder can recover canonical keys from a byte stream.
        for key in KeyName::ALL {
            assert_eq!(
                KeyName::from_bytes(key.bytes()),
                Some(key),
                "from_bytes did not round-trip for {:?}",
                key
            );
        }
        // Unknown / empty sequences yield None.
        assert_eq!(KeyName::from_bytes(b""), None);
        assert_eq!(KeyName::from_bytes(b"zzz"), None);
    }

    /// Deserialize a single `Action` from a one-item YAML sequence (the shape
    /// actions appear in inside a script) so the full `Action` grammar is
    /// exercised, including the bare-string `Text` form.
    fn deserialize_single_action(yaml_item: &str) -> Action {
        let seq: Vec<Action> =
            serde_yaml::from_str(yaml_item).expect("action item should deserialize");
        assert_eq!(seq.len(), 1, "expected exactly one action");
        seq.into_iter().next().unwrap()
    }

    #[test]
    fn action_serialize_roundtrips_per_variant() {
        // For each `Action` variant, serializing then deserializing must yield
        // an equal value, so a produced script is consumable by the loader.
        let cases = vec![
            Action::Text("echo hi".to_string()),
            Action::Input {
                text: "/".to_string(),
                pre_nl_delay: 0.2,
                post_nl_delay: 0.5,
            },
            Action::Marker {
                label: "intro".to_string(),
            },
            Action::Comment {
                comment: "a caption".to_string(),
            },
            Action::Key {
                keys: vec![KeyName::Down, KeyName::Down, KeyName::Enter],
            },
            Action::Key {
                keys: vec![KeyName::Esc],
            },
            Action::Wait { seconds: 1.5 },
            Action::End,
        ];

        for original in cases {
            let yaml = serde_yaml::to_string(&original)
                .unwrap_or_else(|e| panic!("serialize failed for {original:?}: {e}"));
            let back: Action = serde_yaml::from_str(&yaml)
                .unwrap_or_else(|e| panic!("deserialize failed for {yaml:?}: {e}"));
            assert_eq!(back, original, "round-trip mismatch via YAML:\n{yaml}");
        }
    }

    #[test]
    fn action_text_serializes_as_bare_scalar() {
        // `Text` must be a bare scalar (not a mapping) so it matches the
        // human-authored grammar and `visit_str`.
        let yaml = serde_yaml::to_string(&Action::Text("ls -la".to_string())).unwrap();
        assert_eq!(yaml.trim(), "ls -la");
        assert_eq!(
            deserialize_single_action("- ls -la"),
            Action::Text("ls -la".to_string())
        );
    }

    #[test]
    fn action_end_serializes_as_end_rec_mapping() {
        // `End` must serialize to the `END_REC:` mapping the loader recognizes.
        let yaml = serde_yaml::to_string(&Action::End).unwrap();
        assert!(yaml.contains("END_REC"), "unexpected END serialization: {yaml}");
        assert_eq!(deserialize_single_action(&format!("- {}", yaml.trim())), Action::End);
    }

    #[test]
    fn parse_key_shorthand_actions() {
        // The `KeyName: count` shorthand repeats a single key `count` times.
        let yaml = script_yaml_with_actions("- Down: 3\n- Enter: 1");
        let script: Script = serde_yaml::from_str(&yaml).expect("should parse key actions");
        assert_eq!(
            script.actions[0],
            Action::Key {
                keys: vec![KeyName::Down, KeyName::Down, KeyName::Down],
            }
        );
        assert_eq!(
            script.actions[1],
            Action::Key {
                keys: vec![KeyName::Enter],
            }
        );
    }

    #[test]
    fn parse_key_shorthand_without_count_is_single_press() {
        // A bare key with no count (`Esc:` → null value, or the explicit
        // `Esc: ~`) is treated as a single press.
        let yaml = script_yaml_with_actions("- Esc:\n- Enter: ~");
        let script: Script = serde_yaml::from_str(&yaml).expect("should parse key actions");
        assert_eq!(
            script.actions[0],
            Action::Key {
                keys: vec![KeyName::Esc],
            }
        );
        assert_eq!(
            script.actions[1],
            Action::Key {
                keys: vec![KeyName::Enter],
            }
        );
    }

    #[test]
    fn parse_keys_list_action() {
        // The `Keys: [..]` form queues several distinct keys in one action, in
        // order, without any implicit newline.
        let yaml = script_yaml_with_actions("- Keys: [Down, Down, Enter]");
        let script: Script = serde_yaml::from_str(&yaml).expect("should parse keys list");
        assert_eq!(
            script.actions[0],
            Action::Key {
                keys: vec![KeyName::Down, KeyName::Down, KeyName::Enter],
            }
        );
    }

    #[test]
    fn progress_label_describes_each_action() {
        // The live progress line (replacing the old dots) shows a short,
        // human-readable description of the action currently being recorded.
        assert_eq!(
            Action::Text("sudo whoami".to_string()).progress_label(),
            "type \"sudo whoami\""
        );
        assert_eq!(
            Action::Marker {
                label: "intro".to_string()
            }
            .progress_label(),
            "marker \"intro\""
        );
        assert_eq!(
            Action::Key {
                keys: vec![KeyName::Down, KeyName::Enter],
            }
            .progress_label(),
            "key Down Enter"
        );
        assert_eq!(
            Action::Wait { seconds: 2.0 }.progress_label(),
            "wait 2s"
        );
        assert_eq!(Action::End.progress_label(), "end recording");
    }

    #[test]
    fn parse_action_mapping_with_two_keys_fails() {
        // An action mapping must have exactly one tag key; two is an error.
        let yaml = script_yaml_with_actions("- {Down: 1, Enter: 1}");
        let err = serde_yaml::from_str::<Script>(&yaml)
            .expect_err("two tags in one mapping should fail");
        assert!(err.to_string().contains("exactly one"), "error was: {err}");
    }

    #[test]
    fn parse_empty_keys_list_fails() {
        let yaml = script_yaml_with_actions("- Keys: []");
        let err = serde_yaml::from_str::<Script>(&yaml)
            .expect_err("empty keys list should fail");
        assert!(err.to_string().contains("at least one"), "error was: {err}");
    }

    #[test]
    fn parse_zero_count_key_fails() {
        let yaml = script_yaml_with_actions("- Down: 0");
        let err = serde_yaml::from_str::<Script>(&yaml)
            .expect_err("a zero repeat count should fail");
        assert!(err.to_string().contains(">= 1"), "error was: {err}");
    }

    #[test]
    fn key_delay_defaults_when_omitted() {
        // A script that omits `key_delay` still parses and gets the default
        // range so existing scripts keep working.
        let yaml = script_yaml_with_actions("- Down: 1");
        let script: Script = serde_yaml::from_str(&yaml).expect("should parse");
        assert_eq!(script.key_delay, default_key_delay());
    }

    #[test]
    fn key_delay_is_read_from_yaml() {
        let yaml = r#"
output_file: "out.cast"
start_delay: 0.1
end_delay: 0.1
typing_delay: [0.0, 0.0]
pre_nl_delay: [0.0, 0.0]
post_nl_delay: [0.0, 0.0]
key_delay: [0.3, 0.4]
actions:
- Down: 1
"#;
        let script: Script = serde_yaml::from_str(yaml).expect("should parse");
        assert_eq!(script.key_delay, (0.3, 0.4));
    }

    #[test]
    fn parse_invalid_key_name_fails() {
        let yaml = script_yaml_with_actions("- NotAKey: 1");
        assert!(serde_yaml::from_str::<Script>(&yaml).is_err());
    }

    /// Build a minimal script YAML with the given `actions:` fragment spliced in.
    fn script_yaml_with_actions(actions_fragment: &str) -> String {
        format!(
            r#"
output_file: "out.cast"
start_delay: 0.1
end_delay: 0.1
typing_delay: [0.0, 0.0]
pre_nl_delay: [0.0, 0.0]
post_nl_delay: [0.0, 0.0]
actions:
{actions_fragment}
"#
        )
    }

    #[test]
    fn parse_wait_action() {
        let yaml = script_yaml_with_actions("- Wait: 2.0");
        let script: Script = serde_yaml::from_str(&yaml).expect("should parse Wait action");
        assert_eq!(script.actions[0], Action::Wait { seconds: 2.0 });
    }

    #[test]
    fn parse_wait_negative_seconds_fails() {
        let yaml = script_yaml_with_actions("- Wait: -1.0");
        let err = serde_yaml::from_str::<Script>(&yaml)
            .expect_err("negative seconds should fail");
        assert!(err.to_string().contains(">= 0"), "error was: {err}");
    }

    #[test]
    fn parse_end_action() {
        // `END_REC:` (a null value) ends the recording; it becomes `Action::End`.
        let yaml = script_yaml_with_actions("- \"cmd\"\n- END_REC:");
        let script: Script = serde_yaml::from_str(&yaml).expect("should parse END_REC action");
        assert_eq!(script.actions.last().unwrap(), &Action::End);
    }

    #[test]
    fn end_key_is_a_keypress_not_end_recording() {
        // The `End` special key must still be usable as a normal keypress and
        // must NOT be mistaken for the recording-stop action (`END_REC`).
        let yaml = script_yaml_with_actions("- End: 2");
        let script: Script = serde_yaml::from_str(&yaml).expect("should parse End key");
        assert_eq!(
            script.actions[0],
            Action::Key {
                keys: vec![KeyName::End, KeyName::End],
            }
        );
    }

    #[test]
    fn parse_marker_and_comment_actions() {
        // Natural single-key tag mappings for Marker/Comment.
        let yaml = script_yaml_with_actions("- Marker: END\n- Comment: hello");
        let script: Script = serde_yaml::from_str(&yaml).expect("should parse");
        assert_eq!(
            script.actions[0],
            Action::Marker {
                label: "END".to_string()
            }
        );
        assert_eq!(
            script.actions[1],
            Action::Comment {
                comment: "hello".to_string()
            }
        );
    }

    #[test]
    fn parse_input_action_mapping() {
        // `Input:` takes a nested mapping value.
        let yaml = script_yaml_with_actions(
            "- Input: {text: \"/\", pre_nl_delay: 0.1, post_nl_delay: 0.2}",
        );
        let script: Script = serde_yaml::from_str(&yaml).expect("should parse Input");
        assert_eq!(
            script.actions[0],
            Action::Input {
                text: "/".to_string(),
                pre_nl_delay: 0.1,
                post_nl_delay: 0.2,
            }
        );
    }

    #[test]
    fn parse_scalar_delay_is_fixed_range() {
        // A single number delay becomes a fixed `(x, x)` range (no jitter).
        let yaml = r#"
output_file: "out.cast"
start_delay: 0.1
end_delay: 0.1
typing_delay: 0.02
pre_nl_delay: [0.0, 0.0]
post_nl_delay: [0.0, 0.0]
actions:
- Down: 1
"#;
        let script: Script = serde_yaml::from_str(yaml).expect("should parse");
        assert_eq!(script.typing_delay, (0.02, 0.02));
    }

    #[test]
    fn parse_ms_delay_converts_to_seconds() {
        // A `_ms` field is given in milliseconds and converted to seconds.
        let yaml = r#"
output_file: "out.cast"
start_delay: 0.1
end_delay: 0.1
typing_delay: [0.0, 0.0]
pre_nl_delay: [0.0, 0.0]
post_nl_delay: [0.0, 0.0]
key_delay_ms: 150
actions:
- Down: 1
"#;
        let script: Script = serde_yaml::from_str(yaml).expect("should parse");
        assert_eq!(script.key_delay, (0.15, 0.15));
    }

    #[test]
    fn parse_delay_seconds_and_ms_together_fails() {
        // A field may not be given in both seconds and milliseconds at once.
        let yaml = r#"
output_file: "out.cast"
start_delay: 0.1
end_delay: 0.1
typing_delay: [0.0, 0.0]
pre_nl_delay: [0.0, 0.0]
post_nl_delay: [0.0, 0.0]
key_delay: 0.15
key_delay_ms: 150
actions:
- Down: 1
"#;
        let err = serde_yaml::from_str::<Script>(yaml)
            .expect_err("both key_delay and key_delay_ms should fail");
        assert!(err.to_string().contains("not both"), "error was: {err}");
    }

    /// Build a minimal script YAML with a custom `sudo:` fragment spliced in.
    fn script_yaml_with_sudo(sudo_fragment: &str) -> String {
        format!(
            r#"
output_file: "out.cast"
start_delay: 0.1
end_delay: 0.1
typing_delay: [0.0, 0.0]
pre_nl_delay: [0.0, 0.0]
post_nl_delay: [0.0, 0.0]
{sudo_fragment}
actions:
- sudo something
- exit
"#
        )
    }

    #[test]
    fn sudo_flag_true_enables_default_prompts() {
        let yaml = script_yaml_with_sudo("sudo: true");
        let script: Script = serde_yaml::from_str(&yaml).expect("should parse sudo: true");
        assert!(script.sudo_enabled());
        assert_eq!(
            script.sudo.as_ref().unwrap().prompts,
            vec!["assword".to_string(), "[sudo]".to_string()]
        );
    }

    #[test]
    fn sudo_empty_mapping_enables_default_prompts() {
        let yaml = script_yaml_with_sudo("sudo: {}");
        let script: Script = serde_yaml::from_str(&yaml).expect("should parse sudo: {}");
        assert!(script.sudo_enabled());
        assert_eq!(
            script.sudo.as_ref().unwrap().prompts,
            vec!["assword".to_string(), "[sudo]".to_string()]
        );
    }

    #[test]
    fn sudo_explicit_prompts_override_defaults() {
        let yaml = script_yaml_with_sudo("sudo:\n  prompts:\n  - \"> \"\n  - \"assword\"");
        let script: Script =
            serde_yaml::from_str(&yaml).expect("should parse sudo with prompts");
        assert!(script.sudo_enabled());
        assert_eq!(
            script.sudo.as_ref().unwrap().prompts,
            vec!["> ".to_string(), "assword".to_string()]
        );
    }

    #[test]
    fn sudo_absent_or_false_is_disabled() {
        // Absent entirely.
        let yaml = r#"
output_file: "out.cast"
start_delay: 0.1
end_delay: 0.1
typing_delay: [0.0, 0.0]
pre_nl_delay: [0.0, 0.0]
post_nl_delay: [0.0, 0.0]
actions:
- echo hi
- exit
"#;
        let script: Script = serde_yaml::from_str(yaml).expect("should parse");
        assert!(!script.sudo_enabled());

        // Explicit `sudo: false`.
        let yaml = script_yaml_with_sudo("sudo: false");
        let script: Script = serde_yaml::from_str(&yaml).expect("should parse sudo: false");
        assert!(!script.sudo_enabled());
    }

    #[test]
    fn decode_simple_escapes() {
        assert_eq!(decode_escapes("abc").unwrap(), b"abc");
        assert_eq!(decode_escapes("a\\rb").unwrap(), b"a\rb");
        assert_eq!(decode_escapes("a\\nb").unwrap(), b"a\nb");
        assert_eq!(decode_escapes("a\\tb").unwrap(), b"a\tb");
        assert_eq!(decode_escapes("a\\\\b").unwrap(), b"a\\b");
    }

    #[test]
    fn decode_unicode_escapes() {
        // \u001b is ESC (0x1b).
        assert_eq!(decode_escapes("\\u001b").unwrap(), vec![0x1b]);
        // The demo's down-arrow sequence: ESC O B.
        assert_eq!(decode_escapes("\\u001bOB").unwrap(), vec![0x1b, b'O', b'B']);
        // A line from demo.yaml.
        assert_eq!(
            decode_escapes("\\r\\u001bOB\\r\\u001bOB\\u001bOB").unwrap(),
            vec![b'\r', 0x1b, b'O', b'B', b'\r', 0x1b, b'O', b'B', 0x1b, b'O', b'B']
        );
        // The delete-key sequence from demo.yaml: "/" then ESC [ 3 ~.
        assert_eq!(
            decode_escapes("/\\u001b[3~").unwrap(),
            vec![b'/', 0x1b, b'[', b'3', b'~']
        );
    }

    #[test]
    fn decode_hex_escape() {
        assert_eq!(decode_escapes("\\x1b").unwrap(), vec![0x1b]);
    }

    // Simulates the PTY reader's carry-buffer reassembly over a byte stream cut
    // into arbitrary reads, then runs `chunks_to_events` and asserts the
    // reconstructed data matches the original exactly (no U+FFFD).
    fn reassemble_and_decode(reads: &[&[u8]]) -> String {
        use crate::pty::valid_utf8_prefix_len;
        let t0 = Instant::now();
        let mut chunks: Vec<OutputChunk> = Vec::new();
        let mut carry: Vec<u8> = Vec::new();
        for read in reads {
            carry.extend_from_slice(read);
            let valid = valid_utf8_prefix_len(&carry);
            if valid == 0 {
                continue;
            }
            let bytes: Vec<u8> = carry.drain(..valid).collect();
            chunks.push(OutputChunk {
                instant: Instant::now(),
                bytes,
            });
        }
        // Flush leftover carry (EOF behaviour).
        if !carry.is_empty() {
            chunks.push(OutputChunk {
                instant: Instant::now(),
                bytes: std::mem::take(&mut carry),
            });
        }
        chunks_to_events(t0, chunks)
            .into_iter()
            .filter_map(|e| match e {
                Event::Output { data, .. } => Some(data),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn multibyte_split_across_reads_reconstructs_exactly() {
        // '→' is 0xE2 0x86 0x92; split it across two reads.
        let original = "a→b";
        let bytes = original.as_bytes();
        let (first, second) = bytes.split_at(2); // "a" + first byte of '→'
        let joined = reassemble_and_decode(&[first, second]);
        assert_eq!(joined, original);
        assert!(!joined.contains('\u{fffd}'));
    }

    #[test]
    fn escape_block_split_across_reads_roundtrips() {
        // An alternate-screen enable + a graphics block split at boundaries.
        let original = "\u{1b}[?1049h\u{1b}_Gpayload\u{1b}\\rest";
        let bytes = original.as_bytes();
        let (a, rest) = bytes.split_at(3);
        let (b, c) = rest.split_at(5);
        let joined = reassemble_and_decode(&[a, b, c]);
        assert_eq!(joined, original);
        assert!(!joined.contains('\u{fffd}'));
    }

    #[test]
    fn clean_ascii_is_unchanged() {
        let original = "plain ascii output line\r\n";
        let joined = reassemble_and_decode(&[original.as_bytes()]);
        assert_eq!(joined, original);
    }

    #[test]
    fn read_ending_on_boundary_leaves_empty_carry() {
        // Two reads, each ending exactly on a char boundary.
        let joined = reassemble_and_decode(&["a→".as_bytes(), "→b".as_bytes()]);
        assert_eq!(joined, "a→→b");
        assert!(!joined.contains('\u{fffd}'));
    }

    #[test]
    fn split_units_keeps_escape_sequences_intact() {
        let units = split_into_units("a\\u001bOB");
        assert_eq!(units, vec!["a", "\\u001b", "O", "B"]);
    }

    #[test]
    fn strip_leaves_ordinary_output_untouched() {
        // Plain text and non-query escapes (SGR colour, cursor move, mode set,
        // APC graphics) must be preserved byte-for-byte.
        for input in [
            b"hello world\r\n".as_slice(),
            b"\x1b[36martie\x1b[00m:\x1b[34m~\x1b[00m ".as_slice(),
            b"\x1b[100D\x1b[2K\r".as_slice(),
            b"\x1b[?25l\x1b[?2004h".as_slice(),
            b"\x1b_Gpayload\x1b\\".as_slice(),
        ] {
            assert_eq!(strip_terminal_queries(input), input.to_vec());
        }
    }

    #[test]
    fn strip_removes_cursor_position_query() {
        assert_eq!(strip_terminal_queries(b"\x1b[6n"), Vec::<u8>::new());
        // Surrounded by real output: only the query is removed.
        assert_eq!(strip_terminal_queries(b"abc\x1b[6ndef"), b"abcdef".to_vec());
    }

    #[test]
    fn strip_removes_device_status_and_attributes_queries() {
        assert_eq!(strip_terminal_queries(b"\x1b[5n"), Vec::<u8>::new());
        assert_eq!(strip_terminal_queries(b"\x1b[c"), Vec::<u8>::new());
        assert_eq!(strip_terminal_queries(b"\x1b[0c"), Vec::<u8>::new());
    }

    #[test]
    fn strip_removes_colour_queries_but_keeps_colour_sets() {
        // Colour *queries* (ending in "?") are removed, with either terminator.
        assert_eq!(strip_terminal_queries(b"\x1b]11;?\x1b\\"), Vec::<u8>::new());
        assert_eq!(strip_terminal_queries(b"\x1b]10;?\x07"), Vec::<u8>::new());
        // Colour *sets*/*reports* are legitimate output and must survive.
        let set = b"\x1b]11;rgb:2222/1f1f/2222\x1b\\";
        assert_eq!(strip_terminal_queries(set), set.to_vec());
    }

    #[test]
    fn strip_removes_the_reported_scrambled_sequence() {
        // The exact bytes recorded in the bug report: an OSC 11 colour query
        // immediately followed by a DSR cursor-position query. Both must go.
        assert_eq!(
            strip_terminal_queries(b"\x1b]11;?\x1b\\\x1b[6n"),
            Vec::<u8>::new()
        );
        // Mixed with the surrounding frame the child actually drew.
        let input = b"\x1b[?2004l\r\x1b]11;?\x1b\\\x1b[6nroot\r\n";
        assert_eq!(
            strip_terminal_queries(input),
            b"\x1b[?2004l\rroot\r\n".to_vec()
        );
    }

    #[test]
    fn strip_keeps_incomplete_trailing_sequence_verbatim() {
        // A query split at the end of a chunk (no final/terminator byte yet) is
        // preserved so the remainder can be handled when it arrives.
        assert_eq!(strip_terminal_queries(b"text\x1b[6"), b"text\x1b[6".to_vec());
        assert_eq!(
            strip_terminal_queries(b"text\x1b]11;?"),
            b"text\x1b]11;?".to_vec()
        );
    }

    #[test]
    fn chunks_to_events_drops_query_only_chunk() {
        // A chunk that is nothing but a query pair produces no event at all.
        let t0 = Instant::now();
        let chunks = vec![OutputChunk {
            instant: Instant::now(),
            bytes: b"\x1b]11;?\x1b\\\x1b[6n".to_vec(),
        }];
        assert!(chunks_to_events(t0, chunks).is_empty());
    }

    // Records a trivial `echo` script end-to-end through a real PTY. Ignored by
    // default because it depends on a working shell/PTY environment.
    #[test]
    #[ignore]
    fn smoke_record_echo() {
        let script = Script {
            output_file: "smoke.cast".to_string(),
            start_delay: (0.05, 0.05),
            end_delay: (0.1, 0.1),
            typing_delay: (0.0, 0.0),
            pre_nl_delay: (0.0, 0.0),
            post_nl_delay: (0.1, 0.1),
            key_delay: (0.0, 0.0),
            with_comments: false,
            comments_at_top: false,
            actions: vec![
                Action::Text("echo hello-smoke".to_string()),
                Action::Text("exit".to_string()),
            ],
            filters: vec![],
            cols: Some(80),
            rows: Some(24),
            sudo: None,
        };
        let cast = script.run(true, false, None).expect("recording should succeed");
        assert_eq!(cast.header.width, 80);
        // The echoed text should appear somewhere in the output.
        let joined: String = cast
            .events
            .iter()
            .filter_map(|e| match e {
                Event::Output { data, .. } => Some(data.clone()),
                _ => None,
            })
            .collect();
        assert!(joined.contains("hello-smoke"), "output was: {joined}");
    }

    // Records a script containing a `Wait` and asserts the recording's total
    // duration (last event time) reflects at least the wait. Ignored by default
    // because it depends on a working shell/PTY environment.
    #[test]
    #[ignore]
    fn wait_action_delays_recording() {
        const WAIT_SECS: f64 = 1.0;
        let script = Script {
            output_file: "wait.cast".to_string(),
            start_delay: (0.05, 0.05),
            end_delay: (0.1, 0.1),
            typing_delay: (0.0, 0.0),
            pre_nl_delay: (0.0, 0.0),
            post_nl_delay: (0.05, 0.05),
            key_delay: (0.0, 0.0),
            with_comments: false,
            comments_at_top: false,
            actions: vec![
                Action::Text("echo before-wait".to_string()),
                Action::Wait { seconds: WAIT_SECS },
                Action::Text("echo after-wait".to_string()),
                Action::Text("exit".to_string()),
            ],
            filters: vec![],
            cols: Some(80),
            rows: Some(24),
            sudo: None,
        };
        let cast = script.run(true, false, None).expect("recording should succeed");
        let last_time = cast
            .events
            .iter()
            .filter_map(|e| match e {
                Event::Output { time, .. } => Some(*time),
                _ => None,
            })
            .fold(0.0_f64, f64::max);
        assert!(
            last_time >= WAIT_SECS,
            "last output time {last_time} should reflect the {WAIT_SECS}s wait"
        );
    }

    // Records a child that prints a large stream of multi-byte UTF-8 glyphs
    // (enough to straddle the 8 KB PTY read boundary) and asserts the saved
    // cast reconstructs the glyphs with no injected replacement characters.
    // Ignored by default because it depends on a working shell/PTY.
    #[test]
    #[ignore]
    fn utf8_straddling_read_boundary_has_no_replacement_chars() {
        // Print ~4000 '→' glyphs (3 bytes each => ~12 KB), forcing at least
        // one multi-byte char to be split across an 8 KB read.
        let script = Script {
            output_file: "utf8-straddle.cast".to_string(),
            start_delay: (0.05, 0.05),
            end_delay: (0.3, 0.3),
            typing_delay: (0.0, 0.0),
            pre_nl_delay: (0.0, 0.0),
            post_nl_delay: (0.2, 0.2),
            key_delay: (0.0, 0.0),
            with_comments: false,
            comments_at_top: false,
            actions: vec![
                // `printf` repeats the arrow; `%.0s` prints nothing per arg, and
                // the brace expansion supplies 4000 args.
                Action::Text("printf '\\u2192%.0s' {1..4000}; echo".to_string()),
                Action::Text("exit".to_string()),
            ],
            filters: vec![],
            cols: Some(80),
            rows: Some(24),
            sudo: None,
        };
        let cast = script.run(true, false, None).expect("recording should succeed");
        let joined: String = cast
            .events
            .iter()
            .filter_map(|e| match e {
                Event::Output { data, .. } => Some(data.clone()),
                _ => None,
            })
            .collect();
        assert!(
            !joined.contains('\u{fffd}'),
            "the reassembled cast contains injected U+FFFD replacement chars"
        );
        // The arrow glyphs must round-trip into the output.
        assert!(joined.contains('→'), "expected arrow glyphs in output");
    }

    // Records a script with a top-level `sudo:` block through a real PTY and
    // asserts the password is TYPED (char-by-char, recorded — no redaction),
    // that it is typed only once (latch), and that the recording continues past
    // auth. Ignored by default because it depends on a working shell/PTY.
    #[test]
    #[ignore]
    fn sudo_password_is_typed_char_by_char() {
        const SECRET: &str = "s3cr3t-typed";
        // A stub that stands in for a sudo prompt: it prints a sudo-style prompt
        // at RUNTIME (so the recorder's prompt scan matches), reads one line
        // (echoed by the terminal), then prints a running marker.
        //
        // The prompt text is assembled from fragments so the *typed command
        // echo* never contains the contiguous needle substrings (`[sudo]` /
        // `assword`); only the runtime output does.
        let stub = "printf '%s%s' '[sud' 'o] pass'; \
                    printf 'word for tester: '; read -r pw; echo GOT=$pw; echo RUNNING-NOW"
            .to_string();
        let script = Script {
            output_file: "sudo-typed.cast".to_string(),
            start_delay: (0.05, 0.05),
            end_delay: (0.3, 0.3),
            typing_delay: (0.0, 0.0),
            pre_nl_delay: (0.0, 0.0),
            post_nl_delay: (0.3, 0.3),
            key_delay: (0.0, 0.0),
            with_comments: false,
            comments_at_top: false,
            actions: vec![
                Action::Text(stub),
                Action::Text("exit".to_string()),
            ],
            filters: vec![],
            cols: Some(80),
            rows: Some(24),
            sudo: Some(SudoConfig::default()),
        };
        let cast = script
            .run(true, false, Some(SECRET))
            .expect("recording should succeed");
        let joined: String = cast
            .events
            .iter()
            .filter_map(|e| match e {
                Event::Output { data, .. } => Some(data.clone()),
                _ => None,
            })
            .collect();
        // No redaction: the echoed password + prompt are recorded as-is, and the
        // stub read the whole secret (proving char-by-char typing + Enter).
        assert!(
            joined.contains(&format!("GOT={SECRET}")),
            "the child did not receive the full typed password: {joined}"
        );
        // The recording continues to the running program after auth.
        assert!(
            joined.contains("RUNNING-NOW"),
            "expected the post-auth program output in the cast: {joined}"
        );
    }

    // A `sudo:` block with no password supplied must produce a clear error once
    // a prompt appears. Ignored by default because it spawns a real PTY.
    #[test]
    #[ignore]
    fn sudo_password_missing_is_an_error() {
        let stub = "printf '%s%s' '[sud' 'o] pass'; printf 'word: '; read -r pw"
            .to_string();
        let script = Script {
            output_file: "sudo-missing.cast".to_string(),
            start_delay: (0.05, 0.05),
            end_delay: (0.1, 0.1),
            typing_delay: (0.0, 0.0),
            pre_nl_delay: (0.0, 0.0),
            post_nl_delay: (0.3, 0.3),
            key_delay: (0.0, 0.0),
            with_comments: false,
            comments_at_top: false,
            actions: vec![Action::Text(stub), Action::Text("exit".to_string())],
            filters: vec![],
            cols: Some(80),
            rows: Some(24),
            sudo: Some(SudoConfig::default()),
        };
        let err = script.run(true, false, None).expect_err("should error");
        assert!(err.to_string().contains("sudo"));
    }

    fn chunk(bytes: &[u8]) -> OutputChunk {
        OutputChunk {
            instant: Instant::now(),
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn write_chunks_to_writes_drawing_bytes_in_order() {
        // Ordinary drawing bytes (mode set, UTF-8 text) are mirrored verbatim
        // and in order; only *query* sequences are removed (see the next test).
        let chunks = vec![
            chunk(b"\x1b[?1049h"),
            chunk("héllo→".as_bytes()),
            chunk(b"world"),
        ];
        let mut sink: Vec<u8> = Vec::new();
        write_chunks_to(&mut sink, &chunks);
        let mut expected: Vec<u8> = Vec::new();
        expected.extend_from_slice(b"\x1b[?1049h");
        expected.extend_from_slice("héllo→".as_bytes());
        expected.extend_from_slice(b"world");
        assert_eq!(sink, expected);
    }

    #[test]
    fn write_chunks_to_strips_terminal_queries_from_live_view() {
        // The live `--watch` mirror must NOT forward the child's terminal query
        // probes to the user's real terminal: it would answer them into this
        // process's stdin and (in cooked/echo mode) echo the replies onto the
        // screen as scrambled bytes. The exact bug sequence — an OSC 11 colour
        // query plus a DSR cursor-position query, wrapped in real drawing bytes
        // — must be mirrored with only the queries removed.
        let chunks = vec![chunk(b"\x1b[?2004l\rroot\x1b]11;?\x1b\\\x1b[6n\r\n")];
        let mut sink: Vec<u8> = Vec::new();
        write_chunks_to(&mut sink, &chunks);
        assert_eq!(sink, b"\x1b[?2004l\rroot\r\n".to_vec());
    }

    #[test]
    fn mirror_and_capture_moves_chunks_into_out() {
        // With watch=false, nothing is mirrored but the chunks are still
        // captured; with watch=true, the same capture happens (mirroring to
        // stdout is a side-effect verified separately via `write_chunks_to`).
        let mut out: Vec<OutputChunk> = Vec::new();
        mirror_and_capture(&mut out, vec![chunk(b"abc"), chunk(b"def")], false);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].bytes, b"abc");
        assert_eq!(out[1].bytes, b"def");

        mirror_and_capture(&mut out, vec![chunk(b"ghi")], true);
        assert_eq!(out.len(), 3);
        assert_eq!(out[2].bytes, b"ghi");
    }

    // The live mirror is a side-effect only: given the SAME drained chunks, the
    // captured output must be byte-identical whether or not `watch` is set. This
    // is the deterministic core of the "cast unchanged by --watch" guarantee.
    // (A two-run real-PTY comparison is inherently non-deterministic because a
    // live shell prompt embeds a wall-clock timestamp and timing-dependent
    // output, so it is not asserted here.)
    #[test]
    fn watch_does_not_change_captured_bytes() {
        let drained = || {
            vec![
                chunk(b"\x1b[?1049h"),
                chunk("watch-fidelity→done".as_bytes()),
                chunk(b"\r\n"),
            ]
        };
        let mut plain: Vec<OutputChunk> = Vec::new();
        mirror_and_capture(&mut plain, drained(), false);
        let mut watched: Vec<OutputChunk> = Vec::new();
        mirror_and_capture(&mut watched, drained(), true);

        let bytes = |chunks: &[OutputChunk]| -> Vec<u8> {
            chunks.iter().flat_map(|c| c.bytes.clone()).collect()
        };
        assert_eq!(
            bytes(&plain),
            bytes(&watched),
            "the --watch mirror must not change the captured bytes"
        );
    }
}
