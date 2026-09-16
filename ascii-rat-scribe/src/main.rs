//! ascii-rat-scribe — interactive recorder.
//!
//! Runs a target command inside a PTY, forwards the operator's keystrokes to
//! it while mirroring the child's output to the real terminal, and translates
//! the captured keystrokes + idle gaps into a `demo.yaml` script that
//! `ascii-rat-bard` can replay.
//!
//! With `--sudo` the recorder also answers a password prompt itself, so a
//! privileged program can be driven during a recording without the password
//! being transcribed into the produced script.

mod capture;
mod decoder;
mod emit;
mod scrub;

use anyhow::{Context, Result};
use ascii_rat_stage::script::SudoConfig;
use ascii_rat_stage::secret::SecretString;
use ascii_rat_stage::util;
use clap::{CommandFactory, Parser};
use clap_complete::{Shell, generate};
use std::process::ExitCode;
use std::time::Duration;

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

    /// Answer sudo password prompts during the recording. Asks once for the
    /// password (hidden) before recording starts, then types it whenever a
    /// prompt appears, so it is never transcribed into the script. The produced
    /// script gets `sudo: true` so `ascii-rat-bard` does the same on replay.
    #[arg(long = "sudo")]
    sudo: bool,

    /// Prompt substring that triggers typing the password (repeatable,
    /// case-insensitive). Defaults to the built-in sudo prompts. Implies
    /// `--sudo`.
    #[arg(long = "sudo-prompt", value_name = "SUBSTRING")]
    sudo_prompt: Vec<String>,

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

    // Resolve sudo handling before touching the terminal: the hidden password
    // prompt needs cooked mode, and `capture::record` switches to raw mode.
    let sudo_config = sudo_config(&cli);
    let sudo = match &sudo_config {
        Some(cfg) => Some(capture::SudoAuth {
            // Wrapped immediately: `prompt_password` hands back a plain String
            // that nothing would otherwise wipe.
            password: SecretString::new(
                rpassword::prompt_password("Sudo password: ").with_context(|| {
                    "failed to read the sudo password (a terminal is required for the hidden \
                     prompt; run in an interactive terminal)"
                })?,
            ),
            prompts: cfg.prompts.clone(),
            char_delay: Duration::from_millis(cli.typing_delay_ms),
        }),
        None => None,
    };

    let options = capture::CaptureOptions {
        command,
        cols,
        rows,
        wait_threshold_ms: cli.wait_threshold_ms,
        wait_round_ms: cli.round_wait_ms,
        sudo,
    };

    let capture = capture::record(&options)?;
    let mut actions = capture.actions;

    // Raw mode is restored by now, so warnings can be printed normally. The
    // password is borrowed from `options` rather than copied: a second buffer
    // would be a second thing to wipe.
    if let Some(auth) = options.sudo.as_ref() {
        report_sudo(capture.sudo_types);
        let report = scrub::scrub_password(&mut actions, auth.password.expose());
        if report.leaked() {
            eprintln!(
                "warning: the password was found in the captured keystrokes and has been \
                 removed from the script ({} action(s)). This happens when a prompt is not \
                 matched and you type the password by hand; consider --sudo-prompt.",
                report.removed
            );
        }
    }

    let script = emit::ScriptDoc {
        output_file: cli.cast.clone(),
        cols,
        rows,
        typing_delay_ms: cli.typing_delay_ms,
        sudo: sudo_config,
        actions,
    };
    emit::write_script(&script, &cli.output)?;

    eprintln!(
        "wrote {} action(s) to {}",
        script.actions.len(),
        cli.output.display()
    );
    Ok(())
}

/// Build the sudo configuration from the CLI, or `None` when sudo handling was
/// not requested. `--sudo-prompt` implies `--sudo`, so giving prompts alone is
/// enough to enable it.
fn sudo_config(cli: &Cli) -> Option<SudoConfig> {
    if !cli.sudo && cli.sudo_prompt.is_empty() {
        return None;
    }
    Some(if cli.sudo_prompt.is_empty() {
        SudoConfig::default()
    } else {
        SudoConfig {
            prompts: cli.sudo_prompt.clone(),
        }
    })
}

/// Warn about the two ways `--sudo` can disagree with what replay will do.
fn report_sudo(sudo_types: usize) {
    match sudo_types {
        // The flag was given but the prompt never showed up. Most likely the
        // prompt wording differs from the configured needles.
        0 => eprintln!(
            "warning: --sudo was given but no password prompt matched, so the password was \
             never used. If the program did prompt, set --sudo-prompt to a substring of its \
             prompt."
        ),
        1 => {}
        // `ascii-rat-bard` types the password for the first prompt only: its
        // one-shot latch is never reset, so a second prompt goes unanswered.
        n => eprintln!(
            "warning: answered {n} password prompts, but ascii-rat-bard only answers the \
             first one on replay. The replay will stall at the second prompt unless you \
             restructure the script or grant passwordless sudo for this command."
        ),
    }
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
    let (term_cols, term_rows) =
        crossterm::terminal::size().unwrap_or((DEFAULT_COLS, DEFAULT_ROWS));
    let resolved_cols = cols.filter(|&c| c > 0).unwrap_or(term_cols);
    let resolved_rows = rows.filter(|&r| r > 0).unwrap_or(term_rows);
    (
        if resolved_cols == 0 {
            DEFAULT_COLS
        } else {
            resolved_cols
        },
        if resolved_rows == 0 {
            DEFAULT_ROWS
        } else {
            resolved_rows
        },
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
