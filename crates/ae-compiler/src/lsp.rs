use std::cell::OnceCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, BufRead, Write};
use std::rc::Rc;

#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(BTreeMap<String, JsonValue>),
}

impl JsonValue {
    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            JsonValue::Object(map) => map.get(key),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsonValue::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            JsonValue::Number(n) => Some(*n as i64),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            JsonValue::Array(arr) => Some(arr),
            _ => None,
        }
    }

    pub fn parse(s: &str) -> Option<JsonValue> {
        let mut chars = s.trim().char_indices().peekable();
        Self::parse_value(&mut chars, s)
    }

    fn parse_value(
        chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
        s: &str,
    ) -> Option<JsonValue> {
        Self::skip_whitespace(chars);
        let (_, ch) = *chars.peek()?;
        match ch {
            'n' => {
                Self::consume_str(chars, "null")?;
                Some(JsonValue::Null)
            }
            't' => {
                Self::consume_str(chars, "true")?;
                Some(JsonValue::Bool(true))
            }
            'f' => {
                Self::consume_str(chars, "false")?;
                Some(JsonValue::Bool(false))
            }
            '"' => Self::parse_string(chars, s).map(JsonValue::String),
            '[' => Self::parse_array(chars, s).map(JsonValue::Array),
            '{' => Self::parse_object(chars, s).map(JsonValue::Object),
            '-' | '0'..='9' => Self::parse_number(chars, s).map(JsonValue::Number),
            _ => None,
        }
    }

    fn skip_whitespace(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>) {
        while let Some((_, ch)) = chars.peek() {
            if ch.is_whitespace() {
                chars.next();
            } else {
                break;
            }
        }
    }

    fn consume_str(
        chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
        expected: &str,
    ) -> Option<()> {
        for exp in expected.chars() {
            let (_, ch) = chars.next()?;
            if ch != exp {
                return None;
            }
        }
        Some(())
    }

    fn parse_string(
        chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
        _s: &str,
    ) -> Option<String> {
        chars.next();
        let mut out = String::new();
        while let Some((_, ch)) = chars.next() {
            match ch {
                '"' => return Some(out),
                '\\' => {
                    let (_, esc) = chars.next()?;
                    match esc {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        '/' => out.push('/'),
                        'b' => out.push('\x08'),
                        'f' => out.push('\x0c'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'u' => {
                            let mut hex = String::new();
                            for _ in 0..4 {
                                hex.push(chars.next()?.1);
                            }
                            let codepoint = u32::from_str_radix(&hex, 16).ok()?;
                            out.push(char::from_u32(codepoint)?);
                        }
                        _ => return None,
                    }
                }
                c => out.push(c),
            }
        }
        None
    }

    fn parse_number(
        chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
        s: &str,
    ) -> Option<f64> {
        let (start, _) = *chars.peek()?;
        let mut end = start;
        while let Some((idx, ch)) = chars.peek() {
            if matches!(ch, '0'..='9' | '-' | '+' | '.' | 'e' | 'E') {
                end = *idx + ch.len_utf8();
                chars.next();
            } else {
                break;
            }
        }
        s[start..end].parse::<f64>().ok()
    }

    fn parse_array(
        chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
        s: &str,
    ) -> Option<Vec<JsonValue>> {
        chars.next();
        let mut elements = Vec::new();
        Self::skip_whitespace(chars);
        if let Some((_, ']')) = chars.peek() {
            chars.next();
            return Some(elements);
        }
        loop {
            let val = Self::parse_value(chars, s)?;
            elements.push(val);
            Self::skip_whitespace(chars);
            match chars.next() {
                Some((_, ']')) => break,
                Some((_, ',')) => Self::skip_whitespace(chars),
                _ => return None,
            }
        }
        Some(elements)
    }

    fn parse_object(
        chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
        s: &str,
    ) -> Option<BTreeMap<String, JsonValue>> {
        chars.next();
        let mut map = BTreeMap::new();
        Self::skip_whitespace(chars);
        if let Some((_, '}')) = chars.peek() {
            chars.next();
            return Some(map);
        }
        loop {
            Self::skip_whitespace(chars);
            let key = Self::parse_string(chars, s)?;
            Self::skip_whitespace(chars);
            if chars.next()?.1 != ':' {
                return None;
            }
            let val = Self::parse_value(chars, s)?;
            map.insert(key, val);
            Self::skip_whitespace(chars);
            match chars.next() {
                Some((_, '}')) => break,
                Some((_, ',')) => Self::skip_whitespace(chars),
                _ => return None,
            }
        }
        Some(map)
    }

    pub fn serialize(&self) -> String {
        match self {
            JsonValue::Null => "null".to_owned(),
            JsonValue::Bool(b) => if *b { "true".to_owned() } else { "false".to_owned() },
            JsonValue::Number(n) => {
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    format!("{}", *n as i64)
                } else {
                    format!("{n}")
                }
            }
            JsonValue::String(s) => format!("\"{}\"", escape_json_str(s)),
            JsonValue::Array(arr) => {
                let items = arr.iter().map(|item| item.serialize()).collect::<Vec<_>>().join(",");
                format!("[{items}]")
            }
            JsonValue::Object(map) => {
                let kvs = map
                    .iter()
                    .map(|(k, v)| format!("\"{}\":{}", escape_json_str(k), v.serialize()))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{{{kvs}}}")
            }
        }
    }
}

fn escape_json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub uri: String,
    pub range: Range,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: u32,
    pub code: Option<String>,
    pub message: String,
}

pub struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut line_starts = vec![0];
        for (idx, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(idx + 1);
            }
        }
        Self { line_starts }
    }

    pub fn offset_to_position(&self, text: &str, offset: usize) -> Position {
        let offset = text.floor_char_boundary(offset.min(text.len()));
        let line = match self.line_starts.binary_search(&offset) {
            Ok(l) => l,
            Err(l) => l.saturating_sub(1),
        };
        let col_start = self.line_starts[line];
        let character = text[col_start..offset.min(self.line_end(text, line))].encode_utf16().count() as u32;
        Position {
            line: line as u32,
            character,
        }
    }

    pub fn position_to_offset(&self, text: &str, pos: Position) -> usize {
        let line = pos.line as usize;
        if line >= self.line_starts.len() {
            return text.len();
        }
        let start = self.line_starts[line];
        let end = self.line_end(text, line);
        let mut units = 0;
        for (idx, ch) in text[start..end].char_indices() {
            units += ch.len_utf16();
            if units > pos.character as usize {
                return start + idx;
            }
        }
        end
    }

    fn line_end(&self, text: &str, line: usize) -> usize {
        let Some(&next_start) = self.line_starts.get(line + 1) else {
            return text.len();
        };
        let newline = next_start - 1;
        newline - usize::from(newline > 0 && text.as_bytes()[newline - 1] == b'\r')
    }
}

pub struct Document {
    pub uri: String,
    pub version: i64,
    pub text: String,
    pub line_index: LineIndex,
    /// The resolver's name resolution for this document's text, computed
    /// lazily and invalidated on every update (R3d: hover and definition
    /// read the def table instead of matching text). The imports resolve
    /// from disk next to the document, so imported defs carry their own
    /// file's name for cross-file definition.
    pub resolution: OnceCell<std::sync::Arc<(ae_resolve::module::Module, Vec<(String, ae_resolve::module::Module)>, ae_resolve::resolve::Resolution)>>,
}

impl Document {
    pub fn new(uri: String, version: i64, text: String) -> Self {
        let line_index = LineIndex::new(&text);
        Self {
            uri,
            version,
            text,
            line_index,
            resolution: OnceCell::new(),
        }
    }

    pub fn update(&mut self, version: i64, text: String) {
        self.version = version;
        self.line_index = LineIndex::new(&text);
        self.resolution = OnceCell::new();
        self.text = text;
    }

