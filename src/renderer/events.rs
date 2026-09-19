//! Structured rendering events
//!
//! [`Renderer::render`][crate::renderer::Renderer::render] produces terminal
//! output: a flat string with optional ANSI escape codes. That is ideal for
//! printing, but tools like editors and language servers often want the
//! *structure* of the diagnostic instead: which text came from the source,
//! where each annotation points, and what is mere decoration.
//!
//! [`Renderer::render_events`][crate::renderer::Renderer::render_events]
//! exposes exactly that, as a streaming sequence of [`RenderEvent`]s. Both
//! entry points share a single layout pass, so the events always describe
//! the same output that [`Renderer::render`][crate::renderer::Renderer::render]
//! would have produced; [`render_plain`] can rebuild the plain-text rendering
//! from the events alone.
//!
//! # Example
//!
//! ```
//! use annotate_snippets::renderer::{EventKind, Renderer};
//! use annotate_snippets::{AnnotationKind, Group, Level, Snippet};
//!
//! let report = &[Group::with_title(Level::ERROR.primary_title("mismatched types")).element(
//!     Snippet::source("let x: u32 = \"hi\";")
//!         .path("src/main.rs")
//!         .annotation(AnnotationKind::Primary.span(13..17).label("expected `u32`")),
//! )];
//!
//! let renderer = Renderer::plain();
//! let events: Vec<_> = renderer.render_events(report).collect();
//!
//! // The source line is identifiable, along with where it came from
//! let source = events.iter().find(|e| e.kind == EventKind::Source).unwrap();
//! assert_eq!(source.text, "let x: u32 = \"hi\";");
//! assert_eq!(
//!     source.source.as_ref().unwrap().origin.as_deref(),
//!     Some("src/main.rs")
//! );
//! assert_eq!(source.source.as_ref().unwrap().byte_range, 0..18);
//!
//! // Decoration, like the underline, explicitly has no source range
//! let underline = events
//!     .iter()
//!     .find(|e| e.kind == EventKind::Underline)
//!     .unwrap();
//! assert_eq!(underline.text, "^^^^");
//! assert!(underline.source.is_none());
//!
//! // No information is lost: the events rebuild the plain-text rendering
//! assert_eq!(
//!     annotate_snippets::renderer::events::render_plain(events),
//!     renderer.render(report),
//! );
//! ```

use alloc::borrow::Cow;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::Range;

use anstyle::Style;

use super::graphics;
use super::preprocess::{Preprocessed, PreprocessedGroup};
use super::styled_buffer::{CellTag, StyledBuffer};
use super::stylesheet::Stylesheet;
use super::{ElementStyle, Renderer};
use crate::{Level, Report};

/// A run of visible text with a single semantic role and style
///
/// Events are emitted in reading order (top to bottom, left to right) and
/// tile the whole rendering: concatenating the `text` of every event on a
/// line, placed at its [`column`][RenderEvent::column], reproduces that
/// output line exactly. Runs are split on semantic boundaries (kind, style,
/// or source range), never on whether colors are enabled, so a plain
/// [`Renderer`] and a styled one produce the same sequence of events
/// modulo the [`style`][RenderEvent::style] field.
///
/// All positions are in character cells of the rendered output, counting
/// from 0. Byte offsets into the original source are carried separately by
/// [`source`][RenderEvent::source]; the two never get conflated, which
/// matters for lines containing tabs, CRLF line endings, wide characters,
/// or combining characters.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderEvent {
    /// Index of the [`Group`][crate::Group] within the
    /// [`Report`] this event belongs to
    pub group: usize,
    /// 0-based line within the rendering of this group
    pub line: usize,
    /// 0-based visual column (in character cells) at which `text` starts
    pub column: usize,
    /// The visible text of this run, without any ANSI escape codes
    pub text: String,
    /// The semantic role of this run
    pub kind: EventKind,
    /// The resolved terminal style of this run
    ///
    /// With [`Renderer::plain`][crate::renderer::Renderer::plain] this is
    /// always the default style; disabling colors never changes how the
    /// output is split into events.
    pub style: Style,
    /// Where in the original source this text was drawn from
    ///
    /// This is only `Some` for [`EventKind::Source`] events. Decorative
    /// characters that do not appear in the original source (gutters,
    /// underlines, labels, elision markers, suggestion text, ...) explicitly
    /// have no source range.
    pub source: Option<SourceRef>,
}

