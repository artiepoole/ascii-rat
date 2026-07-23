//! Post-processing filter passes over recorded cast events.
//!
//! This is a Rust port of the `Filter` hierarchy in the reference `script.py`.
//! Filters are applied in order after the recorded events have been merged
//! with inserted marker/comment events.

use crate::cast::{Event, Header};
use anyhow::{Context, Result};
use regex::Regex;
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::fmt;

/// SGR reverse-video on.
const REV_START: &str = "\u{1b}[7m";
/// SGR reset.
const REV_END: &str = "\u{1b}[m";

/// A post-processing pass over the recorded events.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    RegexReplacement { regex: String, replacement: String },
    StartMarker { start_label: String },
    EndMarker { end_label: String },
    Comment,
}

impl Filter {
    /// Apply this filter to the events, returning the transformed list.
    ///
    /// Mirrors the `apply` methods of the reference `Filter` subclasses.
    pub fn apply(&self, header: &Header, events: Vec<Event>) -> Result<Vec<Event>> {
        match self {
            Filter::RegexReplacement { regex, replacement } => {
                let re = Regex::new(regex)
                    .with_context(|| format!("invalid regex in filter: {regex}"))?;
                let new_events = events
                    .into_iter()
                    .map(|event| match event {
                        Event::Output { time, data } => Event::Output {
                            time,
                            data: re.replace_all(&data, replacement.as_str()).into_owned(),
                        },
                        other => other,
                    })
                    .collect();
                Ok(new_events)
            }
            Filter::StartMarker { start_label } => {
                let mut new_events = Vec::new();
                let mut started = false;
                for event in events {
                    if started {
                        new_events.push(event);
                    } else if let Event::Marker { label, .. } = &event {
                        if label == start_label {
                            started = true;
                        }
                    }
                }
                Ok(new_events)
            }
            Filter::EndMarker { end_label } => {
                let mut new_events = Vec::new();
                for event in events {
                    if let Event::Marker { label, .. } = &event {
                        if label == end_label {
                            break;
                        }
                    }
                    new_events.push(event);
                }
                Ok(new_events)
            }
            Filter::Comment => {
                let cols = header.width as usize;
                let rows = header.height;
                let mut new_events = Vec::with_capacity(events.len());
                // The overlay draw sequence for the currently-active comment.
                // A full-screen TUI on the alternate screen erases the status
                // line, so we re-emit this overlay after each subsequent output
                // event to keep the caption visible on top.
                let mut active_overlay: Option<String> = None;
                for event in events {
                    match event {
                        Event::Comment { time, top, comment } => {
                            // A new comment supersedes the previous one.
                            let overlay = comment_overlay_data(top, &comment, cols, rows);
                            active_overlay = Some(overlay.clone());
                            new_events.push(Event::Output {
                                time,
                                data: overlay,
                            });
                        }
                        Event::Output { time, data } => {
                            // Repaint the active overlay after the output so the
                            // TUI's redraw does not permanently hide it.
                            match &active_overlay {
                                Some(overlay) => new_events.push(Event::Output {
                                    time,
                                    data: format!("{data}{overlay}"),
                                }),
                                None => new_events.push(Event::Output { time, data }),
                            }
                        }
                        other => new_events.push(other),
                    }
                }
                Ok(new_events)
            }
        }
    }
}

/// Build the overlay draw sequence for a comment: save the cursor, move to the
/// status line, draw the reversed-video centered text, and restore the cursor
/// (mirrors `CommentFilter.modify_event`).
fn comment_overlay_data(top: bool, comment: &str, num_cols: usize, num_rows: u16) -> String {
    let line_num = if top { 1 } else { num_rows };
    let centered = center(comment, num_cols);
    format!("\u{1b}[s\u{1b}[{line_num};1H{REV_START}{centered}{REV_END}\u{1b}[u")
}

/// Center `text` in a field of `width` columns (Python's `f'{s:^{width}}'`).
fn center(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        return text.to_string();
    }
    let total = width - len;
    let left = total / 2;
    let right = total - left;
    format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
}

