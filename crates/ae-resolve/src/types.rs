//! Type-spelling classification over the shared grammar's byte ranges
//! (ADR-0228, R3c step 1). The spelling surface mirrors the compiler's
//! `parameter_type`/`named_type` (`crates/ae-compiler/src/frontend/
//! types.rs`); the classifier produces name-level shapes only — no `Type`
//! ids, no instantiation, no visibility, no borrow-flavor checking. The
//! compiler stays the semantic authority; an unknown classification is a
//! dump diagnostic, never a compile error.

use std::ops::Range;

use ae_syntax::{Kind, Token};

use crate::{DefId, DefKind};

/// The compiler's built-in prelude types (`prelude_enums` and
/// `prelude_generics` in `crates/ae-compiler/src/frontend/mod.rs`): names a
/// program may spell without declaring or importing anything. They resolve
/// to a distinct classification so a dump never mistakes them for unknown.
pub const PRELUDE_TYPES: &[&str] = &[
    "ReadResult", "WriteResult", "IntResult", "DirResult", "FileResult",
    "StatusResult", "BoolResult", "CommandResult", "FloatResult", "Option",
];

/// How a spelled type was classified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeSpelling {
    /// `i64`, `f64`, `bool`, `string`.
    Primitive(&'static str),
    /// The copy-semantics handles: `atomic`, `mutex`, `guard`, `chan`,
    /// `exec`, `task_scope`, `pool`.
    Handle(&'static str),
    /// `shared[T]`, `vec`, `vec[string]`, `vec[f64]`, `map[string,i64]`,
    /// `map[string,f64]`. Element restrictions are checker material.
    Composite { family: &'static str, arguments: Vec<TypeSpelling> },
    /// `*const T` / `*mut T`.
    Pointer { mutable: bool, pointee: Box<TypeSpelling> },
    /// `&T` / `&mut T` in parameter and fn-value positions.
    Borrow { mutable: bool, inner: Box<TypeSpelling> },
    /// `fn(T, …) -> R`.
    FnValue { parameters: Vec<TypeSpelling>, returns: Box<TypeSpelling> },
    /// A struct or enum of this module.
    Named { def: DefId },
    /// A generic declaration's parameter (`T` in `fn report[T: Area]`).
    TypeParameter(String),
    /// `Name[T1, …]` on a generic item.
    GenericInstance { def: DefId, arguments: Vec<TypeSpelling> },
    /// A compiler prelude type (`PRELUDE_TYPES`): `Option[T]` and the
    /// result shapes.
    Prelude { name: &'static str, arguments: Vec<TypeSpelling> },
    /// A name the module does not own (import surface in an accepted
    /// program; a mistake otherwise).
    Unknown(String),
}

impl TypeSpelling {
    /// The name a dump line shows for this classification.
    pub fn describe(&self) -> String {
        match self {
            TypeSpelling::Primitive(name) | TypeSpelling::Handle(name) => (*name).to_owned(),
            TypeSpelling::Composite { family, arguments } => {
                if arguments.is_empty() {
                    (*family).to_owned()
                } else {
                    let inner: Vec<String> = arguments.iter().map(|argument| argument.describe()).collect();
                    format!("{family}[{}]", inner.join(", "))
                }
            }
            TypeSpelling::Pointer { mutable, pointee } => {
                format!("*{} {}", if *mutable { "mut" } else { "const" }, pointee.describe())
            }
            TypeSpelling::Borrow { mutable, inner } => {
                format!("&{} {}", if *mutable { "mut " } else { "" }, inner.describe())
            }
            TypeSpelling::FnValue { parameters, returns } => {
                let inner: Vec<String> = parameters.iter().map(|parameter| parameter.describe()).collect();
                format!("fn({}) -> {}", inner.join(", "), returns.describe())
            }
            TypeSpelling::Named { def: DefId(id) } => format!("#{id}"),
            TypeSpelling::TypeParameter(name) => name.clone(),
            TypeSpelling::GenericInstance { def: DefId(id), arguments } => {
                let inner: Vec<String> = arguments.iter().map(|argument| argument.describe()).collect();
                format!("#{id}[{}]", inner.join(", "))
            }
            TypeSpelling::Prelude { name, arguments } => {
                if arguments.is_empty() {
                    (*name).to_owned()
                } else {
                    let inner: Vec<String> = arguments.iter().map(|argument| argument.describe()).collect();
                    format!("{name}[{}]", inner.join(", "))
                }
            }
            TypeSpelling::Unknown(name) => format!("unknown {name}"),
        }
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, TypeSpelling::Unknown(_))
    }
}

/// One classified spelling: the shape plus the byte range it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spelled {
    pub range: Range<usize>,
    pub spelling: TypeSpelling,
}

/// Where a classified spelling sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpellingRole {
    Parameter,
    Return,
    Field,
    VariantPayload,
    ClosureParameter,
    /// The return spelling of a closure literal.
    ClosureReturn,
}