/// The semantic role of a [`RenderEvent`]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// A title or message header, including the level name
    Header,
    /// Message text, such as the body of a title or note
    Text,
    /// An origin line locating the diagnostic, e.g. `--> src/main.rs:1:14`
    Origin,
    /// A line number or separator in the gutter
    LineNumber,
    /// Text drawn verbatim from the source
    ///
    /// These events always carry a [`source`][RenderEvent::source] range.
    Source,
    /// The label of an annotation
    Label,
    /// Underline markers highlighting an annotation, e.g. `^^^^`
    Underline,
    /// A vertical bar of a multiline annotation, at the given nesting depth
    Sidebar {
        /// 1-based nesting depth of the multiline annotation this bar
        /// belongs to
        depth: usize,
    },
    /// An elision marker such as `...` standing in for hidden source lines
    /// or trimmed text
    Fold,
    /// Content of a suggestion block that is neither an addition nor a
    /// removal, e.g. the suggested replacement text
    Suggestion,
    /// Text added by a suggestion
    Addition,
    /// Text removed by a suggestion
    Removal,
}

/// A byte range within one of the report's sources
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceRef {
    /// The origin (path) of the source, as given to
    /// [`Snippet::path`][crate::Snippet::path]; `None` for anonymous sources
    pub origin: Option<String>,
    /// Byte range within that source
    ///
    /// When whitespace was normalized for display (e.g. a tab rendered as
    /// four spaces), every produced cell points at the byte range of the
    /// source char it was expanded from, so the range may be shorter than
    /// the displayed text.
    pub byte_range: Range<usize>,
}

/// Streaming iterator over the [`RenderEvent`]s of a [`Report`]
///
/// Events are produced one [`Group`][crate::Group] at a time, so consumers
/// can start processing (or stop early) without waiting for the whole
/// report to be laid out, and without holding the rendered string.
///
/// Created by [`Renderer::render_events`][crate::renderer::Renderer::render_events].
#[derive(Debug)]
pub struct RenderEvents<'r, 'g> {
    renderer: &'r Renderer,
    layout: LayoutState<'g>,
    max_line_num_len: usize,
    report_primary_path: Option<&'g Cow<'g, str>>,
    group_len: usize,
    group: usize,
    pending: vec::IntoIter<RenderEvent>,
}

enum LayoutState<'g> {
    Full(vec::IntoIter<PreprocessedGroup<'g>>),
    Done,
}

impl core::fmt::Debug for LayoutState<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Full(_) => f.write_str("Full(..)"),
            Self::Done => f.write_str("Done"),
        }
    }
}

impl<'r, 'g> RenderEvents<'r, 'g> {
    fn empty(renderer: &'r Renderer) -> Self {
        RenderEvents {
            renderer,
            layout: LayoutState::Done,
            max_line_num_len: 0,
            report_primary_path: None,
            group_len: 0,
            group: 0,
            pending: Vec::new().into_iter(),
        }
    }
}

impl Iterator for RenderEvents<'_, '_> {
    type Item = RenderEvent;

    fn next(&mut self) -> Option<RenderEvent> {
        loop {
            if let Some(event) = self.pending.next() {
                return Some(event);
            }
            match &mut self.layout {
                LayoutState::Full(groups) => {
                    let group = groups.next()?;
                    let (buffer, level) = graphics::layout_group(
                        self.renderer,
                        self.max_line_num_len,
                        self.group_len,
                        self.group,
                        self.report_primary_path,
                        group,
                    );
                    self.pending =
                        buffer_to_events(&buffer, &level, &self.renderer.stylesheet, self.group)
                            .into_iter();
                    self.group += 1;
                }
                LayoutState::Done => return None,
            }
        }
    }
}

