//! Thin wrapper around `portable-pty` for spawning a child in a PTY and
//! capturing its output as timestamped chunks.

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

/// Re-export of `portable-pty`'s [`CommandBuilder`] so downstream crates can
/// construct the command passed to [`PtySession::spawn`] without depending on a
/// specific `portable-pty` version themselves.
pub use portable_pty::CommandBuilder as PtyCommandBuilder;
use std::io::Read;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A timestamped chunk of raw output bytes read from the child.
#[derive(Debug)]
pub struct OutputChunk {
    /// Instant the chunk was read (used to compute relative event times).
    pub instant: Instant,
    pub bytes: Vec<u8>,
}

/// A single shared writer to the PTY master.
///
/// Both the caller (typing scripted keystrokes via [`PtySession::write`]) and
/// the background reader thread (auto-answering terminal queries, see
/// [`answer_terminal_queries`]) write to the child through this one handle.
/// `portable-pty` forbids taking the master writer more than once, so it is
/// shared behind a mutex instead of cloned.
type SharedWriter = Arc<Mutex<Box<dyn std::io::Write + Send>>>;

/// A running child process inside a PTY, with a background reader thread.
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    writer: SharedWriter,
    child: Box<dyn Child + Send + Sync>,
    reader_thread: Option<JoinHandle<()>>,
    output_rx: Receiver<OutputChunk>,
}

impl PtySession {
    /// Open a PTY of the given size, spawn `command`, and start draining output.
    pub fn spawn(command: CommandBuilder, cols: u16, rows: u16) -> Result<PtySession> {
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open PTY")?;

        let child = pair
            .slave
            .spawn_command(command)
            .context("failed to spawn child command in PTY")?;
        // The slave handle is no longer needed once the child owns it.
        drop(pair.slave);

        let writer: SharedWriter = Arc::new(Mutex::new(
            pair.master
                .take_writer()
                .context("failed to take PTY writer")?,
        ));
        let mut reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone PTY reader")?;

        let (tx, rx) = mpsc::channel::<OutputChunk>();
        // Give the reader thread its own handle to the shared writer so it can
        // reply to terminal queries the moment they are read.
        let responder_writer = Arc::clone(&writer);
        let reader_thread = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            // Carry buffer: holds the trailing bytes of a read that form an
            // incomplete multi-byte UTF-8 sequence. They are prepended to the
            // next read so a character/escape byte run is never split across an
            // emitted chunk (which would otherwise corrupt into U+FFFD).
            let mut carry: Vec<u8> = Vec::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        // EOF: flush any leftover carry bytes verbatim so
                        // nothing is dropped (invalid UTF-8 only as a last
                        // resort).
                        if !carry.is_empty() {
                            let chunk = OutputChunk {
                                instant: Instant::now(),
                                bytes: std::mem::take(&mut carry),
                            };
                            let _ = tx.send(chunk);
                        }
                        break;
                    }
                    Ok(n) => {
                        let instant = Instant::now();
                        // Act as a real terminal: reply to any device/color
                        // queries in this read so the child consumes the answers
                        // itself. Otherwise the answers a real terminal supplies
                        // on playback leak onto the screen as scrambled bytes.
                        answer_terminal_queries(&buf[..n], &responder_writer);
                        // Combine any carried tail with the freshly-read bytes.
                        carry.extend_from_slice(&buf[..n]);
                        // Emit the largest valid UTF-8 prefix; keep the
                        // incomplete tail (if any) in `carry` for next time.
                        let valid = valid_utf8_prefix_len(&carry);
                        if valid == 0 {
                            // The whole buffer is an incomplete tail; wait for
                            // more bytes before emitting anything.
                            continue;
                        }
                        let bytes = carry[..valid].to_vec();
                        carry.drain(..valid);
                        let chunk = OutputChunk { instant, bytes };
                        if tx.send(chunk).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(PtySession {
            master: pair.master,
            writer,
            child,
            reader_thread: Some(reader_thread),
            output_rx: rx,
        })
    }

