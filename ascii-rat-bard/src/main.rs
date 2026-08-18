//! Scripted asciinema recorder.
//!
//! Reads a YAML script of demo inputs, drives a child process inside a PTY,
//! and records the session as an asciicast v2 `.cast` file.

use anyhow::{Context, Result, bail};
use ascii_rat_stage::cast::AsciiCast;
use ascii_rat_stage::script::Script;
use ascii_rat_stage::util;
use clap::{CommandFactory, Parser};
use clap_complete::{Shell, generate};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Generate scripted asciinema recordings from a YAML script.
#[derive(Debug, Parser)]
#[command(
    name = "ascii-rat-bard",
    about = "Generate scripted asciinema recordings",
    version
)]
struct Cli {
    /// The scripted session to record.
    #[arg(required_unless_present = "completions")]
    script_file: Option<PathBuf>,

    /// Print a shell completion script for the given shell and exit.
    #[arg(long = "completions", value_name = "SHELL")]
    completions: Option<Shell>,

    /// Don't run the script (skip recording).
    #[arg(short = 'd', long = "dont-run")]
    dont_run: bool,

    /// Don't print script progress.
    #[arg(short = 'q', long = "quiet")]
    quiet: bool,

    /// Mirror the recorded screen to your terminal live while recording.
    #[arg(short = 'w', long = "watch")]
    watch: bool,

    /// Print markers as a Markdown list.
    #[arg(short = 'm', long = "print-markers")]
    print_markers: bool,

    /// HTML element ID for the video element (used with --print-markers).
    #[arg(long = "data-id")]
    data_id: Option<String>,
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

    let script_file = cli
        .script_file
        .expect("script_file is required unless --completions is given");
    let script_file = script_file
        .canonicalize()
        .with_context(|| format!("invalid script file: {script_file:?}"))?;

    let script = Script::from_yaml(&script_file)?;

    // Resolve the output file relative to the script's directory.
    let output_file = resolve_output_file(&script_file, &script.output_file);

    if !cli.dont_run {
        // If the script has a top-level `sudo:` block, prompt once (hidden)
        // before recording starts. Never stored in the script.
        let sudo_password = if script.sudo_enabled() {
            let pw = rpassword::prompt_password("Sudo password: ").with_context(|| {
                "failed to read the sudo password (a terminal is required for the hidden \
                 prompt; run in an interactive terminal)"
            })?;
            Some(pw)
        } else {
            None
        };

        let cast = script.run(cli.quiet, cli.watch, sudo_password.as_deref())?;
        cast.save(&output_file)
            .with_context(|| format!("failed to save cast to {output_file:?}"))?;
        if !cli.quiet {
            println!("demo saved to {}", output_file.display());
        }
    }

    if cli.print_markers {
        if !output_file.exists() {
            bail!(
                "cannot print markers: cast file {output_file:?} does not exist \
                 (run without --dont-run first)"
            );
        }
        let cast = AsciiCast::load(&output_file)?;
        util::print_marker_md_list(&cast, cli.data_id.as_deref());
    }

    Ok(())
}

/// Resolve `output_file` relative to the directory containing the script.
fn resolve_output_file(script_file: &Path, output_file: &str) -> PathBuf {
    let candidate = Path::new(output_file);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else if let Some(dir) = script_file.parent() {
        dir.join(candidate)
    } else {
        candidate.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_file_resolves_relative_to_script_dir() {
        let script = Path::new("/tmp/demos/demo.yaml");
        let resolved = resolve_output_file(script, "out.cast");
        assert_eq!(resolved, PathBuf::from("/tmp/demos/out.cast"));
    }

    #[test]
    fn output_file_absolute_is_kept() {
        let script = Path::new("/tmp/demos/demo.yaml");
        let resolved = resolve_output_file(script, "/var/tmp/out.cast");
        assert_eq!(resolved, PathBuf::from("/var/tmp/out.cast"));
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn completions_generate_for_bash_zsh_fish() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let mut buf = Vec::new();
            generate(shell, &mut Cli::command(), "ascii-rat-bard", &mut buf);
            let text = String::from_utf8(buf).expect("completions are UTF-8");
            assert!(
                text.contains("ascii-rat-bard"),
                "{shell} completions should mention the binary name"
            );
        }
    }
}
