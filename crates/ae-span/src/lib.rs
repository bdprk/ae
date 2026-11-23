//! Source positions and structured diagnostics shared by compiler stages.
use std::fmt;
use std::ops::Range;

/// Byte range within one source file. Both ends must be UTF-8 boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub file: Option<String>,
    pub range: Range<usize>,
    pub line: usize,
    pub column: usize,
}

/// A source view which validates byte offsets before calculating positions.
pub struct SourceFile<'a> {
    pub name: Option<&'a str>,
    pub text: &'a str,
}
impl SourceFile<'_> {
    pub fn span(&self, range: Range<usize>) -> Option<Span> {
        if range.start > range.end || self.text.get(range.clone()).is_none() { return None; }
        let prefix = &self.text[..range.start];
        let line_start = prefix.as_bytes().iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let line_prefix = &prefix[line_start..];
        let column = if line_prefix.is_ascii() { line_prefix.len() } else { line_prefix.chars().count() } + 1;
        Some(Span {
            file: self.name.filter(|name| !name.is_empty()).map(str::to_owned),
            range,
            line: prefix.bytes().filter(|b| *b == b'\n').count() + 1,
            column,
        })
    }
}

/// Precomputed line-start byte offsets for O(log L) line/column span lookups
/// when constructing many spans against the same source text.
pub struct LineIndex<'a> {
    pub name: Option<&'a str>,
    pub text: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    pub fn new(name: Option<&'a str>, text: &'a str) -> Self {
        let mut line_starts = vec![0];
        for (i, &b) in text.as_bytes().iter().enumerate() {
            if b == b'\n' { line_starts.push(i + 1); }
        }
        Self { name, text, line_starts }
    }

    pub fn span(&self, range: Range<usize>) -> Option<Span> {
        if range.start > range.end || self.text.get(range.clone()).is_none() { return None; }
        let line_idx = self.line_starts.partition_point(|&start| start <= range.start) - 1;
        let line_prefix = &self.text[self.line_starts[line_idx]..range.start];
        let column = if line_prefix.is_ascii() { line_prefix.len() } else { line_prefix.chars().count() } + 1;
        Some(Span {
            file: self.name.filter(|name| !name.is_empty()).map(str::to_owned),
            range,
            line: line_idx + 1,
            column,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub span: Span,
    pub message: Option<String>,
}

/// Codes, secondary labels and suggestions are optional while the existing
/// fail-fast frontend is migrated. Missing fields are never invented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: Option<String>,
    pub message: String,
    pub primary: Option<Label>,
    pub secondary: Vec<Label>,
    pub suggestions: Vec<String>,
}
impl Diagnostic {
    pub fn at(span: Span, message: impl Into<String>) -> Self {
        Self { code: None, message: message.into(), primary: Some(Label { span, message: None }), secondary: Vec::new(), suggestions: Vec::new() }
    }
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestions.push(suggestion.into());
        self
    }
    pub fn with_secondary(mut self, span: Span, message: impl Into<String>) -> Self {
        self.secondary.push(Label { span, message: Some(message.into()) });
        self
    }
}
impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(label) = &self.primary {
            if let Some(file) = &label.span.file { write!(f, "{file}:")?; }
            write!(f, "{}:{}: ", label.span.line, label.span.column)?;
        }
        f.write_str(&self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    // The reversed range is the point of the last assertion: a caller that
    // hands span() an inverted range must get None, not a panic.
    #[allow(clippy::reversed_empty_ranges)]
    fn positions_are_unicode_aware_and_ranges_are_checked() {
        let source = SourceFile { name: Some("test.ae"), text: "中a\r\n😀x" };
        let span = source.span(10..11).unwrap();
        assert_eq!((span.line, span.column), (2, 2));
        assert_eq!(Diagnostic::at(span, "problem").to_string(), "test.ae:2:2: problem");
        assert!(source.span(1..1).is_none());
        assert!(source.span(0..99).is_none());
        assert!(source.span(4..3).is_none());
    }
    #[test]
    fn empty_and_end_of_file_locations_work() {
        let source = SourceFile { name: None, text: "a\n" };
        let span = source.span(2..2).unwrap();
        assert_eq!((span.line, span.column), (2, 1));
        assert_eq!(Diagnostic::at(span, "end").to_string(), "2:1: end");
    }
    #[test]
    fn line_index_matches_source_file_span() {
        let text = "fn main() {\n    let s = \"中文\";\r\n    😀\n}\n";
        let sf = SourceFile { name: Some("main.ae"), text };
        let idx = LineIndex::new(Some("main.ae"), text);
        for pos in 0..=text.len() {
            assert_eq!(idx.span(pos..pos), sf.span(pos..pos));
        }
        assert!(idx.span(0..999).is_none());
    }
}
