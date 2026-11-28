//! Exact UTF-8 byte offset <-> LSP UTF-16 `(line, character)` conversion.

/// Zero-based LSP position using UTF-16 code units for `character`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

impl Position {
    /// Construct a new [`Position`].
    #[must_use]
    pub const fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }
}

/// Zero-based LSP half-open range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    /// Construct a new [`Range`].
    #[must_use]
    pub const fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }
}

/// Precomputed line-start byte offsets for O(log L) UTF-8 <-> UTF-16 conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    /// Build a [`LineIndex`] for `text`.
    #[must_use]
    pub fn new(text: &str) -> Self {
        let mut line_starts = vec![0];
        for (idx, &b) in text.as_bytes().iter().enumerate() {
            if b == b'\n' {
                line_starts.push(idx + 1);
            }
        }
        Self { line_starts }
    }

    /// Return the number of lines in the indexed text.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Convert a UTF-8 byte `offset` in `text` to an LSP UTF-16 [`Position`].
    ///
    /// Out-of-bounds offsets are clamped to `text.len()`, and mid-codepoint
    /// offsets are floored to the nearest UTF-8 character boundary.
    #[must_use]
    pub fn offset_to_position(&self, text: &str, offset: usize) -> Position {
        let clamped = text.floor_char_boundary(offset.min(text.len()));
        let line = match self.line_starts.binary_search(&clamped) {
            Ok(l) => l,
            Err(l) => l.saturating_sub(1),
        };
        let col_start = self.line_starts[line];
        let line_end = self.line_end(text, line);
        let col_end = clamped.min(line_end);
        let character = text[col_start..col_end].encode_utf16().count() as u32;
        Position {
            line: line as u32,
            character,
        }
    }

    /// Convert an LSP UTF-16 [`Position`] in `text` to a UTF-8 byte offset.
    ///
    /// Out-of-bounds lines clamp to `text.len()`, out-of-bounds characters
    /// clamp to the line's end (excluding `\r`/`\n`), and positions inside a
    /// surrogate pair floor to the character's starting byte offset.
    #[must_use]
    pub fn position_to_offset(&self, text: &str, pos: Position) -> usize {
        let line = pos.line as usize;
        if line >= self.line_starts.len() {
            return text.len();
        }
        let start = self.line_starts[line];
        let end = self.line_end(text, line);
        let target_units = pos.character as usize;
        let mut units = 0;
        for (idx, ch) in text[start..end].char_indices() {
            let ch_units = ch.len_utf16();
            if units + ch_units > target_units {
                return start + idx;
            }
            units += ch_units;
        }
        end
    }

    /// Convert an LSP [`Range`] into a clamped, non-inverted UTF-8 byte range.
    #[must_use]
    pub fn range_to_offsets(&self, text: &str, range: Range) -> std::ops::Range<usize> {
        let start = self.position_to_offset(text, range.start);
        let end = self.position_to_offset(text, range.end).max(start);
        start..end
    }

    /// Convert a UTF-8 byte range into an LSP [`Range`].
    #[must_use]
    pub fn offsets_to_range(&self, text: &str, range: std::ops::Range<usize>) -> Range {
        let start = self.offset_to_position(text, range.start);
        let end = self.offset_to_position(text, range.end.max(range.start));
        Range { start, end }
    }

    fn line_end(&self, text: &str, line: usize) -> usize {
        let Some(&next_start) = self.line_starts.get(line + 1) else {
            return text.len();
        };
        let newline = next_start - 1;
        newline - usize::from(newline > self.line_starts[line] && text.as_bytes()[newline - 1] == b'\r')
    }
}