/// One entry of the module's spelling table: the owner (a function, struct
/// or enum def), where the spelling sits, the name it annotates (parameters
/// and fields; empty otherwise) and the classification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpellingEntry {
    pub owner: DefId,
    pub role: SpellingRole,
    pub name: String,
    pub spelled: Spelled,
}

impl SpellingRole {
    pub fn name(self) -> &'static str {
        match self {
            SpellingRole::Parameter => "parameter",
            SpellingRole::Return => "return",
            SpellingRole::Field => "field",
            SpellingRole::VariantPayload => "variant-payload",
            SpellingRole::ClosureParameter => "closure-parameter",
            SpellingRole::ClosureReturn => "closure-return",
        }
    }
}

/// Classify the type spelling inside `window`. `items` answers which names
/// are this module's structs/enums; `type_parameters` are the names the
/// enclosing declaration (and, for impl methods, its impl) binds; `self_name`
/// is the impl target when the spelling sits inside an impl, so `Self`
/// resolves like the compiler's `impl_self` (ADR-0186).
pub fn classify(
    tokens: &[Token],
    items: &dyn Fn(&str) -> Option<DefId>,
    type_parameters: &[String],
    self_name: Option<&str>,
    window: Range<usize>,
) -> Spelled {
    let start = tokens.iter().position(|token| token.offset == window.start)
        .unwrap_or(0);
    let mut parser = SpellingParser { tokens, cursor: start, end: window.end, items, type_parameters, self_name };
    let spelling = parser.spelling();
    Spelled { range: window, spelling }
}

struct SpellingParser<'a> {
    tokens: &'a [Token],
    cursor: usize,
    end: usize,
    items: &'a dyn Fn(&str) -> Option<DefId>,
    type_parameters: &'a [String],
    self_name: Option<&'a str>,
}

impl SpellingParser<'_> {
    fn current(&self) -> Option<&Token> {
        self.tokens.get(self.cursor).filter(|token| token.offset < self.end && token.kind != Kind::Eof)
    }

    fn take(&mut self, kind: Kind) -> bool {
        if self.current().is_some_and(|token| token.kind == kind) {
            self.cursor += 1;
            return true;
        }
        false
    }

    fn spelling(&mut self) -> TypeSpelling {
        // Clone the kind: the borrow must end before the cursor moves.
        let Some(kind) = self.current().map(|token| token.kind.clone()) else {
            return TypeSpelling::Unknown(String::new());
        };
        match kind {
            Kind::Star => {
                self.cursor += 1;
                let mutable = match self.current().map(|token| token.kind.clone()) {
                    Some(Kind::Mut) => {
                        self.cursor += 1;
                        true
                    }
                    Some(Kind::Const) => {
                        self.cursor += 1;
                        false
                    }
                    _ => false,
                };
                let pointee = Box::new(self.spelling());
                TypeSpelling::Pointer { mutable, pointee }
            }
            Kind::Amp => {
                self.cursor += 1;
                let mutable = self.take(Kind::Mut);
                let inner = Box::new(self.spelling());
                TypeSpelling::Borrow { mutable, inner }
            }
            Kind::Fn => {
                self.cursor += 1;
                let mut parameters = Vec::new();
                if self.take(Kind::LParen) {
                    while !self.take(Kind::RParen) {
                        if self.current().is_none() {
                            break;
                        }
                        parameters.push(self.spelling());
                        if !self.take(Kind::Comma) {
                            self.take(Kind::RParen);
                            break;
                        }
                    }
                }
                let returns = if self.take(Kind::Arrow) {
                    Box::new(self.spelling())
                } else {
                    Box::new(TypeSpelling::Unknown(String::new()))
                };
                TypeSpelling::FnValue { parameters, returns }
            }
            Kind::Ident(name) => {
                self.cursor += 1;
                self.named(name)
            }
            _ => {
                self.cursor += 1;
                TypeSpelling::Unknown(String::new())
            }
        }
    }

    fn named(&mut self, name: String) -> TypeSpelling {
        match name.as_str() {
            "i64" | "f64" | "bool" | "string" => {
                return TypeSpelling::Primitive(match name.as_str() {
                    "i64" => "i64",
                    "f64" => "f64",
                    "bool" => "bool",
                    _ => "string",
                });
            }
            "atomic" | "mutex" | "guard" | "chan" | "exec" | "task_scope" | "pool" => {
                return TypeSpelling::Handle(match name.as_str() {
                    "atomic" => "atomic",
                    "mutex" => "mutex",
                    "guard" => "guard",
                    "chan" => "chan",
                    "exec" => "exec",
                    "task_scope" => "task_scope",
                    _ => "pool",
                });
            }
            "shared" | "vec" | "map" => {
                let family = match name.as_str() {
                    "shared" => "shared",
                    "vec" => "vec",
                    _ => "map",
                };
                let mut arguments = Vec::new();
                if self.take(Kind::LBracket) {
                    while !self.take(Kind::RBracket) {
                        if self.current().is_none() {
                            break;
                        }
                        arguments.push(self.spelling());
                        if !self.take(Kind::Comma) {
                            self.take(Kind::RBracket);
                            break;
                        }
                    }
                }
                return TypeSpelling::Composite { family, arguments };
            }
            _ => {}
        }
        let bracketed = self.take(Kind::LBracket);
        let mut arguments = Vec::new();
        if bracketed {
            while !self.take(Kind::RBracket) {
                if self.current().is_none() {
                    break;
                }
                arguments.push(self.spelling());
                if !self.take(Kind::Comma) {
                    self.take(Kind::RBracket);
                    break;
                }
            }
        }
        if let Some(spelled) = PRELUDE_TYPES.iter().find(|prelude| **prelude == name) {
            return TypeSpelling::Prelude { name: spelled, arguments };
        }
        if name == "Self" {
            return match self.self_name.and_then(|target| (self.items)(target)) {
                Some(def) => TypeSpelling::Named { def },
                None => TypeSpelling::Unknown("Self".to_owned()),
            };
        }
        if self.type_parameters.contains(&name) {
            return TypeSpelling::TypeParameter(name);
        }
        match (self.items)(&name) {
            Some(def) => {
                // Only composite-shape items carry type arguments: a generic
                // enum or struct. The def kind decides; the checker rejects
                // wrong arity.
                if bracketed {
                    TypeSpelling::GenericInstance { def, arguments }
                } else if arguments.is_empty() {
                    TypeSpelling::Named { def }
                } else {
                    TypeSpelling::GenericInstance { def, arguments }
                }
            }
            None => TypeSpelling::Unknown(name),
        }
    }
}