    /// The document's module, disk-loaded imports and name resolution,
    /// computed on first use from its text. The imports come from disk next
    /// to the document (the use headers' modules), so imported defs carry
    /// their own file's name. Documents the shared module parse rejects
    /// (lex or declaration diagnostics) have none: their features fall
    /// back to the text heuristics.
    pub fn resolution(&self) -> Option<&std::sync::Arc<(ae_resolve::module::Module, Vec<(String, ae_resolve::module::Module)>, ae_resolve::resolve::Resolution)>> {
        if let Some(cached) = self.resolution.get() {
            return Some(cached);
        }
        let tokens = Rc::new(ae_syntax::lex_diagnostic(&self.text).ok()?);
        let module = ae_resolve::module::parse_module_tokens(&self.uri, &self.text, tokens.as_slice()).ok()?;
        let imports = self.disk_imports();
        let resolution = ae_resolve::resolve::resolve_with_tokens(&module, &imports, tokens.as_slice());
        self.resolution.set(std::sync::Arc::new((module, imports, resolution))).ok()?;
        self.resolution.get()
    }

    /// The use headers' modules, loaded from disk next to this document.
    /// A document that is not a disk file (unsaved buffer) resolves
    /// single-file.
    fn disk_imports(&self) -> Vec<(String, ae_resolve::module::Module)> {
        let Some(path) = self.disk_path() else { return Vec::new() };
        match ae_resolve::imports::load_module_transitive(&path) {
            Ok((_, imports)) => imports,
            Err(_) => Vec::new(),
        }
    }

    /// The disk path behind a `file:///` document URI, if any.
    fn disk_path(&self) -> Option<std::path::PathBuf> {
        let rest = self.uri.strip_prefix("file:///")?;
        Some(std::path::PathBuf::from(rest))
    }
}

pub struct LspServer {
    documents: HashMap<String, Document>,
    cancelled_requests: HashSet<i64>,
}

/// The occurrence covering `offset`, with its resolved def. Occurrences in
/// closure bodies are included (the resolver walked them), so cursors inside
/// closures resolve too.
fn occurrence_at(
    resolution: &ae_resolve::resolve::Resolution,
    offset: usize,
) -> Option<(&ae_resolve::Occurrence, &ae_resolve::DefEntry)> {
    let occurrence = resolution.functions.iter().flat_map(|function| &function.occurrences)
        .find(|occurrence| occurrence.range.start <= offset && offset < occurrence.range.end)?;
    let def = resolution.defs.get(occurrence.target?.0 as usize)?;
    Some((occurrence, def))
}

/// The def under `offset`: an occurrence's target when the cursor reads or
/// writes a name, otherwise the def whose own declaration name the cursor
/// sits on (declarations have no occurrence of their own). An occurrence
/// wins, because it disambiguates shadowed names.
fn def_at(
    resolution: &ae_resolve::resolve::Resolution,
    offset: usize,
) -> Option<&ae_resolve::DefEntry> {
    if let Some((_, def)) = occurrence_at(resolution, offset) {
        return Some(def);
    }
    resolution.defs.iter().find(|def| {
        !def.imported && def.range.start <= offset && offset < def.range.end
    })
}

/// The display form of a spelling, with named defs rendered as their item
/// names (the bare `describe()` prints def ids, which are table internals).
fn describe_spelling(
    spelling: &ae_resolve::TypeSpelling,
    resolution: &ae_resolve::resolve::Resolution,
) -> String {
    match spelling {
        ae_resolve::TypeSpelling::Named { def } => {
            resolution.defs.get(def.0 as usize).map(|entry| entry.name.clone()).unwrap_or_else(|| "unknown".to_owned())
        }
        ae_resolve::TypeSpelling::Borrow { mutable, inner } => {
            format!("&{}{}", if *mutable { "mut " } else { "" }, describe_spelling(inner, resolution))
        }
        ae_resolve::TypeSpelling::Pointer { mutable, pointee } => {
            format!("*{}{}", if *mutable { "mut " } else { "" }, describe_spelling(pointee, resolution))
        }
        other => other.describe(),
    }
}

/// The LSP SymbolKind number for a def kind (12 function, 23 struct,
/// 10 enum, 11 trait, 2 module-ish impl, 13 variable fallback).
fn symbol_kind_number(kind: ae_resolve::DefKind) -> u32 {
    match kind {
        ae_resolve::DefKind::Function => 12,
        ae_resolve::DefKind::Struct => 23,
        ae_resolve::DefKind::Enum => 10,
        ae_resolve::DefKind::Trait => 11,
        ae_resolve::DefKind::Impl => 2,
        _ => 13,
    }
}

/// An imported function's declared signature, sliced from its own module's
/// source (`fn name(params) -> ret`) — the entry-side def table has no
/// parameter spellings for imported items.
fn imported_fn_signature(
    imports: &[(String, ae_resolve::module::Module)],
    def: &ae_resolve::DefEntry,
) -> Option<String> {
    let (_, import_module) = imports.iter()
        .find(|(_, module)| module.declarations.iter().any(|declaration| declaration.name == def.name))?;
    let declaration = import_module.declarations.iter().find(|declaration| declaration.name == def.name)?;
    let body_start = declaration.body.as_ref().map(|body| body.start)?;
    let signature = import_module.source.get(declaration.range.start..body_start)?.trim().to_owned();
    Some(format!("fn {}", signature))
}

fn kind_label(kind: ae_resolve::DefKind) -> &'static str {
    match kind {
        ae_resolve::DefKind::Function => "fn",
        ae_resolve::DefKind::Struct => "struct",
        ae_resolve::DefKind::Enum => "enum",
        ae_resolve::DefKind::Variant => "variant",
        ae_resolve::DefKind::Trait => "trait",
        ae_resolve::DefKind::Impl => "impl",
        ae_resolve::DefKind::Parameter => "parameter",
        ae_resolve::DefKind::ClosureParameter => "closure parameter",
        ae_resolve::DefKind::Binding => "let",
        ae_resolve::DefKind::ForBinding => "for binding",
        ae_resolve::DefKind::PatternBinding => "pattern binding",
    }
}

impl Default for LspServer {
    fn default() -> Self {
        Self::new()
    }
}

impl LspServer {
    pub fn new() -> Self {
        Self {
            documents: HashMap::new(),
            cancelled_requests: HashSet::new(),
        }
    }

    pub fn handle_message(&mut self, raw: &str) -> Vec<String> {
        let Some(json) = JsonValue::parse(raw) else {
            return vec![Self::make_error_response(None, -32700, "Parse error")];
        };

        let method = json.get("method").and_then(|m| m.as_str());
        let id = json.get("id").and_then(|v| v.as_i64());

        if let Some(method) = method {
            if let Some(id) = id {
                if self.cancelled_requests.remove(&id) {
                    return vec![Self::make_error_response(Some(id), -32800, "Request cancelled")];
                }
                vec![self.dispatch_request(id, method, json.get("params"))]
            } else {
                self.dispatch_notification(method, json.get("params"))
            }
        } else {
            Vec::new()
        }
    }

