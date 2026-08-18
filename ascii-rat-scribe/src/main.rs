//! ascii-rat-scribe — interactive recorder.
//!
//! Runs a target command inside a PTY, forwards the operator's keystrokes to
//! it while mirroring the child's output to the real terminal, and translates
//! the captured keystrokes + idle gaps into a `demo.yaml` script that
//! `ascii-rat-bard` can replay.

mod capture;
mod decoder;
mod emit;

use anyhow::Result;
use ascii_rat_stage::util;
use clap::{CommandFactory, Parser};
use clap_complete::{generate, Shell};
use std::process::ExitCode;

/// Record a live terminal session into a `demo.yaml` script.
#[derive(Debug, Parser)]
#[command(
    name = "ascii-rat-scribe",
    about = "Record a live terminal session into a demo.yaml script",
    version
)]
struct Cli {
    /// Print a shell completion script for the given shell and exit.
    #[arg(long = "completions", value_name = "SHELL")]
    completions: Option<Shell>,

    /// Where to write the produced script.
    #[arg(short = 'o', long = "output", default_value = "demo.yaml")]
    output: std::path::PathBuf,

    /// `output_file` field written into the produced script (the `.cast` that
    /// `ascii-rat-bard` will later record to).
    #[arg(long = "cast", default_value = "demo.cast")]
    cast: String,

    /// Idle time (milliseconds) after which a gap becomes a `Wait` action.
    #[arg(long = "wait-threshold-ms", default_value_t = 500)]
    wait_threshold_ms: u64,

    /// Round each recorded `Wait` to the nearest this many milliseconds, for a
    /// tidier script. Set to `0` to keep millisecond-precise waits.
    #[arg(long = "round-wait-ms", default_value_t = 500)]
    round_wait_ms: u64,

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
    /// `--` is treated as the command line. If omitted, your default shell is
    /// recorded so you get a clean terminal you can type into.
    #[arg(trailing_var_arg = true)]
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
    if let Some(shell) = cli.completions {
        let mut cmd = Cli::command();
        let name = cmd.get_name().to_string();
        generate(shell, &mut cmd, name, &mut std::io::stdout());
        return Ok(());
    }

    let command = if cli.command.is_empty() {
        vec![util::default_shell()]
    } else {
        cli.command.clone()
    };

    let (cols, rows) = resolve_size(cli.cols, cli.rows);

    let options = capture::CaptureOptions {
        command,
        cols,
        rows,
        wait_threshold_ms: cli.wait_threshold_ms,
        wait_round_ms: cli.round_wait_ms,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn completions_generate_for_bash_zsh_fish() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let mut buf = Vec::new();
            generate(shell, &mut Cli::command(), "ascii-rat-scribe", &mut buf);
            let text = String::from_utf8(buf).expect("completions are UTF-8");
            assert!(
                text.contains("ascii-rat-scribe"),
                "{shell} completions should mention the binary name"
            );
        }
    }
}