// Custom deserialization: a filter is a mapping keyed by `filter_id`, mirroring
// `parse_filter` in the reference implementation.
impl<'de> Deserialize<'de> for Filter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FilterVisitor;

        impl<'de> Visitor<'de> for FilterVisitor {
            type Value = Filter;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a filter mapping with a `filter_id` field")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Filter, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut filter_id: Option<String> = None;
                let mut regex: Option<String> = None;
                let mut replacement: Option<String> = None;
                let mut start_label: Option<String> = None;
                let mut end_label: Option<String> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "filter_id" => filter_id = Some(map.next_value()?),
                        "regex" => regex = Some(map.next_value()?),
                        "replacement" => replacement = Some(map.next_value()?),
                        "start_label" => start_label = Some(map.next_value()?),
                        "end_label" => end_label = Some(map.next_value()?),
                        other => {
                            return Err(de::Error::unknown_field(
                                other,
                                &[
                                    "filter_id",
                                    "regex",
                                    "replacement",
                                    "start_label",
                                    "end_label",
                                ],
                            ))
                        }
                    }
                }

                let filter_id =
                    filter_id.ok_or_else(|| de::Error::missing_field("filter_id"))?;
                match filter_id.as_str() {
                    "RegexReplacementFilter" => Ok(Filter::RegexReplacement {
                        regex: regex.ok_or_else(|| de::Error::missing_field("regex"))?,
                        replacement: replacement
                            .ok_or_else(|| de::Error::missing_field("replacement"))?,
                    }),
                    "StartMarkerFilter" => Ok(Filter::StartMarker {
                        start_label: start_label
                            .ok_or_else(|| de::Error::missing_field("start_label"))?,
                    }),
                    "EndMarkerFilter" => Ok(Filter::EndMarker {
                        end_label: end_label
                            .ok_or_else(|| de::Error::missing_field("end_label"))?,
                    }),
                    "CommentFilter" => Ok(Filter::Comment),
                    other => Err(de::Error::custom(format!("Invalid filter {other}"))),
                }
            }
        }

        deserializer.deserialize_map(FilterVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_end_marker_filter() {
        let yaml = "filter_id: EndMarkerFilter\nend_label: END\n";
        let f: Filter = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            f,
            Filter::EndMarker {
                end_label: "END".to_string()
            }
        );
    }

    #[test]
    fn deserialize_regex_filter() {
        let yaml = "filter_id: RegexReplacementFilter\nregex: 'a+'\nreplacement: 'b'\n";
        let f: Filter = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            f,
            Filter::RegexReplacement {
                regex: "a+".to_string(),
                replacement: "b".to_string()
            }
        );
    }

    #[test]
    fn deserialize_comment_filter() {
        let yaml = "filter_id: CommentFilter\n";
        let f: Filter = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(f, Filter::Comment);
    }

    #[test]
    fn deserialize_invalid_filter() {
        let yaml = "filter_id: Nope\n";
        let res: Result<Filter, _> = serde_yaml::from_str(yaml);
        assert!(res.is_err());
    }

    fn out(time: f64, data: &str) -> Event {
        Event::Output {
            time,
            data: data.to_string(),
        }
    }

    fn marker(time: f64, label: &str) -> Event {
        Event::Marker {
            time,
            label: label.to_string(),
        }
    }

    #[test]
    fn end_marker_truncates_at_label() {
        let header = Header::new(80, 24);
        let events = vec![
            out(0.0, "a"),
            out(1.0, "b"),
            marker(1.5, "END"),
            out(2.0, "c"),
        ];
        let filtered = Filter::EndMarker {
            end_label: "END".to_string(),
        }
        .apply(&header, events)
        .unwrap();
        assert_eq!(filtered, vec![out(0.0, "a"), out(1.0, "b")]);
    }

    #[test]
    fn start_marker_drops_before_label() {
        let header = Header::new(80, 24);
        let events = vec![
            out(0.0, "a"),
            marker(0.5, "START"),
            out(1.0, "b"),
            out(2.0, "c"),
        ];
        let filtered = Filter::StartMarker {
            start_label: "START".to_string(),
        }
        .apply(&header, events)
        .unwrap();
        assert_eq!(filtered, vec![out(1.0, "b"), out(2.0, "c")]);
    }

    #[test]
    fn regex_replacement_touches_output_only() {
        let header = Header::new(80, 24);
        let events = vec![out(0.0, "secret123"), marker(1.0, "secret123")];
        let filtered = Filter::RegexReplacement {
            regex: r"\d+".to_string(),
            replacement: "***".to_string(),
        }
        .apply(&header, events)
        .unwrap();
        assert_eq!(
            filtered,
            vec![out(0.0, "secret***"), marker(1.0, "secret123")]
        );
    }

    #[test]
    fn comment_filter_renders_top_status_line() {
        let header = Header::new(20, 10);
        let events = vec![Event::Comment {
            time: 0.5,
            top: true,
            comment: "Hi".to_string(),
        }];
        let filtered = Filter::Comment.apply(&header, events).unwrap();
        assert_eq!(filtered.len(), 1);
        match &filtered[0] {
            Event::Output { time, data } => {
                assert_eq!(*time, 0.5);
                // Save cursor, move to line 1 col 1.
                assert!(data.starts_with("\u{1b}[s\u{1b}[1;1H"));
                // Reversed video wrapping, centered in 20 cols.
                assert!(data.contains("\u{1b}[7m"));
                assert!(data.contains("\u{1b}[m"));
                // Restore cursor at the end.
                assert!(data.ends_with("\u{1b}[u"));
                // "Hi" centered in 20 columns: 9 left, 9 right.
                assert!(data.contains(&format!("{}Hi{}", " ".repeat(9), " ".repeat(9))));
            }
            other => panic!("expected Output event, got {other:?}"),
        }
    }

    #[test]
    fn comment_filter_bottom_uses_rows_line() {
        let header = Header::new(20, 10);
        let events = vec![Event::Comment {
            time: 0.0,
            top: false,
            comment: "x".to_string(),
        }];
        let filtered = Filter::Comment.apply(&header, events).unwrap();
        match &filtered[0] {
            Event::Output { data, .. } => {
                // Bottom comment moves to the last row (height = 10).
                assert!(data.starts_with("\u{1b}[s\u{1b}[10;1H"));
            }
            other => panic!("expected Output event, got {other:?}"),
        }
    }

    fn comment(time: f64, top: bool, text: &str) -> Event {
        Event::Comment {
            time,
            top,
            comment: text.to_string(),
        }
    }

    fn output_data(event: &Event) -> &str {
        match event {
            Event::Output { data, .. } => data,
            other => panic!("expected Output event, got {other:?}"),
        }
    }

    #[test]
    fn output_after_active_comment_reemits_overlay() {
        let header = Header::new(20, 10);
        let overlay = comment_overlay_data(true, "Hi", 20, 10);
        let events = vec![comment(0.0, true, "Hi"), out(1.0, "TUI-frame")];
        let filtered = Filter::Comment.apply(&header, events).unwrap();
        assert_eq!(filtered.len(), 2);
        // The comment itself becomes the overlay output.
        assert_eq!(output_data(&filtered[0]), overlay);
        // The following output event has the overlay repainted after it.
        assert_eq!(output_data(&filtered[1]), format!("TUI-frame{overlay}"));
    }

    #[test]
    fn later_comment_supersedes_previous_overlay() {
        let header = Header::new(20, 10);
        let first = comment_overlay_data(true, "One", 20, 10);
        let second = comment_overlay_data(true, "Two", 20, 10);
        let events = vec![
            comment(0.0, true, "One"),
            out(1.0, "frameA"),
            comment(2.0, true, "Two"),
            out(3.0, "frameB"),
        ];
        let filtered = Filter::Comment.apply(&header, events).unwrap();
        assert_eq!(filtered.len(), 4);
        // First comment overlay, then frameA repaints the first overlay.
        assert_eq!(output_data(&filtered[1]), format!("frameA{first}"));
        // Second comment overlay, then frameB repaints the SECOND overlay only.
        assert_eq!(output_data(&filtered[2]), second);
        assert_eq!(output_data(&filtered[3]), format!("frameB{second}"));
        // frameB must NOT contain the first (superseded) overlay text "One".
        assert!(!output_data(&filtered[3]).contains("One"));
    }

    #[test]
    fn output_before_any_comment_is_untouched() {
        let header = Header::new(20, 10);
        let events = vec![out(0.0, "boot"), comment(1.0, true, "Hi")];
        let filtered = Filter::Comment.apply(&header, events).unwrap();
        // The pre-comment output is unchanged (no overlay appended).
        assert_eq!(output_data(&filtered[0]), "boot");
    }

    #[test]
    fn center_matches_python_semantics() {
        // Python f'{s:^{w}}' puts the extra space on the right for odd padding.
        assert_eq!(center("ab", 5), " ab  ");
        assert_eq!(center("abc", 3), "abc");
        assert_eq!(center("abcd", 2), "abcd");
    }
}