    /// Write raw bytes to the child (keystrokes).
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("PTY writer mutex poisoned"))?;
        writer.write_all(bytes).context("failed to write to PTY")?;
        writer.flush().context("failed to flush PTY writer")?;
        Ok(())
    }

    /// Drain all output chunks that have arrived so far (non-blocking).
    pub fn drain_output(&self) -> Vec<OutputChunk> {
        let mut chunks = Vec::new();
        while let Ok(chunk) = self.output_rx.try_recv() {
            chunks.push(chunk);
        }
        chunks
    }

    /// Check whether the child has exited, without blocking.
    ///
    /// Returns `Ok(true)` once the child has terminated, `Ok(false)` while it
    /// is still running. Used by an interactive driver (e.g. the recorder) to
    /// stop the capture loop as soon as the wrapped program exits.
    pub fn try_wait(&mut self) -> Result<bool> {
        Ok(self
            .child
            .try_wait()
            .context("failed to poll child status")?
            .is_some())
    }

    /// Block until the child's recent output contains one of `needles`, then
    /// return every chunk consumed while waiting (so the caller can keep or
    /// discard them).
    ///
    /// Matching is case-insensitive and performed against a rolling decoded
    /// tail of the output (lossy decode used **only** for matching — the
    /// returned bytes are untouched). Used to wait for sudo's password prompt
    /// (`"assword"` / `"[sudo]"`) before typing the password so it is never sent
    /// before the prompt appears.
    ///
    /// Behaviour:
    /// - Returns `Ok(consumed)` as soon as any needle is found.
    /// - Returns `Ok(consumed)` if the child disconnects (EOF) before a match,
    ///   so a crashed child does not raise a spurious error.
    /// - Returns an error if `timeout` elapses before a match.
    pub fn wait_for_output_matching(
        &self,
        needles: &[&str],
        timeout: Duration,
    ) -> Result<Vec<OutputChunk>> {
        wait_for_output_matching_on(&self.output_rx, needles, timeout)
    }

    /// Like [`PtySession::wait_for_output_matching`], but invokes `on_chunk`
    /// with each chunk *as it arrives* (before checking for a match), so a live
    /// `--watch` view can mirror the child's output during the wait instead of
    /// only after it completes.
    ///
    /// The chunks are still returned (unchanged) so the caller can capture them
    /// into the cast; `on_chunk` is for side effects (live mirroring) only.
    pub fn wait_for_output_matching_each<F>(
        &self,
        needles: &[&str],
        timeout: Duration,
        on_chunk: F,
    ) -> Result<Vec<OutputChunk>>
    where
        F: FnMut(&OutputChunk),
    {
        wait_for_output_matching_each_on(&self.output_rx, needles, timeout, on_chunk)
    }

    /// Terminate the child, join the reader thread, and return any remaining
    /// output chunks.
    pub fn close(mut self) -> Result<Vec<OutputChunk>> {
        // Best-effort: signal the child to stop, then wait for it.
        let _ = self.child.kill();
        let _ = self.child.wait();

        // Killing the child above closes the slave side, so the reader sees
        // EOF and its thread exits even though the shared writer `Arc` is also
        // held by that thread (dropping this reference alone would not send
        // EOF). Dropping the master here releases the remaining PTY resources.
        drop(self.writer);
        drop(self.master);

        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }

        let mut chunks = Vec::new();
        while let Ok(chunk) = self.output_rx.try_recv() {
            chunks.push(chunk);
        }
        Ok(chunks)
    }
}

/// Block on `rx` until the accumulated output contains one of `needles`
/// (case-insensitive), returning the chunks consumed while waiting.
///
/// Shared implementation behind [`PtySession::wait_for_output_matching`],
/// factored out so it can be unit-tested against a plain channel without a real
/// PTY. See that method for the full behaviour contract.
pub(crate) fn wait_for_output_matching_on(
    rx: &Receiver<OutputChunk>,
    needles: &[&str],
    timeout: Duration,
) -> Result<Vec<OutputChunk>> {
    // No per-chunk side effect: delegate to the callback form with a no-op.
    wait_for_output_matching_each_on(rx, needles, timeout, |_| {})
}

