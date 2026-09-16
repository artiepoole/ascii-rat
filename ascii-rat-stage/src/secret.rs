//! A string that is wiped from memory when dropped.
//!
//! Used for the sudo password, which — unlike `sudo`'s own use of one — has to
//! be held for the whole length of a recording, because the prompt it answers
//! may not appear until minutes in. That long window is the reason this wrapper
//! exists.
//!
//! # What this does and does not buy you
//!
//! Dropping a plain `String` frees its heap buffer without altering the bytes,
//! so the plaintext lingers in freed memory until something else reuses it. That
//! widens the window in which the password can reach persistent storage through
//! a core dump or a swapped-out page. [`SecretString`] overwrites the buffer
//! before releasing it, closing that window at the point of drop.
//!
//! It cannot retroactively erase copies made before the value got here. In
//! particular `rpassword::prompt_password` returns a plain `String`
//! (`rtoolbox`'s zeroing wrapper moves the buffer out before its own `Drop`
//! runs, so the returned value is unprotected), and one of its input-fixup paths
//! reallocates. Wrap the password as early as possible to keep that residue to a
//! minimum, and treat this as damage limitation rather than a guarantee.
//!
//! Nor does it defend against an attacker who can read the process's memory
//! while it is running: `/proc/<pid>/mem` is open to the same user and to root.

use std::fmt;
use std::ptr;
use std::sync::atomic::{Ordering, compiler_fence, fence};

/// A `String` whose bytes are overwritten with zeroes when it is dropped.
///
/// Deliberately does not implement `Clone`, `Debug` (beyond a redacted form), or
/// `Display`: every copy is another buffer to wipe, and secrets should not be
/// printable by accident. Read the contents with [`SecretString::expose`] at the
/// point of use.
pub struct SecretString {
    inner: String,
}

impl SecretString {
    /// Take ownership of `secret`, wiping it on drop.
    ///
    /// The `String` is moved, not copied, so this does not add a second buffer.
    pub fn new(secret: String) -> SecretString {
        SecretString { inner: secret }
    }

    /// Borrow the plaintext.
    ///
    /// Named to make the disclosure obvious at call sites. Avoid holding the
    /// returned slice, and never copy it into an unprotected `String`.
    pub fn expose(&self) -> &str {
        &self.inner
    }

    /// Whether the secret is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl From<String> for SecretString {
    fn from(secret: String) -> SecretString {
        SecretString::new(secret)
    }
}

/// Redacted, so a stray `{:?}` in a log or error cannot disclose the secret.
impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.zero();
    }
}

impl SecretString {
    /// Destroy the contents in place. This is exactly what [`Drop`] does; it is
    /// a named method so the behaviour can be tested without reading freed
    /// memory (which would be undefined behaviour, and in practice races with
    /// the allocator writing its own bookkeeping into the released block).
    fn zero(&mut self) {
        wipe(&mut self.inner);
    }
}

/// Overwrite a string's bytes with zeroes in place.
///
/// `write_volatile` stops the compiler from eliding writes to memory it can see
/// is about to be freed, and the fences stop the writes being reordered past the
/// deallocation.
///
/// Takes `&mut str` because only the contents are destroyed; length and capacity
/// are left alone, so no reallocation can move the plaintext elsewhere.
///
/// Exposed so callers can also wipe an intermediate buffer that transiently held
/// a secret (see `scrub`), not only values owned by a [`SecretString`].
pub fn wipe(text: &mut str) {
    // SAFETY: zero is valid UTF-8, so the string stays well-formed. The write is
    // within the string's own allocation and no other reference exists while we
    // hold `&mut`.
    for byte in unsafe { text.as_bytes_mut() } {
        unsafe { ptr::write_volatile(byte, 0u8) };
    }
    fence(Ordering::SeqCst);
    compiler_fence(Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_the_secret_it_was_given() {
        let secret = SecretString::new("hunter2".to_string());
        assert_eq!(secret.expose(), "hunter2");
        assert!(!secret.is_empty());
    }

    #[test]
    fn empty_is_reported() {
        assert!(SecretString::new(String::new()).is_empty());
    }

    #[test]
    fn debug_does_not_disclose_the_secret() {
        let secret = SecretString::new("hunter2".to_string());
        let shown = format!("{secret:?}");
        assert!(!shown.contains("hunter2"), "debug output leaked: {shown}");
        assert!(shown.contains("redacted"));
    }

    #[test]
    fn wipe_zeroes_the_bytes_in_place() {
        let mut text = "hunter2".to_string();
        let capacity = text.capacity();
        wipe(&mut text);
        assert_eq!(text.as_bytes(), &[0u8; 7]);
        // Wiping must not reallocate, or the old bytes would survive elsewhere.
        assert_eq!(text.capacity(), capacity);
    }

    #[test]
    fn wipe_handles_an_empty_string() {
        let mut text = String::new();
        wipe(&mut text);
        assert!(text.is_empty());
    }

    /// The wipe that `Drop` performs must destroy the contents, not merely
    /// release them.
    ///
    /// Exercises the same method `Drop` calls rather than inspecting the buffer
    /// after a real drop: reading freed memory is undefined behaviour, and the
    /// allocator writes free-list bookkeeping into the block, so such a check
    /// would be both unsound and unreliable.
    #[test]
    fn drop_destroys_the_contents() {
        let mut secret = SecretString::new("hunter2".to_string());
        let capacity = secret.inner.capacity();
        secret.zero();
        assert_eq!(secret.expose().as_bytes(), &[0u8; 7]);
        assert!(!secret.expose().contains("hunter2"));
        // No reallocation, so no unwiped copy is left behind.
        assert_eq!(secret.inner.capacity(), capacity);
    }
}