impl Renderer {
    /// Render a diagnostic [`Report`] as a stream of
    /// structured [`RenderEvent`]s
    ///
    /// This uses the same layout pass as
    /// [`render`][crate::renderer::Renderer::render], so the events always
    /// match the string rendering; see the
    /// [module documentation][crate::renderer::events] for details.
    pub fn render_events<'r, 'g>(&'r self, groups: Report<'g>) -> RenderEvents<'r, 'g> {
        if self.short_message {
            let (buffer, level) = graphics::layout_short_message(self, groups);
            let mut events = RenderEvents::empty(self);
            events.pending = buffer_to_events(&buffer, &level, &self.stylesheet, 0).into_iter();
            return events;
        }
        let Preprocessed {
            max_line_num,
            report_primary_path,
            groups,
        } = Preprocessed::preprocess(groups);
        let max_line_num_len = if self.anonymized_snippet_line_numbers {
            graphics::ANONYMIZED_LINE_NUM.len()
        } else {
            graphics::num_decimal_digits(max_line_num)
        };
        let group_len = groups.len();
        RenderEvents {
            renderer: self,
            layout: LayoutState::Full(groups.into_iter()),
            max_line_num_len,
            report_primary_path,
            group_len,
            group: 0,
            pending: Vec::new().into_iter(),
        }
    }
}

/// Rebuild the plain-text rendering of a report from its [`RenderEvent`]s
///
/// The result is exactly what
/// [`Renderer::render`][crate::renderer::Renderer::render] produces with
/// [`Renderer::plain`][crate::renderer::Renderer::plain]: every event's
/// `text` is placed at its [`column`][RenderEvent::column], gaps are filled
/// with spaces, lines are joined with `\n`, and groups are separated by a
/// blank line. Styles are dropped.
///
/// # Example
///
/// ```
/// use annotate_snippets::renderer::Renderer;
/// use annotate_snippets::renderer::events::render_plain;
/// use annotate_snippets::{AnnotationKind, Group, Level, Snippet};
///
/// let report = &[Group::with_title(Level::WARNING.primary_title("unused variable")).element(
///     Snippet::source("fn main() { let x = 1; }")
///         .path("src/main.rs")
///         .annotation(AnnotationKind::Primary.span(21..22).label("unused variable: `x`")),
/// )];
///
/// let renderer = Renderer::styled();
/// let events = renderer.render_events(report).collect::<Vec<_>>();
/// assert_eq!(render_plain(events), Renderer::plain().render(report));
/// ```
pub fn render_plain(events: impl IntoIterator<Item = RenderEvent>) -> String {
    let mut groups: BTreeMap<usize, BTreeMap<usize, Vec<(usize, String)>>> = BTreeMap::new();
    for event in events {
        groups
            .entry(event.group)
            .or_default()
            .entry(event.line)
            .or_default()
            .push((event.column, event.text));
    }
    let mut out = String::new();
    for (g, (_, lines)) in groups.iter().enumerate() {
        if g != 0 {
            out.push('\n');
        }
        for (l, (_, runs)) in lines.iter().enumerate() {
            if l != 0 {
                out.push('\n');
            }
            let width = runs
                .iter()
                .map(|(column, text)| column + text.chars().count())
                .max()
                .unwrap_or(0);
            let mut line = vec![' '; width];
            for (column, text) in runs {
                for (i, ch) in text.chars().enumerate() {
                    line[column + i] = ch;
                }
            }
            out.extend(line);
        }
    }
    out
}

/// Convert a laid-out [`StyledBuffer`] into [`RenderEvent`]s
///
/// Consecutive cells are coalesced into a single event as long as their
/// kind, [`ElementStyle`], and source range line up. The resolved terminal
/// style is deliberately *not* part of the coalescing key, so enabling or
/// disabling colors never changes how the output is split into events.
fn buffer_to_events(
    buffer: &StyledBuffer,
    level: &Level<'_>,
    stylesheet: &Stylesheet,
    group: usize,
) -> Vec<RenderEvent> {
    let mut events = Vec::new();
    for (line, cells) in buffer.lines().enumerate() {
        let mut current: Option<RenderEvent> = None;
        let mut current_style = ElementStyle::NoStyle;
        for (column, cell) in cells.iter().enumerate() {
            let style = cell.style();
            let meta = cell.meta();
            let kind = event_kind(meta.tag, style);
            let source = meta.source.map(|source| SourceRef {
                origin: buffer.source_origin(source.source_id).cloned().flatten(),
                byte_range: source.byte_start..source.byte_end,
            });
            let continues = current.as_ref().is_some_and(|event| {
                event.kind == kind
                    && style == current_style
                    && source_continues(event.source.as_ref(), source.as_ref())
            });
            if continues {
                let event = current.as_mut().expect("checked above");
                event.text.push(cell.ch());
                if let (Some(event_source), Some(source)) = (event.source.as_mut(), source.as_ref())
                {
                    event_source.byte_range.end = source.byte_range.end;
                }
            } else {
                if let Some(event) = current.take() {
                    events.push(event);
                }
                current_style = style;
                current = Some(RenderEvent {
                    group,
                    line,
                    column,
                    text: String::from(cell.ch()),
                    kind,
                    style: style.color_spec(level, stylesheet),
                    source,
                });
            }
        }
        if let Some(event) = current.take() {
            events.push(event);
        }
    }
    events
}