/// Callback-driven variant of [`wait_for_output_matching_on`]: `on_chunk` is
/// invoked with each chunk *as it arrives* (before the match check), so callers
/// can mirror output live during the wait rather than only after it completes.
/// See [`PtySession::wait_for_output_matching`] for the full behaviour contract.
pub(crate) fn wait_for_output_matching_each_on<F>(
    rx: &Receiver<OutputChunk>,
    needles: &[&str],
    timeout: Duration,
    mut on_chunk: F,
) -> Result<Vec<OutputChunk>>
where
    F: FnMut(&OutputChunk),
{
    // Pre-lowercase the needles once for case-insensitive matching.
    let lowered: Vec<String> = needles.iter().map(|n| n.to_ascii_lowercase()).collect();
    let deadline = Instant::now() + timeout;
    let mut consumed: Vec<OutputChunk> = Vec::new();
    // Rolling decoded tail used only for substring matching.
    let mut tail = String::new();

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("timed out waiting for expected output");
        }
        match rx.recv_timeout(remaining) {
            Ok(chunk) => {
                // Mirror this chunk live before anything else, so the watch view
                // updates the instant output arrives during the wait.
                on_chunk(&chunk);
                tail.push_str(&String::from_utf8_lossy(&chunk.bytes));
                consumed.push(chunk);
                let hay = tail.to_ascii_lowercase();
                if lowered.iter().any(|n| hay.contains(n.as_str())) {
                    return Ok(consumed);
                }
                // Keep only the last ~4096 bytes of the tail so a long
                // prompt-free stream does not grow it without bound (a prompt
                // needle is far shorter than this window).
                if tail.len() > 4096 {
                    let start = tail.len() - 4096;
                    // Advance to a char boundary to keep `tail` valid UTF-8.
                    let start = (start..tail.len())
                        .find(|&i| tail.is_char_boundary(i))
                        .unwrap_or(tail.len());
                    tail.drain(..start);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                anyhow::bail!("timed out waiting for expected output");
            }
            // Child gone: return whatever we consumed instead of erroring.
            Err(RecvTimeoutError::Disconnected) => return Ok(consumed),
        }
    }
}

/// Return the length of the longest prefix of `bytes` that is valid UTF-8.
///
/// If the buffer ends with an *incomplete* multi-byte UTF-8 sequence, that
/// trailing (potentially completable) run is excluded so it can be carried over
/// to the next read. A genuine encoding error (not just a truncated tail) is
/// treated as valid up to the error so it is still emitted (never dropped).
pub fn valid_utf8_prefix_len(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(e) => {
            let valid = e.valid_up_to();
            match e.error_len() {
                // `None` => the error is an incomplete sequence at the very end:
                // keep only the valid prefix and carry the tail over.
                None => valid,
                // `Some(_)` => a real invalid sequence in the middle; emit up to
                // and including it (as-is) so nothing is lost. Emitting the
                // whole buffer keeps the reader progressing; the lossy decode in
                // `chunks_to_events` handles the rare bad byte without panicking.
                Some(_) => bytes.len(),
            }
        }
    }
}

/// Scan a freshly-read output slice for terminal *queries* and write the reply
/// a real terminal would send back to the child through `writer`.
///
/// A full-screen TUI probes the terminal for its capabilities (cursor
/// position, device attributes, background/foreground colour, …) and blocks or
/// misbehaves until it receives an answer. During recording there is no real
/// terminal on the child's input, so those queries go unanswered. Worse, the
/// query bytes get echoed/captured and, on playback, a real terminal *does*
/// answer them — its answers then leak onto the screen as scrambled control
/// sequences (e.g. `\u001b]11;rgb:.../\u001b\` and `\u001b[7;1R`).
///
/// By replying here — as the child produces each query — the child consumes the
/// answer itself, so nothing leaks on playback. The replies deliberately report
/// generic, safe values; their exact content is irrelevant to the recorded
/// output because the child absorbs them.
fn answer_terminal_queries(bytes: &[u8], writer: &SharedWriter) {
    let reply = build_query_reply(bytes);
    if reply.is_empty() {
        return;
    }
    if let Ok(mut w) = writer.lock() {
        use std::io::Write;
        let _ = w.write_all(&reply);
        let _ = w.flush();
    }
}

