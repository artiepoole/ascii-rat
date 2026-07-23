// Throwaway end-to-end verification for the typed-sudo flow.
//
// Records a real PTY session with a top-level `sudo:` block against a
// `sudo`-style command that prints a password prompt at runtime, then reads a
// line and continues to a running program. The password is typed
// character-by-character and recorded (no redaction). Saves the produced cast
// to disk so it can be validated as asciicast v2 and inspected.
use ascii_rat_stage::script::{Action, KeyName, Script, SudoConfig};

fn main() -> anyhow::Result<()> {
    let secret = "verify-secret-typed";
    // Prompt text assembled at runtime so the typed command echo never contains
    // the contiguous needle substrings (mirrors the real `sudo snap-rat-vibes`
    // where the typed command has no prompt text). `read -r pw; echo GOT=$pw`
    // proves the full password was typed char-by-char and terminated by Enter.
    let stub = "printf '%s%s' '[sud' 'o] pass'; \
                printf 'word for verify: '; read -r pw; \
                echo GOT=$pw; sleep 0.6; echo RUNNING-NOW"
        .to_string();

    let script = Script {
        output_file: "verify-sudo.cast".to_string(),
        start_delay: (0.1, 0.1),
        end_delay: (0.4, 0.4),
        typing_delay: (0.0, 0.02),
        pre_nl_delay: (0.1, 0.11),
        post_nl_delay: (0.8, 0.81),
        key_delay: (0.0, 0.02),
        with_comments: false,
        comments_at_top: false,
        // Typed strings no longer submit on their own; press Enter explicitly to
        // run each command.
        actions: vec![
            Action::Text(stub),
            Action::Key {
                keys: vec![KeyName::Enter.into()],
            },
            Action::Text("exit".to_string()),
            Action::Key {
                keys: vec![KeyName::Enter.into()],
            },
        ],
        filters: vec![],
        cols: Some(100),
        rows: Some(40),
        sudo: Some(SudoConfig::default()),
    };

    let cast = script.run(true, false, Some(secret))?;
    cast.save("verify-sudo.cast")?;
    println!("saved verify-sudo.cast with {} events", cast.events.len());
    Ok(())
}
