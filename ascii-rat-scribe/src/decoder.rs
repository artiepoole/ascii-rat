//! Incremental byte + timing → [`Action`] decoder.
//!
//! The recorder feeds the operator's raw keystroke bytes (exactly what is sent
//! to the child) into a [`Decoder`], together with the wall-clock instant each
//! read arrived. The decoder turns that stream into the same `Action`s that
//! `ascii-rat-bard` consumes:
//!
//! - Contiguous printable characters collapse into a single [`Action::Text`].
//! - Recognised escape / control sequences become [`Action::Key`] using the
//!   canonical [`KeyName`] vocabulary (via [`KeyName::from_bytes`]).
//! - An idle gap longer than the configured threshold becomes an
//!   [`Action::Wait`], inserted before the next action.
//!
//! Partial escape sequences that straddle a read boundary are buffered so a
//! multi-byte key is never split into garbage text.

use ascii_rat_stage::script::{Action, Key, KeyName};
use std::time::{Duration, Instant};

/// The longest byte sequence any [`Key`] maps to. Plain nav keys are up to 4
/// bytes (`\x1b[3~`); modified cursor/nav keys use the CSI-with-parameter form
/// and are up to 6 bytes (`\x1b[1;5C`, `\x1b[6;5~`). Used to decide when a lone
/// `ESC` can no longer be the start of a known key.
const MAX_KEY_LEN: usize = 6;

/// Incremental decoder turning a keystroke byte/timing stream into `Action`s.
pub struct Decoder {
    /// Idle gap at or above which a `Wait` is emitted before the next action.
    wait_threshold: Duration,
    /// Granularity (milliseconds) to round emitted `Wait` durations to. `0`
    /// disables rounding (waits keep their millisecond-precise value).
    wait_round_ms: u64,
    /// Bytes seen but not yet resolved into an action (a pending escape run or
    /// an incomplete UTF-8 tail).
    pending: Vec<u8>,
    /// Printable characters accumulated for the current `Text` action.
    text: String,
    /// Instant of the previous processed input, for gap computation.
    last_input: Option<Instant>,
    /// Whether a `Wait` is owed before the next non-wait action (a long idle
    /// gap was observed while `pending`/`text` were empty).
    pending_wait: Option<f64>,
    /// The finished actions, in order.
    actions: Vec<Action>,
}

impl Decoder {
    /// Create a decoder that inserts a `Wait` when the idle gap reaches
    /// `wait_threshold_ms` milliseconds, rounding each emitted `Wait` to the
    /// nearest `wait_round_ms` milliseconds (`0` disables rounding).
    pub fn new(wait_threshold_ms: u64, wait_round_ms: u64) -> Decoder {
        Decoder {
            wait_threshold: Duration::from_millis(wait_threshold_ms),
            wait_round_ms,
            pending: Vec::new(),
            text: String::new(),
            last_input: None,
            pending_wait: None,
            actions: Vec::new(),
        }
    }

    /// Feed a chunk of operator bytes read at `instant` into the decoder.
    pub fn feed(&mut self, bytes: &[u8], instant: Instant) {
        // Account for an idle gap since the previous input. The gap is measured
        // to the *start* of this chunk; it becomes a `Wait` before whatever
        // action this chunk produces.
        if let Some(prev) = self.last_input {
            let gap = instant.saturating_duration_since(prev);
            if gap >= self.wait_threshold {
                self.note_gap(gap.as_secs_f64());
            }
        }
        self.last_input = Some(instant);

        for &b in bytes {
            self.pending.push(b);
            self.try_resolve();
        }
    }

    /// Flush any buffered text/pending bytes into a final action list.
    ///
    /// Called once the session ends. Any incomplete escape run left in
    /// `pending` is treated as literal text so nothing is silently dropped.
    pub fn finish(mut self) -> Vec<Action> {
        self.drain_pending_as_text();
        self.flush_text();
        self.actions
    }

    /// Record an idle gap of `seconds`. If text is currently buffered it is
    /// flushed first so the `Wait` lands *between* actions, matching how a
    /// human-authored script reads.
    fn note_gap(&mut self, seconds: f64) {
        self.flush_text();
        let rounded = self.round_wait(seconds);
        self.pending_wait = Some(match self.pending_wait.take() {
            Some(existing) => existing + rounded,
            None => rounded,
        });
    }

