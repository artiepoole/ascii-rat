//! Assemble captured actions into a `demo.yaml` script and write it out.
//!
//! The produced document uses exactly the field names `ascii-rat-stage`'s
//! `Script` deserializer accepts, and the action list relies on
//! `Action`'s `Serialize` impl (added in `ascii-rat-stage::script`). An
//! `END_REC` action is always appended so the recording has a definite end,
//! matching how hand-authored scripts terminate.

use anyhow::{Context, Result};
use ascii_rat_stage::script::Action;
use serde::Serialize;
use std::path::Path;

/// The in-memory form of the script to emit.
pub struct ScriptDoc {
    /// The `.cast` path `ascii-rat-bard` will later record to.
    pub output_file: String,
    pub cols: u16,
    pub rows: u16,
    /// Per-character typing delay written into the header (milliseconds).
    pub typing_delay_ms: u64,
    /// The captured actions (without the trailing `END_REC`, which is added
    /// during emission).
    pub actions: Vec<Action>,
}

/// Default header delay values (milliseconds) written into the emitted script.
///
/// `ascii-rat-stage`'s loader requires `start_delay`, `end_delay`,
/// `typing_delay`, `pre_nl_delay`, and `post_nl_delay` (only `key_delay`
/// defaults), so the emitter always writes them. The values match the feel of
/// the hand-authored `demo.yaml`.
const DEFAULT_START_DELAY_MS: u64 = 500;
const DEFAULT_END_DELAY_MS: u64 = 500;
const DEFAULT_PRE_NL_DELAY_MS: u64 = 200;
const DEFAULT_POST_NL_DELAY_MS: u64 = 500;

/// Header comment line prepended to every emitted script, so a produced file is
/// self-identifying as scribe output. A leading `#` line is a YAML comment, so
/// it is ignored when the file is loaded back through `Script::from_yaml`.
const HEADER_COMMENT: &str = "# recorded by ascii-rat-scribe";

/// The serializable shape written to YAML.
///
/// Field names and types mirror `ascii-rat-stage`'s `ScriptRaw` so the produced
/// file parses back via `Script::from_yaml`.
#[derive(Serialize)]
struct ScriptYaml {
    output_file: String,
    cols: u16,
    rows: u16,
    start_delay_ms: u64,
    end_delay_ms: u64,
    typing_delay_ms: u64,
    pre_nl_delay_ms: u64,
    post_nl_delay_ms: u64,
    actions: Vec<Action>,
}

impl ScriptDoc {
    /// Build the serializable document, appending the terminating `END_REC`
    /// action if the captured stream did not already end with one.
    fn to_yaml_doc(&self) -> ScriptYaml {
        let mut actions = self.actions.clone();
        if !matches!(actions.last(), Some(Action::End)) {
            actions.push(Action::End);
        }
        ScriptYaml {
            output_file: self.output_file.clone(),
            cols: self.cols,
            rows: self.rows,
            start_delay_ms: DEFAULT_START_DELAY_MS,
            end_delay_ms: DEFAULT_END_DELAY_MS,
            typing_delay_ms: self.typing_delay_ms,
            pre_nl_delay_ms: DEFAULT_PRE_NL_DELAY_MS,
            post_nl_delay_ms: DEFAULT_POST_NL_DELAY_MS,
            actions,
        }
    }

    /// Serialize the document to a YAML string, prefixed with the
    /// `# recorded by ascii-rat-scribe` header comment.
    pub fn to_yaml_string(&self) -> Result<String> {
        let body = serde_yaml::to_string(&self.to_yaml_doc())
            .context("failed to serialize script to YAML")?;
        Ok(format!("{HEADER_COMMENT}\n{body}"))
    }
}

/// Write the script document to `path` as YAML.
pub fn write_script(doc: &ScriptDoc, path: &Path) -> Result<()> {
    let yaml = doc.to_yaml_string()?;
    std::fs::write(path, yaml).with_context(|| format!("failed to write script to {path:?}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ascii_rat_stage::script::{KeyName, Script};

    fn sample_doc(actions: Vec<Action>) -> ScriptDoc {
        ScriptDoc {
            output_file: "demo.cast".to_string(),
            cols: 100,
            rows: 40,
            typing_delay_ms: 75,
            actions,
        }
    }

    #[test]
    fn emitted_yaml_parses_via_script_from_yaml() {
        let doc = sample_doc(vec![
            Action::Text("ls".to_string()),
            Action::Key {
                keys: vec![KeyName::Enter.into()],
            },
            Action::Wait { seconds: 1.0 },
            Action::Key {
                keys: vec![KeyName::Down.into()],
            },
        ]);
        let yaml = doc.to_yaml_string().expect("serialize");

        // Write to a temp file and load through the real loader.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("scribe-emit-{}.yaml", std::process::id()));
        std::fs::write(&path, &yaml).unwrap();
        let script = Script::from_yaml(&path).expect("emitted YAML should parse");
        std::fs::remove_file(&path).ok();

        assert_eq!(script.output_file, "demo.cast");
        assert_eq!(script.cols, Some(100));
        assert_eq!(script.rows, Some(40));
        assert_eq!(script.typing_delay, (0.075, 0.075));

        // First actions preserved in order.
        assert_eq!(script.actions[0], Action::Text("ls".to_string()));
        assert_eq!(
            script.actions[1],
            Action::Key {
                keys: vec![KeyName::Enter.into()]
            }
        );
        assert_eq!(script.actions[2], Action::Wait { seconds: 1.0 });
        assert_eq!(
            script.actions[3],
            Action::Key {
                keys: vec![KeyName::Down.into()]
            }
        );
        // Terminated by END_REC.
        assert_eq!(script.actions.last().unwrap(), &Action::End);
    }

    #[test]
    fn emitted_yaml_carries_recorded_by_header_and_still_parses() {
        // Every scribe output file must start with the identifying comment, and
        // that leading YAML comment must not stop the file from loading back.
        let doc = sample_doc(vec![Action::Text("ls".to_string())]);
        let yaml = doc.to_yaml_string().expect("serialize");
        assert!(
            yaml.starts_with("# recorded by ascii-rat-scribe\n"),
            "emitted YAML should start with the recorded-by header:\n{yaml}"
        );

        let dir = std::env::temp_dir();
        let path = dir.join(format!("scribe-header-{}.yaml", std::process::id()));
        std::fs::write(&path, &yaml).unwrap();
        let script = Script::from_yaml(&path).expect("header-prefixed YAML should parse");
        std::fs::remove_file(&path).ok();
        assert_eq!(script.output_file, "demo.cast");
    }

    #[test]
    fn end_rec_is_not_duplicated() {
        let doc = sample_doc(vec![Action::Text("x".to_string()), Action::End]);
        let yaml = doc.to_yaml_string().unwrap();
        let ends = yaml.matches("END_REC").count();
        assert_eq!(ends, 1, "END_REC should appear exactly once:\n{yaml}");
    }
}
