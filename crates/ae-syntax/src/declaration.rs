use std::ops::Range;

use crate::{Diagnostic, Kind, Token};

/// Find the token immediately after the brace body whose opening brace has
/// already been consumed. On failure, return the position of the end token.
pub fn brace_body_end(tokens: &[Token], start: usize) -> Result<usize, usize> {
    delimited_body_end(tokens, start, Kind::LBrace, Kind::RBrace)
}

/// Capture a type spelling as a byte range, stopping before any of `stops`
/// (bracket-nested tokens never stop the scan). Closure headers use this for
/// parameter and return types; the spelling is re-resolved by the compiler,
/// which owns the semantic type rules.
pub fn type_spelling(
    file: &str,
    source: &str,
    tokens: &[Token],
    start: usize,
    stops: &[Kind],
) -> Result<(Range<usize>, usize), Diagnostic> {
    let mut parser = DeclarationParser { file, source, tokens, pos: start };
    let ty = parser.ty(stops, false)?;
    Ok((ty, parser.pos))
}

/// Skip a generic parameter list if it starts at this token. An incomplete
/// list stops at EOF so the caller can handle it in its normal error path.
pub fn skip_type_parameter_list(tokens: &[Token], start: usize) -> usize {
    if !tokens.get(start).is_some_and(|token| token.kind == Kind::LBracket) {
        return start;
    }
    delimited_body_end(tokens, start + 1, Kind::LBracket, Kind::RBracket)
        .unwrap_or_else(|end| end)
}

fn delimited_body_end(
    tokens: &[Token],
    start: usize,
    open: Kind,
    close: Kind,
) -> Result<usize, usize> {
    let mut depth = 1;
    for (index, token) in tokens.iter().enumerate().skip(start) {
        match token.kind {
            ref kind if *kind == open => depth += 1,
            ref kind if *kind == close => {
                depth -= 1;
                if depth == 0 {
                    return Ok(index + 1);
                }
            }
            Kind::Eof => return Err(index),
            _ => {}
        }
    }
    Err(tokens.len())
}

/// Locate the next top-level trait for forward registration. A malformed
/// trailing body is left for the declaration parser to diagnose.
pub fn next_top_level_trait(tokens: &[Token], mut cursor: usize) -> Option<usize> {
    while let Some(token) = tokens.get(cursor) {
        match token.kind {
            Kind::Trait => return Some(cursor),
            Kind::LBrace => cursor = brace_body_end(tokens, cursor + 1).ok()?,
            Kind::Eof => break,
            _ => cursor += 1,
        }
    }
    None
}

/// One `name: Type` in a parameter list or struct body. The type stays a
/// byte range: the shared grammar records the spelling, the compiler
/// resolves it. A receiver spelled bare (`self`, `&self`, `&mut self`) has
/// no spelling after the name, so its `ty` is an empty range.
#[derive(Clone, Debug, PartialEq)]
pub struct TypedName {
    pub name: String,
    pub name_range: Range<usize>,
    pub mutable: bool,
    pub ty: Range<usize>,
}

/// Read the qualifier after the star in a raw pointer type.
pub fn pointer_mutability(file: &str, source: &str, token: &Token) -> Result<bool, Diagnostic> {
    match token.kind {
        Kind::Mut => Ok(true),
        Kind::Const => Ok(false),
        _ => Err(crate::diagnostic_range(file, source, token.offset..token.end,
            "expected 'const' or 'mut' after '*' in a pointer type")),
    }
}

pub const MAX_FUNCTION_PARAMETERS: usize = 64;
pub const MAX_TYPE_PARAMETERS: usize = 8;

/// Validate the spelling and uniqueness of a generic parameter name.
pub fn validate_type_parameter<'a>(
    name: &str,
    mut existing: impl Iterator<Item = &'a str>,
) -> Result<(), String> {
    if !matches!(name.as_bytes(), [b'A'..=b'Z']) {
        return Err("type parameters are single uppercase letters, e.g. 'T'".into());
    }
    if existing.any(|previous| previous == name) {
        return Err(format!("duplicate type parameter '{name}'"));
    }
    Ok(())
}

fn skip_single_bound(tokens: &[Token], start: usize) -> usize {
    let Some(token) = tokens.get(start) else { return start };
    if matches!(&token.kind, Kind::Ident(name) if matches!(name.as_str(), "Fn" | "FnMut" | "FnOnce"))
        && tokens.get(start + 1).is_some_and(|next| next.kind == Kind::LParen)
    {
        let mut pos = delimited_body_end(tokens, start + 2, Kind::LParen, Kind::RParen)
            .unwrap_or_else(|end| end);
        if tokens.get(pos).is_some_and(|next| next.kind == Kind::Arrow) {
            pos += 1;
            let mut paren = 0usize;
            let mut bracket = 0usize;
            while let Some(tok) = tokens.get(pos) {
                match tok.kind {
                    Kind::LParen => paren += 1,
                    Kind::RParen => {
                        if paren == 0 { break; }
                        paren -= 1;
                    }
                    Kind::LBracket => bracket += 1,
                    Kind::RBracket => {
                        if bracket == 0 && paren == 0 { break; }
                        bracket = bracket.saturating_sub(1);
                    }
                    Kind::Plus | Kind::Comma if paren == 0 && bracket == 0 => break,
                    Kind::Eof => break,
                    _ => {}
                }
                pos += 1;
            }
        }
        return pos;
    }
    start + 1
}

