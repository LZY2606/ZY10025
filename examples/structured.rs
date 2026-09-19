//! Consume the structured rendering events and convert them to JSON
//!
//! [`Renderer::render_events`] exposes the same layout that
//! [`Renderer::render`] prints, but as structured data. This example turns
//! the event stream into a JSON document by hand; the library itself does
//! not depend on any JSON crate.

use annotate_snippets::renderer::{EventKind, RenderEvent, Renderer, SourceRef};
use annotate_snippets::{AnnotationKind, Group, Level, Snippet};

fn main() {
    let report = &[
        Group::with_title(Level::ERROR.primary_title("mismatched types").id("E0308")).element(
            Snippet::source("let x: u32 = \"hi\";")
                .path("src/main.rs")
                .annotation(AnnotationKind::Primary.span(13..17).label("expected `u32`")),
        ),
    ];

    let renderer = Renderer::plain();
    let events = renderer.render_events(report);
    let mut json = String::from("[");
    for (i, event) in events.enumerate() {
        if i != 0 {
            json.push(',');
        }
        json.push_str("\n  ");
        json.push_str(&event_to_json(&event));
    }
    json.push_str("\n]");
    println!("{json}");
}

fn event_to_json(event: &RenderEvent) -> String {
    let kind = match event.kind {
        EventKind::Header => "\"header\"".to_owned(),
        EventKind::Text => "\"text\"".to_owned(),
        EventKind::Origin => "\"origin\"".to_owned(),
        EventKind::LineNumber => "\"line_number\"".to_owned(),
        EventKind::Source => "\"source\"".to_owned(),
        EventKind::Label => "\"label\"".to_owned(),
        EventKind::Underline => "\"underline\"".to_owned(),
        EventKind::Sidebar { depth } => format!("{{\"sidebar\":{depth}}}"),
        EventKind::Fold => "\"fold\"".to_owned(),
        EventKind::Suggestion => "\"suggestion\"".to_owned(),
        EventKind::Addition => "\"addition\"".to_owned(),
        EventKind::Removal => "\"removal\"".to_owned(),
    };
    let source = match &event.source {
        Some(SourceRef { origin, byte_range }) => {
            let origin = match origin {
                Some(origin) => format!("\"{}\"", escape(origin)),
                None => "null".to_owned(),
            };
            format!(
                "{{\"origin\":{origin},\"byte_start\":{},\"byte_end\":{}}}",
                byte_range.start, byte_range.end
            )
        }
        None => "null".to_owned(),
    };
    format!(
        "{{\"group\":{},\"line\":{},\"column\":{},\"kind\":{kind},\"text\":\"{}\",\"source\":{source}}}",
        event.group,
        event.line,
        event.column,
        escape(&event.text),
    )
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => {
                escaped.push_str(&format!("\\u{:04x}", u32::from(ch)));
            }
            ch => escaped.push(ch),
        }
    }
    escaped
}
