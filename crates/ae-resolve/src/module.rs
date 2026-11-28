//! Module assembly: one source file into declarations plus per-function
//! statement trees, all ranges absolute to the source.

use ae_span::Diagnostic;
use ae_syntax::declaration::{parse_declaration_tokens, parse_impl_method_tokens, Declaration, DeclarationKind};
use ae_syntax::statement::{parse_statements_until_close, Statement};
use ae_syntax::Token;

/// One function unit: a top-level `fn` or a method of an impl body. The
/// declaration keeps parameters, return spelling and body range; `body`
/// holds the statement tree when the function has one.
#[derive(Debug, PartialEq)]
pub struct FnUnit {
    pub declaration: Declaration,
    /// Index into `Module::declarations` when this is an impl method.
    pub impl_owner: Option<usize>,
    /// `None` for a trait method signature or an `extern "C"` declaration
    /// without a body (the grammar never opens those bodies).
    pub body: Option<Vec<Statement>>,
}

#[derive(Debug, PartialEq)]
pub struct Module {
    pub file: String,
    pub source: String,
    pub use_paths: Vec<String>,
    /// Top-level declarations in source order. Trait and impl bodies are
    /// opaque ranges here; impl methods are expanded into `functions`.
    pub declarations: Vec<Declaration>,
    /// Top-level functions and impl methods, in source order.
    pub functions: Vec<FnUnit>,
}

/// Parse one source file into the resolver's module form. Every range in
/// the result is absolute to `source`.
pub fn parse_module(file: &str, source: &str) -> Result<Module, Diagnostic> {
    let tokens = ae_syntax::lex_diagnostic(source)?;
    parse_module_tokens(file, source, &tokens)
}

/// Assemble a module from the lexer-produced tokens for `source`. A
/// statement-level error inside a body is fatal here; the compiler's index
/// uses the recovering form below.
pub fn parse_module_tokens(file: &str, source: &str, tokens: &[Token]) -> Result<Module, Diagnostic> {
    let (module, errors) = parse_module_tokens_recovering(file, source, tokens)?;
    match errors.into_iter().next() {
        Some((_, diagnostic)) => Err(diagnostic),
        None => Ok(module),
    }
}

/// Assemble a module, recovering from a statement-level error inside one
/// function body: the diagnostic is recorded against the body's opening brace
/// (the key the compiler uses for a collected body) and the parse continues
/// with the next declaration. Header and declaration-level errors stay fatal.
pub fn parse_module_tokens_recovering(
    file: &str,
    source: &str,
    tokens: &[Token],
) -> Result<(Module, Vec<(usize, Diagnostic)>), Diagnostic> {
    let headers = ae_syntax::module::parse_use_headers(file, source, tokens)?;
    let declarations = parse_declaration_tokens(file, source, tokens, headers.body_start)?;
    let mut functions = Vec::new();
    let mut errors: Vec<(usize, Diagnostic)> = Vec::new();
    for (index, declaration) in declarations.iter().enumerate() {
        match &declaration.kind {
            DeclarationKind::Function { .. } => match body_statements(file, source, tokens, declaration) {
                Ok(body) => functions.push(FnUnit { declaration: declaration.clone(), impl_owner: None, body }),
                Err(diagnostic) => {
                    errors.push((declaration.body.as_ref().map(|range| range.start).unwrap_or(declaration.range.start), diagnostic));
                    functions.push(FnUnit { declaration: declaration.clone(), impl_owner: None, body: None });
                }
            },
            DeclarationKind::Impl { .. } => {
                for method in impl_methods(file, source, tokens, declaration)? {
                    match body_statements(file, source, tokens, &method) {
                        Ok(body) => functions.push(FnUnit { declaration: method, impl_owner: Some(index), body }),
                        Err(diagnostic) => {
                            errors.push((method.body.as_ref().map(|range| range.start).unwrap_or(method.range.start), diagnostic));
                            functions.push(FnUnit { declaration: method, impl_owner: Some(index), body: None });
                        }
                    }
                }
            }
            // Struct bodies are fields, enum bodies are variants: both are
            // on the declaration already. Trait bodies stay opaque; their
            // signatures are generic-dispatch surface for the type checker.
            DeclarationKind::Struct { .. } | DeclarationKind::Enum { .. } | DeclarationKind::Trait { .. } => {}
        }
    }
    Ok((Module {
        file: file.to_owned(),
        source: source.to_owned(),
        use_paths: headers.paths,
        declarations,
        functions,
    }, errors))
}

/// Parse a function body whose declaration carries `body` covering
/// `{ ... }` (`opaque_body` starts the range at the `{`).
fn body_statements(file: &str, source: &str, tokens: &[Token], declaration: &Declaration) -> Result<Option<Vec<Statement>>, Diagnostic> {
    let Some(body) = declaration.body.clone() else {
        return Ok(None);
    };
    let Some(open) = tokens.iter().position(|token| token.offset == body.start) else {
        return Err(ae_syntax::diagnostic_range(file, source, body, "expected opening brace for function body"));
    };
    let (statements, close) = parse_statements_until_close(file, source, tokens, open + 1)?;
    debug_assert!(tokens.get(close).is_some_and(|token| token.offset < body.end), "the body parser stops inside its range");
    Ok(Some(statements))
}

/// Expand the opaque impl body through the shared method-declaration parser.
fn impl_methods(file: &str, source: &str, tokens: &[Token], declaration: &Declaration) -> Result<Vec<Declaration>, Diagnostic> {
    let Some(body) = declaration.body.clone() else {
        return Ok(Vec::new());
    };
    let start = tokens.partition_point(|token| token.offset <= body.start);
    parse_impl_method_tokens(file, source, tokens, start)
}
