//! Wrapper types for the asciinema file format (version 2).
//!
//! This is a Rust port of the reference `cast.py`. The on-disk format is a
//! sequence of newline-separated JSON values: a single JSON object header,
//! followed by one JSON array `[time, code, data]` per event.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// The asciicast v2 header (first line of the file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Header {
    pub version: u32,
    pub width: u16,
    pub height: u16,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub timestamp: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub duration: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub idle_time_limit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub theme: Option<Theme>,
}

/// Optional terminal theme in the header.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    pub fg: String,
    pub bg: String,
    pub palette: Vec<String>,
}

impl Header {
    pub fn new(width: u16, height: u16) -> Self {
        Header {
            version: 2,
            width,
            height,
            timestamp: None,
            duration: None,
            idle_time_limit: None,
            command: None,
            title: None,
            env: None,
            theme: None,
        }
    }
}

/// A single event in the recording.
///
/// The `Comment` variant is internal-only: it is not a valid asciinema event
/// and must be converted into an `Output` event by the comment filter before
/// serialization.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Output { time: f64, data: String },
    Input { time: f64, data: String },
    Marker { time: f64, label: String },
    Resize { time: f64, columns: u16, rows: u16 },
    /// Internal-only event; must be filtered before saving.
    Comment { time: f64, top: bool, comment: String },
}

impl Event {
    pub fn time(&self) -> f64 {
        match self {
            Event::Output { time, .. }
            | Event::Input { time, .. }
            | Event::Marker { time, .. }
            | Event::Resize { time, .. }
            | Event::Comment { time, .. } => *time,
        }
    }

    /// Convert an event into its serialized `[time, code, data]` JSON array.
    ///
    /// Returns an error for `Comment` events, which are not valid asciinema
    /// events and must be filtered first.
    pub fn as_data(&self) -> Result<serde_json::Value> {
        let value = match self {
            Event::Output { time, data } => {
                serde_json::json!([time, "o", data])
            }
            Event::Input { time, data } => {
                serde_json::json!([time, "i", data])
            }
            Event::Marker { time, label } => {
                serde_json::json!([time, "m", label])
            }
            Event::Resize {
                time,
                columns,
                rows,
            } => {
                serde_json::json!([time, "r", format!("{columns}x{rows}")])
            }
            Event::Comment { .. } => {
                bail!("Comment events must be filtered before saving")
            }
        };
        Ok(value)
    }
}

/// An asciinema screencast: a header plus an ordered list of events.
#[derive(Debug, Clone)]
pub struct AsciiCast {
    pub header: Header,
    pub events: Vec<Event>,
}

impl AsciiCast {
    pub fn new(header: Header, events: Vec<Event>) -> Self {
        AsciiCast { header, events }
    }

    /// Apply a sequence of filters in order, returning a new cast.
    pub fn filter_events(&self, filters: &[crate::filters::Filter]) -> Result<AsciiCast> {
        let mut events = self.events.clone();
        for filter in filters {
            events = filter.apply(&self.header, events)?;
        }
        Ok(AsciiCast {
            header: self.header.clone(),
            events,
        })
    }

    /// Merge chronologically-sorted `inserted` events into the recorded stream.
    ///
    /// Mirrors `cast.py::insert_events`: the inserted events must be sorted by
    /// time, otherwise an error is returned.
    pub fn insert_events(&self, inserted: Vec<Event>) -> Result<AsciiCast> {
        if inserted.is_empty() {
            return Ok(self.clone());
        }
        if self.events.is_empty() {
            return Ok(AsciiCast {
                header: self.header.clone(),
                events: inserted,
            });
        }

        let times: Vec<f64> = inserted.iter().map(|e| e.time()).collect();
        let mut sorted = times.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if times != sorted {
            bail!("Events must be sorted chronologically");
        }

        let mut new_events: Vec<Event> = Vec::new();
        let mut iter = inserted.into_iter();
        let mut next_event: Option<Event> = iter.next();

        for current_event in self.events.iter().cloned() {
            match &next_event {
                None => {
                    new_events.push(current_event);
                    continue;
                }
                Some(next) if current_event.time() <= next.time() => {
                    new_events.push(current_event);
                    continue;
                }
                _ => {}
            }

            while let Some(next) = &next_event {
                if next.time() < current_event.time() {
                    new_events.push(next.clone());
                    next_event = iter.next();
                } else {
                    break;
                }
            }

            new_events.push(current_event);
        }

        if let Some(next) = next_event.take() {
            new_events.push(next);
        }
        for event in iter {
            new_events.push(event);
        }

        Ok(AsciiCast {
            header: self.header.clone(),
            events: new_events,
        })
    }

