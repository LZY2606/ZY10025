//! Structured rendering events
//!
//! [`Renderer::render`][crate::Renderer::render] lays a [`Report`][crate::Report]
//! out as terminal text.  The types in this module expose that *same* layout
//! as structured data, so consumers like editors can build their own
//! presentation instead of parsing ANSI text back apart.
//!
//! # Example
//!
//! ```
//! # use annotate_snippets::*;
//! # use annotate_snippets::renderer::*;
//! let report = &[Group::with_title(Level::ERROR.primary_title("mismatched types"))
//!     .element(
//!         Snippet::source("let x: u32 = \"hi\";")
//!             .path("src/main.rs")
//!             .annotation(AnnotationKind::Primary.span(13..17).label("expected `u32`")),
//!     )];
//!
//! let renderer = Renderer::plain();
//! for event in renderer.render_events(report) {
//!     if let Some(source) = &event.source {
//!         println!(
//!             "line {} col {}: {:?} quotes bytes {:?} of {:?}",
//!             event.line, event.column, event.text, source.byte_range, source.path,
//!         );
//!     }
//! }
//! ```

use alloc::borrow::ToOwned;
use alloc::string::String;
use core::ops::Range;

use anstyle::Style;

use super::graphics::Rendered;
use super::styled_buffer::{Decor, StyledChar};
use super::{ElementStyle, Renderer};

/// A run of visible text in the rendered output, with everything an editor
/// needs to present it: position, style, semantics, and source provenance.
///
/// Events are emitted in reading order (top to bottom, left to right) and
/// their `text`s concatenate back into the exact string produced by
/// [`Renderer::render`], so no characters are ever dropped or invented.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// 0-based line within the whole rendered output.
    pub line: usize,
    /// 0-based column (in rendered characters) within [`line`][Self::line].
    pub column: usize,
    /// The visible text of this run; never contains `'\n'`.
    pub text: String,
    /// The resolved terminal style for this run.
    ///
    /// This is the only part of an `Event` affected by styling
    /// configuration ([`Renderer::styled`] vs [`Renderer::plain`]); event
    /// splitting and positions are identical either way.
    pub style: Style,
    /// What this run represents in the diagnostic.
    pub kind: EventKind,
    /// Where in the original source this run was quoted from.
    ///
    /// Decorative characters (gutters, underlines, labels, elision markers,
    /// suggested replacement text, ...) do not appear in the original source
    /// and always have `None` here.
    pub source: Option<SourceRef>,
}

/// What a run of rendered text represents in the diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// A [`Title`][crate::Title] or its level tag (e.g. `error: ...`).
    Title,
    /// A free-standing [`Message`][crate::Message] or other unclassified text.
    Text,
    /// An [`Origin`][crate::Origin] line/column reference (e.g. `src/main.rs:1:15`).
    Origin,
    /// A line number in the gutter.
    LineNumber,
    /// A vertical sidebar bar connecting elements.
    ///
    /// `depth` is `0` for the main gutter; nested multiline annotations draw
    /// their bars at increasing depths.
    Sidebar {
        /// Nesting depth of the bar; `0` is the main gutter.
        depth: usize,
    },
    /// Text quoted from the original source; see [`Event::source`].
    Source,
    /// An annotation underline (e.g. `^^^^`, `----`, multiline frames).
    Underline,
    /// An annotation label.
    Label,
    /// An elision marker for folded-away content (`...`, `…`, `┆`).
    Fold,
    /// Suggested added text or its markers (`+ `, `~`).
    Addition,
    /// Suggested removed text or its markers (`- `).
    Removal,
    /// Other structural separators (file-start arrows, note separators, ...).
    Separator,
}

/// The original source location a rendered [`Event`] was quoted from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceRef {
    /// The [`Snippet::path`][crate::Snippet::path] of the source, if one was
    /// provided.
    pub path: Option<String>,
    /// The 1-based line number within the source.
    pub line: usize,
    /// Byte offsets into the source text.
    ///
    /// This is deliberately kept distinct from [`column_range`][Self::column_range]:
    /// tabs, wide characters, and combining characters all make the two
    /// disagree.
    pub byte_range: Range<usize>,
    /// 0-based display columns within the source line.
    pub column_range: Range<usize>,
}

