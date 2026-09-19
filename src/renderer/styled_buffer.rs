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
}

/// Where a rendered character comes from within the original source text.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SourcePoint {
    /// Index into the render's source table.
    pub(crate) source: u16,
    /// 1-based line number within the source.
    pub(crate) line: u32,
    /// Byte offset of the source character this glyph derives from.
    pub(crate) byte: u32,
    /// Byte offset one past the source character this glyph derives from.
    pub(crate) byte_end: u32,
    /// Display column of the source character within its source line.
    pub(crate) display: u32,
    /// Display column one past the source character within its source line.
    pub(crate) display_end: u32,
}

/// The decorative role of a rendered character, if any.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) enum Decor {
    /// Regular content; classify by style instead.
    #[default]
    None,
    /// A line number in the gutter.
    LineNumber,
    /// A vertical sidebar bar at the given multiline-annotation depth
    /// (`0` is the main gutter).
    Sidebar(u16),
    /// An elision marker for folded-away content (`...`, `...`, `┆`).
    Fold,
    /// Other structural separators (arrows, note separators, ...).
    Separator,
}

/// Extra per-character information used to produce structured events.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct CharMeta {
    pub(crate) source: Option<SourcePoint>,
    pub(crate) decor: Decor,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StyledChar {
    pub(crate) ch: char,
    pub(crate) style: ElementStyle,
    pub(crate) meta: CharMeta,
}

impl StyledChar {
    pub(crate) const SPACE: Self = Self::new(' ', ElementStyle::NoStyle);

    pub(crate) const fn new(ch: char, style: ElementStyle) -> Self {
        Self {
            ch,
            style,
            meta: CharMeta {
                source: None,
                decor: Decor::None,
            },
        }
    }
}

impl StyledBuffer {
    pub(crate) fn new() -> Self {
        Self { lines: vec![] }
    }

    fn ensure_lines(&mut self, line: usize) {
        if line >= self.lines.len() {
            self.lines.resize(line + 1, Vec::new());
        }
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
        self.putc_meta(line, col, chr, style, CharMeta::default());
    }

    /// Like [`Self::putc`], additionally recording structured-rendering metadata.
    pub(crate) fn putc_meta(
        &mut self,
        line: usize,
        col: usize,
        chr: char,
        style: ElementStyle,
        meta: CharMeta,
    ) {
        self.ensure_lines(line);
        if col >= self.lines[line].len() {
            self.lines[line].resize(col + 1, StyledChar::SPACE);
        }
        self.lines[line][col] = StyledChar { ch: chr, style, meta };
    }

    /// Sets `string` with `style` for given `line`, starting from `col`.
    /// If `line` does not exist in our buffer, adds empty lines up to the given
    /// and fills the last line with unstyled whitespace.
    pub(crate) fn puts(&mut self, line: usize, col: usize, string: &str, style: ElementStyle) {
        self.puts_with(line, col, string, style, |_| CharMeta::default());
    }

    /// Like [`Self::puts`], marking every character with the same decorative role.
    pub(crate) fn puts_decor(
        &mut self,
        line: usize,
        col: usize,
        string: &str,
        style: ElementStyle,
        decor: Decor,
    ) {
        self.puts_with(line, col, string, style, |_| CharMeta {
            source: None,
            decor,
        });
    }

    /// Like [`Self::puts`], recording the source provenance of each character.
    ///
    /// `provenance` must have exactly one entry per `char` of `string`.
    pub(crate) fn puts_src(
        &mut self,
        line: usize,
        col: usize,
        string: &str,
        style: ElementStyle,
        provenance: &[SourcePoint],
    ) {
        debug_assert_eq!(provenance.len(), string.chars().count());
        let mut provenance = provenance.iter().copied();
        self.puts_with(line, col, string, style, |_| CharMeta {
            source: provenance.next(),
            decor: Decor::None,
        });
    }

    fn puts_with(
        &mut self,
        line: usize,
        col: usize,
        string: &str,
        style: ElementStyle,
        mut meta: impl FnMut(usize) -> CharMeta,
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
            line[col] = StyledChar {
                ch: chr,
                style,
                meta: meta(offset),
            };
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

    /// Like [`Self::append`], marking every character with the same decorative role.
    pub(crate) fn append_decor(
        &mut self,
        line: usize,
        string: &str,
        style: ElementStyle,
        decor: Decor,
    ) {
        if line >= self.lines.len() {
            self.puts_decor(line, 0, string, style, decor);
        } else {
            let col = self.lines[line].len();
            self.puts_decor(line, col, string, style, decor);
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
        self.lines[line].splice(
            start..end,
            string.chars().map(|c| StyledChar {
                ch: c,
                style: ElementStyle::LineNumber,
                meta: CharMeta {
                    source: None,
                    decor: Decor::Fold,
                },
            }),
        );
    }

    pub(crate) fn num_lines(&self) -> usize {
        self.lines.len()
    }

    /// The rendered lines, for structured event emission.
    pub(crate) fn lines(&self) -> &[Vec<StyledChar>] {
        &self.lines
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
