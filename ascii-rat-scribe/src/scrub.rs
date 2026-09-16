//! Post-recording safety net: make sure a hand-typed password never reaches the
//! emitted script.
//!
//! With `--sudo`, the password is typed by the recorder itself and so never
//! passes through the keystroke decoder. But the prompt matching can miss — a
//! widget whose wording is not in the configured `prompts` will sit there
//! waiting, the operator will type the password by hand, and *those* keystrokes
//! are transcribed like any other input.
//!
//! [`scrub_password`] closes that gap after the fact: it reconstructs the typed
//! character stream from the captured actions, looks for the password in it, and
//! removes the offending text.
//!
//! Reconstruction is necessary rather than checking each action in isolation,
//! because the decoder flushes buffered text whenever it notes an idle gap. A
//! pause in the middle of typing a password therefore splits it across two
//! separate `Text` actions, and neither one contains the whole secret.

use ascii_rat_stage::script::{Action, Key, KeyName};
use ascii_rat_stage::secret::wipe;

/// Outcome of a scrub pass.
#[derive(Debug, PartialEq)]
pub struct ScrubReport {
    /// How many `Text` actions were removed because they carried part of the
    /// password.
    pub removed: usize,
    /// Whether a trailing Enter that submitted the password was also removed.
    pub removed_submit: bool,
}

impl ScrubReport {
    /// Whether anything was found and removed.
    pub fn leaked(&self) -> bool {
        self.removed > 0
    }
}

