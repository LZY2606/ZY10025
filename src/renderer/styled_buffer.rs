//! Adapted from [styled_buffer]
//!
//! [styled_buffer]: https://github.com/rust-lang/rust/blob/894f7a4ba6554d3797404bbf550d9919df060b97/compiler/rustc_errors/src/styled_buffer.rs

use alloc::string::String;
use alloc::{vec, vec::Vec};
use core::fmt::{self, Write};

use crate::Level;
use crate::renderer::ElementStyle;
use crate::renderer::stylesheet::Stylesheet;

#[derive(Debug)]
pub(crate) struct StyledBuffer {
    lines: Vec<Vec<StyledChar>>,
    /// Identity of each source that cells can point back into, indexed by
    /// [`CellSource::source_id`].
    sources: Vec<Option<String>>,
    /// Tag stamped on cells written through [`StyledBuffer::putc`],
    /// [`StyledBuffer::puts`], [`StyledBuffer::append`] and
    /// [`StyledBuffer::replace`].
    current_tag: CellTag,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StyledChar {
    ch: char,
    style: ElementStyle,
    meta: CellMeta,
}

/// Provenance attached to a single rendered cell
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct CellMeta {
    /// Byte range of the source text this cell was rendered from, if any
    pub(crate) source: Option<CellSource>,
    pub(crate) tag: CellTag,
}

impl CellMeta {
    pub(crate) const NONE: Self = Self {
        source: None,
        tag: CellTag::None,
    };
    pub(crate) const FOLD: Self = Self {
        source: None,
        tag: CellTag::Fold,
    };

    pub(crate) const fn source(source: CellSource) -> Self {
        Self {
            source: Some(source),
            tag: CellTag::None,
        }
    }
}

/// Byte range within one of the [`StyledBuffer`]'s registered sources
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CellSource {
    pub(crate) source_id: usize,
    pub(crate) byte_start: usize,
    pub(crate) byte_end: usize,
}

/// Semantic role of a cell that cannot be derived from its [`ElementStyle`]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) enum CellTag {
    #[default]
    None,
    /// An elision marker such as `...` standing in for hidden source text
    Fold,
    /// A vertical bar of a multiline annotation, at the given nesting depth
    Sidebar { depth: usize },
    /// Content of a machine-applicable suggestion block
    Suggestion,
}

impl StyledChar {
    pub(crate) const SPACE: Self = Self::new(' ', ElementStyle::NoStyle);

    pub(crate) const fn new(ch: char, style: ElementStyle) -> Self {
        Self {
            ch,
            style,
            meta: CellMeta::NONE,
        }
    }

    pub(crate) const fn with_meta(ch: char, style: ElementStyle, meta: CellMeta) -> Self {
        Self { ch, style, meta }
    }

    pub(crate) fn ch(&self) -> char {
        self.ch
    }

    pub(crate) fn style(&self) -> ElementStyle {
        self.style
    }

    pub(crate) fn meta(&self) -> CellMeta {
        self.meta
    }
}

impl StyledBuffer {
    pub(crate) fn new() -> Self {
        Self {
            lines: vec![],
            sources: vec![],
            current_tag: CellTag::None,
        }
    }

    fn ensure_lines(&mut self, line: usize) {
        if line >= self.lines.len() {
            self.lines.resize(line + 1, Vec::new());
        }
    }

    /// Register a source (identified by its origin path, if any) and return
    /// the id that [`CellSource::source_id`] refers to.
    pub(crate) fn intern_source(&mut self, origin: Option<&str>) -> usize {
        let origin = origin.map(String::from);
        if let Some(id) = self.sources.iter().position(|o| o == &origin) {
            id
        } else {
            self.sources.push(origin);
            self.sources.len() - 1
        }
    }

    /// The origin path of a source registered with [`StyledBuffer::intern_source`]
    pub(crate) fn source_origin(&self, source_id: usize) -> Option<&Option<String>> {
        self.sources.get(source_id)
    }

    /// Set the [`CellTag`] stamped on subsequently written cells, returning
    /// the previous one
    pub(crate) fn set_tag(&mut self, tag: CellTag) -> CellTag {
        core::mem::replace(&mut self.current_tag, tag)
    }

    /// Iterate over the rendered lines of cells
    pub(crate) fn lines(&self) -> impl Iterator<Item = &[StyledChar]> {
        self.lines.iter().map(Vec::as_slice)
    }