    fn dispatch_request(&mut self, id: i64, method: &str, params: Option<&JsonValue>) -> String {
        match method {
            "initialize" => {
                let mut caps = BTreeMap::new();
                caps.insert("textDocumentSync".to_owned(), JsonValue::Number(1.0));
                caps.insert("positionEncoding".to_owned(), JsonValue::String("utf-16".to_owned()));
                caps.insert("hoverProvider".to_owned(), JsonValue::Bool(true));
                caps.insert("definitionProvider".to_owned(), JsonValue::Bool(true));

                let mut comp = BTreeMap::new();
                let triggers = vec![JsonValue::String(".".to_owned()), JsonValue::String(":".to_owned())];
                comp.insert("triggerCharacters".to_owned(), JsonValue::Array(triggers));
                caps.insert("completionProvider".to_owned(), JsonValue::Object(comp));
                caps.insert("renameProvider".to_owned(), JsonValue::Bool(true));
                caps.insert("documentSymbolProvider".to_owned(), JsonValue::Bool(true));
                caps.insert("signatureHelpProvider".to_owned(), JsonValue::Bool(true));
                caps.insert("foldingRangeProvider".to_owned(), JsonValue::Bool(true));
                caps.insert("codeActionProvider".to_owned(), JsonValue::Bool(true));
                caps.insert("inlayHintProvider".to_owned(), JsonValue::Bool(true));

                let mut res = BTreeMap::new();
                res.insert("capabilities".to_owned(), JsonValue::Object(caps));
                Self::make_response(id, JsonValue::Object(res))
            }
            "shutdown" => Self::make_response(id, JsonValue::Null),
            "textDocument/hover" => {
                let Some(params) = params else {
                    return Self::make_response(id, JsonValue::Null);
                };
                let uri = params.get("textDocument").and_then(|t| t.get("uri")).and_then(|u| u.as_str()).unwrap_or("");
                let line = params.get("position").and_then(|p| p.get("line")).and_then(|v| v.as_i64()).unwrap_or(0) as u32;
                let character = params.get("position").and_then(|p| p.get("character")).and_then(|v| v.as_i64()).unwrap_or(0) as u32;
                let hover_info = self.hover(uri, Position { line, character });
                match hover_info {
                    Some(info) => {
                        let mut contents = BTreeMap::new();
                        contents.insert("kind".to_owned(), JsonValue::String("markdown".to_owned()));
                        contents.insert("value".to_owned(), JsonValue::String(info));
                        let mut res = BTreeMap::new();
                        res.insert("contents".to_owned(), JsonValue::Object(contents));
                        Self::make_response(id, JsonValue::Object(res))
                    }
                    None => Self::make_response(id, JsonValue::Null),
                }
            }
            "textDocument/documentSymbol" => {
                let Some(params) = params else {
                    return Self::make_response(id, JsonValue::Null);
                };
                let uri = params.get("textDocument").and_then(|t| t.get("uri")).and_then(|u| u.as_str()).unwrap_or("").to_owned();
                let symbols = self.document_symbols(&uri);
                let items: Vec<JsonValue> = symbols.into_iter().map(|(name, kind, range)| {
                    let mut location = BTreeMap::new();
                    location.insert("uri".to_owned(), JsonValue::String(uri.clone()));
                    location.insert("range".to_owned(), Self::range_to_json(&range));
                    let mut symbol = BTreeMap::new();
                    symbol.insert("name".to_owned(), JsonValue::String(name));
                    symbol.insert("kind".to_owned(), JsonValue::Number(kind as f64));
                    symbol.insert("location".to_owned(), JsonValue::Object(location));
                    JsonValue::Object(symbol)
                }).collect();
                Self::make_response(id, JsonValue::Array(items))
            }
            "textDocument/references" => {
                let Some(params) = params else {
                    return Self::make_response(id, JsonValue::Null);
                };
                let uri = params.get("textDocument").and_then(|t| t.get("uri")).and_then(|u| u.as_str()).unwrap_or("").to_owned();
                let line = params.get("position").and_then(|p| p.get("line")).and_then(|v| v.as_i64()).unwrap_or(0) as u32;
                let character = params.get("position").and_then(|p| p.get("character")).and_then(|v| v.as_i64()).unwrap_or(0) as u32;
                let include_declaration = matches!(
                    params.get("context").and_then(|c| c.get("includeDeclaration")),
                    Some(JsonValue::Bool(true)) | None
                );
                let refs = self.references(&uri, Position { line, character }, include_declaration);
                let items: Vec<JsonValue> = refs.into_iter().map(|location| {
                    let mut map = BTreeMap::new();
                    map.insert("uri".to_owned(), JsonValue::String(location.uri));
                    map.insert("range".to_owned(), Self::range_to_json(&location.range));
                    JsonValue::Object(map)
                }).collect();
                Self::make_response(id, JsonValue::Array(items))
            }
            "workspace/symbol" => {
                let query = params.and_then(|p| p.get("query")).and_then(|q| q.as_str()).unwrap_or("").to_lowercase();
                let items: Vec<JsonValue> = self.workspace_symbols(&query).into_iter().map(|(uri, name, kind, range)| {
                    let mut location = BTreeMap::new();
                    location.insert("uri".to_owned(), JsonValue::String(uri));
                    location.insert("range".to_owned(), Self::range_to_json(&range));
                    let mut symbol = BTreeMap::new();
                    symbol.insert("name".to_owned(), JsonValue::String(name));
                    symbol.insert("kind".to_owned(), JsonValue::Number(kind as f64));
                    symbol.insert("location".to_owned(), JsonValue::Object(location));
                    JsonValue::Object(symbol)
                }).collect();
                Self::make_response(id, JsonValue::Array(items))
            }
            "textDocument/signatureHelp" => {
                let Some(params) = params else {
                    return Self::make_response(id, JsonValue::Null);
                };
                let uri = params.get("textDocument").and_then(|t| t.get("uri")).and_then(|u| u.as_str()).unwrap_or("");
                let line = params.get("position").and_then(|p| p.get("line")).and_then(|v| v.as_i64()).unwrap_or(0) as u32;
                let character = params.get("position").and_then(|p| p.get("character")).and_then(|v| v.as_i64()).unwrap_or(0) as u32;
                match self.signature_help(uri, Position { line, character }) {
                    Some(signature) => Self::make_response(id, signature),
                    None => Self::make_response(id, JsonValue::Null),
                }
            }
            "textDocument/inlayHint" => {
                let Some(params) = params else {
                    return Self::make_response(id, JsonValue::Null);
                };
                let uri = params.get("textDocument").and_then(|t| t.get("uri")).and_then(|u| u.as_str()).unwrap_or("");
                let hints: Vec<JsonValue> = self.inlay_hints(uri).into_iter().map(|(position, label)| {
                    let mut hint = BTreeMap::new();
                    hint.insert("position".to_owned(), {
                        let mut p = BTreeMap::new();
                        p.insert("line".to_owned(), JsonValue::Number(position.line as f64));
                        p.insert("character".to_owned(), JsonValue::Number(position.character as f64));
                        JsonValue::Object(p)
                    });
                    hint.insert("label".to_owned(), JsonValue::String(label));
                    hint.insert("kind".to_owned(), JsonValue::Number(1.0));
                    JsonValue::Object(hint)
                }).collect();
                Self::make_response(id, JsonValue::Array(hints))
            }
            "textDocument/codeAction" => {
                let Some(params) = params else {
                    return Self::make_response(id, JsonValue::Null);
                };
                let uri = params.get("textDocument").and_then(|t| t.get("uri")).and_then(|u| u.as_str()).unwrap_or("");
                let fixes = self.quick_fixes(uri);
                let actions: Vec<JsonValue> = fixes.into_iter().map(|(title, diagnostic)| {
                    let mut action = BTreeMap::new();
                    action.insert("title".to_owned(), JsonValue::String(title));
                    action.insert("kind".to_owned(), JsonValue::String("quickfix".to_owned()));
                    let mut diag = BTreeMap::new();
                    diag.insert("range".to_owned(), Self::range_to_json(&diagnostic.range));
                    diag.insert("message".to_owned(), JsonValue::String(diagnostic.message.clone()));
                    if let Some(code) = &diagnostic.code {
                        diag.insert("code".to_owned(), JsonValue::String(code.clone()));
                    }
                    action.insert("diagnostics".to_owned(), JsonValue::Array(vec![JsonValue::Object(diag)]));
                    JsonValue::Object(action)
                }).collect();
                Self::make_response(id, JsonValue::Array(actions))
            }
            "textDocument/foldingRange" => {
                let Some(params) = params else {
                    return Self::make_response(id, JsonValue::Null);
                };
                let uri = params.get("textDocument").and_then(|t| t.get("uri")).and_then(|u| u.as_str()).unwrap_or("");
                let ranges: Vec<JsonValue> = self.folding_ranges(uri).into_iter().map(|(start, end, kind)| {
                    let mut range = BTreeMap::new();
                    range.insert("startLine".to_owned(), JsonValue::Number(start as f64));
                    range.insert("endLine".to_owned(), JsonValue::Number(end as f64));
                    if !kind.is_empty() {
                        range.insert("kind".to_owned(), JsonValue::String(kind.to_owned()));
                    }
                    JsonValue::Object(range)
                }).collect();
                Self::make_response(id, JsonValue::Array(ranges))
            }
            "textDocument/definition" => {
                let Some(params) = params else {
                    return Self::make_response(id, JsonValue::Null);
                };
                let uri = params.get("textDocument").and_then(|t| t.get("uri")).and_then(|u| u.as_str()).unwrap_or("");
                let line = params.get("position").and_then(|p| p.get("line")).and_then(|v| v.as_i64()).unwrap_or(0) as u32;
                let character = params.get("position").and_then(|p| p.get("character")).and_then(|v| v.as_i64()).unwrap_or(0) as u32;
                let def = self.definition(uri, Position { line, character });
                match def {
                    Some(loc) => Self::make_response(id, Self::location_to_json(&loc)),
                    None => Self::make_response(id, JsonValue::Null),
                }
            }
            "textDocument/completion" => {
                let Some(params) = params else {
                    return Self::make_response(id, JsonValue::Array(Vec::new()));
                };
                let uri = params.get("textDocument").and_then(|t| t.get("uri")).and_then(|u| u.as_str()).unwrap_or("");
                let line = params.get("position").and_then(|p| p.get("line")).and_then(|v| v.as_i64()).unwrap_or(0) as u32;
                let character = params.get("position").and_then(|p| p.get("character")).and_then(|v| v.as_i64()).unwrap_or(0) as u32;
                let items = self.completion(uri, Position { line, character });
                let json_items = items
                    .into_iter()
                    .map(|(label, kind, detail)| {
                        let mut m = BTreeMap::new();
                        m.insert("label".to_owned(), JsonValue::String(label));
                        m.insert("kind".to_owned(), JsonValue::Number(kind as f64));
                        if let Some(detail) = detail {
                            m.insert("detail".to_owned(), JsonValue::String(detail));
                        }
                        JsonValue::Object(m)
                    })
                    .collect::<Vec<_>>();
                Self::make_response(id, JsonValue::Array(json_items))
            }
            "textDocument/rename" => {
                let Some(params) = params else {
                    return Self::make_response(id, JsonValue::Null);
                };
                let uri = params.get("textDocument").and_then(|t| t.get("uri")).and_then(|u| u.as_str()).unwrap_or("");
                let line = params.get("position").and_then(|p| p.get("line")).and_then(|v| v.as_i64()).unwrap_or(0) as u32;
                let character = params.get("position").and_then(|p| p.get("character")).and_then(|v| v.as_i64()).unwrap_or(0) as u32;
                let new_name = params.get("newName").and_then(|n| n.as_str()).unwrap_or("");
                let edit = self.rename(uri, Position { line, character }, new_name);
                match edit {
                    Some(edit) => Self::make_response(id, edit),
                    None => Self::make_response(id, JsonValue::Null),
                }
            }
            _ => Self::make_error_response(Some(id), -32601, "Method not found"),
        }
    }