/// Remove any trace of `password` from `actions`.
///
/// Concatenates the payloads of every [`Action::Text`] in order, searches that
/// stream for `password`, and deletes the `Text` actions overlapping the match.
/// An empty password is ignored.
///
/// The Enter that submitted the password is removed too when it directly follows
/// the match: on replay `ascii-rat-bard` sends its own carriage return after
/// typing the password, so a leftover Enter would submit a second, empty line.
/// The decoder merges consecutive keypresses into one `Key { keys: [..] }`, so
/// this may mean stripping just the leading Enter from a multi-key action rather
/// than dropping the action outright.
pub fn scrub_password(actions: &mut Vec<Action>, password: &str) -> ScrubReport {
    let mut report = ScrubReport {
        removed: 0,
        removed_submit: false,
    };
    if password.is_empty() {
        return report;
    }

    // Reconstruct the typed stream, remembering which action each span came
    // from so a match can be mapped back to actions.
    let mut stream = String::new();
    // (action index, start offset in `stream`, end offset in `stream`)
    let mut spans: Vec<(usize, usize, usize)> = Vec::new();
    for (idx, action) in actions.iter().enumerate() {
        if let Action::Text(text) = action {
            let start = stream.len();
            stream.push_str(text);
            spans.push((idx, start, stream.len()));
        }
    }

    let found = stream.find(password);
    // The reconstruction holds a copy of the password whenever the operator
    // typed it, so destroy it before returning rather than leaving it in freed
    // memory.
    wipe(&mut stream);
    let Some(hit) = found else {
        return report;
    };
    let (hit_start, hit_end) = (hit, hit + password.len());

    // Every text action overlapping the match carries part of the password.
    let doomed: Vec<usize> = spans
        .iter()
        .filter(|(_, start, end)| *start < hit_end && hit_start < *end)
        .map(|(idx, _, _)| *idx)
        .collect();
    let Some(&last_doomed) = doomed.last() else {
        return report;
    };

    // Strip the submitting Enter, if the next action begins with one.
    if let Some(action) = actions.get_mut(last_doomed + 1)
        && let Action::Key { keys } = action
        && keys.first() == Some(&Key::plain(KeyName::Enter))
    {
        keys.remove(0);
        report.removed_submit = true;
        // Removing the only key would leave an empty action, which is not
        // meaningful in a script.
        if keys.is_empty() {
            actions.remove(last_doomed + 1);
        }
    }

    // Drop the text actions themselves, back to front so indices stay valid.
    for idx in doomed.iter().rev() {
        // The removed payload still holds the password; destroy the contents
        // rather than letting the `String` be freed with the plaintext intact.
        if let Action::Text(mut text) = actions.remove(*idx) {
            wipe(&mut text);
        }
        report.removed += 1;
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Action {
        Action::Text(s.to_string())
    }

    fn enter() -> Action {
        Action::Key {
            keys: vec![Key::plain(KeyName::Enter)],
        }
    }

    #[test]
    fn removes_a_password_typed_in_one_burst() {
        let mut actions = vec![text("sudo whoami"), enter(), text("hunter2"), enter()];
        let report = scrub_password(&mut actions, "hunter2");
        assert!(report.leaked());
        assert_eq!(report.removed, 1);
        assert!(report.removed_submit);
        // Only the command and its Enter survive.
        assert_eq!(actions, vec![text("sudo whoami"), enter()]);
    }

    #[test]
    fn removes_a_password_split_across_actions_by_a_pause() {
        // The decoder flushes text on an idle gap, so a pause mid-password
        // yields two Text actions with a Wait between them. Neither contains
        // the whole password.
        let mut actions = vec![
            text("sudo whoami"),
            enter(),
            text("hun"),
            Action::Wait { seconds: 1.0 },
            text("ter2"),
            enter(),
        ];
        let report = scrub_password(&mut actions, "hunter2");
        assert!(report.leaked());
        assert_eq!(report.removed, 2);
        assert!(report.removed_submit);
        assert_eq!(
            actions,
            vec![text("sudo whoami"), enter(), Action::Wait { seconds: 1.0 }]
        );
    }

    #[test]
    fn strips_only_the_leading_enter_of_a_merged_key_action() {
        // The decoder merges consecutive keys, so the submitting Enter can share
        // an action with later keystrokes that must be preserved.
        let mut actions = vec![
            text("hunter2"),
            Action::Key {
                keys: vec![
                    Key::plain(KeyName::Enter),
                    Key::plain(KeyName::Down),
                    Key::plain(KeyName::Enter),
                ],
            },
        ];
        let report = scrub_password(&mut actions, "hunter2");
        assert!(report.removed_submit);
        assert_eq!(
            actions,
            vec![Action::Key {
                keys: vec![Key::plain(KeyName::Down), Key::plain(KeyName::Enter)],
            }]
        );
    }

    #[test]
    fn leaves_actions_untouched_when_the_password_is_absent() {
        let before = vec![text("sudo whoami"), enter(), text("exit"), enter()];
        let mut actions = before.clone();
        let report = scrub_password(&mut actions, "hunter2");
        assert!(!report.leaked());
        assert!(!report.removed_submit);
        assert_eq!(actions, before);
    }

    #[test]
    fn an_empty_password_is_ignored() {
        let before = vec![text("sudo whoami"), enter()];
        let mut actions = before.clone();
        let report = scrub_password(&mut actions, "");
        assert!(!report.leaked());
        assert_eq!(actions, before);
    }

    #[test]
    fn keeps_a_following_non_enter_action() {
        // Nothing submitted the password (no Enter), so only the text goes.
        let mut actions = vec![text("hunter2"), Action::Wait { seconds: 2.0 }];
        let report = scrub_password(&mut actions, "hunter2");
        assert_eq!(report.removed, 1);
        assert!(!report.removed_submit);
        assert_eq!(actions, vec![Action::Wait { seconds: 2.0 }]);
    }

    #[test]
    fn removes_a_password_embedded_in_a_longer_text_action() {
        // No Enter separated them, so the password shares an action with other
        // typing. The whole action goes: keeping part of it risks keeping part
        // of the secret.
        let mut actions = vec![text("echo hunter2 please")];
        let report = scrub_password(&mut actions, "hunter2");
        assert_eq!(report.removed, 1);
        assert!(actions.is_empty());
    }
}