/// Build the concatenated reply bytes for every terminal query found in `bytes`.
///
/// Split out from [`answer_terminal_queries`] so the query→reply mapping can be
/// unit-tested without a live PTY. Returns an empty vector when there is nothing
/// to answer. Recognised queries:
/// - `ESC [ 6 n` (DSR — cursor position) → `ESC [ 1 ; 1 R`
/// - `ESC [ 5 n` (DSR — device status)   → `ESC [ 0 n` (OK)
/// - `ESC [ c` / `ESC [ 0 c` (Primary DA) → a VT100-with-AVO identity
/// - `ESC ] 10 ; ?` (OSC — foreground)   → a light-grey foreground report
/// - `ESC ] 11 ; ?` (OSC — background)   → a black background report
fn build_query_reply(bytes: &[u8]) -> Vec<u8> {
    const ESC: u8 = 0x1b;
    const BEL: u8 = 0x07;
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != ESC {
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            // CSI sequences: ESC [ ...
            Some(b'[') => {
                // Read the CSI parameter/intermediate bytes up to the final byte.
                let start = i + 2;
                let mut j = start;
                while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                    j += 1;
                }
                if j >= bytes.len() {
                    // Incomplete CSI at the end of the read; stop scanning.
                    break;
                }
                let params = &bytes[start..j];
                let final_byte = bytes[j];
                match final_byte {
                    // Device Status Report.
                    b'n' => {
                        if params == b"6" {
                            // Cursor position report: row 1, col 1.
                            out.extend_from_slice(&[ESC, b'[', b'1', b';', b'1', b'R']);
                        } else if params == b"5" {
                            // Terminal OK.
                            out.extend_from_slice(&[ESC, b'[', b'0', b'n']);
                        }
                    }
                    // Primary Device Attributes (ESC[c or ESC[0c).
                    b'c' => {
                        if params.is_empty() || params == b"0" {
                            // VT100 with Advanced Video Option.
                            out.extend_from_slice(&[ESC, b'[', b'?', b'1', b';', b'2', b'c']);
                        }
                    }
                    _ => {}
                }
                i = j + 1;
            }
            // OSC sequences: ESC ] ... terminated by BEL or ST (ESC \).
            Some(b']') => {
                let start = i + 2;
                let mut j = start;
                // Find the terminator: BEL, or ST (ESC \).
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
                    // Incomplete OSC at the end of the read; stop scanning.
                    break;
                }
                let body = &bytes[start..j];
                // Answer only colour *queries* (ending in "?"), echoing the ST
                // form of terminator so the child parses it consistently.
                if body == b"10;?" {
                    // Foreground: light grey.
                    out.extend_from_slice(b"\x1b]10;rgb:c7c7/c7c7/c7c7\x1b\\");
                } else if body == b"11;?" {
                    // Background: black.
                    out.extend_from_slice(b"\x1b]11;rgb:0000/0000/0000\x1b\\");
                }
                i = j + term_len;
            }
            // Any other/unknown escape: skip the ESC and continue.
            _ => {
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        build_query_reply, valid_utf8_prefix_len, wait_for_output_matching_each_on,
        wait_for_output_matching_on, OutputChunk,
    };
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn chunk(bytes: &[u8]) -> OutputChunk {
        OutputChunk {
            instant: Instant::now(),
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn wait_matches_needle_across_accumulated_chunks() {
        let (tx, rx) = mpsc::channel();
        // The needle "[sudo]" is split across two chunks and only completes
        // once both have arrived.
        tx.send(chunk(b"prompting... [su")).unwrap();
        tx.send(chunk(b"do] password for user:")).unwrap();
        let consumed =
            wait_for_output_matching_on(&rx, &["assword", "[sudo]"], Duration::from_secs(1))
                .expect("needle should be found");
        // Both chunks are consumed up to and including the match.
        assert_eq!(consumed.len(), 2);
    }

    #[test]
    fn wait_each_mirrors_every_chunk_before_the_match() {
        let (tx, rx) = mpsc::channel();
        // Two non-matching chunks arrive before the one containing the needle.
        tx.send(chunk(b"line one\r\n")).unwrap();
        tx.send(chunk(b"line two\r\n")).unwrap();
        tx.send(chunk(b"done __DONE__\r\n")).unwrap();
        let mut mirrored: Vec<Vec<u8>> = Vec::new();
        let consumed = wait_for_output_matching_each_on(
            &rx,
            &["__DONE__"],
            Duration::from_secs(1),
            |c| mirrored.push(c.bytes.clone()),
        )
        .expect("needle should be found");
        // The callback fired for every consumed chunk, in order, as they
        // arrived — including the ones before the match (the whole point: the
        // live view is fed during the wait, not only after it).
        assert_eq!(mirrored.len(), 3);
        assert_eq!(consumed.len(), 3);
        assert_eq!(mirrored[0], b"line one\r\n");
        assert_eq!(mirrored[1], b"line two\r\n");
        assert_eq!(mirrored[2], b"done __DONE__\r\n");
    }

    #[test]
    fn wait_is_case_insensitive() {
        let (tx, rx) = mpsc::channel();
        tx.send(chunk(b"Enter PASSWORD: ")).unwrap();
        let consumed = wait_for_output_matching_on(&rx, &["assword"], Duration::from_secs(1))
            .expect("case-insensitive match should succeed");
        assert_eq!(consumed.len(), 1);
    }

    #[test]
    fn wait_times_out_when_no_prompt_appears() {
        let (tx, rx) = mpsc::channel();
        tx.send(chunk(b"some unrelated output\r\n")).unwrap();
        // Keep the sender alive so the channel does not disconnect; the wait
        // must time out rather than match or return early.
        let err = wait_for_output_matching_on(&rx, &["assword", "[sudo]"], Duration::from_millis(50))
            .expect_err("should time out");
        assert!(err.to_string().contains("timed out"));
        drop(tx);
    }

    #[test]
    fn wait_returns_consumed_on_disconnect() {
        let (tx, rx) = mpsc::channel();
        tx.send(chunk(b"partial output")).unwrap();
        drop(tx); // child gone before a prompt appears
        let consumed = wait_for_output_matching_on(&rx, &["assword"], Duration::from_secs(1))
            .expect("disconnect should not error");
        assert_eq!(consumed.len(), 1);
    }

    #[test]
    fn full_valid_utf8_returns_full_length() {
        let s = "hello → world".as_bytes();
        assert_eq!(valid_utf8_prefix_len(s), s.len());
    }

    #[test]
    fn plain_ascii_returns_full_length() {
        let s = b"plain ascii only";
        assert_eq!(valid_utf8_prefix_len(s), s.len());
    }

    #[test]
    fn incomplete_trailing_multibyte_is_excluded() {
        // "a→": '→' is 0xE2 0x86 0x92. Truncate to keep only the first two
        // bytes of the 3-byte sequence.
        let mut bytes = b"a".to_vec();
        bytes.extend_from_slice(&[0xE2, 0x86]); // incomplete '→'
        // Only the 'a' (1 byte) is a valid prefix; the tail is carried over.
        assert_eq!(valid_utf8_prefix_len(&bytes), 1);
    }

    #[test]
    fn read_ending_exactly_on_boundary_has_empty_tail() {
        let bytes = "a→".as_bytes(); // full, ends on a boundary
        assert_eq!(valid_utf8_prefix_len(bytes), bytes.len());
    }

    #[test]
    fn no_reply_for_ordinary_output() {
        // Plain text and non-query escape sequences (colour SGR, cursor moves)
        // must not trigger any reply.
        assert!(build_query_reply(b"hello world").is_empty());
        assert!(build_query_reply(b"\x1b[100Dsome text\x1b[0m").is_empty());
        assert!(build_query_reply(b"\x1b[?25l\x1b[?2004h").is_empty());
    }

    #[test]
    fn answers_cursor_position_report() {
        // ESC[6n (DSR-CPR) → ESC[1;1R
        assert_eq!(build_query_reply(b"\x1b[6n"), b"\x1b[1;1R");
    }

    #[test]
    fn answers_device_status_ok() {
        // ESC[5n (DSR) → ESC[0n
        assert_eq!(build_query_reply(b"\x1b[5n"), b"\x1b[0n");
    }

    #[test]
    fn answers_primary_device_attributes() {
        // ESC[c and ESC[0c both request Primary DA → VT100 with AVO.
        assert_eq!(build_query_reply(b"\x1b[c"), b"\x1b[?1;2c");
        assert_eq!(build_query_reply(b"\x1b[0c"), b"\x1b[?1;2c");
    }

    #[test]
    fn answers_background_colour_query() {
        // OSC 11 ; ? terminated by ST → a background colour report.
        assert_eq!(
            build_query_reply(b"\x1b]11;?\x1b\\"),
            b"\x1b]11;rgb:0000/0000/0000\x1b\\"
        );
        // BEL terminator is accepted too.
        assert_eq!(
            build_query_reply(b"\x1b]11;?\x07"),
            b"\x1b]11;rgb:0000/0000/0000\x1b\\"
        );
    }

    #[test]
    fn answers_foreground_colour_query() {
        assert_eq!(
            build_query_reply(b"\x1b]10;?\x1b\\"),
            b"\x1b]10;rgb:c7c7/c7c7/c7c7\x1b\\"
        );
    }

    #[test]
    fn answers_the_reported_scrambled_query_sequence() {
        // The exact sequence from the bug report: an OSC 11 background-colour
        // query immediately followed by a DSR cursor-position query
        // (`\u001b]11;?\u001b\\\u001b[6n`). Both must be answered so neither
        // leaks onto the screen on playback.
        let reply = build_query_reply(b"\x1b]11;?\x1b\\\x1b[6n");
        assert_eq!(reply, b"\x1b]11;rgb:0000/0000/0000\x1b\\\x1b[1;1R");
    }

    #[test]
    fn colour_query_only_answers_the_query_form() {
        // A colour *set* (no trailing "?") or a colour *report* must not be
        // answered — only the "?" query form triggers a reply.
        assert!(build_query_reply(b"\x1b]11;rgb:2222/1f1f/2222\x1b\\").is_empty());
    }

    #[test]
    fn incomplete_trailing_query_is_ignored() {
        // A query split at the end of a read (no terminator/final byte yet)
        // produces no reply; it will be handled once the rest arrives.
        assert!(build_query_reply(b"text\x1b[6").is_empty());
        assert!(build_query_reply(b"text\x1b]11;?").is_empty());
    }
}