fn source_continues(previous: Option<&SourceRef>, next: Option<&SourceRef>) -> bool {
    match (previous, next) {
        (None, None) => true,
        (Some(previous), Some(next)) => {
            previous.origin == next.origin && previous.byte_range.end == next.byte_range.start
        }
        _ => false,
    }
}

fn event_kind(tag: CellTag, style: ElementStyle) -> EventKind {
    let styled = match style {
        ElementStyle::MainHeaderMsg | ElementStyle::HeaderMsg | ElementStyle::Level(_) => {
            EventKind::Header
        }
        ElementStyle::LineAndColumn => EventKind::Origin,
        ElementStyle::LineNumber => EventKind::LineNumber,
        ElementStyle::Quotation => EventKind::Source,
        ElementStyle::UnderlinePrimary | ElementStyle::UnderlineSecondary => EventKind::Underline,
        ElementStyle::LabelPrimary | ElementStyle::LabelSecondary => EventKind::Label,
        ElementStyle::Addition => EventKind::Addition,
        ElementStyle::Removal => EventKind::Removal,
        ElementStyle::NoStyle => EventKind::Text,
    };
    match tag {
        CellTag::None => styled,
        CellTag::Fold => EventKind::Fold,
        CellTag::Sidebar { depth } => EventKind::Sidebar { depth },
        // Keep the more specific style-derived kind (e.g. `Addition`) and
        // only fall back to `Suggestion` for otherwise unremarkable text.
        CellTag::Suggestion if styled == EventKind::Text => EventKind::Suggestion,
        CellTag::Suggestion => styled,
    }
}

#[cfg(test)]
mod tests {
    use alloc::borrow::ToOwned;
    use alloc::vec::Vec;

    use super::*;
    use crate::{AnnotationKind, Group, Level, Patch, Snippet};