    /// Round an idle gap (seconds) for a tidy script. When `wait_round_ms` is
    /// set, the gap snaps to the nearest multiple of that many milliseconds;
    /// otherwise it is only trimmed to millisecond precision.
    fn round_wait(&self, seconds: f64) -> f64 {
        if self.wait_round_ms == 0 {
            // Trim to millisecond precision for a tidy script.
            return (seconds * 1000.0).round() / 1000.0;
        }
        let ms = seconds * 1000.0;
        let step = self.wait_round_ms as f64;
        (ms / step).round() * step / 1000.0
    }

    /// Attempt to resolve the `pending` buffer into one or more actions.
    fn try_resolve(&mut self) {
        loop {
            if self.pending.is_empty() {
                return;
            }
            let first = self.pending[0];
            if first == 0x1b {
                if !self.try_resolve_escape() {
                    // Need more bytes to decide; wait for the next read.
                    return;
                }
            } else if !self.try_resolve_simple() {
                return;
            }
        }
    }

    /// Resolve a control/printable byte at the front of `pending` that is not
    /// an escape sequence. Returns `false` when more bytes are needed (an
    /// incomplete UTF-8 sequence).
    fn try_resolve_simple(&mut self) -> bool {
        let first = self.pending[0];

        // A plain space (0x20) is deliberately NOT decoded as the `Space` key:
        // it is a printable character, so it collapses into the surrounding
        // `Text` action (a typed " ") rather than splitting words into separate
        // actions. `Space:` remains a valid *hand-written* key, but the recorder
        // never emits it. (This does not affect `Ctrl-Space`, whose NUL byte is
        // handled via the control-byte path.)
        if first != b' ' {
            // Named single-byte keys (Enter, Tab, Backspace).
            if let Some(key) = Key::from_bytes(&[first]) {
                self.pending.remove(0);
                self.push_key(key);
                return true;
            }
        }

        // A line feed (`\n`, 0x0a) is treated as Enter too: `KeyName::Enter`
        // canonically emits CR (`\r`), but some terminals deliver Return as LF,
        // so both map to the same key when recording.
        if first == b'\n' {
            self.pending.remove(0);
            self.push_key(Key::plain(KeyName::Enter));
            return true;
        }

        // Any remaining C0 control byte / DEL that is not a recognized key
        // (`Key::from_bytes` above already handled the `Ctrl-<letter>` control
        // bytes) is dropped: it is not part of the script grammar.
        if first < 0x20 || first == 0x7f {
            self.pending.remove(0);
            return true;
        }

        // A printable character (possibly multi-byte UTF-8). Decode the longest
        // valid character prefix; if the buffer ends mid-character, wait.
        match std::str::from_utf8(&self.pending) {
            Ok(s) => {
                let ch = s.chars().next().unwrap();
                let len = ch.len_utf8();
                self.text.push(ch);
                self.pending.drain(..len);
                true
            }
            Err(e) => {
                let valid = e.valid_up_to();
                if valid > 0 {
                    let s = std::str::from_utf8(&self.pending[..valid]).unwrap();
                    self.text.push_str(s);
                    self.pending.drain(..valid);
                    true
                } else if e.error_len().is_some() {
                    // A genuinely invalid byte: drop it so we make progress.
                    self.pending.remove(0);
                    true
                } else {
                    // Incomplete trailing multi-byte char: need more bytes.
                    false
                }
            }
        }
    }

    /// Resolve an escape sequence at the front of `pending`. Returns `false`
    /// when more bytes are needed to disambiguate a possibly-longer key.
    ///
    /// `ESC` (`Esc`) is a prefix of every arrow/nav sequence, so a short match
    /// is only committed once no *longer* known key could still extend the
    /// current buffer (either because a longer key is impossible given the
    /// bytes seen, or the buffer has reached [`MAX_KEY_LEN`]).
    fn try_resolve_escape(&mut self) -> bool {
        // If the buffer is still a strict prefix of some longer known key,
        // more bytes might complete that longer key — wait for them.
        if self.pending.len() < MAX_KEY_LEN && self.is_prefix_of_longer_key() {
            return false;
        }

        // Longest-match against known keys: try the longest known key length
        // down to a lone ESC. This covers plain nav keys (`\x1b[3~`, SS3
        // arrows) and modified CSI sequences (`\x1b[1;5C`, `\x1b[6;5~`) alike.
        let max = MAX_KEY_LEN.min(self.pending.len());
        for len in (1..=max).rev() {
            if let Some(key) = Key::from_bytes(&self.pending[..len]) {
                self.pending.drain(..len);
                self.push_key(key);
                return true;
            }
        }

        // Enough bytes and nothing matched: treat the lone ESC as the Esc key
        // and re-examine the rest on the next loop iteration.
        self.pending.remove(0);
        self.push_key(Key::plain(KeyName::Esc));
        true
    }

