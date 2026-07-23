//! Utilities: Markdown marker listing (port of `util.py`).

use crate::cast::{AsciiCast, Event};

/// Build the Markdown seek-link lines for every marker in the cast.
///
/// Mirrors `util.py::marker_md_list`.
pub fn marker_md_list(cast: &AsciiCast, data_video_id: Option<&str>) -> Vec<String> {
    let data_video_attr = match data_video_id {
        Some(id) => format!(" data-video=\"{id}\""),
        None => String::new(),
    };

    let mut lines = Vec::new();
    for event in &cast.events {
        if let Event::Marker { time, label } = event {
            let link = format!(
                "<a{data_video_attr} data-seek-to=\"{time}\" href=\"javascript:;\">{label}</a>"
            );
            lines.push(link);
        }
    }
    lines
}

/// Print the markers as a numbered Markdown list (port of `print_marker_md_list`).
pub fn print_marker_md_list(cast: &AsciiCast, data_video_id: Option<&str>) {
    for (ix, line) in marker_md_list(cast, data_video_id).iter().enumerate() {
        println!("{}. {}", ix + 1, line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cast::Header;

    #[test]
    fn markers_render_as_seek_links() {
        let events = vec![
            Event::Output {
                time: 0.0,
                data: "x".into(),
            },
            Event::Marker {
                time: 1.5,
                label: "Intro".into(),
            },
            Event::Marker {
                time: 3.25,
                label: "END".into(),
            },
        ];
        let cast = AsciiCast::new(Header::new(80, 24), events);
        let lines = marker_md_list(&cast, None);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            "<a data-seek-to=\"1.5\" href=\"javascript:;\">Intro</a>"
        );
        assert_eq!(
            lines[1],
            "<a data-seek-to=\"3.25\" href=\"javascript:;\">END</a>"
        );
    }

    #[test]
    fn markers_include_data_video_id() {
        let events = vec![Event::Marker {
            time: 2.0,
            label: "M".into(),
        }];
        let cast = AsciiCast::new(Header::new(80, 24), events);
        let lines = marker_md_list(&cast, Some("vid1"));
        assert_eq!(
            lines[0],
            "<a data-video=\"vid1\" data-seek-to=\"2\" href=\"javascript:;\">M</a>"
        );
    }
}