/// Read the bounds following a colon in source order. Each item includes the
/// token index of the bound; an invalid name ends the sequence.
pub fn type_bound_tokens(tokens: &[Token], mut index: usize) -> impl Iterator<Item = (usize, &Token)> {
    let mut finished = false;
    std::iter::from_fn(move || {
        if finished {
            return None;
        }
        let token = tokens.get(index)?;
        let bound = (index, token);
        let next = skip_single_bound(tokens, index);
        if matches!(token.kind, Kind::Ident(_))
            && tokens.get(next).is_some_and(|next| next.kind == Kind::Plus)
        {
            index = next + 1;
        } else {
            finished = true;
        }
        Some(bound)
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypeParameter {
    pub name: String,
    pub bounds: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnumVariant {
    pub name: String,
    pub payload: Vec<Range<usize>>,
    pub range: Range<usize>,
}

/// One `fn name(param: Type, ...) -> Type;` signature inside a trait body.
/// The types stay byte ranges: the compiler owns their semantics.
#[derive(Clone, Debug, PartialEq)]
pub struct TraitMethod {
    pub name: String,
    pub name_range: Range<usize>,
    pub parameters: Vec<TypedName>,
    pub returns: Range<usize>,
    pub range: Range<usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DeclarationKind {
    Function {
        parameters: Vec<TypedName>,
        returns: Option<Range<usize>>,
        unsafe_fn: bool,
        abi: Option<String>,
    },
    Struct { fields: Vec<TypedName> },
    Enum { variants: Vec<EnumVariant> },
    Trait { methods: Vec<TraitMethod> },
    Impl { target: Option<Range<usize>> },
}

#[derive(Clone, Debug, PartialEq)]
pub enum DeclarationAttribute {
    ReprC,
    NoMangle,
    DeriveCopy,
}

/// Parse an optional declaration attribute and return the next token index.
pub fn parse_declaration_attribute(
    file: &str,
    source: &str,
    tokens: &[Token],
    start: usize,
) -> Result<(Option<DeclarationAttribute>, usize), Diagnostic> {
    let mut parser = DeclarationParser { file, source, tokens, pos: start };
    if !parser.consume(Kind::Hash) {
        return Ok((None, start));
    }
    parser.take(Kind::LBracket)?;
    let attribute = match parser.identifier()? {
        "repr" => {
            parser.take(Kind::LParen)?;
            let inner = parser.identifier()?;
            if inner != "C" {
                return Err(parser.error(&format!("only '#[repr(C)]' is supported, found '#[repr({inner})]'")));
            }
            parser.take(Kind::RParen)?;
            DeclarationAttribute::ReprC
        }
        "no_mangle" => DeclarationAttribute::NoMangle,
        "derive" => {
            parser.take(Kind::LParen)?;
            let inner = parser.identifier()?;
            if inner != "Copy" {
                return Err(parser.error(&format!("only '#[derive(Copy)]' is supported, found '#[derive({inner})]'")));
            }
            parser.take(Kind::RParen)?;
            DeclarationAttribute::DeriveCopy
        }
        other => return Err(parser.error(&format!("unknown attribute '{other}'; supported attributes are '#[repr(C)]', '#[no_mangle]', and '#[derive(Copy)]'"))),
    };
    parser.take(Kind::RBracket)?;
    Ok((Some(attribute), parser.pos))
}

/// Parse zero or more stacked declaration attributes and return the next token index.
pub fn parse_declaration_attributes(
    file: &str,
    source: &str,
    tokens: &[Token],
    mut start: usize,
) -> Result<(Vec<DeclarationAttribute>, usize), Diagnostic> {
    let mut attributes = Vec::new();
    loop {
        let (attribute, next) = parse_declaration_attribute(file, source, tokens, start)?;
        match attribute {
            Some(attr) => {
                attributes.push(attr);
                start = next;
            }
            None => return Ok((attributes, start)),
        }
    }
}

/// Parse an extern function prefix and return its ABI and following token index.
/// The cursor must point to the `extern` keyword in a lexer-produced token stream.
pub fn parse_extern_function_prefix<'a>(
    file: &str,
    source: &str,
    tokens: &'a [Token],
    start: usize,
) -> Result<(&'a str, usize), Diagnostic> {
    let mut parser = DeclarationParser { file, source, tokens, pos: start };
    if !parser.consume(Kind::Extern) {
        return Err(parser.error("expected 'extern'"));
    }
    let Kind::Text(abi) = &tokens[parser.pos].kind else {
        return Err(parser.error(&format!("expected the ABI string \"C\", found {:?}", parser.current().kind)));
    };
    parser.pos += 1;
    if abi != "C" {
        return Err(parser.error("only the C ABI is supported; write extern \"C\" fn"));
    }
    parser.take(Kind::Fn)?;
    Ok((abi, parser.pos))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Declaration {
    pub attribute: Option<DeclarationAttribute>,
    pub attributes: Vec<DeclarationAttribute>,
    pub name: String,
    pub type_parameters: Vec<TypeParameter>,
    pub kind: DeclarationKind,
    /// Whether the declaration was spelled `pub`. The grammar only records
    /// the flag; visibility is the compiler's decision.
    pub public: bool,
    pub range: Range<usize>,
    pub body: Option<Range<usize>>,
}

/// Read top-level declaration boundaries in a source fragment.
/// Type spellings and function bodies are retained as source ranges.
pub fn parse_declarations(file: &str, source: &str) -> Result<Vec<Declaration>, Diagnostic> {
    let tokens = declaration_tokens(file, source)?;
    let imports = crate::module::parse_use_headers(file, source, &tokens)?;
    parse_declaration_tokens(file, source, &tokens, imports.body_start)
}

/// Read declarations from lexer-produced tokens ending in EOF. Start at the
/// module body index returned by the import-header parser.
pub fn parse_declaration_tokens(
    file: &str,
    source: &str,
    tokens: &[Token],
    body_start: usize,
) -> Result<Vec<Declaration>, Diagnostic> {
    let mut parser = DeclarationParser { file, source, tokens, pos: body_start };
    let mut declarations = Vec::new();
    while parser.current().kind != Kind::Eof {
        declarations.push(parser.declaration()?);
    }
    Ok(declarations)
}

/// Expand an impl body using the original tokens and their absolute ranges.
/// The start index points just after the impl's opening brace.
pub fn parse_impl_method_tokens(
    file: &str,
    source: &str,
    tokens: &[Token],
    start: usize,
) -> Result<Vec<Declaration>, Diagnostic> {
    let mut parser = DeclarationParser { file, source, tokens, pos: start };
    let mut methods = Vec::new();
    while parser.current().kind != Kind::RBrace {
        methods.push(parser.declaration()?);
    }
    Ok(methods)
}

#[derive(Debug)]
pub struct RecoveredDeclarations {
    pub declarations: Vec<Declaration>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Continue at the next top-level declaration after a syntax error.
/// Recovery is bounded to 32 diagnostics and does not enter brace bodies.
pub fn recover_declarations(file: &str, source: &str) -> RecoveredDeclarations {
    let mut result = RecoveredDeclarations { declarations: Vec::new(), diagnostics: Vec::new() };
    let tokens = match crate::lex_recovered(source) {
        crate::LexOutcome::Clean(tokens) => tokens,
        crate::LexOutcome::Recovered(tokens, errors) => {
            result.diagnostics = errors.into_iter().take(MAX_DECLARATION_DIAGNOSTICS)
                .map(|error| diagnostic_in_file(file, error)).collect();
            tokens
        }
        crate::LexOutcome::Fatal(errors) => {
            result.diagnostics = errors.into_iter().take(MAX_DECLARATION_DIAGNOSTICS)
                .map(|error| diagnostic_in_file(file, error)).collect();
            return result;
        }
    };
    let mut parser = DeclarationParser { file, source, tokens: &tokens, pos: 0 };
    match crate::module::parse_use_headers(file, source, &tokens) {
        Ok(imports) => parser.pos = imports.body_start,
        Err(_) if result.diagnostics.len() == MAX_DECLARATION_DIAGNOSTICS => return result,
        Err(error) => {
            // Start recovery at the rejected header token so a missing
            // semicolon cannot hide the declaration that follows it.
            if let Some(label) = &error.primary {
                parser.pos = tokens.partition_point(|token| token.offset < label.span.range.start);
            }
            result.diagnostics.push(error);
            parser.synchronize(0);
        }
    }
    recover_declaration_body(&mut parser, &mut result);
    result
}

const MAX_DECLARATION_DIAGNOSTICS: usize = 32;

/// Recover declaration syntax from the existing module token stream.
pub fn recover_declaration_tokens(
    file: &str,
    source: &str,
    tokens: &[Token],
    body_start: usize,
) -> RecoveredDeclarations {
    let mut parser = DeclarationParser { file, source, tokens, pos: body_start };
    let mut result = RecoveredDeclarations { declarations: Vec::new(), diagnostics: Vec::new() };
    recover_declaration_body(&mut parser, &mut result);
    result
}

fn recover_declaration_body(parser: &mut DeclarationParser<'_>, result: &mut RecoveredDeclarations) {
    while result.diagnostics.len() < MAX_DECLARATION_DIAGNOSTICS && parser.current().kind != Kind::Eof {
        let start = parser.pos;
        match parser.declaration() {
            Ok(declaration) => result.declarations.push(declaration),
            Err(error) => {
                result.diagnostics.push(error);
                if result.diagnostics.len() == MAX_DECLARATION_DIAGNOSTICS {
                    break;
                }
                parser.synchronize(start);
            }
        }
    }
}

fn declaration_tokens(file: &str, source: &str) -> Result<Vec<Token>, Diagnostic> {
    crate::lex_diagnostic(source).map_err(|error| diagnostic_in_file(file, error))
}

fn diagnostic_in_file(file: &str, mut error: Diagnostic) -> Diagnostic {
    if let Some(label) = &mut error.primary {
        label.span.file = Some(file.to_owned());
    }
    error
}

pub(crate) fn expression_type_arguments(
    file: &str,
    source: &str,
    tokens: &[Token],
    start: usize,
) -> Result<(Vec<Range<usize>>, usize), Diagnostic> {
    let mut parser = DeclarationParser { file, source, tokens, pos: start + 1 };
    let arguments = crate::parse_comma_list(
        &mut parser,
        |parser, _| parser.ty(&[Kind::Comma, Kind::RBracket], false),
        |parser| parser.consume(Kind::Comma),
    )?;
    if !parser.consume(Kind::RBracket) {
        return Err(parser.error("expected ']' after type arguments"));
    }
    Ok((arguments, parser.pos))
}

struct DeclarationParser<'a> {
    file: &'a str,
    source: &'a str,
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> DeclarationParser<'a> {
    fn current(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn error(&self, message: &str) -> Diagnostic {
        let token = self.current();
        crate::diagnostic_range(self.file, self.source, token.offset..token.end, message)
    }

    fn take(&mut self, expected: Kind) -> Result<(), Diagnostic> {
        crate::expect_token(self.file, self.source, self.current(), expected)?;
        self.pos += 1;
        Ok(())
    }

    fn identifier(&mut self) -> Result<&'a str, Diagnostic> {
        let name = crate::parse_identifier(self.file, self.source, &self.tokens[self.pos])?;
        self.pos += 1;
        Ok(name)
    }

    fn consume(&mut self, kind: Kind) -> bool {
        if self.current().kind != kind {
            return false;
        }
        self.pos += 1;
        true
    }



    fn ty(&mut self, stops: &[Kind], fields: bool) -> Result<Range<usize>, Diagnostic> {
        let start = self.pos;
        let mut closing = Vec::new();
        loop {
            let kind = &self.current().kind;
            if closing.is_empty() && (stops.contains(kind)
                || (fields && self.pos > start && self.at_field_name())) {
                break;
            }
            match kind {
                Kind::LParen => closing.push(Kind::RParen),
                Kind::LBracket => closing.push(Kind::RBracket),
                Kind::RParen | Kind::RBracket if closing.last() == Some(kind) => {
                    closing.pop();
                }
                Kind::Fn if matches!(self.tokens.get(self.pos + 1).map(|token| &token.kind), Some(Kind::LParen)) => {}
                Kind::Impl if self.pos == start => {}
                Kind::Semi if closing.last() == Some(&Kind::RBracket) => {}
                Kind::Star => {
                    self.pos += 1;
                    pointer_mutability(self.file, self.source, self.current())?;
                }
                Kind::Ident(_) | Kind::Number(_) | Kind::Amp
                | Kind::Const | Kind::Mut | Kind::Arrow | Kind::Comma => {}
                _ => return Err(self.error("unexpected token in type spelling")),
            }
            self.pos += 1;
        }
        if self.pos == start {
            return Err(self.error("expected a type"));
        }
        Ok(self.tokens[start].offset..self.tokens[self.pos - 1].end)
    }

    fn at_field_name(&self) -> bool {
        matches!(self.current().kind, Kind::Ident(_))
            && matches!(self.tokens.get(self.pos + 1).map(|token| &token.kind), Some(Kind::Colon))
    }

    /// The receiver spellings of an impl method: `self`, `&self`, `&mut
    /// self`. None of them carries a type spelling — the impl target types
    /// the receiver — so `TypedName::ty` stays an empty range. A leading
    /// `&` can only start a receiver here (a borrowed parameter is written
    /// `name: &Type`), which mirrors the compiler's own parameter rule
    /// (ADR-0184); `self: Type` rewinds into the plain typed-parameter
    /// path, where the compiler owns the rejection.
    fn receiver(&mut self) -> Result<Option<TypedName>, Diagnostic> {
        const RECEIVER_HINT: &str =
            "write '&self' or '&mut self' to borrow the receiver; any other borrowed parameter is written 'name: &Type' or 'name: &mut Type'";
        if self.consume(Kind::Amp) {
            let mutable = self.consume(Kind::Mut);
            let token = self.current();
            let Kind::Ident(name) = &token.kind else {
                return Err(self.error(RECEIVER_HINT));
            };
            if name != "self" {
                return Err(self.error(RECEIVER_HINT));
            }
            let (name_range, end) = (token.offset..token.end, token.end);
            self.pos += 1;
            return Ok(Some(TypedName {
                name: "self".to_owned(),
                name_range,
                mutable,
                ty: end..end,
            }));
        }
        let token = self.current();
        if let Kind::Ident(name) = &token.kind
            && name == "self"
            && !matches!(self.tokens.get(self.pos + 1).map(|next| &next.kind), Some(Kind::Colon))
        {
            let (name_range, end) = (token.offset..token.end, token.end);
            self.pos += 1;
            return Ok(Some(TypedName {
                name: "self".to_owned(),
                name_range,
                mutable: false,
                ty: end..end,
            }));
        }
        Ok(None)
    }

    fn typed_names(&mut self, close: Kind, fields: bool) -> Result<Vec<TypedName>, Diagnostic> {
        let mut names = Vec::new();
        while self.current().kind != close {
            if !fields && names.len() >= MAX_FUNCTION_PARAMETERS {
                return Err(self.error(&format!("function parameter limit exceeded ({MAX_FUNCTION_PARAMETERS})")));
            }
            if !fields && names.is_empty()
                && let Some(receiver) = self.receiver()?
            {
                names.push(receiver);
                if self.consume(Kind::Comma) {
                    continue;
                }
                break;
            }
            let mutable = !fields && self.consume(Kind::Var);
            let token = self.current();
            let name_range = token.offset..token.end;
            let name = self.identifier()?.to_owned();
            self.take(Kind::Colon)?;
            let ty = if fields {
                self.ty(&[Kind::Comma, Kind::RBrace], true)?
            } else {
                self.ty(&[Kind::Comma, Kind::RParen, Kind::LBrace], false)?
            };
            names.push(TypedName { name, name_range, mutable, ty });
            if !self.consume(Kind::Comma) && !(fields && self.at_field_name()) {
                break;
            }
            if !fields && self.current().kind == close {
                return Err(self.error("expected a parameter after ','"));
            }
        }
        Ok(names)
    }

    fn synchronize(&mut self, start: usize) {
        // A failed declaration may already have consumed a brace opener.
        // Restart at its first token to skip complete bodies consistently.
        let mut index = start;
        let failed_keyword = start + usize::from(matches!(
            &self.tokens[start].kind, Kind::Ident(name) if name == "pub"
        ));
        loop {
            let token = &self.tokens[index];
            match &token.kind {
                Kind::Eof => break,
                Kind::LBrace => {
                    index = brace_body_end(self.tokens, index + 1).unwrap_or_else(|end| end);
                    continue;
                }
                Kind::Struct | Kind::Enum | Kind::Trait | Kind::Impl | Kind::Fn | Kind::Unsafe | Kind::Extern | Kind::Hash if index > failed_keyword && index >= self.pos => {
                    let next = self.tokens.get(index + 1).map(|next| &next.kind);
                    let starts_declaration = match token.kind {
                        Kind::Fn => matches!(next, Some(Kind::Ident(_))),
                        Kind::Unsafe => matches!(next, Some(Kind::Fn)),
                        Kind::Extern => matches!(next, Some(Kind::Text(_))),
                        Kind::Hash => matches!(next, Some(Kind::LBracket)),
                        _ => true,
                    };
                    if starts_declaration {
                        if index > start && matches!(&self.tokens[index - 1].kind, Kind::Ident(name) if name == "pub") {
                            index -= 1;
                        }
                        break;
                    }
                }
                _ => {}
            }
            index += 1;
        }
        self.pos = index;
    }

    fn enum_variants(&mut self) -> Result<Vec<EnumVariant>, Diagnostic> {
        let mut variants = Vec::new();
        while !self.consume(Kind::RBrace) {
            let start = self.current().offset;
            let name = self.identifier()?.to_owned();
            let mut payload = Vec::new();
            if self.consume(Kind::LParen) && !self.consume(Kind::RParen) {
                loop {
                    payload.push(self.ty(&[Kind::Comma, Kind::RParen], false)?);
                    if !self.consume(Kind::Comma) {
                        break;
                    }
                }
                if !self.consume(Kind::RParen) {
                    return Err(self.error("expected ')' after enum payload"));
                }
            }
            variants.push(EnumVariant { name, payload, range: start..self.tokens[self.pos - 1].end });
            self.consume(Kind::Comma);
        }
        Ok(variants)
    }

    fn attributes(&mut self) -> Result<Vec<DeclarationAttribute>, Diagnostic> {
        let (attributes, end) = parse_declaration_attributes(self.file, self.source, self.tokens, self.pos)?;
        self.pos = end;
        Ok(attributes)
    }

    fn opaque_body(&mut self) -> Result<usize, Diagnostic> {
        let start = self.current().offset;
        self.take(Kind::LBrace)?;
        match brace_body_end(self.tokens, self.pos) {
            Ok(end) => {
                self.pos = end;
                Ok(start)
            }
            Err(end) => {
                self.pos = end;
                Err(self.error("expected '}' before end of file"))
            }
        }
    }

    fn trait_body(&mut self) -> Result<Vec<TraitMethod>, Diagnostic> {
        self.take(Kind::LBrace)?;
        let mut methods = Vec::new();
        while self.current().kind != Kind::RBrace {
            if self.current().kind == Kind::Eof { return Err(self.error("expected '}' before end of file")); }
            let method_start = self.current().offset;
            self.take(Kind::Fn)?;
            let name_pos = self.pos;
            let name = self.identifier()?.to_owned();
            let name_range = self.tokens[name_pos].offset..self.tokens[name_pos].end;
            self.take(Kind::LParen)?;
            let mut parameters = Vec::new();
            if self.current().kind != Kind::RParen {
                loop {
                    if parameters.len() >= 8 { return Err(self.error("trait method parameter limit exceeded (8)")); }
                    let parameter_pos = self.pos;
                    let parameter = self.identifier()?.to_owned();
                    let parameter_range = self.tokens[parameter_pos].offset..self.tokens[parameter_pos].end;
                    self.take(Kind::Colon)?;
                    let ty = self.ty(&[Kind::Comma, Kind::RParen], false)?;
                    parameters.push(TypedName { name: parameter, name_range: parameter_range, mutable: false, ty });
                    if self.current().kind != Kind::Comma { break; }
                    self.pos += 1;
                }
            }
            self.take(Kind::RParen)?;
            self.take(Kind::Arrow)?;
            let returns = self.ty(&[Kind::Semi], false)?;
            self.take(Kind::Semi)?;
            let end = self.tokens[self.pos - 1].end;
            methods.push(TraitMethod { name, name_range, parameters, returns, range: method_start..end });
        }
        self.take(Kind::RBrace)?;
        Ok(methods)
    }

    fn declaration(&mut self) -> Result<Declaration, Diagnostic> {
        let start = self.current().offset;
        // The attribute may precede or follow `pub`: `#[repr(C)] pub struct`
        // is the natural spelling, and `pub #[repr(C)] struct` stays legal.
        let attributes = self.attributes()?;
        let public = if matches!(&self.current().kind, Kind::Ident(name) if name == "pub") {
            self.pos += 1;
            true
        } else {
            false
        };
        let attribute = attributes.first().cloned();
        for attr in &attributes {
            match attr {
                DeclarationAttribute::ReprC if self.current().kind != Kind::Struct => {
                    return Err(self.error("'#[repr(C)]' applies to a struct declaration"));
                }
                DeclarationAttribute::NoMangle if self.current().kind != Kind::Extern => {
                    return Err(self.error("'#[no_mangle]' applies to an extern function definition"));
                }
                DeclarationAttribute::DeriveCopy if !matches!(self.current().kind, Kind::Struct | Kind::Enum) => {
                    return Err(self.error("'#[derive(Copy)]' applies to a struct or enum declaration"));
                }
                _ => {}
            }
        }
        let unsafe_fn = self.consume(Kind::Unsafe);
        let abi = if !unsafe_fn && self.current().kind == Kind::Extern {
            let (abi, next) = parse_extern_function_prefix(self.file, self.source, self.tokens, self.pos)?;
            self.pos = next;
            Some(abi.to_owned())
        } else {
            None
        };
        if unsafe_fn && self.current().kind != Kind::Fn {
            return Err(self.error("expected 'fn' after function qualifier"));
        }
        let keyword = if abi.is_some() {
            Kind::Fn
        } else {
            let keyword = match self.current().kind {
                Kind::Fn => Kind::Fn,
                Kind::Struct => Kind::Struct,
                Kind::Enum => Kind::Enum,
                Kind::Trait => Kind::Trait,
                Kind::Impl => Kind::Impl,
                _ => return Err(self.error("expected a top-level declaration")),
            };
            self.pos += 1;
            keyword
        };
        let name = self.identifier()?.to_owned();
        let mut type_parameters: Vec<TypeParameter> = Vec::new();
        if self.consume(Kind::LBracket) {
            loop {
                if type_parameters.len() >= MAX_TYPE_PARAMETERS {
                    return Err(self.error("type parameter limit exceeded (8)"));
                }
                let name = self.identifier()?.to_owned();
                validate_type_parameter(&name, type_parameters.iter().map(|parameter| parameter.name.as_str()))
                    .map_err(|message| self.error(&message))?;
                let mut bounds = Vec::new();
                if self.consume(Kind::Colon) {
                    for (index, token) in type_bound_tokens(self.tokens, self.pos) {
                        self.pos = index;
                        let Kind::Ident(bound) = &token.kind else {
                            return Err(self.error("expected identifier").with_code("E0010"));
                        };
                        let next = skip_single_bound(self.tokens, index);
                        if next > index + 1 {
                            let end_offset = self.tokens[next - 1].end;
                            bounds.push(self.source[token.offset..end_offset].to_owned());
                        } else {
                            bounds.push(bound.clone());
                        }
                        self.pos = next;
                    }
                }
                type_parameters.push(TypeParameter { name, bounds });
                if !self.consume(Kind::Comma) {
                    break;
                }
            }
            if !self.consume(Kind::RBracket) {
                return Err(self.error("expected ']' after type parameters"));
            }
        }

        let (kind, body_start) = if keyword == Kind::Fn {
            self.take(Kind::LParen)?;
            let parameters = self.typed_names(Kind::RParen, false)?;
            self.take(Kind::RParen)?;
            let foreign = abi.is_some();
            let exposes_body = attribute == Some(DeclarationAttribute::NoMangle);
            let returns = if name == "main" {
                if foreign {
                    return Err(self.error("main cannot be an extern declaration"));
                }
                if !type_parameters.is_empty() {
                    return Err(self.error("main cannot be generic"));
                }
                if !parameters.is_empty() {
                    return Err(self.error("main cannot have parameters"));
                }
                None
            } else if foreign || self.current().kind == Kind::Arrow {
                self.take(Kind::Arrow)?;
                let stops = if foreign { &[Kind::LBrace, Kind::Semi][..] } else { &[Kind::LBrace][..] };
                Some(self.ty(stops, false)?)
            } else {
                None
            };
            if foreign && !exposes_body {
                crate::expect_token(self.file, self.source, self.current(), Kind::Semi)?;
            }
            let function = DeclarationKind::Function { parameters, returns, unsafe_fn, abi };
            if foreign && self.current().kind == Kind::Semi {
                if exposes_body {
                    return Err(self.error("'#[no_mangle]' requires a function body"));
                }
                self.pos += 1;
                let end = self.tokens[self.pos - 1].end;
                return Ok(Declaration { attribute, attributes: attributes.clone(), public, name, type_parameters, kind: function, range: start..end, body: None });
            }
            let body_start = self.opaque_body()?;
            (function, body_start)
        } else if keyword == Kind::Trait {
            let body_start = self.current().offset;
            let methods = self.trait_body()?;
            (DeclarationKind::Trait { methods }, body_start)
        } else if keyword == Kind::Impl {
            // Without a 'for' clause the declaration name is the inherent
            // implementation target. With one, it names the trait.
            let target = if self.consume(Kind::For) {
                Some(self.ty(&[Kind::LBrace], false)?)
            } else {
                None
            };
            (DeclarationKind::Impl { target }, self.opaque_body()?)
        } else if keyword == Kind::Enum {
            let body_start = self.current().offset;
            self.take(Kind::LBrace)?;
            let variants = self.enum_variants()?;
            (DeclarationKind::Enum { variants }, body_start)
        } else {
            let body_start = self.current().offset;
            self.take(Kind::LBrace)?;
            let fields = self.typed_names(Kind::RBrace, true)?;
            if !self.consume(Kind::RBrace) {
                return Err(self.error("expected '}' after fields"));
            }
            (DeclarationKind::Struct { fields }, body_start)
        };
        let end = self.tokens[self.pos - 1].end;
        Ok(Declaration { attribute, attributes, public, name, type_parameters, kind, range: start..end, body: Some(body_start..end) })
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trait_and_impl_ranges_follow_the_repository_examples() {
        let source = include_str!("../../../examples/traits.ae");
        let declarations = parse_declarations("traits.ae", source).unwrap();
        assert!(matches!(declarations[0].kind, DeclarationKind::Trait { .. }));
        assert_eq!(declarations[0].name, "Area");
        let DeclarationKind::Impl { target: Some(target) } = &declarations[1].kind else {
            panic!("expected trait implementation");
        };
        assert_eq!(&source[target.clone()], "i64");
        assert!(source[declarations[1].body.clone().unwrap()].contains("return shape * shape;"));

        let source = include_str!("../../../examples/generic_impl.ae");
        let declarations = parse_declarations("generic_impl.ae", source).unwrap();
        let implementation = declarations.iter().find(|declaration|
            matches!(declaration.kind, DeclarationKind::Impl { target: None })).unwrap();
        assert_eq!(implementation.name, "Pair");
        let parameters: Vec<_> = implementation.type_parameters.iter().map(|parameter| parameter.name.as_str()).collect();
        assert_eq!(parameters, ["K", "V"]);
    }

    #[test]
    fn reads_attributes_on_structs_and_foreign_definitions() {
        let source = r#"pub #[repr(C)] struct Point { x: i64 }
#[no_mangle] extern "C" fn identity(value: i64) -> i64 { return value; }"#;
        let declarations = parse_declarations("case.ae", source).unwrap();
        assert_eq!(declarations[0].attribute, Some(DeclarationAttribute::ReprC));
        assert_eq!(declarations[0].range.start, 0);
        assert_eq!(declarations[1].attribute, Some(DeclarationAttribute::NoMangle));
        assert_eq!(declarations[1].range.start, source.find("#[no_mangle]").unwrap());
        assert!(parse_declarations("case.ae",
            r#"#[no_mangle] extern "C" fn identity(value: i64) -> i64;"#).is_err());

        let broken = "fn broken(value: i64 { }\n".to_owned() + source;
        let recovered = recover_declarations("case.ae", &broken);
        assert_eq!(recovered.diagnostics.len(), 1);
        assert_eq!(recovered.declarations[0].attribute, Some(DeclarationAttribute::ReprC));
    }

    #[test]
    fn signature_retains_mutability_and_callable_and_borrowed_types() {
        let source = "fn apply(var value: i64, call: fn(i64, &string) -> i64, values: &[i64; 3]) -> i64 { return value; }";
        let declarations = parse_declarations("case.ae", source).unwrap();
        let DeclarationKind::Function { parameters, .. } = &declarations[0].kind else {
            panic!("expected function declaration");
        };
        assert!(parameters[0].mutable);
        assert_eq!(&source[parameters[0].name_range.clone()], "value");
        assert!(!parameters[1].mutable);
        assert_eq!(&source[parameters[1].ty.clone()], "fn(i64, &string) -> i64");
        assert_eq!(&source[parameters[2].ty.clone()], "&[i64; 3]");
    }

    #[test]
    fn function_parameter_limit_points_to_the_first_extra_parameter() {
        let parameters: Vec<_> = (0..=MAX_FUNCTION_PARAMETERS)
            .map(|index| format!("arg{index}: i64")).collect();
        let limit = MAX_FUNCTION_PARAMETERS;
        let valid = format!("fn f({}) -> i64 {{ return 0; }}", parameters[..limit].join(", "));
        assert!(parse_declarations("case.ae", &valid).is_ok());
        let invalid = format!("fn f({}) -> i64 {{ return 0; }}", parameters.join(", "));
        let error = parse_declarations("case.ae", &invalid).unwrap_err();
        assert_eq!(error.message, format!("function parameter limit exceeded ({limit})"));
        assert_eq!(&invalid[error.primary.unwrap().span.range], format!("arg{limit}"));
    }

    #[test]
    fn function_return_annotations_follow_the_entry_point_rule() {
        let source = "fn helper() {} fn main() {}";
        let declarations = parse_declarations("case.ae", source).unwrap();
        assert_eq!(declarations.len(), 2);
        assert_eq!(declarations[0].name, "helper");
        assert!(matches!(declarations[0].kind, DeclarationKind::Function { returns: None, .. }));
        assert_eq!(declarations[1].name, "main");

        for invalid in ["fn main() -> i64 {}", "fn main[T]() {}", "fn main(x: i64) {}"] {
            assert!(parse_declarations("case.ae", invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn extern_declarations_require_a_return_type_and_semicolon() {
        for (source, expected) in [
            (r#"extern "C" fn foreign();"#, "expected Arrow, found Semi"),
            (r#"extern "C" fn foreign() -> i64 {}"#, "expected Semi, found LBrace"),
        ] {
            let error = parse_declarations("case.ae", source).unwrap_err();
            assert_eq!(error.message, expected);
            let range = error.primary.unwrap().span.range;
            assert!(matches!(&source[range], ";" | "{"));
        }
    }

    #[test]
    fn reads_foreign_prototypes_and_unsafe_definitions() {
        let source = "extern \"C\" fn foreign(value: i64) -> i64; unsafe fn local() -> i64 { return 1; }";
        let declarations = parse_declarations("case.ae", source).unwrap();
        assert!(matches!(&declarations[0].kind,
            DeclarationKind::Function { abi: Some(abi), unsafe_fn: false, .. } if abi == "C"));
        assert_eq!(declarations[0].body, None);
        assert_eq!(&source[declarations[0].range.clone()], "extern \"C\" fn foreign(value: i64) -> i64;");
        assert!(matches!(&declarations[1].kind,
            DeclarationKind::Function { abi: None, unsafe_fn: true, .. }));
        assert_eq!(&source[declarations[1].body.clone().unwrap()], "{ return 1; }");
        assert!(parse_declarations("case.ae", include_str!("../../../tests/run/extern_c_call.ae")).is_ok());
        assert!(parse_declarations("case.ae", include_str!("../../../tests/run/unsafe_fn_call.ae")).is_ok());
    }

    #[test]
    fn enum_payload_spellings_and_variant_boundaries_are_preserved() {
        let source = "pub enum Pair[T] { Empty Value(T, vec[string]), } fn main() {}";
        let declarations = parse_declarations("case.ae", source).unwrap();
        let DeclarationKind::Enum { variants } = &declarations[0].kind else {
            panic!("expected enum declaration");
        };
        assert_eq!(variants.len(), 2);
        assert_eq!(&source[variants[0].range.clone()], "Empty");
        assert_eq!(&source[variants[1].range.clone()], "Value(T, vec[string])");
        assert_eq!(&source[variants[1].payload[0].clone()], "T");
        assert_eq!(&source[variants[1].payload[1].clone()], "vec[string]");
        assert_eq!(declarations[1].name, "main");
        assert!(parse_declarations("case.ae", include_str!("../../../examples/enums.ae")).is_ok());
    }

    #[test]
    fn missing_parameter_delimiter_recovers_at_the_following_declaration() {
        let source = "fn broken(value: i64 { print(1); }\npub struct Next[T] { value: T }\nfn main() {}";
        let recovered = recover_declarations("case.ae", source);
        assert_eq!(recovered.diagnostics.len(), 1);
        let diagnostic = recovered.diagnostics.first().unwrap();
        assert_eq!(parse_declarations("case.ae", source).unwrap_err(), *diagnostic);
        let offset = source.find('{').unwrap();
        assert_eq!(diagnostic.primary.as_ref().unwrap().span.range, offset..offset + 1);
        assert_eq!(recovered.declarations.len(), 2);
        assert_eq!(recovered.declarations[0].name, "Next");
        assert_eq!(recovered.declarations[0].range.start, source.find("pub struct").unwrap());
        assert_eq!(recovered.declarations[1].name, "main");
    }

    #[test]
    fn declaration_recovery_combines_lexical_and_syntax_errors() {
        let source = "@ fn broken(value: i64 { } fn main() {}";
        let recovered = recover_declarations("case.ae", source);
        assert_eq!(recovered.diagnostics.len(), 2);
        assert_eq!(recovered.diagnostics[0], parse_declarations("case.ae", source).unwrap_err());
        assert_eq!(recovered.diagnostics[0].code.as_deref(), Some("E0001"));
        assert_eq!(recovered.declarations.len(), 1);
        assert_eq!(recovered.declarations[0].name, "main");

        let damaged = format!("{} fn broken( {{ }}", "@ ".repeat(32));
        assert_eq!(recover_declarations("case.ae", &damaged).diagnostics.len(), 32);
    }

    #[test]
    fn malformed_import_header_preserves_following_declarations() {
        let source = "use \"valid.ae\";\nuse \"missing-terminator.ae\"\npub struct Item { value: i64 }\nfn main() {}";
        let recovered = recover_declarations("case.ae", source);
        assert_eq!(recovered.diagnostics.len(), 1);
        assert_eq!(recovered.diagnostics[0], parse_declarations("case.ae", source).unwrap_err());
        assert_eq!(recovered.declarations.len(), 2);
        assert_eq!(recovered.declarations[0].range.start, source.find("pub struct").unwrap());
        assert_eq!(recovered.declarations[1].name, "main");
    }

    #[test]
    fn reads_declarations_after_headers_in_the_module_example() {
        let source = include_str!("../../../examples/modules/main.ae");
        let declarations = parse_declarations("main.ae", source).unwrap();
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].name, "main");
        assert_eq!(declarations[0].range.start, source.find("fn main").unwrap());
        let recovered = recover_declarations("main.ae", source);
        assert!(recovered.diagnostics.is_empty());
        assert_eq!(recovered.declarations, declarations);
    }

    #[test]
    fn recovery_caps_repeated_declaration_errors() {
        let source = format!("{}fn main() {{}}", "fn broken( { }\n".repeat(40));
        let recovered = recover_declarations("case.ae", &source);
        assert_eq!(recovered.diagnostics.len(), 32);
        assert!(recovered.declarations.is_empty());
        assert!(recovered.diagnostics.windows(2).all(|pair| {
            pair[0].primary.as_ref().unwrap().span.range.start
                < pair[1].primary.as_ref().unwrap().span.range.start
        }));
    }
}