    /// Whether the current `pending` buffer could still be extended into a
    /// longer known key, so the decoder should wait for more bytes rather than
    /// commit a shorter match (or a lone `Esc`).
    ///
    /// An escape run that has not yet reached a sequence terminator (a CSI
    /// final byte in `@`..`~`, or the SS3 letter after `ESC O`) may still grow
    /// into a full nav/modified key, so it is treated as an incomplete prefix.
    fn is_prefix_of_longer_key(&self) -> bool {
        let p = &self.pending;
        // A lone ESC could begin any escape sequence.
        if p.as_slice() == [0x1b] {
            return true;
        }
        // `ESC O` (SS3 introducer) awaits its final letter.
        if p.as_slice() == [0x1b, b'O'] {
            return true;
        }
        // A CSI run (`ESC [ ...`) is incomplete until a final byte arrives.
        if p.len() >= 2 && p[0] == 0x1b && p[1] == b'[' {
            let last = *p.last().unwrap();
            let has_final = p.len() > 2 && (0x40..=0x7e).contains(&last);
            return !has_final;
        }
        false
    }

    /// Append a decoded key, flushing any buffered text and owed wait first.
    fn push_key(&mut self, key: Key) {
        self.flush_text();
        self.flush_wait();
        // Merge consecutive single-key actions into one `Key { keys: [..] }`
        // so `Down Down Enter` becomes one action, matching script style.
        if let Some(Action::Key { keys }) = self.actions.last_mut() {
            keys.push(key);
        } else {
            self.actions.push(Action::Key { keys: vec![key] });
        }
    }

    /// Flush accumulated printable text into a `Text` action (if any).
    fn flush_text(&mut self) {
        if !self.text.is_empty() {
            self.flush_wait();
            let text = std::mem::take(&mut self.text);
            self.actions.push(Action::Text(text));
        }
    }

    /// Emit an owed `Wait` action (if any) before the next action.
    fn flush_wait(&mut self) {
        if let Some(seconds) = self.pending_wait.take() {
            if seconds > 0.0 {
                self.actions.push(Action::Wait { seconds });
            }
        }
    }