    fn basic_report() -> Vec<Group<'static>> {
        Vec::from([
            Group::with_title(Level::ERROR.primary_title("mismatched types")).element(
                Snippet::source("let x: u32 = \"hi\";")
                    .path("src/main.rs")
                    .annotation(AnnotationKind::Primary.span(13..17).label("expected `u32`")),
            ),
        ])
    }

    fn events(renderer: &Renderer, report: Report<'_>) -> Vec<RenderEvent> {
        renderer.render_events(report).collect()
    }

    fn assert_rebuilds(renderer: &Renderer, report: Report<'_>) -> Vec<RenderEvent> {
        let events = events(renderer, report);
        let rebuilt = render_plain(events.clone());
        let expected = renderer.render(report);
        assert_eq!(rebuilt, expected, "events lost characters");
        events
    }

    #[test]
    fn events_match_string_snapshots() {
        let report = basic_report();
        let renderer = Renderer::plain();
        let expected = r#"error: mismatched types
 --> src/main.rs:1:14
  |
1 | let x: u32 = "hi";
  |              ^^^^ expected `u32`
"#;
        // The string rendering is unchanged ...
        assert_eq!(renderer.render(&report), expected.trim_end());
        // ... and the events rebuild it without losing a character
        let events = assert_rebuilds(&renderer, &report);

        let source = events.iter().find(|e| e.kind == EventKind::Source).unwrap();
        assert_eq!(source.text, "let x: u32 = \"hi\";");
        assert_eq!(source.line, 3);
        assert_eq!(source.column, 4);
        assert_eq!(
            source.source,
            Some(SourceRef {
                origin: Some("src/main.rs".to_owned()),
                byte_range: 0..18,
            })
        );

        let underline = events
            .iter()
            .find(|e| e.kind == EventKind::Underline)
            .unwrap();
        assert_eq!(underline.text, "^^^^");
        assert_eq!(underline.column, 17);
        assert!(underline.source.is_none());

        let label = events.iter().find(|e| e.kind == EventKind::Label).unwrap();
        assert_eq!(label.text, "expected `u32`");
    }

    #[test]
    fn source_events_carry_verbatim_byte_ranges() {
        let report = basic_report();
        let events = events(&Renderer::plain(), &report);
        for event in events.iter().filter(|e| e.kind == EventKind::Source) {
            let source = event.source.as_ref().unwrap();
            assert_eq!(source.origin.as_deref(), Some("src/main.rs"));
            assert_eq!(
                &"let x: u32 = \"hi\";"[source.byte_range.clone()],
                event.text
            );
        }
    }

    #[test]
    fn decor_events_have_no_source_range() {
        let report = basic_report();
        let events = events(&Renderer::plain(), &report);
        // Every semantic kind shows up in this rendering
        for kind in [
            EventKind::Header,
            EventKind::Origin,
            EventKind::LineNumber,
            EventKind::Source,
            EventKind::Underline,
            EventKind::Label,
        ] {
            assert!(events.iter().any(|e| e.kind == kind), "missing {kind:?}");
        }
        // Only text drawn from the source has a source range
        for event in &events {
            assert_eq!(event.source.is_some(), event.kind == EventKind::Source);
        }
    }

    #[test]
    fn trimming_emits_fold_events() {
        let source = "/*这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。*/?";
        let report = Vec::from([Group::with_level(Level::ERROR).element(
            Snippet::source(source).annotation(
                AnnotationKind::Primary
                    .span(source.len() - 2..source.len() - 1)
                    .label("expected item"),
            ),
        )]);
        let renderer = Renderer::plain();
        let expected = r#"
  |
1 | ... 的。这是宽的。这是宽的。这是宽的。这是宽的。这是宽的。*/?
  |                                                            ^ expected item
"#;
        assert_eq!(renderer.render(&report), expected[1..].trim_end());
        let events = assert_rebuilds(&renderer, &report);

        let fold = events.iter().find(|e| e.kind == EventKind::Fold).unwrap();
        assert_eq!(fold.text, "...");
        assert!(fold.source.is_none());

        // The surviving source text still points at the right bytes
        let source_event = events.iter().find(|e| e.kind == EventKind::Source).unwrap();
        let byte_range = source_event.source.as_ref().unwrap().byte_range.clone();
        assert_eq!(&source[byte_range], source_event.text);
    }

    #[test]
    fn multiple_origins_are_distinguished() {
        let report = Vec::from([Group::with_title(Level::ERROR.primary_title("two files"))
            .element(
                Snippet::source("fn a() {}")
                    .path("a.rs")
                    .annotation(AnnotationKind::Primary.span(4..5).label("here")),
            )
            .element(
                Snippet::source("fn b() {}")
                    .path("b.rs")
                    .annotation(AnnotationKind::Context.span(4..5).label("there")),
            )]);
        let events = assert_rebuilds(&Renderer::plain(), &report);

        let origins: Vec<_> = events
            .iter()
            .filter(|e| e.kind == EventKind::Source)
            .map(|e| e.source.as_ref().unwrap().origin.clone())
            .collect();
        assert_eq!(origins, [Some("a.rs".to_owned()), Some("b.rs".to_owned())]);

        let origin_lines: Vec<_> = events
            .iter()
            .filter(|e| e.kind == EventKind::Origin)
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(origin_lines, ["a.rs:1:5", "b.rs:1:5"]);
    }

    #[test]
    fn overlapping_annotations_events() {
        let report = Vec::from([
            Group::with_title(Level::ERROR.primary_title("overlap")).element(
                Snippet::source("abcdefgh")
                    .path("o.rs")
                    .annotation(AnnotationKind::Primary.span(1..5).label("first"))
                    .annotation(AnnotationKind::Context.span(3..7).label("second")),
            ),
        ]);
        let renderer = Renderer::plain();
        let expected = r#"error: overlap
 --> o.rs:1:2
  |
1 | abcdefgh
  |  ^^^^--
  |  | |
  |  | second
  |  first
"#;
        assert_eq!(renderer.render(&report), expected.trim_end());
        let events = assert_rebuilds(&renderer, &report);

        let labels: Vec<_> = events
            .iter()
            .filter(|e| e.kind == EventKind::Label)
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(labels, ["second", "first"]);
    }

    #[test]
    fn empty_label_events() {
        let report =
            Vec::from([
                Group::with_title(Level::ERROR.primary_title("empty label")).element(
                    Snippet::source("abcdefgh")
                        .path("e.rs")
                        .annotation(AnnotationKind::Primary.span(2..4).label("")),
                ),
            ]);
        let events = assert_rebuilds(&Renderer::plain(), &report);
        // No empty runs are emitted
        assert!(events.iter().all(|e| !e.text.is_empty()));
        let underline = events
            .iter()
            .find(|e| e.kind == EventKind::Underline)
            .unwrap();
        assert_eq!(underline.text, "^^");
    }

    #[test]
    fn multiline_suggestion_events() {
        let source = "fn main() {\n    foo();\n}\n";
        let report = Vec::from([Group::with_title(Level::HELP.primary_title("rewrite it"))
            .element(
                Snippet::source(source)
                    .path("main.rs")
                    .patch(Patch::new(17..22, "bar(\n        1,\n    )")),
            )]);
        let renderer = Renderer::plain();
        let expected = r#"help: rewrite it
 --> main.rs:2:6
  |
2 ~     fbar(
3 +         1,
4 +     )
  |
"#;
        assert_eq!(renderer.render(&report), expected.trim_end());
        let events = assert_rebuilds(&renderer, &report);

        assert!(
            events
                .iter()
                .any(|e| e.kind == EventKind::Suggestion && e.text == "    f")
        );
        assert!(events.iter().any(|e| e.kind == EventKind::Addition));
        // Suggestion text is synthesized, not drawn from the source
        assert!(
            events
                .iter()
                .all(|e| e.source.is_none() || e.kind == EventKind::Source)
        );
    }

    #[test]
    fn fold_marker_events() {
        let source = "line one\nline two\nline three\nline four\nline five\nline six\nline seven\n";
        let report = Vec::from([
            Group::with_title(Level::ERROR.primary_title("folded")).element(
                Snippet::source(source)
                    .path("f.rs")
                    .fold(true)
                    .annotation(AnnotationKind::Primary.span(0..4).label("start"))
                    .annotation(AnnotationKind::Primary.span(60..64).label("end")),
            ),
        ]);
        let events = assert_rebuilds(&Renderer::plain(), &report);

        let fold = events.iter().find(|e| e.kind == EventKind::Fold).unwrap();
        assert_eq!(fold.text, "...");
        assert!(fold.source.is_none());

        // The lines on either side of the fold still map to their bytes
        let source_lines: Vec<_> = events
            .iter()
            .filter(|e| e.kind == EventKind::Source)
            .collect();
        assert_eq!(source_lines.len(), 2);
        assert_eq!(source_lines[0].source.as_ref().unwrap().byte_range, 0..8);
        assert_eq!(source_lines[1].source.as_ref().unwrap().byte_range, 58..68);
    }

    #[test]
    fn crlf_wide_combining_and_tab_events() {
        let source = "fn f() {\r\n\tlet s = \"こんにちは\";\r\n    let e = \"e\u{301}\";\r\n}\r\n";
        let report = Vec::from([
            Group::with_title(Level::ERROR.primary_title("bytes")).element(
                Snippet::source(source)
                    .path("c.rs")
                    .fold(false)
                    .annotation(AnnotationKind::Primary.span(19..36).label("wide"))
                    .annotation(AnnotationKind::Context.span(52..55).label("comb")),
            ),
        ]);
        let renderer = Renderer::plain();
        let expected = "error: bytes\n --> c.rs:2:10\n  |\n1 | fn f() {\n2 |     let s = \"こんにちは\";\n  |             ^^^^^^^^^^^^ wide\n3 |     let e = \"e\u{301}\";\n  |              - comb\n4 | }\n  |";
        assert_eq!(renderer.render(&report), expected);
        let events = assert_rebuilds(&renderer, &report);

        let sources: Vec<_> = events
            .iter()
            .filter(|e| e.kind == EventKind::Source)
            .collect();
        // CRLF line endings are not part of any line's byte range
        assert_eq!(sources[0].text, "fn f() {");
        assert_eq!(sources[0].source.as_ref().unwrap().byte_range, 0..8);
        // The tab is displayed as four spaces, each pointing at the tab's byte
        let tab_spaces: Vec<_> = sources
            .iter()
            .filter(|e| e.source.as_ref().unwrap().byte_range == (10..11))
            .collect();
        assert_eq!(tab_spaces.len(), 3);
        assert!(tab_spaces.iter().all(|e| e.text == " "));
        // ... while the following text starts at the byte after the tab
        let line = sources
            .iter()
            .find(|e| e.text.contains("こんにちは"))
            .unwrap();
        assert_eq!(line.source.as_ref().unwrap().byte_range, 10..37);
        // Visual columns and byte offsets are reported independently: the
        // combining char is one cell but two bytes
        let comb_line = sources.iter().find(|e| e.text.contains('\u{301}')).unwrap();
        assert_eq!(comb_line.source.as_ref().unwrap().byte_range, 39..57);
        let comb_underline = events
            .iter()
            .filter(|e| e.kind == EventKind::Underline)
            .nth(1)
            .unwrap();
        assert_eq!(comb_underline.text, "-");
        assert_eq!(comb_underline.column, 17);
    }

    #[test]
    fn sidebar_depth_events() {
        let source = "fn foo() {\n    let x = 1;\n    let y = 2;\n}\n";
        let report = Vec::from([Group::with_title(Level::ERROR.primary_title("multiline"))
            .element(
                Snippet::source(source)
                    .path("m.rs")
                    .annotation(AnnotationKind::Primary.span(4..30).label("whole block")),
            )]);
        let events = assert_rebuilds(&Renderer::plain(), &report);

        let sidebars: Vec<_> = events
            .iter()
            .filter_map(|e| match e.kind {
                EventKind::Sidebar { depth } => Some((depth, e.text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(sidebars, [(1, "|")]);
    }

    #[test]
    fn color_off_only_changes_style() {
        let report = basic_report();
        let plain = events(&Renderer::plain(), &report);
        let styled = events(&Renderer::styled(), &report);

        // Identical splitting and positions ...
        let shape = |events: &[RenderEvent]| {
            events
                .iter()
                .map(|e| {
                    (
                        e.group,
                        e.line,
                        e.column,
                        e.text.clone(),
                        e.kind,
                        e.source.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(shape(&plain), shape(&styled));

        // ... only the resolved style differs
        assert!(plain.iter().all(|e| e.style == Style::new()));
        assert!(styled.iter().any(|e| e.style != Style::new()));
    }

    #[test]
    fn events_stream_per_group() {
        let report = Vec::from([
            Group::with_title(Level::ERROR.primary_title("first")).element(
                Snippet::source("fn a() {}")
                    .path("a.rs")
                    .annotation(AnnotationKind::Primary.span(4..5).label("here")),
            ),
            Group::with_title(Level::WARNING.primary_title("second")).element(
                Snippet::source("fn b() {}")
                    .path("b.rs")
                    .annotation(AnnotationKind::Primary.span(4..5).label("there")),
            ),
        ]);
        let renderer = Renderer::plain();

        // Consumers can stop early without laying out the remaining groups
        let mut stream = renderer.render_events(&report);
        let first: Vec<_> = stream.by_ref().take(3).collect();
        assert_eq!(first.len(), 3);
        assert!(first.iter().all(|e| e.group == 0));
        let rest: Vec<_> = stream.collect();
        let all: Vec<_> = first.into_iter().chain(rest).collect();

        // Both groups are covered and the text is fully recovered
        assert!(all.iter().any(|e| e.group == 0));
        assert!(all.iter().any(|e| e.group == 1));
        assert_eq!(render_plain(all), renderer.render(&report));
    }

    #[test]
    fn short_message_events() {
        let report = basic_report();
        let renderer = Renderer::plain().short_message(true);
        let expected = "src/main.rs:1:14: error: mismatched types: expected `u32`";
        assert_eq!(renderer.render(&report), expected);
        let events = assert_rebuilds(&renderer, &report);

        assert_eq!(events[0].kind, EventKind::Origin);
        assert_eq!(events[0].text, "src/main.rs:1:14: ");
        assert_eq!(events[1].kind, EventKind::Header);
        assert_eq!(events[1].text, "error");
    }
}