    fn dispatch_notification(&mut self, method: &str, params: Option<&JsonValue>) -> Vec<String> {
        match method {
            "textDocument/didOpen" => {
                let Some(params) = params else { return Vec::new(); };
                let doc = params.get("textDocument");
                let uri = doc.and_then(|d| d.get("uri")).and_then(|u| u.as_str()).unwrap_or("").to_owned();
                let version = doc.and_then(|d| d.get("version")).and_then(|v| v.as_i64()).unwrap_or(0);
                let text = doc.and_then(|d| d.get("text")).and_then(|t| t.as_str()).unwrap_or("").to_owned();
                self.documents.insert(uri.clone(), Document::new(uri.clone(), version, text));
                vec![self.publish_diagnostics(&uri)]
            }
            "textDocument/didChange" => {
                let Some(params) = params else { return Vec::new(); };
                let uri = params.get("textDocument").and_then(|d| d.get("uri")).and_then(|u| u.as_str()).unwrap_or("").to_owned();
                let version = params.get("textDocument").and_then(|d| d.get("version")).and_then(|v| v.as_i64()).unwrap_or(0);
                let Some(doc) = self.documents.get_mut(&uri) else { return Vec::new(); };
                if version <= doc.version {
                    return Vec::new();
                }
                let change = params.get("contentChanges").and_then(JsonValue::as_array)
                    .and_then(|changes| changes.last());
                let Some(change) = change.filter(|change| change.get("range").is_none()) else {
                    return Vec::new();
                };
                let Some(text) = change.get("text").and_then(JsonValue::as_str) else {
                    return Vec::new();
                };
                doc.update(version, text.to_owned());
                vec![self.publish_diagnostics(&uri)]
            }
            "textDocument/didClose" => {
                let Some(params) = params else { return Vec::new(); };
                let uri = params.get("textDocument").and_then(|d| d.get("uri")).and_then(|u| u.as_str()).unwrap_or("");
                self.documents.remove(uri)
                    .map(|_| self.publish_diagnostics(uri))
                    .into_iter()
                    .collect()
            }
            "$/cancelRequest" => {
                if let Some(id) = params.and_then(|p| p.get("id")).and_then(|v| v.as_i64()) {
                    self.cancelled_requests.insert(id);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    pub fn check_text(&self, text: &str) -> Vec<Diagnostic> {
        let Err(diagnostic) = crate::frontend::check_source_diagnostic(text) else {
            return Vec::new();
        };
        let index = LineIndex::new(text);
        let offsets = diagnostic.primary.map_or(0..0, |label| label.span.range);
        let start = index.offset_to_position(text, offsets.start);
        let end = index.offset_to_position(text, offsets.end);
        vec![Diagnostic {
            range: Range { start, end },
            severity: 1,
            code: diagnostic.code,
            message: diagnostic.message,
        }]
    }

    pub fn publish_diagnostics(&self, uri: &str) -> String {
        let diags = self
            .documents
            .get(uri)
            .map(|doc| self.check_text(&doc.text))
            .unwrap_or_default();

        let diag_arr = diags
            .into_iter()
            .map(|d| {
                let mut m = BTreeMap::new();
                m.insert("range".to_owned(), Self::range_to_json(&d.range));
                m.insert("severity".to_owned(), JsonValue::Number(d.severity as f64));
                if let Some(code) = d.code {
                    m.insert("code".to_owned(), JsonValue::String(code));
                }
                m.insert("message".to_owned(), JsonValue::String(d.message));
                JsonValue::Object(m)
            })
            .collect::<Vec<_>>();

        let mut params = BTreeMap::new();
        params.insert("uri".to_owned(), JsonValue::String(uri.to_owned()));
        params.insert("diagnostics".to_owned(), JsonValue::Array(diag_arr));
        if let Some(doc) = self.documents.get(uri) {
            params.insert("version".to_owned(), JsonValue::Number(doc.version as f64));
        }

        let mut notif = BTreeMap::new();
        notif.insert("jsonrpc".to_owned(), JsonValue::String("2.0".to_owned()));
        notif.insert("method".to_owned(), JsonValue::String("textDocument/publishDiagnostics".to_owned()));
        notif.insert("params".to_owned(), JsonValue::Object(params));
        JsonValue::Object(notif).serialize()
    }

    /// R3d: hover from the def table. The occurrence under the cursor names
    /// its def; functions render their parameter and return spellings,
    /// structs render their fields, bindings render their inferred type.
    fn hover_from_defs(&self, doc: &Document, offset: usize) -> Option<String> {
        let cached = doc.resolution()?;
        let resolution = &cached.2;
        let def = def_at(resolution, offset)?;
        let describe = |spelling: &ae_resolve::TypeSpelling| describe_spelling(spelling, resolution);
        match def.kind {
            ae_resolve::DefKind::Function => {
                // An imported function's signature comes from its own
                // module's declaration text; the entry-side def table has
                // no parameter spellings for it.
                if def.imported {
                    if let Some(signature) = imported_fn_signature(&cached.1, def) {
                        return Some(format!("```ae\n{signature}\n```"));
                    }
                }
                let mut params = String::new();
                for entry in resolution.defs.iter().filter(|entry| {
                    entry.owner == Some(def.id) && entry.kind == ae_resolve::DefKind::Parameter
                }) {
                    if !params.is_empty() { params.push_str(", "); }
                    let ty = entry.inferred.as_ref().map(&describe).unwrap_or_else(|| "deferred".to_owned());
                    params.push_str(&format!("{}: {}", entry.name, ty));
                }
                let mut returns = String::new();
                if let Some(entry) = resolution.spellings.iter().find(|entry| {
                    entry.owner == def.id && entry.role == ae_resolve::SpellingRole::Return
                }) {
                    returns = format!(" -> {}", describe(&entry.spelled.spelling));
                }
                Some(format!("```ae\nfn {}({params}){returns}\n```", def.name))
            }
            ae_resolve::DefKind::Struct => {
                let mut fields = String::new();
                for entry in resolution.spellings.iter().filter(|entry| {
                    entry.owner == def.id && !entry.name.is_empty()
                }) {
                    if !fields.is_empty() { fields.push_str(", "); }
                    fields.push_str(&format!("{}: {}", entry.name, describe(&entry.spelled.spelling)));
                }
                Some(format!("```ae\nstruct {} {{ {fields} }}\n```", def.name))
            }
            ae_resolve::DefKind::Binding | ae_resolve::DefKind::Parameter
            | ae_resolve::DefKind::ForBinding | ae_resolve::DefKind::PatternBinding => {
                let ty = def.inferred.as_ref().map(&describe).unwrap_or_else(|| "deferred".to_owned());
                Some(format!("{} {}\ntype: {ty}", kind_label(def.kind), def.name))
            }
            _ => Some(format!("```ae\n{} {}\n```", kind_label(def.kind), def.name)),
        }
    }

    /// R3d: inlay hints — every file-local binding with a resolved type gets
/// a `: type` hint right after its name, so the editor shows the inferred
/// shape the def table carries. Deferred shapes stay silent.
    fn inlay_hints(&self, uri: &str) -> Vec<(Position, String)> {
        let Some(doc) = self.documents.get(uri) else {
            return Vec::new();
        };
        let Some(cached) = doc.resolution() else {
            return Vec::new();
        };
        let resolution = &cached.2;
        resolution.defs.iter()
            .filter(|def| !def.imported && def.kind == ae_resolve::DefKind::Binding)
            .filter_map(|def| {
                let shape = def.inferred.as_ref()?;
                let label = format!(": {}", describe_spelling(shape, resolution));
                Some((doc.line_index.offset_to_position(&doc.text, def.range.end), label))
            })
            .collect()
    }

    /// R3d: quick fixes — each diagnostic suggestion becomes a quickfix
/// action titled with the suggestion text and carrying the diagnostic it
/// addresses.
    fn quick_fixes(&self, uri: &str) -> Vec<(String, Diagnostic)> {
        let Some(doc) = self.documents.get(uri) else {
            return Vec::new();
        };
        let Err(diagnostic) = crate::frontend::check_source_diagnostic(&doc.text) else {
            return Vec::new();
        };
        let index = LineIndex::new(&doc.text);
        let offsets = diagnostic.primary.map_or(0..0, |label| label.span.range);
        let start = index.offset_to_position(&doc.text, offsets.start);
        let end = index.offset_to_position(&doc.text, offsets.end);
        let located = Diagnostic {
            range: Range { start, end },
            severity: 1,
            code: diagnostic.code.clone(),
            message: diagnostic.message.clone(),
        };
        diagnostic.suggestions.iter()
            .map(|suggestion| (suggestion.clone(), located.clone()))
            .collect()
    }

    /// R3d: folding ranges — a brace-balanced brace pair folds from its line
/// to the line of its closing brace; a run of `//` comment lines folds as
/// one comment region. Purely lexical, no resolution needed.
    fn folding_ranges(&self, uri: &str) -> Vec<(u32, u32, &'static str)> {
        let Some(doc) = self.documents.get(uri) else {
            return Vec::new();
        };
        let mut ranges = Vec::new();
        let mut line_start = 0usize;
        let mut line = 0u32;
        let mut open: Vec<u32> = Vec::new();
        let mut comment_block: Option<u32> = None;
        let mut in_block_comment = false;
        let bytes = doc.text.as_bytes();
        let mut index = 0usize;
        while index < bytes.len() {
            match bytes[index] {
                b'\n' => {
                    let text = &doc.text[line_start..index];
                    let trimmed = text.trim_start().starts_with("//");
                    match (trimmed, comment_block) {
                        (true, None) => comment_block = Some(line),
                        (false, Some(start)) => {
                            let end = line.saturating_sub(1);
                            if end > start { ranges.push((start, end, "comment")); }
                            comment_block = None;
                        }
                        _ => {}
                    }
                    line += 1;
                    line_start = index + 1;
                }
                b'{' if !in_block_comment => open.push(line),
                b'}' if !in_block_comment => {
                    if let Some(start) = open.pop() {
                        if line > start { ranges.push((start, line, "")); }
                    }
                }
                b'/' if index + 1 < bytes.len() && bytes[index + 1] == b'*' => { in_block_comment = true; index += 1; }
                b'*' if in_block_comment && index + 1 < bytes.len() && bytes[index + 1] == b'/' => { in_block_comment = false; index += 1; }
                _ => {}
            }
            index += 1;
        }
        if let Some(start) = comment_block {
            let end = line.saturating_sub(1);
            if end > start { ranges.push((start, end, "comment")); }
        }
        ranges
    }

    /// R3d: signature help — when the cursor sits inside a call's argument
    /// list, the enclosing call's callee name is found by walking back over
    /// the balanced parentheses, and the def table renders its declared
    /// signature with the active parameter index.
    fn signature_help(&self, uri: &str, pos: Position) -> Option<JsonValue> {
        let doc = self.documents.get(uri)?;
        let offset = doc.line_index.position_to_offset(&doc.text, pos);
        let cached = doc.resolution()?;
        let resolution = &cached.2;
        // Walk back from the cursor over the current argument list.
        let mut depth = 0usize;
        let mut index = offset;
        let bytes = doc.text.as_bytes();
        while index > 0 {
            index -= 1;
            match bytes[index] {
                b')' => depth += 1,
                b'(' => {
                    if depth == 0 { break; }
                    depth -= 1;
                }
                _ => {}
            }
        }
        if index == 0 { return None; }
        // The '(' at `index`; its callee name sits just before the spaces.
        let mut name_end = index;
        while name_end > 0 && bytes[name_end - 1].is_ascii_whitespace() { name_end -= 1; }
        let mut name_start = name_end;
        while name_start > 0 && (bytes[name_start - 1].is_ascii_alphanumeric() || bytes[name_start - 1] == b'_') {
            name_start -= 1;
        }
        if name_start == name_end { return None; }
        let callee = &doc.text[name_start..name_end];
        // The active parameter: count commas since the '(' at depth zero.
        let mut active = 0usize;
        let mut depth = 0usize;
        for &byte in &bytes[index + 1..offset] {
            match byte {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                b',' if depth == 0 => active += 1,
                _ => {}
            }
        }
        let def = resolution.defs.iter().find(|def| def.name == callee
            && (def.kind == ae_resolve::DefKind::Function))?;
        let signature = if def.imported {
            imported_fn_signature(&cached.1, def)?
        } else {
            let mut params = String::new();
            for entry in resolution.defs.iter().filter(|entry| {
                entry.owner == Some(def.id) && entry.kind == ae_resolve::DefKind::Parameter
            }) {
                if !params.is_empty() { params.push_str(", "); }
                let ty = entry.inferred.as_ref()
                    .map(|shape| describe_spelling(shape, resolution))
                    .unwrap_or_else(|| "deferred".to_owned());
                params.push_str(&format!("{}: {}", entry.name, ty));
            }
            let mut returns = String::new();
            if let Some(entry) = resolution.spellings.iter().find(|entry| {
                entry.owner == def.id && entry.role == ae_resolve::SpellingRole::Return
            }) {
                returns = format!(" -> {}", describe_spelling(&entry.spelled.spelling, resolution));
            }
            format!("fn {}({params}){returns}", def.name)
        };
        let mut label = BTreeMap::new();
        label.insert("label".to_owned(), JsonValue::String(signature));
        let mut res = BTreeMap::new();
        res.insert("signatures".to_owned(), JsonValue::Array(vec![JsonValue::Object(label)]));
        res.insert("activeSignature".to_owned(), JsonValue::Number(0.0));
        res.insert("activeParameter".to_owned(), JsonValue::Number(active as f64));
        Some(JsonValue::Object(res))
    }

    /// R3d: the document outline from the def table — every file-local
    /// function, struct, enum and trait with its declared name range.
    fn document_symbols(&self, uri: &str) -> Vec<(String, u32, Range)> {
        let Some(doc) = self.documents.get(uri) else {
            return Vec::new();
        };
        let Some(resolution) = doc.resolution() else {
            return Vec::new();
        };
        let resolution = &resolution.2;
        resolution.defs.iter().filter(|def| {
            !def.imported && !def.kind.is_local()
        }).map(|def| {
            let start = doc.line_index.offset_to_position(&doc.text, def.range.start);
            let end = doc.line_index.offset_to_position(&doc.text, def.range.end);
            (def.name.clone(), symbol_kind_number(def.kind), Range { start, end })
        }).collect()
    }

    /// R3d: find all references — every occurrence targeting the resolved
    /// def, plus the def's own declaration when asked. Imported defs
    /// resolve within their own file's occurrences only (the single-file
    /// occurrence walk does not reach other files).
    fn references(&self, uri: &str, pos: Position, include_declaration: bool) -> Vec<Location> {
        let Some(doc) = self.documents.get(uri) else {
            return Vec::new();
        };
        let offset = doc.line_index.position_to_offset(&doc.text, pos);
        let Some(resolution) = doc.resolution() else {
            return Vec::new();
        };
        let resolution = &resolution.2;
        let Some(def) = def_at(resolution, offset) else {
            return Vec::new();
        };
        let mut locations = Vec::new();
        if include_declaration && !def.imported {
            let start = doc.line_index.offset_to_position(&doc.text, def.range.start);
            let end = doc.line_index.offset_to_position(&doc.text, def.range.end);
            locations.push(Location { uri: doc.uri.clone(), range: Range { start, end } });
        }
        for function in &resolution.functions {
            for occurrence in &function.occurrences {
                if occurrence.target != Some(def.id) {
                    continue;
                }
                // An occurrence's range belongs to the file the resolution
                // was computed for — the entry document here.
                let start = doc.line_index.offset_to_position(&doc.text, occurrence.range.start);
                let end = doc.line_index.offset_to_position(&doc.text, occurrence.range.end);
                locations.push(Location { uri: doc.uri.clone(), range: Range { start, end } });
            }
        }
        locations
    }

    /// R3d: workspace-wide symbol search over every open document's def
    /// table — file-local items whose name contains the query
    /// (case-insensitive), each with its own document's URI and name range.
    fn workspace_symbols(&self, query: &str) -> Vec<(String, String, u32, Range)> {
        let mut out = Vec::new();
        for (uri, doc) in &self.documents {
            let Some(resolution) = doc.resolution() else { continue };
            let resolution = &resolution.2;
            for def in &resolution.defs {
                if def.imported || def.kind.is_local() {
                    continue;
                }
                if def.name.to_lowercase().contains(query) {
                    let start = doc.line_index.offset_to_position(&doc.text, def.range.start);
                    let end = doc.line_index.offset_to_position(&doc.text, def.range.end);
                    out.push((uri.clone(), def.name.clone(), symbol_kind_number(def.kind), Range { start, end }));
                }
            }
        }
        out
    }

    /// R3d: the def's own range is the definition site. Imported defs carry
    /// ranges absolute to their own file, which this slice does not map to
    /// document URIs yet — they fall back to the text scan.
    fn definition_from_defs(&self, doc: &Document, offset: usize) -> Option<Location> {
        let cached = doc.resolution()?;
        let resolution = &cached.2;
        eprintln!("PROBE defs={} imports={} at={:?}", resolution.defs.len(), cached.1.len(), def_at(resolution, offset).map(|d| (d.id.0, d.name.clone(), d.imported)));
        let def = def_at(resolution, offset)?;
        if def.imported {
            return self.imported_definition(doc, &cached, def);
        }
        let start = doc.line_index.offset_to_position(&doc.text, def.range.start);
        let end = doc.line_index.offset_to_position(&doc.text, def.range.end);
        Some(Location { uri: doc.uri.clone(), range: Range { start, end } })
    }

    /// R3d: an imported def's definition site — the owning import module's
    /// file on disk, found by matching the def's name and its own file's
    /// name span against each import's declarations.
    fn imported_definition(
        &self,
        doc: &Document,
        cached: &std::sync::Arc<(ae_resolve::module::Module, Vec<(String, ae_resolve::module::Module)>, ae_resolve::resolve::Resolution)>,
        def: &ae_resolve::DefEntry,
    ) -> Option<Location> {
        let (_, imports, _) = &**cached;
        // An imported declaration's range spans its whole text, while the
        // def's range is the name token (the resolver's import_name_range),
        // so the match is by name — top-level names are unique per module.
        let (file_name, declaration) = imports.iter().find_map(|(file, import_module)| {
            import_module.declarations.iter()
                .find(|declaration| declaration.name == def.name)
                .map(|declaration| (file, declaration))
        })?;
        let disk_dir = doc.disk_path()?.parent()?.to_path_buf();
        let import_path = disk_dir.join(file_name);
        let source = std::fs::read_to_string(&import_path).ok()?;
        let index = LineIndex::new(&source);
        let start = index.offset_to_position(&source, declaration.range.start);
        let end = index.offset_to_position(&source, declaration.range.end);
        let dir = disk_dir.to_string_lossy().replace('\\', "/");
        let uri = format!("file:///{dir}/{file_name}");
        Some(Location { uri, range: Range { start, end } })
    }

    pub fn hover(&self, uri: &str, pos: Position) -> Option<String> {
        let doc = self.documents.get(uri)?;
        let offset = doc.line_index.position_to_offset(&doc.text, pos);
        // R3d: the def table answers first; the text heuristics below stay
        // as the fallback for builtins, prelude items and unresolved names.
        if let Some(markup) = self.hover_from_defs(doc, offset) {
            return Some(markup);
        }
        let ident = extract_ident_at(&doc.text, offset)?;

        match ident {
            "exec_new" => Some("fn exec_new() -> exec".to_owned()),
            "exec_run" => Some("fn exec_run(ex: exec) -> i64".to_owned()),
            "task_spawn" => Some("fn task_spawn(ex: exec, task: || -> i64)".to_owned()),
            "task_yield" => Some("fn task_yield()".to_owned()),
            "scope_new" => Some("fn scope_new() -> task_scope".to_owned()),
            "pool_new" => Some("fn pool_new(workers: i64) -> pool".to_owned()),
            "pool_join" => Some("fn pool_join(p: pool) -> i64".to_owned()),
            "print" => Some("fn print(val: any)\nOutputs formatted representation to stdout.".to_owned()),
            "len" => Some("fn len(v: any) -> i64\nReturns number of elements in vector or bytes in string.".to_owned()),
            _ => {
                for line in doc.text.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with("fn ") && trimmed.contains(ident) {
                        return Some(format!("```ae\n{trimmed}\n```"));
                    }
                    if trimmed.starts_with("struct ") && trimmed.contains(ident) {
                        return Some(format!("```ae\n{trimmed}\n```"));
                    }
                }
                Some(format!("`{ident}`"))
            }
        }
    }

    pub fn definition(&self, uri: &str, pos: Position) -> Option<Location> {
        let doc = self.documents.get(uri)?;
        let offset = doc.line_index.position_to_offset(&doc.text, pos);
        // R3d: the def table's def range is the definition; the text scan
        // below stays as the fallback for names the single-file pass does
        // not own (builtins, prelude, imports).
        if let Some(location) = self.definition_from_defs(doc, offset) {
            return Some(location);
        }
        let ident = extract_ident_at(&doc.text, offset)?;

        for (line_idx, line) in doc.text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("fn ") || trimmed.starts_with("struct ") || trimmed.starts_with("let ") || trimmed.starts_with("var ") {
                if let Some(col) = line.find(ident) {
                    let start = Position {
                        line: line_idx as u32,
                        character: line[..col].encode_utf16().count() as u32,
                    };
                    let end = Position {
                        line: line_idx as u32,
                        character: line[..col + ident.len()].encode_utf16().count() as u32,
                    };
                    return Some(Location {
                        uri: uri.to_owned(),
                        range: Range { start, end },
                    });
                }
            }
        }
        None
    }

    pub fn completion(&self, uri: &str, pos: Position) -> Vec<(String, u32, Option<String>)> {
        let mut out = Vec::new();
        let Some(doc) = self.documents.get(uri) else {
            return out;
        };

        let offset = doc.line_index.position_to_offset(&doc.text, pos);
        let prefix_bytes = doc.text[..offset].as_bytes();

        let is_dot = prefix_bytes.iter().rev().find(|&&b| !b.is_ascii_whitespace()) == Some(&b'.');
        if is_dot {
            for line in doc.text.lines() {
                let trimmed = line.trim();
                if let Some(start_brace) = trimmed.find('{') {
                    if let Some((inside, _)) = trimmed[start_brace + 1..].split_once('}') {
                        for field_part in inside.split(',') {
                            if let Some((field, _)) = field_part.split_once(':') {
                                let f = field.trim();
                                if is_ident(f) {
                                    out.push((f.to_owned(), 5, Some("field".to_owned())));
                                }
                            }
                        }
                    }
                }
                if trimmed.contains(':') && !trimmed.starts_with("fn ") && !trimmed.starts_with("let ") && !trimmed.starts_with("var ") && !trimmed.starts_with("struct ") {
                    if let Some((field, _)) = trimmed.split_once(':') {
                        let f = field.trim();
                        if is_ident(f) {
                            out.push((f.to_owned(), 5, Some("field".to_owned())));
                        }
                    }
                }
            }
            return out;
        }

        let builtins = [
            ("print", 3, "fn print(val: any)"),
            ("len", 3, "fn len(val: any) -> i64"),
            ("push", 3, "fn push(v: &mut vec, val: any)"),
            ("pop", 3, "fn pop(v: &mut vec) -> any"),
            ("exec_new", 3, "fn exec_new() -> exec"),
            ("exec_run", 3, "fn exec_run(ex: exec) -> i64"),
            ("task_spawn", 3, "fn task_spawn(ex: exec, task: || -> i64)"),
            ("task_yield", 3, "fn task_yield()"),
            ("pool_new", 3, "fn pool_new(workers: i64) -> pool"),
            ("pool_join", 3, "fn pool_join(p: pool) -> i64"),
        ];
        for (name, kind, detail) in builtins {
            out.push((name.to_owned(), kind, Some(detail.to_owned())));
        }

        // Locals use the resolver's visibility ranges. Module items retain
        // their declared signatures.
        if let Some(cached) = doc.resolution() {
            let resolution = &cached.2;
            let mut seen = std::collections::HashSet::new();
            let visible: Vec<_> = resolution.functions.iter().flat_map(|function| &function.binding_visibility)
                .rev().filter(|(_, range)| range.contains(&offset))
                .map(|(id, _)| &resolution.defs[id.0 as usize])
                .filter(|def| seen.insert(&def.name)).collect();
            for def in visible.into_iter().rev() {
                let ty = def.inferred.as_ref()
                    .map(|shape| describe_spelling(shape, resolution))
                    .unwrap_or_else(|| "deferred".to_owned());
                out.push((def.name.clone(), 6, Some(format!("{}: {ty}", def.name))));
            }
            for def in &resolution.defs {
                if def.imported {
                    // An imported function's declared shape comes from its
                    // own module's declaration text.
                    if def.kind == ae_resolve::DefKind::Function {
                        if let Some(signature) = imported_fn_signature(&cached.1, def) {
                            out.push((def.name.clone(), 3, Some(signature)));
                        }
                    }
                    continue;
                }
                let (kind, detail) = match def.kind {
                    ae_resolve::DefKind::Function => {
                        let mut params = String::new();
                        for entry in resolution.defs.iter().filter(|entry| {
                            entry.owner == Some(def.id) && entry.kind == ae_resolve::DefKind::Parameter
                        }) {
                            if !params.is_empty() { params.push_str(", "); }
                            let ty = entry.inferred.as_ref()
                                .map(|shape| describe_spelling(shape, resolution))
                                .unwrap_or_else(|| "deferred".to_owned());
                            params.push_str(&format!("{}: {}", entry.name, ty));
                        }
                        let mut returns = String::new();
                        if let Some(entry) = resolution.spellings.iter().find(|entry| {
                            entry.owner == def.id && entry.role == ae_resolve::SpellingRole::Return
                        }) {
                            returns = format!(" -> {}", describe_spelling(&entry.spelled.spelling, resolution));
                        }
                        (3, format!("fn {}({params}){returns}", def.name))
                    }
                    ae_resolve::DefKind::Struct => (7, format!("struct {}", def.name)),
                    _ => continue,
                };
                out.push((def.name.clone(), kind, Some(detail)));
            }
        }

        let keywords = ["fn", "let", "var", "if", "else", "while", "return", "struct", "enum", "impl", "use", "pub", "true", "false"];
        for kw in keywords {
            out.push((kw.to_owned(), 14, None));
        }

        out
    }

    /// R3d: rename an imported def across files — the import module's own
    /// declaration and internal uses (from a fresh resolution of that
    /// module's source) plus the entry document's occurrences, keyed per
    /// file URI in the WorkspaceEdit.
    fn imported_rename(
        &self,
        doc: &Document,
        cached: &std::sync::Arc<(ae_resolve::module::Module, Vec<(String, ae_resolve::module::Module)>, ae_resolve::resolve::Resolution)>,
        def: &ae_resolve::DefEntry,
        new_name: &str,
    ) -> Option<JsonValue> {
        let (_, imports, resolution) = &**cached;
        let (file_name, import_module) = imports.iter().find(|(_, import_module)| {
            import_module.declarations.iter().any(|declaration| declaration.name == def.name)
        })?;
        // Resolve the import module itself so its internal uses join the
        // edit set.
        let tokens = Rc::new(ae_syntax::lex_diagnostic(&import_module.source).ok()?);
        let import_resolution = ae_resolve::resolve::resolve_with_tokens(import_module, &[], tokens.as_slice());
        let import_def = import_resolution.defs.iter()
            .find(|entry| entry.name == def.name && entry.kind == def.kind)?;

        let disk_dir = doc.disk_path()?.parent()?.to_path_buf();
        let import_path = disk_dir.join(file_name);
        let import_source = std::fs::read_to_string(&import_path).ok()?;
        let import_index = LineIndex::new(&import_source);
        let import_uri = format!("file:///{}/{}", disk_dir.to_string_lossy().replace('\\', "/"), file_name);

        let mut edits: Vec<JsonValue> = Vec::new();
        let mut push_edit = |source: &str, index: &LineIndex, range: std::ops::Range<usize>| {
            let start = index.offset_to_position(source, range.start);
            let end = index.offset_to_position(source, range.end);
            let mut text_edit = BTreeMap::new();
            text_edit.insert("range".to_owned(), Self::range_to_json(&Range { start, end }));
            text_edit.insert("newText".to_owned(), JsonValue::String(new_name.to_owned()));
            edits.push(JsonValue::Object(text_edit));
        };
        push_edit(&import_source, &import_index, import_def.range.clone());
        for function in &import_resolution.functions {
            for occurrence in &function.occurrences {
                if occurrence.target == Some(import_def.id) {
                    push_edit(&import_source, &import_index, occurrence.range.clone());
                }
            }
        }

        let mut entry_edits: Vec<(usize, JsonValue)> = Vec::new();
        let mut push_entry_edit = |range: std::ops::Range<usize>| {
            let start = doc.line_index.offset_to_position(&doc.text, range.start);
            let end = doc.line_index.offset_to_position(&doc.text, range.end);
            let mut text_edit = BTreeMap::new();
            text_edit.insert("range".to_owned(), Self::range_to_json(&Range { start, end }));
            text_edit.insert("newText".to_owned(), JsonValue::String(new_name.to_owned()));
            entry_edits.push((range.start, JsonValue::Object(text_edit)));
        };
        for function in resolution.functions.iter() {
            for occurrence in &function.occurrences {
                if occurrence.target == Some(def.id) {
                    push_entry_edit(occurrence.range.clone());
                }
            }
        }
        entry_edits.sort_by_key(|(start, _)| *start);

        let mut changes = BTreeMap::new();
        changes.insert(doc.uri.clone(), JsonValue::Array(entry_edits.into_iter().map(|(_, edit)| edit).collect()));
        changes.insert(import_uri, JsonValue::Array(edits));
        let mut res = BTreeMap::new();
        res.insert("changes".to_owned(), JsonValue::Object(changes));
        Some(JsonValue::Object(res))
    }

    /// R3d: rename the resolved def only — every occurrence whose target is
    /// the same def, plus the def's own declaration range, ordered by
    /// offset. Scope-exact, unlike the text fallback which renames every
    /// same-named token in the file.
    fn rename_from_defs(&self, doc: &Document, offset: usize, new_name: &str) -> Option<JsonValue> {
        let cached = doc.resolution()?;
        let resolution = &cached.2;
        let def = def_at(resolution, offset)?;
        if def.imported {
            return self.imported_rename(doc, &cached, def, new_name);
        }
        let mut edits: Vec<(usize, JsonValue)> = Vec::new();
        let mut push_edit = |range: std::ops::Range<usize>| {
            let start = doc.line_index.offset_to_position(&doc.text, range.start);
            let end = doc.line_index.offset_to_position(&doc.text, range.end);
            let mut text_edit = BTreeMap::new();
            text_edit.insert("range".to_owned(), Self::range_to_json(&Range { start, end }));
            text_edit.insert("newText".to_owned(), JsonValue::String(new_name.to_owned()));
            edits.push((range.start, JsonValue::Object(text_edit)));
        };
        push_edit(def.range.clone());
        for function in &resolution.functions {
            for occurrence in &function.occurrences {
                if occurrence.target == Some(def.id) {
                    push_edit(occurrence.range.clone());
                }
            }
        }
        edits.sort_by_key(|(start, _)| *start);
        let ordered: Vec<JsonValue> = edits.into_iter().map(|(_, edit)| edit).collect();
        let mut changes = BTreeMap::new();
        changes.insert(doc.uri.clone(), JsonValue::Array(ordered));
        let mut res = BTreeMap::new();
        res.insert("changes".to_owned(), JsonValue::Object(changes));
        Some(JsonValue::Object(res))
    }

    pub fn rename(&self, uri: &str, pos: Position, new_name: &str) -> Option<JsonValue> {
        let doc = self.documents.get(uri)?;
        let offset = doc.line_index.position_to_offset(&doc.text, pos);
        let ident = extract_ident_at(&doc.text, offset)?;
        if !is_ident(new_name) {
            return None;
        }
        // R3d: the def table answers first — scope-exact edits for the
        // resolved def only; the text scan below stays as the fallback for
        // names the single-file pass does not own.
        if let Some(result) = self.rename_from_defs(doc, offset, new_name) {
            return Some(result);
        }

        let mut edits = Vec::new();
        for (line_idx, line) in doc.text.lines().enumerate() {
            let mut search_start = 0;
            while let Some(rel_pos) = line[search_start..].find(ident) {
                let col = search_start + rel_pos;
                let before_ok = col == 0 || !line[..col].chars().next_back().unwrap().is_alphanumeric() && line[..col].chars().next_back().unwrap() != '_';
                let after_idx = col + ident.len();
                let after_ok = after_idx >= line.len() || !line[after_idx..].chars().next().unwrap().is_alphanumeric() && line[after_idx..].chars().next().unwrap() != '_';

                if before_ok && after_ok {
                    let range = Range {
                        start: Position {
                            line: line_idx as u32,
                            character: line[..col].encode_utf16().count() as u32,
                        },
                        end: Position {
                            line: line_idx as u32,
                            character: line[..col + ident.len()].encode_utf16().count() as u32,
                        },
                    };
                    let mut text_edit = BTreeMap::new();
                    text_edit.insert("range".to_owned(), Self::range_to_json(&range));
                    text_edit.insert("newText".to_owned(), JsonValue::String(new_name.to_owned()));
                    edits.push(JsonValue::Object(text_edit));
                }
                search_start = col + ident.len();
            }
        }

        let mut changes = BTreeMap::new();
        changes.insert(uri.to_owned(), JsonValue::Array(edits));
        let mut res = BTreeMap::new();
        res.insert("changes".to_owned(), JsonValue::Object(changes));
        Some(JsonValue::Object(res))
    }

    fn make_response(id: i64, result: JsonValue) -> String {
        let mut map = BTreeMap::new();
        map.insert("jsonrpc".to_owned(), JsonValue::String("2.0".to_owned()));
        map.insert("id".to_owned(), JsonValue::Number(id as f64));
        map.insert("result".to_owned(), result);
        JsonValue::Object(map).serialize()
    }

    fn make_error_response(id: Option<i64>, code: i64, message: &str) -> String {
        let mut err = BTreeMap::new();
        err.insert("code".to_owned(), JsonValue::Number(code as f64));
        err.insert("message".to_owned(), JsonValue::String(message.to_owned()));

        let mut map = BTreeMap::new();
        map.insert("jsonrpc".to_owned(), JsonValue::String("2.0".to_owned()));
        map.insert("id".to_owned(), id.map_or(JsonValue::Null, |i| JsonValue::Number(i as f64)));
        map.insert("error".to_owned(), JsonValue::Object(err));
        JsonValue::Object(map).serialize()
    }

    fn range_to_json(range: &Range) -> JsonValue {
        let mut start = BTreeMap::new();
        start.insert("line".to_owned(), JsonValue::Number(range.start.line as f64));
        start.insert("character".to_owned(), JsonValue::Number(range.start.character as f64));

        let mut end = BTreeMap::new();
        end.insert("line".to_owned(), JsonValue::Number(range.end.line as f64));
        end.insert("character".to_owned(), JsonValue::Number(range.end.character as f64));

        let mut map = BTreeMap::new();
        map.insert("start".to_owned(), JsonValue::Object(start));
        map.insert("end".to_owned(), JsonValue::Object(end));
        JsonValue::Object(map)
    }

    fn location_to_json(loc: &Location) -> JsonValue {
        let mut map = BTreeMap::new();
        map.insert("uri".to_owned(), JsonValue::String(loc.uri.clone()));
        map.insert("range".to_owned(), Self::range_to_json(&loc.range));
        JsonValue::Object(map)
    }
}

fn extract_ident_at(text: &str, offset: usize) -> Option<&str> {
    if text.is_empty() {
        return None;
    }
    let offset = offset.min(text.len());
    let bytes = text.as_bytes();

    let mut start = offset;
    while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
        start -= 1;
    }

    let mut end = offset;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }

    if start == end {
        None
    } else {
        Some(&text[start..end])
    }
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_alphabetic() || first == '_') && chars.all(|c| c.is_alphanumeric() || c == '_')
}

pub fn read_lsp_message(reader: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut size: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            size = rest.trim().parse::<usize>().ok();
        }
    }

    let Some(content_length) = size else {
        return Ok(None);
    };

    let mut buf = vec![0u8; content_length];
    reader.read_exact(&mut buf)?;
    String::from_utf8(buf)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn write_lsp_message(writer: &mut impl Write, payload: &str) -> io::Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n{}", payload.len(), payload)?;
    writer.flush()
}

pub fn run_lsp_loop(mut reader: impl BufRead, mut writer: impl Write) -> io::Result<()> {
    let mut server = LspServer::new();
    while let Some(msg) = read_lsp_message(&mut reader)? {
        let responses = server.handle_message(&msg);
        for resp in responses {
            write_lsp_message(&mut writer, &resp)?;
        }
    }
    Ok(())
}
