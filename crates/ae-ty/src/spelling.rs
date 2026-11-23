//! Editor-facing type spellings (ADR-0228, R3c step 1). The shape is the
//! resolver's, not the compiler's concrete `TyKind`: names and `DefId`s only,
//! because a spelling may not identify a concrete generic instance. The
//! classifier itself stays in `ae-resolve`; this module owns the vocabulary
//! so every crate that names a spelled type shares one definition.

use crate::DefId;

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