/// The def kinds a type spelling may name (structs and enums are types;
/// traits and functions are not).
pub fn is_type_def(kind: DefKind) -> bool {
    matches!(kind, DefKind::Struct | DefKind::Enum)
}

/// Replace the type parameters of a generic declaration with the subject's
/// type arguments: a match subject `Opt[i64]` types its `Some(x)` payload
/// binding as `i64`, not as the symbolic `T` the payload spelling carries.
/// Parameters outside the list stay symbolic.
pub fn substitute(shape: TypeSpelling, parameters: &[String], arguments: &[TypeSpelling]) -> TypeSpelling {
    match shape {
        TypeSpelling::TypeParameter(name) => {
            parameters.iter().position(|parameter| *parameter == name)
                .and_then(|index| arguments.get(index).cloned())
                .unwrap_or(TypeSpelling::TypeParameter(name))
        }
        TypeSpelling::Composite { family, arguments: inner } => TypeSpelling::Composite {
            family,
            arguments: inner.into_iter().map(|argument| substitute(argument, parameters, arguments)).collect(),
        },
        TypeSpelling::Pointer { mutable, pointee } => TypeSpelling::Pointer {
            mutable,
            pointee: Box::new(substitute(*pointee, parameters, arguments)),
        },
        TypeSpelling::Borrow { mutable, inner } => TypeSpelling::Borrow {
            mutable,
            inner: Box::new(substitute(*inner, parameters, arguments)),
        },
        TypeSpelling::FnValue { parameters: fn_params, returns } => TypeSpelling::FnValue {
            parameters: fn_params.into_iter().map(|parameter| substitute(parameter, parameters, arguments)).collect(),
            returns: Box::new(substitute(*returns, parameters, arguments)),
        },
        TypeSpelling::GenericInstance { def, arguments: inner } => TypeSpelling::GenericInstance {
            def,
            arguments: inner.into_iter().map(|argument| substitute(argument, parameters, arguments)).collect(),
        },
        TypeSpelling::Prelude { name, arguments: inner } => TypeSpelling::Prelude {
            name,
            arguments: inner.into_iter().map(|argument| substitute(argument, parameters, arguments)).collect(),
        },
        other => other,
    }
}
