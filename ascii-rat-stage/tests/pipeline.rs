//! Integration test: run the load -> insert -> filter -> serialize pipeline on
//! `demo.yaml` (without spawning a real PTY) and assert the produced `.cast`
//! lines are valid asciicast v2.

use ascii_rat_stage::cast::{AsciiCast, Event, Header};
use ascii_rat_stage::filters::Filter;
use ascii_rat_stage::script::{Action, Script};

/// Load the real demo script and check it parses into the expected shape.
#[test]
fn demo_yaml_loads() {
    let script = Script::from_yaml("demo.yaml").expect("demo.yaml should load");
    assert_eq!(script.cols, Some(100));
    assert_eq!(script.rows, Some(40));
    // The recording is stopped by an `End` action (not a marker + filter).
    assert!(script.actions.iter().any(|a| matches!(a, Action::End)));
    // No filters are configured anymore; `End` replaces the EndMarkerFilter.
    assert!(script.filters.is_empty());
}

/// Build a synthetic recorded cast, insert the demo's marker/comment events,
/// apply the demo's filters, and assert the serialized output is valid v2.
#[test]
fn pipeline_produces_valid_v2_cast() {
    let script = Script::from_yaml("demo.yaml").expect("demo.yaml should load");
    let cols = script.cols.unwrap();
    let rows = script.rows.unwrap();

    // A synthetic recorded output stream.
    let recorded = vec![
        Event::Output {
            time: 0.0,
            data: "\u{1b}[2J".to_string(),
        },
        Event::Output {
            time: 1.0,
            data: "hello world".to_string(),
        },
        Event::Output {
            time: 5.0,
            data: "after end".to_string(),
        },
    ];
    let header = Header::new(cols, rows);
    let cast = AsciiCast::new(header, recorded);

    // Inserted marker/comment events (must be chronologically sorted).
    let inserted = vec![
        Event::Comment {
            time: 0.5,
            top: script.comments_at_top,
            comment: "Intro".to_string(),
        },
        // The END marker sits before the "after end" output so EndMarkerFilter
        // truncates it.
        Event::Marker {
            time: 2.0,
            label: "END".to_string(),
        },
    ];

    let cast = cast.insert_events(inserted).expect("insert should succeed");

    // This test exercises the filter machinery directly. `demo.yaml` no longer
    // configures any filters (it uses an `End` action instead), and the comment
    // caption feature is unused there too, so we build the EndMarker + Comment
    // filters explicitly here to cover the comment-rendering / truncation
    // pipeline (the filter code lives on for scripts that opt into it).
    let filters = vec![
        Filter::EndMarker {
            end_label: "END".to_string(),
        },
        Filter::Comment,
    ];
    let cast = cast.filter_events(&filters).expect("filters should apply");

    // EndMarkerFilter must have dropped everything at/after the END marker.
    assert!(
        cast.events.iter().all(|e| e.time() < 2.0),
        "events after END should be trimmed"
    );
    // No raw Marker/Comment events should remain after filtering.
    assert!(cast
        .events
        .iter()
        .all(|e| !matches!(e, Event::Marker { .. } | Event::Comment { .. })));

    // The comment must have become an output status-line escape sequence.
    let has_status_line = cast.events.iter().any(|e| match e {
        Event::Output { data, .. } => data.contains("\u{1b}[7m") && data.contains("Intro"),
        _ => false,
    });
    assert!(has_status_line, "comment should render as a status line");

    // Serialize and validate every line as asciicast v2.
    let lines = cast.to_lines().expect("serialization should succeed");
    assert!(lines.len() >= 2, "expected header + at least one event");

    // Header line: JSON object with version 2 and the demo's dimensions.
    let header_val: serde_json::Value =
        serde_json::from_str(&lines[0]).expect("header must be JSON");
    assert!(header_val.is_object());
    assert_eq!(header_val["version"], 2);
    assert_eq!(header_val["width"], cols);
    assert_eq!(header_val["height"], rows);

    // Every event line: 3-element JSON array [time, code, data].
    for line in &lines[1..] {
        let val: serde_json::Value = serde_json::from_str(line).expect("event must be JSON");
        let arr = val.as_array().expect("event must be an array");
        assert_eq!(arr.len(), 3, "event must have 3 elements: {line}");
        assert!(arr[0].is_number(), "time must be a number: {line}");
        assert!(arr[1].is_string(), "code must be a string: {line}");
        assert!(arr[2].is_string(), "data must be a string: {line}");
    }
}