    /// Treat whatever is left in `pending` (an incomplete escape or byte run)
    /// as literal text so end-of-session never drops captured input.
    fn drain_pending_as_text(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let leftover = std::mem::take(&mut self.pending);
        let s = String::from_utf8_lossy(&leftover);
        for ch in s.chars() {
            if !ch.is_control() {
                self.text.push(ch);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn printable_chars_collapse_into_one_text_action() {
        let base = Instant::now();
        let mut d = Decoder::new(10_000, 0);
        d.feed(b"ls", at(base, 0));
        let actions = d.finish();
        assert_eq!(actions, vec![Action::Text("ls".to_string())]);
    }

    #[test]
    fn spaces_stay_in_text_and_do_not_become_space_keys() {
        // Typing several words separated by spaces must stay one `Text` action,
        // spaces included — the recorder never emits a `Space` key.
        let base = Instant::now();
        let mut d = Decoder::new(10_000, 0);
        d.feed(b"echo recorded by ascii-rat", at(base, 0));
        let actions = d.finish();
        assert_eq!(
            actions,
            vec![Action::Text("echo recorded by ascii-rat".to_string())]
        );
    }

    #[test]
    fn enter_flushes_text_and_becomes_key() {
        let base = Instant::now();
        let mut d = Decoder::new(10_000, 0);
        d.feed(b"ls", at(base, 0));
        d.feed(b"\r", at(base, 1));
        let actions = d.finish();
        assert_eq!(
            actions,
            vec![
                Action::Text("ls".to_string()),
                Action::Key {
                    keys: vec![KeyName::Enter.into()]
                },
            ]
        );
    }

    #[test]
    fn line_feed_is_treated_as_enter() {
        let base = Instant::now();
        let mut d = Decoder::new(10_000, 0);
        d.feed(b"hi\n", at(base, 0));
        let actions = d.finish();
        assert_eq!(
            actions,
            vec![
                Action::Text("hi".to_string()),
                Action::Key {
                    keys: vec![KeyName::Enter.into()]
                },
            ]
        );
    }

    #[test]
    fn arrow_escape_sequence_becomes_key() {
        let base = Instant::now();
        let mut d = Decoder::new(10_000, 0);
        // SS3 Down: ESC O B
        d.feed(b"\x1bOB", at(base, 0));
        let actions = d.finish();
        assert_eq!(
            actions,
            vec![Action::Key {
                keys: vec![KeyName::Down.into()]
            }]
        );
    }

    #[test]
    fn partial_escape_across_reads_decodes_to_one_key() {
        let base = Instant::now();
        let mut d = Decoder::new(10_000, 0);
        // Delete is ESC [ 3 ~ split across three reads.
        d.feed(b"\x1b", at(base, 0));
        d.feed(b"[3", at(base, 1));
        d.feed(b"~", at(base, 2));
        let actions = d.finish();
        assert_eq!(
            actions,
            vec![Action::Key {
                keys: vec![KeyName::Delete.into()]
            }]
        );
    }

    #[test]
    fn idle_gap_becomes_wait_between_actions() {
        let base = Instant::now();
        let mut d = Decoder::new(400, 0);
        d.feed(b"a", at(base, 0));
        // 1s gap before the next key.
        d.feed(b"\r", at(base, 1000));
        let actions = d.finish();
        assert_eq!(
            actions,
            vec![
                Action::Text("a".to_string()),
                Action::Wait { seconds: 1.0 },
                Action::Key {
                    keys: vec![KeyName::Enter.into()]
                },
            ]
        );
    }

    #[test]
    fn wait_is_rounded_to_nearest_step() {
        let base = Instant::now();
        // Round to the nearest 500ms: a 1.7s gap snaps to 1.5s.
        let mut d = Decoder::new(400, 500);
        d.feed(b"a", at(base, 0));
        d.feed(b"\r", at(base, 1700));
        let actions = d.finish();
        assert_eq!(
            actions,
            vec![
                Action::Text("a".to_string()),
                Action::Wait { seconds: 1.5 },
                Action::Key {
                    keys: vec![KeyName::Enter.into()]
                },
            ]
        );
    }

    #[test]
    fn full_sequence_ls_enter_wait_down() {
        let base = Instant::now();
        let mut d = Decoder::new(400, 0);
        d.feed(b"ls", at(base, 0));
        d.feed(b"\r", at(base, 10));
        // long gap
        d.feed(b"\x1bOB", at(base, 1000));
        let actions = d.finish();
        assert_eq!(
            actions,
            vec![
                Action::Text("ls".to_string()),
                Action::Key {
                    keys: vec![KeyName::Enter.into()]
                },
                Action::Wait { seconds: 0.99 },
                Action::Key {
                    keys: vec![KeyName::Down.into()]
                },
            ]
        );
    }

    #[test]
    fn lone_esc_is_decoded_when_no_longer_match_follows() {
        let base = Instant::now();
        let mut d = Decoder::new(10_000, 0);
        // ESC followed by a printable 'x' (not a known escape sequence): the
        // ESC becomes an Esc key and 'x' becomes text.
        d.feed(b"\x1bxyz", at(base, 0));
        let actions = d.finish();
        assert_eq!(
            actions,
            vec![
                Action::Key {
                    keys: vec![KeyName::Esc.into()]
                },
                Action::Text("xyz".to_string()),
            ]
        );
    }

    #[test]
    fn ctrl_letter_control_byte_becomes_ctrl_key() {
        let base = Instant::now();
        let mut d = Decoder::new(10_000, 0);
        // Ctrl-U (0x15) is the "kill line" combo; it must decode back to a key.
        d.feed(&[0x15], at(base, 0));
        let actions = d.finish();
        assert_eq!(
            actions,
            vec![Action::Key {
                keys: vec![Key::parse("Ctrl-u").unwrap()]
            }]
        );
    }

    #[test]
    fn modified_csi_sequence_becomes_modified_key() {
        let base = Instant::now();
        let mut d = Decoder::new(10_000, 0);
        // Ctrl-Right: ESC [ 1 ; 5 C (a 6-byte modified CSI sequence).
        d.feed(b"\x1b[1;5C", at(base, 0));
        let actions = d.finish();
        assert_eq!(
            actions,
            vec![Action::Key {
                keys: vec![Key::parse("Ctrl-Right").unwrap()]
            }]
        );
    }

    #[test]
    fn shift_tab_sequence_becomes_shift_tab_key() {
        let base = Instant::now();
        let mut d = Decoder::new(10_000, 0);
        // Shift-Tab (back-tab): ESC [ Z.
        d.feed(b"\x1b[Z", at(base, 0));
        let actions = d.finish();
        assert_eq!(
            actions,
            vec![Action::Key {
                keys: vec![Key::parse("Shift-Tab").unwrap()]
            }]
        );
    }
}