    /// Serialize the cast to newline-separated JSON lines.
    pub fn to_lines(&self) -> Result<Vec<String>> {
        let mut lines = Vec::with_capacity(self.events.len() + 1);
        let header_value = serde_json::to_value(&self.header)?;
        lines.push(serde_json::to_string(&header_value)?);
        for event in &self.events {
            let record = event.as_data()?;
            lines.push(serde_json::to_string(&record)?);
        }
        Ok(lines)
    }

    /// Save the cast to `cast_file`, one JSON value per line.
    pub fn save<P: AsRef<Path>>(&self, cast_file: P) -> Result<()> {
        let lines = self.to_lines()?;
        let mut contents = String::new();
        for line in lines {
            contents.push_str(&line);
            contents.push('\n');
        }
        fs::write(cast_file.as_ref(), contents)
            .with_context(|| format!("failed to write cast file {:?}", cast_file.as_ref()))?;
        Ok(())
    }

    /// Load a cast from `cast_file`.
    pub fn load<P: AsRef<Path>>(cast_file: P) -> Result<AsciiCast> {
        let contents = fs::read_to_string(cast_file.as_ref())
            .with_context(|| format!("failed to read cast file {:?}", cast_file.as_ref()))?;
        let mut values = Vec::new();
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(line)
                .with_context(|| format!("invalid JSON line: {line}"))?;
            values.push(value);
        }
        parse_cast(values)
    }
}

/// Parse a sequence of decoded JSON values into an `AsciiCast`.
fn parse_cast(values: Vec<serde_json::Value>) -> Result<AsciiCast> {
    let mut iter = values.into_iter();
    let header_value = iter.next().ok_or_else(|| anyhow!("Missing asciicast header"))?;
    if !header_value.is_object() {
        bail!("Missing asciicast header");
    }
    let header: Header =
        serde_json::from_value(header_value).context("Invalid header data")?;
    if header.version != 2 {
        bail!("Unsupported file format version {}", header.version);
    }

    let mut events = Vec::new();
    for (ix, value) in iter.enumerate() {
        let arr = value
            .as_array()
            .ok_or_else(|| anyhow!("Invalid event on line {}", ix + 1))?;
        if arr.len() != 3 {
            bail!("Invalid event on line {}", ix + 1);
        }
        let time = arr[0]
            .as_f64()
            .ok_or_else(|| anyhow!("Invalid event time on line {}", ix + 1))?;
        let code = arr[1]
            .as_str()
            .ok_or_else(|| anyhow!("Invalid event code on line {}", ix + 1))?;
        let data = arr[2]
            .as_str()
            .ok_or_else(|| anyhow!("Invalid event data on line {}", ix + 1))?
            .to_string();
        let event = match code {
            "o" => Event::Output { time, data },
            "i" => Event::Input { time, data },
            "m" => Event::Marker { time, label: data },
            "r" => {
                let (cols, rows) = parse_resize(&data)
                    .ok_or_else(|| anyhow!("Invalid resize data {data} on line {}", ix + 1))?;
                Event::Resize {
                    time,
                    columns: cols,
                    rows,
                }
            }
            other => bail!("Invalid event code {other} on line {}", ix + 1),
        };
        events.push(event);
    }

    Ok(AsciiCast { header, events })
}

