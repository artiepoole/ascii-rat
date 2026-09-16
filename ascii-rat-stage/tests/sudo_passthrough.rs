//! Integration test for the sudo passthrough primitives against a real child in
//! a real PTY.
//!
//! Drives the same loop `ascii-rat-scribe` runs — drain output, feed it to a
//! [`PromptMatcher`], type the password on a match — against a shell that asks
//! for a password with echo off, exactly as `sudo` does. Verifies the child
//! actually receives the password, which unit tests on the matcher alone cannot
//! show.
//!
//! Unix-only: the fake prompt uses `sh` and `stty`.

#![cfg(unix)]

use ascii_rat_stage::pty::{PtyCommandBuilder, PtySession};
use ascii_rat_stage::script::SudoConfig;
use ascii_rat_stage::sudo::{PromptMatcher, type_password_into};
use std::time::{Duration, Instant};

/// Run `script` in a PTY, answering any matched prompt with `password`.
///
/// Returns the child's full output and how many prompts were answered.
fn run_with_sudo(script: &str, password: &str, prompts: &[String]) -> (String, usize) {
    let mut cmd = PtyCommandBuilder::new("sh");
    cmd.arg("-c");
    cmd.arg(script);
    let mut session = PtySession::spawn(cmd, 80, 24).expect("spawn child in PTY");

    let mut matcher = PromptMatcher::new(prompts);
    let mut output = Vec::new();
    let mut answered = 0usize;
    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        let mut matched = false;
        for chunk in session.drain_output() {
            if matcher.push(&chunk.bytes) {
                matched = true;
            }
            output.extend_from_slice(&chunk.bytes);
        }
        if matched {
            type_password_into(&mut session, password, Duration::from_millis(1))
                .expect("type password");
            answered += 1;
            matcher.reset();
        }
        if session.try_wait().expect("poll child") {
            break;
        }
        assert!(Instant::now() < deadline, "child did not exit in time");
        std::thread::sleep(Duration::from_millis(5));
    }

    for chunk in session.close().unwrap_or_default() {
        output.extend_from_slice(&chunk.bytes);
    }
    (String::from_utf8_lossy(&output).into_owned(), answered)
}

/// A password prompt with echo disabled, the way `sudo` presents one, is
/// detected and answered, and the child receives the password intact.
#[test]
fn answers_a_hidden_password_prompt() {
    let prompts = SudoConfig::default().prompts;
    let (output, answered) = run_with_sudo(
        // `stty -echo` keeps the typed password off the screen, like sudo.
        "printf '[sudo] password for tester: '; stty -echo; read -r pw; stty echo; \
         printf '\\ngot:[%s]\\n' \"$pw\"",
        "hunter2",
        &prompts,
    );
    assert_eq!(answered, 1, "the prompt should be answered once:\n{output}");
    assert!(
        output.contains("got:[hunter2]"),
        "child should receive the password:\n{output}"
    );
    // Echo was off, so the password must not appear in the recorded output other
    // than in the line the child deliberately printed back.
    assert_eq!(
        output.matches("hunter2").count(),
        1,
        "password should only appear where the child echoed it:\n{output}"
    );
}

/// A prompt whose wording matches none of the needles is left alone, so the
/// password is never sent to a program that did not ask for it.
#[test]
fn does_not_answer_an_unmatched_prompt() {
    let prompts = vec!["this will never appear".to_string()];
    let (output, answered) =
        run_with_sudo("printf 'Password: '; echo; echo done", "hunter2", &prompts);
    assert_eq!(answered, 0, "nothing should be typed:\n{output}");
    assert!(!output.contains("hunter2"), "password leaked:\n{output}");
}

/// The matcher re-arms after each answer, so a program that asks twice gets two
/// answers. This is what scribe warns about: bard only answers the first.
#[test]
fn answers_a_second_prompt_after_rearming() {
    let prompts = SudoConfig::default().prompts;
    let (output, answered) = run_with_sudo(
        "stty -echo; printf 'Password: '; read -r a; printf '\\nPassword: '; read -r b; \
         stty echo; printf '\\ngot:[%s][%s]\\n' \"$a\" \"$b\"",
        "hunter2",
        &prompts,
    );
    assert_eq!(answered, 2, "both prompts should be answered:\n{output}");
    assert!(
        output.contains("got:[hunter2][hunter2]"),
        "child should receive both passwords:\n{output}"
    );
}

/// A password containing characters a bulk write would mangle still arrives
/// intact, since it is typed one character at a time.
#[test]
fn types_a_password_with_punctuation_intact() {
    let prompts = SudoConfig::default().prompts;
    let password = "p@ss w0rd!#$%";
    let (output, answered) = run_with_sudo(
        "printf 'Password: '; stty -echo; read -r pw; stty echo; printf '\\ngot:[%s]\\n' \"$pw\"",
        password,
        &prompts,
    );
    assert_eq!(answered, 1, "the prompt should be answered:\n{output}");
    assert!(
        output.contains(&format!("got:[{password}]")),
        "password should arrive intact:\n{output}"
    );
}