    pub(crate) fn render(
        &self,
        level: &Level<'_>,
        stylesheet: &Stylesheet,
        str: &mut String,
    ) -> Result<(), fmt::Error> {
        let capacity = self.lines.iter().map(|line| line.len()).sum();
        str.reserve(capacity);

        for (i, line) in self.lines.iter().enumerate() {
            let mut current_style = stylesheet.none;
            for StyledChar { ch, style, .. } in line {
                let ch_style = style.color_spec(level, stylesheet);
                if ch_style != current_style {
                    if !line.is_empty() {
                        write!(str, "{current_style:#}")?;
                    }
                    current_style = ch_style;
                    write!(str, "{current_style}")?;
                }
                str.push(*ch);
            }
            write!(str, "{current_style:#}")?;
            if i != self.lines.len() - 1 {
                str.push('\n');
            }
        }
        Ok(())
    }

    /// Sets `chr` with `style` for given `line`, `col`.
    /// If `line` does not exist in our buffer, adds empty lines up to the given
    /// and fills the last line with unstyled whitespace.
    pub(crate) fn putc(&mut self, line: usize, col: usize, chr: char, style: ElementStyle) {
        let meta = CellMeta {
            source: None,
            tag: self.current_tag,
        };
        self.putc_meta(line, col, chr, style, meta);
    }

    /// Like [`StyledBuffer::putc`], but with explicit cell provenance
    pub(crate) fn putc_meta(
        &mut self,
        line: usize,
        col: usize,
        chr: char,
        style: ElementStyle,
        meta: CellMeta,
    ) {
        self.ensure_lines(line);
        if col >= self.lines[line].len() {
            self.lines[line].resize(col + 1, StyledChar::SPACE);
        }
        self.lines[line][col] = StyledChar::with_meta(chr, style, meta);
    }

    /// Sets `string` with `style` for given `line`, starting from `col`.
    /// If `line` does not exist in our buffer, adds empty lines up to the given
    /// and fills the last line with unstyled whitespace.
    pub(crate) fn puts(&mut self, line: usize, col: usize, string: &str, style: ElementStyle) {
        let meta = CellMeta {
            source: None,
            tag: self.current_tag,
        };
        self.puts_meta(line, col, string, style, meta);
    }

    /// Like [`StyledBuffer::puts`], but with explicit cell provenance
    pub(crate) fn puts_meta(
        &mut self,
        line: usize,
        col: usize,
        string: &str,
        style: ElementStyle,
        meta: CellMeta,
    ) {
        if string.is_empty() {
            // don't add trailing whitespace (from column offset) for blank strings
            return;
        }

        self.ensure_lines(line);
        let line = &mut self.lines[line];

        let new_len = col + string.chars().count();
        if new_len > line.len() {
            line.resize(new_len, StyledChar::SPACE);
        }

        for (offset, chr) in string.chars().enumerate() {
            let col = col + offset;
            line[col] = StyledChar::with_meta(chr, style, meta);
        }
    }

    /// For given `line` inserts `string` with `style` after old content of that line,
    /// adding lines if needed
    pub(crate) fn append(&mut self, line: usize, string: &str, style: ElementStyle) {
        if line >= self.lines.len() {
            self.puts(line, 0, string, style);
        } else {
            let col = self.lines[line].len();
            self.puts(line, col, string, style);
        }
    }

    pub(crate) fn replace(&mut self, line: usize, start: usize, end: usize, string: &str) {
        if start == end {
            return;
        }
        // If the replacement range would be out of bounds, do nothing, as we
        // can't replace things that don't exist.
        if start > self.lines[line].len() || end > self.lines[line].len() {
            return;
        };
        let meta = CellMeta {
            source: None,
            tag: self.current_tag,
        };
        self.lines[line].splice(
            start..end,
            string
                .chars()
                .map(|c| StyledChar::with_meta(c, ElementStyle::LineNumber, meta)),
        );
    }

    pub(crate) fn num_lines(&self) -> usize {
        self.lines.len()
    }

    /// Set `style` for `line`, `col_start..col_end` range if:
    /// 1. That line and column range exist in `StyledBuffer`
    /// 2. `overwrite` is `true` or existing style is `Style::NoStyle` or `Style::Quotation`
    pub(crate) fn set_style_range(
        &mut self,
        line: usize,
        col_start: usize,
        col_end: usize,
        style: ElementStyle,
        overwrite: bool,
    ) {
        for col in col_start..col_end {
            self.set_style(line, col, style, overwrite);
        }
    }

    /// Set `style` for `line`, `col` if:
    /// 1. That line and column exist in `StyledBuffer`
    /// 2. `overwrite` is `true` or existing style is `Style::NoStyle` or `Style::Quotation`
    pub(crate) fn set_style(
        &mut self,
        line: usize,
        col: usize,
        style: ElementStyle,
        overwrite: bool,
    ) {
        if let Some(ref mut line) = self.lines.get_mut(line)
            && let Some(StyledChar { style: s, .. }) = line.get_mut(col)
            && (overwrite || matches!(s, ElementStyle::NoStyle | ElementStyle::Quotation))
        {
            *s = style;
        }
    }
}
