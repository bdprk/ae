//! Resolver pass over the shared grammar AST (ADR-0224 R3b).
//!
//! The pipeline split gives every name one owner: this crate walks the
//! grammar-only trees of `ae-syntax` and produces a flat def table
//! (`DefEntry`) plus, for every function, the name occurrences with the
//! `DefId` they resolve to. Nothing here checks types — the type checker
//! (R3c) consumes this table — and nothing here talks to the compiler's
//! single-pass frontend.
//!
//! Name model, mirroring the compiler's frontend (ADR-0224 keeps the
//! compiler as the enforcement authority until R3c lands):
//!
//! * Locals are parameters, `let`/`var` bindings, `for` bindings and match
//!   pattern bindings. A binding is live from its declaration to the end of
//!   its block; a duplicate while another binding of the same name is live
//!   is the compiler's `E0104 duplicate variable`. A write target or borrow
//!   argument that names no live local is the compiler's `E0101 unknown
//!   variable`.
//! * Items are the module's own top-level declarations (functions, structs,
//!   enums with their variants, traits, impls) and the methods of impl
//!   bodies. Value-position bare names that are neither local nor item are
//!   `target: None` in the dump: in a compiler-accepted program they are
//!   builtins, prelude or imported names, which this single-file pass does
//!   not own yet. That is an honest gap, recorded in ADR-0227, not a
//!   classification claim.
//! * Closure bodies are opaque byte ranges in the grammar, so the resolver
//!   does not descend into them and does not register their parameters;
//!   trait bodies stay opaque the same way (trait methods are generic
//!   dispatch surface for R3c).
//!
//! The dump (`dump`) is a deterministic text form of the whole table and is
//! pinned by `tests/resolve` UI snapshots through `ae emit-resolve`.

#![allow(clippy::result_large_err)]
pub mod dump;
pub mod dump_types;
pub mod imports;
pub mod module;
pub mod resolve;
pub mod types;

pub use module::{parse_module, FnUnit, Module};
/// The shared operator rules now live in `ae-ty`; the old path stays as a
/// re-export so resolver call sites and snapshots keep one source.
pub use ae_ty as typecheck;
pub use resolve::{resolve, FnOccurrences, Resolution};
pub use dump::dump;
pub use types::{classify, Spelled, TypeSpelling, SpellingRole, SpellingEntry};
pub use imports::{load_and_resolve, load_module_transitive};

use std::ops::Range;

/// Definition identity now lives in `ae-ty`; the path stays for callers.
pub use ae_ty::DefId;

/// What a definition is. `is_local` separates the per-function namespace
/// (lookup first, shadowing nothing outside its block) from the module
/// namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefKind {
    Function,
    Struct,
    Enum,
    Variant,
    Trait,
    Impl,
    Parameter,
    /// A closure literal's parameter: binds inside the closure body only.
    ClosureParameter,
    Binding,
    ForBinding,
    PatternBinding,
}

