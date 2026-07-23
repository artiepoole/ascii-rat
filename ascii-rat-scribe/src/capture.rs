//! The interactive recording loop.
//!
//! Puts the real terminal into raw mode, spawns the target command inside a
//! [`PtySession`], and runs a loop that:
//! - reads the operator's keystrokes from the real stdin, forwards them to the
//!   child, and feeds them (with timing) to the [`Decoder`];
//! - drains the child's output and mirrors it to the real stdout so the
//!   operator sees the program live.
//!
//! The terminal is always restored to cooked mode on exit (normal, error, or
//! panic) via [`RawModeGuard`].

use crate::decoder::Decoder;
use anyhow::{Context, Result};
use ascii_rat_stage::pty::{PtyCommandBuilder as CommandBuilder, PtySession};
use ascii_rat_stage::script::Action;
use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// Options controlling a recording session.
pub struct CaptureOptions {
    /// The command and its arguments to run inside the PTY.
    pub command: Vec<String>,
    pub cols: u16,
    pub rows: u16,
    /// Idle gap threshold (milliseconds) for inserting `Wait` actions.
    pub wait_threshold_ms: u64,
}

/// Restores the terminal to cooked mode when dropped.
struct RawModeGuard;

impl RawModeGuard {
    /// Enable raw mode on the real terminal, returning a guard that disables it
    /// again on drop.
    fn enable() -> Result<RawModeGuard> {
        crossterm::terminal::enable_raw_mode().context("failed to enable terminal raw mode")?;
        Ok(RawModeGuard)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // Best-effort restore; nothing useful to do if this fails during unwind.
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Run an interactive recording session, returning the decoded actions.
///
/// Blocks until the child exits (or its input stream ends), then returns the
/// captured [`Action`]s (without the terminating `END_REC`, which the emitter
/// adds).
pub fn record(options: &CaptureOptions) -> Result<Vec<Action>> {
    // Build the child command line.
    let (program, args) = options
        .command
        .split_first()
        .context("empty command line")?;
    let mut cmd = CommandBuilder::new(program);
    for arg in args {
        cmd.arg(arg);
    }
    if let Ok(term) = std::env::var("TERM") {
        cmd.env("TERM", term);
    }

    // Enter raw mode; restored automatically on any exit path.
    let _guard = RawModeGuard::enable()?;

    let mut session = PtySession::spawn(cmd, options.cols, options.rows)
        .context("failed to spawn command in PTY")?;

    let mut decoder = Decoder::new(options.wait_threshold_ms);
    let stdin_rx = spawn_stdin_reader();
    let mut stdout = std::io::stdout();

    loop {
        // Mirror any child output to the real terminal.
        let mut wrote = false;
        for chunk in session.drain_output() {
            stdout
                .write_all(&chunk.bytes)
                .context("failed to write child output to terminal")?;
            wrote = true;
        }
        if wrote {
            stdout.flush().ok();
        }

        // Forward any operator keystrokes to the child and the decoder.
        match stdin_rx.try_recv() {
            Ok((bytes, instant)) => {
                if !bytes.is_empty() {
                    session
                        .write(&bytes)
                        .context("failed to forward keystroke to child")?;
                    decoder.feed(&bytes, instant);
                }
            }
            Err(TryRecvError::Empty) => {}
            // The stdin reader thread ended (EOF on the real terminal).
            Err(TryRecvError::Disconnected) => {}
        }

        // Stop as soon as the child has exited.
        if session.try_wait()? {
            break;
        }

        // Avoid a busy spin; small enough to stay responsive.
        std::thread::sleep(Duration::from_millis(5));
    }

    // Drain any final output the child produced just before exiting.
    for chunk in session.close().unwrap_or_default() {
        let _ = stdout.write_all(&chunk.bytes);
    }
    stdout.flush().ok();

    Ok(decoder.finish())
}

/// Spawn a thread that reads the real stdin in raw mode and forwards each read
/// (with the instant it arrived) over a channel.
///
/// stdin reads block, so they must live off the main loop; the channel lets the
/// main loop poll for input without blocking on output mirroring.
fn spawn_stdin_reader() -> Receiver<(Vec<u8>, Instant)> {
    let (tx, rx) = mpsc::channel::<(Vec<u8>, Instant)>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    if tx.send((buf[..n].to_vec(), Instant::now())).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}