pub(crate) fn emit_events(
    renderer: &Renderer,
    rendered: &Rendered<'_>,
    sink: &mut dyn FnMut(Event),
) {
    let mut line_offset = 0;
    for group in &rendered.groups {
        let buffer = &group.buffer;
        for (line_idx, line) in buffer.lines().iter().enumerate() {
            emit_line(
                renderer,
                rendered,
                group,
                line,
                line_offset + line_idx,
                sink,
            );
        }
        line_offset += buffer.num_lines();
    }
}

fn emit_line(
    renderer: &Renderer,
    rendered: &Rendered<'_>,
    group: &super::graphics::RenderedGroup<'_>,
    chars: &[StyledChar],
    line: usize,
    sink: &mut dyn FnMut(Event),
) {
    if chars.is_empty() {
        // Emit a placeholder so that every output line is represented and
        // the original text can be reconstructed losslessly.
        sink(Event {
            line,
            column: 0,
            text: String::new(),
            style: ElementStyle::NoStyle.color_spec(&group.level, &renderer.stylesheet),
            kind: EventKind::Text,
            source: None,
        });
        return;
    }
    let mut run: Option<Run> = None;
    for (column, ch) in chars.iter().enumerate() {
        let kind = kind_of(ch);
        let point = ch.meta.source;
        let continues = run.as_mut().is_some_and(|run| run.accepts(ch, kind));
        if !continues {
            if let Some(finished) = run.take() {
                finished.emit(renderer, rendered, group, line, sink);
            }
            run = Some(Run::new(column, ch, kind));
        } else if let Some(run) = &mut run {
            run.push(ch, point);
        }
    }
    if let Some(finished) = run {
        finished.emit(renderer, rendered, group, line, sink);
    }
}

/// A maximal run of characters that belong to the same [`Event`].
struct Run {
    column: usize,
    kind: EventKind,
    style: ElementStyle,
    text: String,
    source: Option<u16>,
    byte_end: u32,
    display_end: u32,
    first: Option<super::styled_buffer::SourcePoint>,
}

impl Run {
    fn new(column: usize, ch: &StyledChar, kind: EventKind) -> Self {
        let point = ch.meta.source;
        let mut text = String::new();
        text.push(ch.ch);
        Self {
            column,
            kind,
            style: ch.style,
            text,
            source: point.map(|p| p.source),
            byte_end: point.map_or(0, |p| p.byte_end),
            display_end: point.map_or(0, |p| p.display_end),
            first: point,
        }
    }

    /// Whether `ch` extends this run: same kind and style, and source
    /// characters that continue the current byte range (expanded characters
    /// like tabs map several glyphs to the same source byte, so ranges may
    /// overlap but never go backwards or skip).
    fn accepts(&self, ch: &StyledChar, kind: EventKind) -> bool {
        if kind != self.kind || ch.style != self.style {
            return false;
        }
        match (self.source, ch.meta.source) {
            (None, None) => true,
            (Some(id), Some(point)) => {
                point.source == id
                    && point.byte >= self.first.unwrap().byte
                    && point.byte <= self.byte_end
            }
            _ => false,
        }
    }

    fn push(&mut self, ch: &StyledChar, point: Option<super::styled_buffer::SourcePoint>) {
        self.text.push(ch.ch);
        if let Some(point) = point {
            self.byte_end = self.byte_end.max(point.byte_end);
            self.display_end = self.display_end.max(point.display_end);
        }
    }

    fn emit(
        self,
        renderer: &Renderer,
        rendered: &Rendered<'_>,
        group: &super::graphics::RenderedGroup<'_>,
        line: usize,
        sink: &mut dyn FnMut(Event),
    ) {
        let source = self.first.map(|first| SourceRef {
            path: rendered
                .sources
                .get(first.source as usize)
                .and_then(|path| path.map(str::to_owned)),
            line: first.line as usize,
            byte_range: first.byte as usize..self.byte_end as usize,
            column_range: first.display as usize..self.display_end as usize,
        });
        sink(Event {
            line,
            column: self.column,
            text: self.text,
            style: self.style.color_spec(&group.level, &renderer.stylesheet),
            kind: self.kind,
            source,
        });
    }
}