impl DefKind {
    pub fn is_local(self) -> bool {
        matches!(
            self,
            DefKind::Parameter | DefKind::ClosureParameter | DefKind::Binding | DefKind::ForBinding | DefKind::PatternBinding
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefEntry {
    pub id: DefId,
    pub name: String,
    pub kind: DefKind,
    /// Byte range of the name itself (the binding's name token for locals,
    /// the declaration's name token for items).
    pub range: Range<usize>,
    /// `Variant` -> its enum, an impl method -> its impl, a local -> its
    /// function. Top-level items and function bodies own nothing.
    pub owner: Option<DefId>,
    /// Locals only: `var` bindings and `var` parameters are mutable. Loop,
    /// pattern and plain `let` bindings are not. Answers whether the
    /// binding itself may be *rebound*.
    pub mutable: bool,
    /// Locals only: whether writes THROUGH the binding are allowed
    /// (`d.field = v`, `d[i] = v`). `var` bindings and `&mut T` / `*mut T`
    /// parameters are; plain parameters, `let` bindings, `&T` parameters
    /// and by-value receivers are not (probes: w7/w8/w9).
    pub writable_through: bool,
    /// Locals only: the shape this binding carries, inferred so far.
    /// `None` is honest `deferred` — the initializer's shape needs a rule
    /// the inference subset does not have yet (ADR-0229 slice 1).
    pub inferred: Option<TypeSpelling>,
    /// True for an item that arrived through a `use` header: its ranges are
    /// absolute to the *imported* file, not this one.
    pub imported: bool,
}

/// Why a name occurrence exists. The type checker refines these; the
/// resolver only records the position the grammar gave it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Read as a value.
    Read,
    /// Called: the callee of a call, possibly behind a specialization.
    Call,
    /// Constructed: the target of a struct literal. Items only.
    Construct,
    /// Borrowed: `&name` / `&mut name` argument or an address cast binding.
    Borrow,
    /// Written: the base of an assignment target. Locals only; a missing
    /// local is `E0101`, a live but immutable one is the compiler's
    /// `cannot assign to immutable variable`.
    Write,
    /// Written through: `*pointer = value` stores through the named
    /// pointer. The pointer binding itself is not modified, so its
    /// mutability is irrelevant — only the pointee's.
    DerefWrite,
    /// Used as the qualifier of `Name::member`.
    Qualifier,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Occurrence {
    pub range: Range<usize>,
    pub name: String,
    pub role: Role,
    /// The def the name resolves to. `None` is unresolved: for value
    /// positions that means builtin/prelude/imported in an accepted
    /// program; for write targets and borrow arguments it comes with an
    /// `E0101` diagnostic.
    pub target: Option<DefId>,
}

#[cfg(test)]
mod tests {
    use crate::{dump, parse_module, resolve, DefKind, Role};

    fn resolve_source(source: &str) -> crate::resolve::Resolution {
        let module = parse_module("case.ae", source).expect("test sources must parse");
        resolve(&module)
    }

    fn spelled_spelling(entry: &crate::SpellingEntry) -> String {
        entry.spelled.spelling.describe()
    }

    #[test]
    fn items_and_variants_get_owner_linked_defs() {
        let resolution = resolve_source("enum Option[T] { Some(T), None }\nfn main() {}\n");
        let kinds: Vec<(String, DefKind, Option<u32>)> = resolution.defs.iter()
            .map(|entry| (entry.name.clone(), entry.kind, entry.owner.map(|id| id.0)))
            .collect();
        assert_eq!(kinds, vec![
            ("Option".to_owned(), DefKind::Enum, None),
            ("Some".to_owned(), DefKind::Variant, Some(0)),
            ("None".to_owned(), DefKind::Variant, Some(0)),
            ("main".to_owned(), DefKind::Function, None),
        ]);
        assert!(resolution.diagnostics.is_empty());
    }

    #[test]
    fn impl_methods_own_their_receiver_and_join_the_impl() {
        let source = "struct Counter { count: i64 }\nimpl Counter {\n    fn peek(&self) -> i64 { return self.count; }\n}\n";
        let resolution = resolve_source(source);
        let peek = resolution.defs.iter().find(|entry| entry.name == "peek").expect("the method is registered");
        let counter = resolution.defs.iter().find(|entry| entry.name == "Counter" && entry.kind == DefKind::Impl)
            .expect("the impl is registered");
        assert_eq!(peek.owner, Some(counter.id));
        let receiver = resolution.defs.iter().find(|entry| entry.name == "self").expect("the receiver is a local");
        assert_eq!(receiver.kind, DefKind::Parameter);
        assert_eq!(&source[receiver.range.clone()], "self", "the def names the receiver token");
        assert!(!receiver.mutable, "&self borrows read-only");
        let occurrences = &resolution.functions[0].occurrences;
        assert!(occurrences.iter().any(|occurrence| occurrence.role == Role::Read && occurrence.name == "self" && occurrence.target == Some(receiver.id)));
    }

    #[test]
    fn match_arms_bind_pattern_payloads_and_point_at_variants() {
        let resolution = resolve_source(
            "enum Shape { Circle(i64) }\nfn area(shape: Shape) -> i64 {\n    match shape {\n        Shape::Circle(radius) => { return radius; }\n    }\n}\n",
        );
        assert!(resolution.diagnostics.is_empty(), "{:?}", resolution.diagnostics);
        let occurrences = &resolution.functions[0].occurrences;
        let circle = resolution.defs.iter().find(|entry| entry.name == "Circle").map(|entry| entry.id).expect("variant def");
        assert!(occurrences.iter().any(|occurrence| occurrence.name == "Circle" && occurrence.target == Some(circle)));
        let radius = resolution.defs.iter().find(|entry| entry.name == "radius").expect("pattern binding");
        assert_eq!(radius.kind, DefKind::PatternBinding);
    }

    #[test]
    fn a_name_is_reusable_after_its_block_ends_but_not_while_live() {
        let resolution = resolve_source(
            "fn main() {\n    let item = 1;\n    if true {\n        let inner = 2;\n        print(inner);\n    }\n    let inner = 3;\n    let item = 4;\n}\n",
        );
        let messages: Vec<String> = resolution.diagnostics.iter().map(|diagnostic| diagnostic.message.clone()).collect();
        assert_eq!(messages, vec!["duplicate variable 'item'".to_owned()]);
    }

    #[test]
    fn write_targets_and_borrows_report_unknown_variables() {
        let resolution = resolve_source(
            "fn take(value: &i64) -> i64 { return value + 1; }\nfn main() {\n    missing = 3;\n    print(take(&ghost));\n}\n",
        );
        let messages: Vec<String> = resolution.diagnostics.iter().map(|diagnostic| diagnostic.message.clone()).collect();
        assert_eq!(messages, vec![
            "unknown variable 'missing'".to_owned(),
            "unknown variable 'ghost'".to_owned(),
        ]);
    }

    #[test]
    fn closure_parameters_bind_inside_their_body() {
        let resolution = resolve_source(
            "fn apply(value: i64) -> i64 {
    let double = |x: i64| -> i64 { return x * 2; };
    return double(value);
}
",
        );
        assert!(resolution.diagnostics.is_empty());
        let locals: Vec<&str> = resolution.defs.iter().filter(|entry| entry.kind.is_local()).map(|entry| entry.name.as_str()).collect();
        assert_eq!(locals, vec!["value", "x", "double"], "the initializer walk declares the closure parameter before the binding it initializes");
        let parameter = resolution.defs.iter().find(|entry| entry.name == "x").expect("closure parameter");
        assert_eq!(parameter.kind, DefKind::ClosureParameter);
        let value = resolution.defs.iter().find(|entry| entry.name == "value").map(|entry| entry.id).expect("outer binding");
        let occurrences = &resolution.functions[0].occurrences;
        assert!(
            occurrences.iter().any(|occurrence| occurrence.name == "x" && occurrence.target == Some(parameter.id)),
            "the body's read of x resolves to the closure parameter"
        );
        let spelling = resolution.spellings.iter()
            .find(|entry| entry.role == crate::SpellingRole::ClosureParameter)
            .expect("the closure parameter's spelling is classified");
        assert_eq!(spelling.name, "x");
        assert_eq!(spelled_spelling(spelling), "i64");
        assert_eq!(
            occurrences.iter().filter(|occurrence| occurrence.name == "value" && occurrence.target == Some(value)).count(),
            1,
            "the outer binding still owns its reads"
        );
    }

    #[test]
    fn qualified_members_resolve_to_variants_and_inherent_methods() {
        let source = "struct Doc { name: string }
impl Doc {
    fn reset() -> i64 { return 0; }
}
enum Shape { Circle(i64) }
fn main() {
    print(Doc::reset());
    print(Shape::Circle(3));
}
";
        let resolution = resolve_source(source);
        assert!(resolution.diagnostics.is_empty(), "{:?}", resolution.diagnostics);
        // The impl method is functions[0]; main comes after it.
        let occurrences = &resolution.functions.last().expect("main is a function").occurrences;
        let reset = resolution.defs.iter().find(|entry| entry.name == "reset").map(|entry| entry.id).expect("method def");
        let circle = resolution.defs.iter().find(|entry| entry.name == "Circle").map(|entry| entry.id).expect("variant def");
        let reset_call = occurrences.iter().find(|occurrence| occurrence.name == "reset").expect("member occurrence");
        assert_eq!(reset_call.target, Some(reset));
        assert_eq!(reset_call.role, Role::Call);
        assert_eq!(&source[reset_call.range.clone()], "reset", "the member names its own token");
        let circle_call = occurrences.iter().find(|occurrence| occurrence.name == "Circle").expect("member occurrence");
        assert_eq!(circle_call.target, Some(circle));
    }

    #[test]
    fn binding_types_infer_from_the_bounded_subset() {
        let resolution = resolve_source(
            "struct S { a: i64, name: string }\nfn fill() -> string {\n    return \"x\";\n}\nfn fill_s() -> S {\n    return S { a: 1, name: \"x\" };\n}\nfn mutate(s: &mut S) -> i64 {\n    s.a = 1;\n    return s.a;\n}\nfn main() {\n    let one = 1;\n    let ratio = 0.5;\n    let label = \"tag\";\n    let flag = true;\n    let copy = one;\n    let filled = fill();\n    let doc = fill_s();\n    let list = vec[1, 2];\n    let combined = one + 2;\n    let field = doc.a;\n    let via_borrow = doc.a;\n    let sum = one + one;\n    let same = one == one;\n    let flagged = flag && true;\n    let negated = -one;\n    print(one);\n}\n",
        );
        let shapes: Vec<(String, String)> = resolution.defs.iter()
            .filter(|entry| entry.kind == crate::DefKind::Binding)
            .map(|entry| (entry.name.clone(), entry.inferred.as_ref().map(|shape| shape.describe()).unwrap_or_else(|| "deferred".to_owned())))
            .collect();
        assert_eq!(shapes, vec![
            ("one".to_owned(), "i64".to_owned()),
            ("ratio".to_owned(), "f64".to_owned()),
            ("label".to_owned(), "string".to_owned()),
            ("flag".to_owned(), "bool".to_owned()),
            ("copy".to_owned(), "i64".to_owned()),
            ("filled".to_owned(), "string".to_owned()),
            ("doc".to_owned(), "#0".to_owned()),
            ("list".to_owned(), "vec[i64]".to_owned()),
            ("combined".to_owned(), "i64".to_owned()),
            ("field".to_owned(), "i64".to_owned()),
            ("via_borrow".to_owned(), "i64".to_owned()),
            ("sum".to_owned(), "i64".to_owned()),
            ("same".to_owned(), "bool".to_owned()),
            ("flagged".to_owned(), "bool".to_owned()),
            ("negated".to_owned(), "i64".to_owned()),
        ], "literals, copies, calls, struct literals, field reads, arithmetic, comparisons, logic and unary minus bind");

        let operators = resolve_source(
            "fn main() {\n    let bits = 1 & 3;\n    let equal = true == false;\n    let product = 1.5 * 2.0;\n    let remainder = 1.5 % 2.0;\n    let ordered = true < false;\n    let mixed = 1 + 2.0;\n}\n",
        );
        let inferred: Vec<_> = operators.defs.iter()
            .filter(|entry| entry.kind == crate::DefKind::Binding)
            .map(|entry| entry.inferred.as_ref().map(|ty| ty.describe()))
            .collect();
        assert_eq!(inferred, vec![
            Some("i64".to_owned()), Some("bool".to_owned()), Some("f64".to_owned()),
            None, None, None,
        ]);

        use crate::resolve::ExpressionType::{Checked, Deferred, Rejected};
        use crate::typecheck::{OperatorError, Scalar};

        let source = "fn scale(x: i64) -> i64 {\n    let n = x + 2;\n    let good = -(n * 3);\n    let bad = (1.5 % 2.0) + 3.0;\n    let later = scale(9) + 1;\n    return good;\n}\n";
        let resolution = resolve_source(source);
        let facts = &resolution.functions[0].expression_types;
        let fact = |fragment: &str| {
            let start = source.find(fragment).unwrap();
            facts.get(&(start..start + fragment.len())).copied()
        };
        assert_eq!(fact("x + 2"), Some(Checked(Scalar::Int)));
        assert_eq!(fact("n * 3"), Some(Checked(Scalar::Int)));
        assert_eq!(fact("-(n * 3)"), Some(Checked(Scalar::Int)));
        assert_eq!(fact("1.5 % 2.0"), Some(Rejected(OperatorError::FloatRemainder)));
        assert_eq!(fact("(1.5 % 2.0) + 3.0"), Some(Deferred));
        assert_eq!(fact("scale(9) + 1"), Some(Deferred));
        assert!(resolution.diagnostics.is_empty());
    }

    #[test]
    fn unsafe_gating_and_struct_literals_match_the_compiler_texts() {
        let resolution = resolve_source(
            "struct Doc { name: string, size: i64 }\nunsafe fn risky(v: i64) -> i64 {\n    return v;\n}\nfn main() {\n    var x = 1;\n    let p = &mut x as *mut i64;\n    ptr_write(p, 5);\n    print(risky(1));\n    let d = Doc { name: \"x\", note: 1, name: \"y\" };\n    print(d.name);\n}\n",
        );
        let messages: Vec<String> = resolution.diagnostics.iter().map(|diagnostic| diagnostic.message.clone()).collect();
        assert_eq!(messages, vec![
            "ptr_write is an unsafe operation; wrap it in an 'unsafe { ... }' block".to_owned(),
            "call to unsafe function 'risky' requires 'unsafe'".to_owned(),
            "struct 'Doc' has no field 'note'".to_owned(),
            "field 'name' specified twice".to_owned(),
            "missing field 'size' in 'Doc construction'".to_owned(),
        ], "inside unsafe every gate lifts; unknown, duplicate and missing fields are the compiler's texts");
    }

    #[test]
    fn pattern_payloads_and_for_in_bindings_carry_their_types() {
        let resolution = resolve_source(
            "enum Opt[T] { Some(T), None }\nfn main() {\n    let v = Opt[string]::Some(\"a\");\n    var total = 0;\n    match v {\n        Opt::Some(text) => { total = 1; }\n        Opt::None => {}\n    }\n    let list = vec[1, 2];\n    for item in list {\n        total += item;\n    }\n    print(total);\n}\n",
        );
        let shapes: Vec<(String, String)> = resolution.defs.iter()
            .filter(|entry| matches!(entry.kind, crate::DefKind::PatternBinding | crate::DefKind::ForBinding))
            .map(|entry| (entry.name.clone(), entry.inferred.as_ref().map(|shape| shape.describe()).unwrap_or_else(|| "deferred".to_owned())))
            .collect();
        assert_eq!(shapes, vec![
            ("text".to_owned(), "string".to_owned()),
            ("item".to_owned(), "i64".to_owned()),
        ], "the payload binding substitutes the subject's type argument; the vector iterable gives its element");
    }

    #[test]
    fn control_flow_diagnostics_match_the_compiler_texts() {
        let resolution = resolve_source(
            "struct S { a: i64 }\nfn wrong_condition() -> i64 {\n    if 1 {\n        return 1;\n    }\n    return 0;\n}\nfn wrong_return() -> string {\n    return 5;\n}\nfn wrong_while() -> i64 {\n    while \"text\" {\n        return 1;\n    }\n    return 0;\n}\nenum Opt[T] { Some(T), None }\nfn missing_arm(v: Opt[i64]) -> i64 {\n    match v {\n        Opt::Some(x) => { return x; }\n    }\n}\nfn ok_condition() -> i64 {\n    if true {\n        return 1;\n    }\n    return 0;\n}\nfn ok_return() -> i64 {\n    return 3;\n}\nfn main() {\n    print(ok_condition());\n}\n",
        );
        let messages: Vec<String> = resolution.diagnostics.iter().map(|diagnostic| diagnostic.message.clone()).collect();
        assert_eq!(messages, vec![
            "condition must be a boolean expression".to_owned(),
            "return type must be string".to_owned(),
            "condition must be a boolean expression".to_owned(),
            "match is not exhaustive: 'Opt[i64]::None' is not handled".to_owned(),
        ], "true conditions, agreed returns and exhaustive matches stay silent");
    }

    #[test]
    fn call_arities_match_the_compiler_texts() {
        let resolution = resolve_source(
            "fn add(a: i64, b: i64) -> i64 {\n    return a + b;\n}\nstruct Pair[K, V] { key: K, value: V }\nfn main() {\n    print(add(1));\n    print(add(1, 2, 3));\n    let pair = Pair[string, i64, i64] { key: \"k\", value: 1, extra: 2 };\n    print(pair.key);\n}\n",
        );
        let messages: Vec<String> = resolution.diagnostics.iter().map(|diagnostic| diagnostic.message.clone()).collect();
        assert_eq!(messages, vec![
            "function 'add' expects 2 arguments, got 1".to_owned(),
            "function 'add' expects 2 arguments, got 3".to_owned(),
            "'Pair' takes 2 type argument(s), found 3".to_owned(),
            "struct 'Pair' has no field 'extra'".to_owned(),
        ]);
    }

    #[test]
    fn writes_respect_the_two_level_mutability_model() {
        // Probes w1/w2/w5/w7/w8/w9: `var` bindings and `&mut` receivers
        // accept; plain parameters, by-value receivers, `&self` receivers
        // and `let` bindings reject, rebinding and through-writes alike.
        let resolution = resolve_source(
            "struct S { a: i64 }\nimpl S {\n    fn replace(&mut self, v: i64) -> i64 {\n        self.a = v;\n        return self.a;\n    }\n}\nfn by_value(s: S) -> i64 {\n    s.a = 1;\n    return s.a;\n}\nfn rebind(x: i64) -> i64 {\n    x = 2;\n    return x;\n}\nfn main() {\n    let x = 1;\n    x = 2;\n    var d = S { a: 0 };\n    d.a = 4;\n    let p = &mut d as *mut S;\n    unsafe { *p = 5; }\n}\n",
        );
        let messages: Vec<String> = resolution.diagnostics.iter().map(|diagnostic| diagnostic.message.clone()).collect();
        assert_eq!(messages, vec![
            "cannot assign to immutable variable 's'".to_owned(),
            "cannot assign to immutable variable 'x'".to_owned(),
            "cannot assign to immutable variable 'x'".to_owned(),
        ], "the &mut receiver write, the var through-write and the deref write pass; the rest reject");
    }

    #[test]
    fn type_spellings_classify_across_the_whole_surface() {
        // Bounds carry trait names only in this language (the compiler
        // rejects generic-type bounds), so a plain `[T]` is the whole
        // surface the classifier needs.
        let source = "struct Pair[K, V] { key: K, value: V }\nenum Opt[T] { Some(T) }\nstruct Doc { note: *mut i64, pair: Pair[string, i64] }\nfn report[T](shape: T) -> Opt[i64] {\n    let callback = |x: *const i64| -> i64 { return 0; };\n    return Option[i64]::Some(3);\n}\n";
        let resolution = resolve_source(source);
        // Every spelling on a declaration, closure parameters included
        // (ADR-0228's former gap, closed). `Option[i64]::Some(3)` in the
        // body is an expression, not a spelling.
        let descriptions: Vec<String> = resolution.spellings.iter().map(|entry| entry.spelled.spelling.describe()).collect();
        assert_eq!(
            descriptions,
            vec![
                "K".to_owned(),
                "V".to_owned(),
                "T".to_owned(),
                "*mut i64".to_owned(),
                "#0[string, i64]".to_owned(),
                "T".to_owned(),
                "#1[i64]".to_owned(),
                "*const i64".to_owned(),
                "i64".to_owned(),
            ]
        );
        assert!(
            resolution.spellings.iter().all(|entry| !entry.spelled.spelling.is_unknown()),
            "the prelude and the module cover every spelling"
        );
        let generic = resolution.spellings.iter()
            .find(|entry| matches!(entry.spelled.spelling, crate::TypeSpelling::GenericInstance { .. }))
            .expect("Pair[string, i64] is a generic instance");
        assert_eq!(generic.spelled.spelling.describe(), "#0[string, i64]");
    }


    #[test]
    fn the_dump_is_deterministic_text() {
        let source = "fn main() {\n    let value = 7;\n    print(value);\n}\n";
        let module = parse_module("case.ae", source).unwrap();
        let resolution = resolve(&module);
        let first = dump(&module, &resolution);
        assert_eq!(first, dump(&module, &resolution), "two runs agree byte for byte");
        assert_eq!(
            first,
            "resolve case.ae\nuse headers: none\nitems:\n#0 function main @3..7\ntypes:\nlocals in #0 main:\n#1 binding value @20..25 owner=#0 = i64\noccurrences in #0 main:\n  41..46 read value -> #1\ndiagnostics: none\n"
        );
    }
}
