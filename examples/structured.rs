//! Render a [`Report`] as structured events and serialize them to JSON by
//! hand — `annotate-snippets` itself does not depend on any JSON crate.

use annotate_snippets::renderer::{Event, EventKind, SourceRef};
use annotate_snippets::{AnnotationKind, Group, Level, Renderer, Snippet};
use std::fmt::Write as _;

fn main() {
    let report = &[Group::with_title(Level::ERROR.primary_title("mismatched types").id("E0308"))
        .element(
            Snippet::source("let x: u32 = \"hi\";")
                .path("src/main.rs")
                .annotation(AnnotationKind::Primary.span(13..17).label("expected `u32`")),
        )];

    let renderer = Renderer::plain();
    let mut json = String::from("[\n");
    let mut first = true;
    // Events stream in; the JSON is built incrementally without ever
    // holding the rendered string.
    renderer.render_events_with(report, &mut |event| {
        if !first {
            json.push_str(",\n");
        }
        first = false;
        let _ = write!(json, "  {}", event_json(&event));
    });
    json.push_str("\n]\n");
    println!("{json}");
}

fn event_json(event: &Event) -> String {
    let mut out = String::from("{");
    let _ = write!(out, "\"line\":{}", event.line);
    let _ = write!(out, ",\"column\":{}", event.column);
    let _ = write!(out, ",\"text\":{}", json_str(&event.text));
    let _ = write!(out, ",\"kind\":\"{}\"", kind_name(event.kind));
    if let Some(source) = &event.source {
        let _ = write!(out, ",\"source\":{}", source_json(source));
    }
    out.push('}');
    out
}

fn source_json(source: &SourceRef) -> String {
    let mut out = String::from("{");
    if let Some(path) = &source.path {
        let _ = write!(out, "\"path\":{},", json_str(path));
    }
    let _ = write!(
        out,
        "\"line\":{},\"bytes\":[{},{}],\"columns\":[{},{}]",
        source.line,
        source.byte_range.start,
        source.byte_range.end,
        source.column_range.start,
        source.column_range.end,
    );
    out.push('}');
    out
}

fn kind_name(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Title => "title",
        EventKind::Text => "text",
        EventKind::Origin => "origin",
        EventKind::LineNumber => "line_number",
        EventKind::Sidebar { .. } => "sidebar",
        EventKind::Source => "source",
        EventKind::Underline => "underline",
        EventKind::Label => "label",
        EventKind::Fold => "fold",
        EventKind::Addition => "addition",
        EventKind::Removal => "removal",
        EventKind::Separator => "separator",
    }
}

fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => {
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}
