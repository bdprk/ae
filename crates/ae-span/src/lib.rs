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
        Some(Span {
            file: self.name.filter(|name| !name.is_empty()).map(str::to_owned),
            range,
            line: prefix.bytes().filter(|b| *b == b'\n').count() + 1,
            column: prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1,
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
}