fn kind_of(ch: &StyledChar) -> EventKind {
    match ch.meta.decor {
        Decor::Fold => EventKind::Fold,
        Decor::Sidebar(depth) => EventKind::Sidebar {
            depth: depth as usize,
        },
        Decor::LineNumber => EventKind::LineNumber,
        Decor::Separator => EventKind::Separator,
        Decor::None => match ch.style {
            ElementStyle::Addition => EventKind::Addition,
            ElementStyle::Removal => EventKind::Removal,
            _ if ch.meta.source.is_some() => EventKind::Source,
            ElementStyle::UnderlinePrimary | ElementStyle::UnderlineSecondary => {
                EventKind::Underline
            }
            ElementStyle::LabelPrimary | ElementStyle::LabelSecondary => EventKind::Label,
            ElementStyle::Level(_) | ElementStyle::MainHeaderMsg | ElementStyle::HeaderMsg => {
                EventKind::Title
            }
            ElementStyle::LineAndColumn => EventKind::Origin,
            ElementStyle::LineNumber => EventKind::Separator,
            ElementStyle::Quotation | ElementStyle::NoStyle => EventKind::Text,
        },
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::renderer::DecorStyle;
    use crate::{AnnotationKind, Group, Level, Origin, Patch, Snippet};

    /// Rebuild the plain-text output purely from events, proving no
    /// characters were dropped or invented.
    fn reconstruct(events: &[Event]) -> String {
        let mut out = String::new();
        let mut line = 0;
        for event in events {
            while line < event.line {
                out.push('\n');
                line += 1;
            }
            out.push_str(&event.text);
        }
        out
    }

    fn simple_report() -> Vec<Group<'static>> {
        vec![Level::ERROR.primary_title("mismatched types").element(
            Snippet::source("let x: u32 = \"hi\";")
                .path("src/main.rs")
                .annotation(AnnotationKind::Primary.span(14..18).label("expected `u32`")),
        )]
    }

    fn trimmed_report() -> Vec<Group<'static>> {
        vec![Level::ERROR.primary_title("trimmed").element(
            Snippet::source(
                "fn main() { let some_quite_long_variable_name = another_long_function_call(argument); }",
            )
            .path("src/main.rs")
            .annotation(
                AnnotationKind::Primary
                    .span(60..75)
                    .label("this bit right here"),
            ),
        )]
    }

    fn multiple_origins_report() -> Vec<Group<'static>> {
        vec![Level::ERROR
            .primary_title("multiple origins")
            .element(Origin::path("src/first.rs").line(3).char_column(2))
            .element(Origin::path("src/second.rs").line(9).char_column(12))]
    }

    fn overlapping_annotations_report() -> Vec<Group<'static>> {
        vec![Level::ERROR.primary_title("overlapping").element(
            Snippet::source("fn foo() {\n    bar();\n    baz();\n}\n")
                .path("src/main.rs")
                .annotation(AnnotationKind::Primary.span(0..22).label("outer"))
                .annotation(AnnotationKind::Context.span(4..30).label("inner")),
        )]
    }

    fn empty_label_report() -> Vec<Group<'static>> {
        vec![Level::ERROR.primary_title("empty label").element(
            Snippet::source("let x = 1;")
                .path("src/main.rs")
                .annotation(AnnotationKind::Primary.span(4..5).label(""))
                .annotation(AnnotationKind::Context.span(8..9)),
        )]
    }

    fn multiline_suggestion_report() -> Vec<Group<'static>> {
        vec![Level::HELP
            .primary_title("consider this")
            .element(Snippet::source("let x = foo();\n").path("src/main.rs").patch(
                Patch::new(8..11, "bar(\n    1,\n    2,\n)"),
            ))]
    }

    fn crlf_report() -> Vec<Group<'static>> {
        vec![Level::ERROR.primary_title("oops").element(
            Snippet::source("First line\r\nSecond oops line")
                .path("<current file>")
                .annotation(AnnotationKind::Primary.span(19..23).label("oops")),
        )]
    }

    fn wide_chars_report() -> Vec<Group<'static>> {
        vec![Level::ERROR.primary_title("wide").element(
            Snippet::source("こんにちは、世界")
                .path("<current file>")
                .annotation(AnnotationKind::Primary.span(18..24).label("world")),
        )]
    }

    fn tabs_report() -> Vec<Group<'static>> {
        vec![Level::ERROR.primary_title("tabs").element(
            Snippet::source("\tfoo\tbar")
                .path("src/main.rs")
                .annotation(AnnotationKind::Primary.span(5..8).label("bar")),
        )]
    }

    fn combining_chars_report() -> Vec<Group<'static>> {
        vec![Level::ERROR.primary_title("combining").element(
            Snippet::source("cafe\u{301} au lait")
                .path("src/main.rs")
                .annotation(AnnotationKind::Primary.span(0..6).label("café")),
        )]
    }

    fn folded_lines_report() -> Vec<Group<'static>> {
        vec![Level::ERROR.primary_title("folded").element(
            Snippet::source("a\nb\nc\nd\ne\nf\ng\nh\n")
                .path("src/main.rs")
                .annotation(AnnotationKind::Primary.span(0..1).label("first"))
                .annotation(AnnotationKind::Primary.span(14..15).label("last")),
        )]
    }

    fn all_reports() -> Vec<(&'static str, Vec<Group<'static>>)> {
        vec![
            ("simple", simple_report()),
            ("trimmed", trimmed_report()),
            ("multiple_origins", multiple_origins_report()),
            ("overlapping_annotations", overlapping_annotations_report()),
            ("empty_label", empty_label_report()),
            ("multiline_suggestion", multiline_suggestion_report()),
            ("crlf", crlf_report()),
            ("wide_chars", wide_chars_report()),
            ("tabs", tabs_report()),
            ("combining_chars", combining_chars_report()),
            ("folded_lines", folded_lines_report()),
        ]
    }

    #[test]
    fn events_reconstruct_rendered_text() {
        for (name, report) in all_reports() {
            for decor_style in [DecorStyle::Ascii, DecorStyle::Unicode] {
                let renderer = Renderer::plain()
                    .decor_style(decor_style)
                    .term_width(40);
                let events = renderer.render_events(&report);
                let expected = renderer.render(&report);
                let actual = reconstruct(&events);
                assert_eq!(actual, expected, "report `{name}` with {decor_style:?}");
            }
        }
    }

    #[test]
    fn short_message_events_reconstruct_rendered_text() {
        let report = simple_report();
        let renderer = Renderer::plain().short_message(true);
        let events = renderer.render_events(&report);
        assert_eq!(reconstruct(&events), renderer.render(&report));
    }

    #[test]
    fn styling_only_affects_event_styles() {
        for (name, report) in all_reports() {
            let plain = Renderer::plain().term_width(40).render_events(&report);
            let styled = Renderer::styled().term_width(40).render_events(&report);
            assert_eq!(plain.len(), styled.len(), "report `{name}`");
            for (plain, styled) in plain.iter().zip(styled.iter()) {
                let plain_unstyled = Event {
                    style: Style::new(),
                    ..plain.clone()
                };
                let styled_unstyled = Event {
                    style: Style::new(),
                    ..styled.clone()
                };
                assert_eq!(plain_unstyled, styled_unstyled, "report `{name}`");
            }
        }
    }

    #[test]
    fn streaming_matches_batch() {
        for (name, report) in all_reports() {
            let renderer = Renderer::plain().term_width(40);
            let batch = renderer.render_events(&report);
            let mut streamed = Vec::new();
            renderer.render_events_with(&report, &mut |event| streamed.push(event));
            assert_eq!(batch, streamed, "report `{name}`");
        }
    }

    #[test]
    fn decor_has_no_source_range() {
        for (name, report) in all_reports() {
            let events = Renderer::plain().term_width(40).render_events(&report);
            for event in &events {
                match event.kind {
                    EventKind::Source => {
                        assert!(event.source.is_some(), "report `{name}`: {event:?}");
                    }
                    _ => assert!(event.source.is_none(), "report `{name}`: {event:?}"),
                }
            }
        }
    }

    #[test]
    fn source_events_cover_quoted_text() {
        let source = "let x: u32 = \"hi\";";
        let report = simple_report();
        let events = Renderer::plain().render_events(&report);
        let quoted: Vec<&Event> = events
            .iter()
            .filter(|e| e.kind == EventKind::Source)
            .collect();
        assert_eq!(quoted.len(), 1);
        let source_ref = quoted[0].source.as_ref().unwrap();
        assert_eq!(source_ref.path.as_deref(), Some("src/main.rs"));
        assert_eq!(source_ref.line, 1);
        assert_eq!(source_ref.byte_range, 0..source.len());
        assert_eq!(source_ref.column_range, 0..source.len());
        assert_eq!(&source[source_ref.byte_range.clone()], quoted[0].text);
    }

    #[test]
    fn crlf_byte_offsets_account_for_line_ending() {
        let report = crlf_report();
        let events = Renderer::plain().render_events(&report);
        let quoted: Vec<&Event> = events
            .iter()
            .filter(|e| e.kind == EventKind::Source)
            .collect();
        assert_eq!(quoted.len(), 1);
        let source_ref = quoted[0].source.as_ref().unwrap();
        // `First line\r\n` is 12 bytes, so the second line starts at byte 12.
        assert_eq!(source_ref.line, 2);
        assert_eq!(source_ref.byte_range, 12..28);
        assert_eq!(source_ref.column_range, 0..16);
        assert_eq!(quoted[0].text, "Second oops line");
    }

    #[test]
    fn wide_chars_distinguish_bytes_from_columns() {
        let report = wide_chars_report();
        let events = Renderer::plain().render_events(&report);
        let quoted: Vec<&Event> = events
            .iter()
            .filter(|e| e.kind == EventKind::Source)
            .collect();
        assert_eq!(quoted.len(), 1);
        let source_ref = quoted[0].source.as_ref().unwrap();
        // 8 characters, 24 bytes, 16 display columns.
        assert_eq!(source_ref.byte_range, 0..24);
        assert_eq!(source_ref.column_range, 0..16);
        assert_eq!(quoted[0].text, "こんにちは、世界");
    }

    #[test]
    fn tabs_expand_visually_but_not_in_bytes() {
        let report = tabs_report();
        let events = Renderer::plain().render_events(&report);
        let quoted: Vec<&Event> = events
            .iter()
            .filter(|e| e.kind == EventKind::Source)
            .collect();
        // Each tab renders as 4 spaces that map back to its single byte, so
        // the 8-byte source line occupies 14 display columns.
        assert_eq!(quoted.len(), 1);
        assert_eq!(quoted[0].text, "    foo    bar");
        assert_eq!(quoted[0].source.as_ref().unwrap().byte_range, 0..8);
        assert_eq!(quoted[0].source.as_ref().unwrap().column_range, 0..14);
    }

    #[test]
    fn combining_chars_keep_byte_offsets() {
        let report = combining_chars_report();
        let events = Renderer::plain().render_events(&report);
        let quoted: Vec<&Event> = events
            .iter()
            .filter(|e| e.kind == EventKind::Source)
            .collect();
        assert_eq!(quoted.len(), 1);
        let source_ref = quoted[0].source.as_ref().unwrap();
        assert_eq!(source_ref.byte_range, 0.."cafe\u{301} au lait".len());
        assert_eq!(quoted[0].text, "cafe\u{301} au lait");
    }

    #[test]
    fn trimming_emits_fold_markers() {
        let report = trimmed_report();
        let events = Renderer::plain().term_width(40).render_events(&report);
        let folds: Vec<&Event> = events
            .iter()
            .filter(|e| e.kind == EventKind::Fold)
            .collect();
        assert!(!folds.is_empty());
        assert!(folds.iter().all(|e| e.text == "..."));
        // The surviving source text still points at the right bytes.
        let quoted: Vec<&Event> = events
            .iter()
            .filter(|e| e.kind == EventKind::Source)
            .collect();
        assert!(!quoted.is_empty());
        for event in quoted {
            let source_ref = event.source.as_ref().unwrap();
            assert_eq!(
                &&"fn main() { let some_quite_long_variable_name = another_long_function_call(argument); }"
                    [source_ref.byte_range.clone()],
                &event.text,
            );
        }
    }

    #[test]
    fn folded_lines_emit_fold_markers() {
        let report = folded_lines_report();
        let events = Renderer::plain().render_events(&report);
        let folds: Vec<&Event> = events
            .iter()
            .filter(|e| e.kind == EventKind::Fold)
            .collect();
        assert_eq!(folds.len(), 1);
        assert_eq!(folds[0].text, "...");
    }

    #[test]
    fn multiline_annotations_have_sidebar_depths() {
        let report = overlapping_annotations_report();
        let events = Renderer::plain().render_events(&report);
        let depths: Vec<usize> = events
            .iter()
            .filter_map(|e| match e.kind {
                EventKind::Sidebar { depth } => Some(depth),
                _ => None,
            })
            .collect();
        assert!(depths.contains(&0));
        assert!(depths.iter().any(|d| *d >= 1));
    }

    #[test]
    fn multiple_origins_emit_origin_events() {
        let report = multiple_origins_report();
        let events = Renderer::plain().render_events(&report);
        let origins: Vec<&Event> = events
            .iter()
            .filter(|e| e.kind == EventKind::Origin)
            .collect();
        assert_eq!(origins.len(), 2);
        assert_eq!(origins[0].text, "src/first.rs:3:2");
        assert_eq!(origins[1].text, "src/second.rs:9:12");
    }

    #[test]
    fn multiline_suggestion_emits_addition_events() {
        let report = multiline_suggestion_report();
        let events = Renderer::plain().render_events(&report);
        assert!(
            events
                .iter()
                .any(|e| e.kind == EventKind::Addition && e.text.contains("+"))
        );
        // The suggested replacement text is not part of the original source.
        for event in &events {
            if event.text.contains("bar(") {
                assert!(event.source.is_none());
            }
        }
    }

    #[test]
    fn removal_suggestion_marks_source_text() {
        let report = vec![Level::HELP.primary_title("remove it").element(
            Snippet::source("let x = 1;\nlet y = 2;\n")
                .path("src/main.rs")
                .patch(Patch::new(0..12, "")),
        )];
        let events = Renderer::plain().render_events(&report);
        let removals: Vec<&Event> = events
            .iter()
            .filter(|e| e.kind == EventKind::Removal)
            .collect();
        assert!(!removals.is_empty());
        // Removed source text keeps pointing into the original source.
        assert!(
            removals
                .iter()
                .any(|e| e.source.is_some() && e.text.contains("let x = 1;"))
        );
    }

    #[test]
    fn event_kinds_for_simple_report() {
        let report = simple_report();
        let events = Renderer::plain().render_events(&report);
        let kinds: Vec<(EventKind, &str)> = events
            .iter()
            .map(|e| (e.kind, e.text.as_str()))
            .collect();
        let expected = [
            (EventKind::Title, "error"),
            (EventKind::Title, ": mismatched types"),
            (EventKind::Text, " "),
            (EventKind::Separator, "--> "),
            (EventKind::Origin, "src/main.rs:1:15"),
            (EventKind::Text, "  "),
            (EventKind::Sidebar { depth: 0 }, "|"),
            (EventKind::LineNumber, "1"),
            (EventKind::Text, " "),
            (EventKind::Sidebar { depth: 0 }, "|"),
            (EventKind::Text, " "),
            (EventKind::Source, "let x: u32 = \"hi\";"),
            (EventKind::Text, "  "),
            (EventKind::Sidebar { depth: 0 }, "|"),
            (EventKind::Text, "               "),
            (EventKind::Underline, "^^^^"),
            (EventKind::Text, " "),
            (EventKind::Label, "expected `u32`"),
        ];
        assert_eq!(kinds, expected);
    }
}