/// Parse a resize payload of the form `"{cols}x{rows}"`.
fn parse_resize(data: &str) -> Option<(u16, u16)> {
    let (cols, rows) = data.split_once('x')?;
    let cols = cols.parse().ok()?;
    let rows = rows.parse().ok()?;
    Some((cols, rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let mut env = BTreeMap::new();
        env.insert("SHELL".to_string(), "/usr/bin/bash".to_string());
        env.insert("TERM".to_string(), "xterm-ghostty".to_string());
        let header = Header {
            version: 2,
            width: 100,
            height: 40,
            timestamp: Some(1783352093),
            duration: None,
            idle_time_limit: None,
            command: None,
            title: None,
            env: Some(env),
            theme: None,
        };
        let cast = AsciiCast::new(header, vec![]);
        let lines = cast.to_lines().unwrap();
        // Only the header line, no events.
        assert_eq!(lines.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(parsed["version"], 2);
        assert_eq!(parsed["width"], 100);
        assert_eq!(parsed["height"], 40);
        assert_eq!(parsed["timestamp"], 1783352093);
        assert_eq!(parsed["env"]["SHELL"], "/usr/bin/bash");
        // Skipped Nones must not appear.
        assert!(parsed.get("duration").is_none());
        assert!(parsed.get("theme").is_none());
    }

    #[test]
    fn event_serialization() {
        let out = Event::Output {
            time: 0.007052,
            data: "hi".to_string(),
        };
        assert_eq!(
            out.as_data().unwrap(),
            serde_json::json!([0.007052, "o", "hi"])
        );

        let marker = Event::Marker {
            time: 1.5,
            label: "END".to_string(),
        };
        assert_eq!(marker.as_data().unwrap(), serde_json::json!([1.5, "m", "END"]));

        let resize = Event::Resize {
            time: 0.0,
            columns: 100,
            rows: 40,
        };
        assert_eq!(
            resize.as_data().unwrap(),
            serde_json::json!([0.0, "r", "100x40"])
        );

        // Comment events cannot be serialized.
        let comment = Event::Comment {
            time: 0.0,
            top: true,
            comment: "hello".to_string(),
        };
        assert!(comment.as_data().is_err());
    }

    #[test]
    fn parse_and_reload_roundtrip() {
        let lines = vec![
            r#"{"version": 2, "width": 100, "height": 40}"#.to_string(),
            r#"[0.1, "o", "hello"]"#.to_string(),
            r#"[0.2, "m", "END"]"#.to_string(),
            r#"[0.3, "r", "80x24"]"#.to_string(),
        ];
        let values: Vec<serde_json::Value> = lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let cast = parse_cast(values).unwrap();
        assert_eq!(cast.header.width, 100);
        assert_eq!(cast.events.len(), 3);
        assert_eq!(
            cast.events[0],
            Event::Output {
                time: 0.1,
                data: "hello".to_string()
            }
        );
        assert_eq!(
            cast.events[2],
            Event::Resize {
                time: 0.3,
                columns: 80,
                rows: 24
            }
        );
    }

    #[test]
    fn insert_events_merges_chronologically() {
        let header = Header::new(80, 24);
        let recorded = vec![
            Event::Output {
                time: 0.0,
                data: "a".into(),
            },
            Event::Output {
                time: 1.0,
                data: "b".into(),
            },
            Event::Output {
                time: 2.0,
                data: "c".into(),
            },
        ];
        let cast = AsciiCast::new(header, recorded);
        let inserted = vec![
            Event::Marker {
                time: 0.5,
                label: "m1".into(),
            },
            Event::Marker {
                time: 1.5,
                label: "m2".into(),
            },
        ];
        let merged = cast.insert_events(inserted).unwrap();
        let times: Vec<f64> = merged.events.iter().map(|e| e.time()).collect();
        assert_eq!(times, vec![0.0, 0.5, 1.0, 1.5, 2.0]);
    }

    #[test]
    fn insert_events_trailing_inserts() {
        let header = Header::new(80, 24);
        let recorded = vec![Event::Output {
            time: 0.0,
            data: "a".into(),
        }];
        let cast = AsciiCast::new(header, recorded);
        let inserted = vec![Event::Marker {
            time: 5.0,
            label: "late".into(),
        }];
        let merged = cast.insert_events(inserted).unwrap();
        let times: Vec<f64> = merged.events.iter().map(|e| e.time()).collect();
        assert_eq!(times, vec![0.0, 5.0]);
    }

    #[test]
    fn insert_events_rejects_unsorted() {
        let header = Header::new(80, 24);
        let cast = AsciiCast::new(
            header,
            vec![Event::Output {
                time: 0.0,
                data: "a".into(),
            }],
        );
        let inserted = vec![
            Event::Marker {
                time: 2.0,
                label: "b".into(),
            },
            Event::Marker {
                time: 1.0,
                label: "a".into(),
            },
        ];
        assert!(cast.insert_events(inserted).is_err());
    }
}
