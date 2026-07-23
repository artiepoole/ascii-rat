//! ascii-rat-scribe — interactive recorder.
//!
//! Runs a target command inside a PTY, forwards the operator's keystrokes to
//! it while mirroring the child's output to the real terminal, and translates
//! the captured keystrokes + idle gaps into a `demo.yaml` script that
//! `ascii-rat-bard` can replay.

mod capture;
mod decoder;
mod emit;

use anyhow::{bail, Result};
use clap::Parser;
use std::process::ExitCode;

/// Record a live terminal session into a `demo.yaml` script.
#[derive(Debug, Parser)]
#[command(
    name = "ascii-rat-scribe",
    about = "Record a live terminal session into a demo.yaml script",
    version
)]
struct Cli {
    /// Where to write the produced script.
    #[arg(short = 'o', long = "output", default_value = "demo.yaml")]
    output: std::path::PathBuf,

    /// `output_file` field written into the produced script (the `.cast` that
    /// `ascii-rat-bard` will later record to).
    #[arg(long = "cast", default_value = "demo.cast")]
    cast: String,

    /// Idle time (milliseconds) after which a gap becomes a `Wait` action.
    #[arg(long = "wait-threshold-ms", default_value_t = 400)]
    wait_threshold_ms: u64,

    /// PTY width in columns (defaults to the current terminal size).
    #[arg(long = "cols")]
    cols: Option<u16>,

    /// PTY height in rows (defaults to the current terminal size).
    #[arg(long = "rows")]
    rows: Option<u16>,

    /// `typing_delay_ms` written into the produced script's header.
    #[arg(long = "typing-delay-ms", default_value_t = 75)]
    typing_delay_ms: u64,

    /// The command (and its arguments) to run and record. Everything after
    /// `--` is treated as the command line.
    #[arg(trailing_var_arg = true, required = true)]
    command: Vec<String>,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    if cli.command.is_empty() {
        bail!("no command given; usage: ascii-rat-scribe [options] -- <command...>");
    }

    let (cols, rows) = resolve_size(cli.cols, cli.rows);

    let options = capture::CaptureOptions {
        command: cli.command.clone(),
        cols,
        rows,
        wait_threshold_ms: cli.wait_threshold_ms,
    };

    let actions = capture::record(&options)?;

    let script = emit::ScriptDoc {
        output_file: cli.cast.clone(),
        cols,
        rows,
        typing_delay_ms: cli.typing_delay_ms,
        actions,
    };
    emit::write_script(&script, &cli.output)?;

    eprintln!("wrote {} action(s) to {}", script.actions.len(), cli.output.display());
    Ok(())
}

/// Resolve the PTY size, honouring explicit `--cols`/`--rows` and otherwise
/// querying the current terminal (falling back to 80x24).
///
/// A queried dimension of `0` (e.g. when stdout is not a real terminal) is
/// treated as "unknown" and replaced by the default so the PTY never gets a
/// zero size.
fn resolve_size(cols: Option<u16>, rows: Option<u16>) -> (u16, u16) {
    const DEFAULT_COLS: u16 = 80;
    const DEFAULT_ROWS: u16 = 24;
    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((DEFAULT_COLS, DEFAULT_ROWS));
    let resolved_cols = cols.filter(|&c| c > 0).unwrap_or(term_cols);
    let resolved_rows = rows.filter(|&r| r > 0).unwrap_or(term_rows);
    (
        if resolved_cols == 0 { DEFAULT_COLS } else { resolved_cols },
        if resolved_rows == 0 { DEFAULT_ROWS } else { resolved_rows },
    )
}
