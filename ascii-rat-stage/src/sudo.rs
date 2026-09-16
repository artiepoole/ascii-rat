//! Shared pieces of the sudo password passthrough, used by both tools.
//!
//! `ascii-rat-bard` answers a password prompt while *replaying* a script (see
//! `script::Script::run`), and `ascii-rat-scribe` answers one while *recording*
//! a live session. Both need the same two primitives, so they live here:
//!
//! - [`PromptMatcher`] — incremental, non-blocking detection of a prompt
//!   substring in the child's output stream.
//! - [`type_password_into`] — send a password to the child one character at a
//!   time, followed by Enter.
//!
//! Neither primitive stores the password anywhere, and neither redacts output:
//! the auth widget's own masking (`*`, or nothing at all) is what keeps the
//! secret off the screen and out of the recording.

use crate::pty::PtySession;
use anyhow::Result;
use std::thread::sleep;
use std::time::Duration;

/// How many bytes of recent output to keep while looking for a prompt.
///
/// A prompt needle is far shorter than this, so the window only has to be big
/// enough that a prompt split across reads still matches. Bounded so a long
/// prompt-free stream cannot grow the buffer without limit.
const TAIL_WINDOW: usize = 4096;

/// Incremental matcher for a set of prompt substrings.
///
/// Feed it the child's output as it arrives with [`PromptMatcher::push`]; it
/// reports `true` on the first push whose accumulated tail contains any needle.
/// Matching is case-insensitive and spans read boundaries, so a prompt split
/// across two chunks is still found.
///
/// Unlike the cumulative scan in `Script::run`, the tail is a *rolling window*:
/// once matched, calling [`PromptMatcher::reset`] re-arms the matcher for a
/// later prompt instead of staying permanently tripped by output already seen.
pub struct PromptMatcher {
    /// Needles, pre-lowercased once for case-insensitive matching.
    lowered: Vec<String>,
    /// Rolling lossy-decoded tail of recent output, used only for matching.
    tail: String,
}

impl PromptMatcher {
    /// Build a matcher for `needles` (matched case-insensitively).
    pub fn new(needles: &[String]) -> PromptMatcher {
        PromptMatcher {
            lowered: needles.iter().map(|n| n.to_ascii_lowercase()).collect(),
            tail: String::new(),
        }
    }

    /// Append `bytes` to the rolling tail and report whether any needle is now
    /// present.
    ///
    /// The lossy decode is for matching only — the caller's copy of the bytes is
    /// untouched, so output is still captured and mirrored verbatim.
    pub fn push(&mut self, bytes: &[u8]) -> bool {
        if self.lowered.is_empty() {
            return false;
        }
        self.tail.push_str(&String::from_utf8_lossy(bytes));
        let hay = self.tail.to_ascii_lowercase();
        let matched = self.lowered.iter().any(|n| hay.contains(n.as_str()));
        self.trim_tail();
        matched
    }

    /// Forget the accumulated tail so a prompt seen earlier cannot re-trigger a
    /// match, arming the matcher for the *next* prompt.
    pub fn reset(&mut self) {
        self.tail.clear();
    }

    /// Drop all but the last [`TAIL_WINDOW`] bytes of the tail, keeping the
    /// truncation on a character boundary so `tail` stays valid UTF-8.
    fn trim_tail(&mut self) {
        if self.tail.len() <= TAIL_WINDOW {
            return;
        }
        let start = self.tail.len() - TAIL_WINDOW;
        let start = (start..self.tail.len())
            .find(|&i| self.tail.is_char_boundary(i))
            .unwrap_or(self.tail.len());
        self.tail.drain(..start);
    }
}

/// Type `password` into the child one character at a time, then send Enter.
///
/// Character-by-character (rather than one bulk write) is deliberate: it drives
/// an interactive PAM widget that consumes individual keystrokes, which a single
/// write does not satisfy. `per_char` is the pause between characters.
///
/// Enter is sent as a carriage return, matching how a terminal submits input.
/// Callers must therefore *not* send their own Enter afterwards.
pub fn type_password_into(
    session: &mut PtySession,
    password: &str,
    per_char: Duration,
) -> Result<()> {
    for ch in password.chars() {
        let mut buf = [0u8; 4];
        session.write(ch.encode_utf8(&mut buf).as_bytes())?;
        sleep(per_char);
    }
    session.write(b"\r")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn needles() -> Vec<String> {
        vec!["assword".to_string(), "[sudo]".to_string()]
    }

    #[test]
    fn matches_a_needle_in_a_single_push() {
        let mut m = PromptMatcher::new(&needles());
        assert!(m.push(b"[sudo] password for bob: "));
    }

    #[test]
    fn matching_is_case_insensitive() {
        let mut m = PromptMatcher::new(&needles());
        assert!(m.push(b"PASSWORD:"));
    }

    #[test]
    fn matches_a_needle_split_across_pushes() {
        // The prompt straddles a read boundary: neither half contains a needle,
        // but the accumulated tail does.
        let mut m = PromptMatcher::new(&needles());
        assert!(!m.push(b"[su"));
        assert!(m.push(b"do] password for bob: "));
    }

    #[test]
    fn no_match_on_unrelated_output() {
        let mut m = PromptMatcher::new(&needles());
        assert!(!m.push(b"total 12\ndrwxr-xr-x 3 bob bob 4096 .\n"));
    }

    #[test]
    fn reset_rearms_the_matcher() {
        let mut m = PromptMatcher::new(&needles());
        assert!(m.push(b"[sudo] password for bob: "));
        // Without a reset the tail still holds the first prompt, so any later
        // push would keep reporting a match.
        assert!(m.push(b"\r\n"));
        m.reset();
        assert!(!m.push(b"\r\n"));
        // A genuinely new prompt matches again.
        assert!(m.push(b"[sudo] password for bob: "));
    }

    #[test]
    fn empty_needles_never_match() {
        let mut m = PromptMatcher::new(&[]);
        assert!(!m.push(b"[sudo] password for bob: "));
    }

    #[test]
    fn tail_is_bounded_but_still_matches_recent_output() {
        let mut m = PromptMatcher::new(&needles());
        // Push far more than the window of prompt-free output.
        for _ in 0..8 {
            assert!(!m.push(&vec![b'x'; TAIL_WINDOW]));
            assert!(m.tail.len() <= TAIL_WINDOW);
        }
        // A prompt arriving after all that noise is still found.
        assert!(m.push(b"[sudo] password for bob: "));
    }

    #[test]
    fn tail_trimming_keeps_valid_utf8() {
        let mut m = PromptMatcher::new(&needles());
        // Multi-byte characters, so a naive byte-index trim would split one.
        for _ in 0..8 {
            assert!(!m.push("é".repeat(TAIL_WINDOW).as_bytes()));
        }
        // Reaching here without a panic means every trim landed on a boundary.
        assert!(m.push(b"password:"));
    }
}
