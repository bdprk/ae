//! The resolver pass: item registration, per-function scope walk, occurrence
//! recording. Scope rules mirror the compiler's frontend so the differential
//! corpus (compiler-accepted programs) resolves without diagnostics.

use std::sync::Arc;
use std::collections::HashMap;
use std::ops::Range;

use ae_span::Diagnostic;
use ae_syntax::declaration::{Declaration, DeclarationKind};
use ae_syntax::expression::{ClosureParam, Expression, ExpressionKind};
use ae_syntax::pattern::{Pattern, PatternKind};
use ae_syntax::statement::{parse_statements_until_close, ElseBranch, Statement, StatementKind};
use ae_syntax::{Kind, Token};

use crate::module::Module;
use crate::typecheck::{self, OperatorError, Scalar};
use crate::types::{classify, SpellingEntry, SpellingRole, TypeSpelling};
use crate::{DefEntry, DefId, DefKind, Occurrence, Role};

/// Everything the pass produced: the def table, the occurrences per
/// function (same order as `Module::functions`) and the name-level
/// diagnostics. Diagnostics never stop the walk — the dump stays complete.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolution {
    pub defs: Vec<DefEntry>,
    pub functions: Vec<FnOccurrences>,
    /// Every classified type spelling, in source order (struct fields and
    /// variant payloads first, then functions in module order).
    pub spellings: Vec<SpellingEntry>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FnOccurrences {
    /// Index into `Module::functions`.
    pub function: usize,
    /// The function's own def id (locals name it as their owner).
    pub function_def: DefId,
    pub occurrences: Vec<Occurrence>,
    /// Scope ranges of local bindings, in declaration order. A later
    /// closure parameter can shadow an earlier binding within its range.
    pub binding_visibility: Vec<(DefId, Range<usize>)>,
    /// Successfully parsed closure bodies reached during name resolution.
    pub closure_bodies: Vec<ClosureBody>,
    /// Type argument ranges for expression specializations, keyed by base-name offset.
    pub specialization_arguments: HashMap<usize, Arc<[Range<usize>]>>,
    /// The pointer qualifier for each cast, keyed by the pointee type offset.
    pub pointer_cast_targets: HashMap<usize, bool>,
    /// Closure syntax keyed by the opening pipe token offset.
    pub closure_headers: HashMap<usize, Arc<Expression>>,
    /// Scalar type-check results, indexed by full expression byte range.
    pub expression_types: HashMap<Range<usize>, ExpressionType>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpressionType {
    Checked(Scalar),
    Rejected(OperatorError),
    Deferred,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClosureBody {
    pub start: usize,
    /// The body's closing brace end, so a consumer can scope it precisely.
    pub end: usize,
    pub statements: Arc<[Statement]>,
    /// A duplicate could denote valid shadowing in the compiler's closure scope.
    pub has_binding_conflict: bool,
    /// The outer defs the body reads — the closure's captures. The compiler
    /// decides the capture MODE (borrow vs value); this is the name
    /// resolution fact a body route needs to re-admit reads of outer
    /// bindings from inside the closure.
    pub captures: Vec<DefId>,
}

pub fn resolve(module: &Module) -> Resolution {
    resolve_with_imports(module, &[])
}

/// Resolve one module against the modules its `use` headers pull in. Only
/// the imports' `pub` items become visible, and only under names the
/// entry module does not claim itself — a collision is the resolver's
/// neutral duplicate diagnostic, first registration wins. Imported defs
/// carry `imported: true` and ranges absolute to their own file. Variants
/// of imported enums are visible too (patterns reach them); imported impl
/// methods are not (dispatch is the type checker's business).
pub fn resolve_with_imports(module: &Module, imports: &[(String, Module)]) -> Resolution {
    match ae_syntax::lex_diagnostic(&module.source) {
        Ok(tokens) => resolve_with_tokens(module, imports, &tokens),
        Err(diagnostic) => {
            let mut resolution = resolve_with_tokens(module, imports, &[]);
            resolution.diagnostics.insert(0, diagnostic);
            resolution
        }
    }
}

/// Resolve against the same token stream used to assemble the entry module.
pub fn resolve_with_tokens(
    module: &Module,
    imports: &[(String, Module)],
    tokens: &[Token],
) -> Resolution {
    let mut pass = Pass::new(module, tokens);
    pass.items();
    pass.seed_imports(imports);
    pass.spellings();
    pass.bodies();
    Resolution { defs: pass.defs, functions: pass.functions, spellings: pass.spellings, diagnostics: pass.diagnostics }
}

struct Pass<'m> {
    module: &'m Module,
    tokens: &'m [Token],
    defs: Vec<DefEntry>,
    /// Item name -> def id. First registration wins; a duplicate is a
    /// diagnostic, not a replacement. Impl methods stay out of this table:
    /// a bare call never reaches them (the compiler routes `Type::method`
    /// and trait dispatch through types, which is R3c's business).
    items: HashMap<String, DefId>,
    /// Enum def id -> variant name -> variant def id.
    variants: HashMap<DefId, HashMap<String, DefId>>,
    /// A type item's def id -> its inherent impl's (method name, method def)
    /// pairs. Trait impls stay out: their methods dispatch through the trait
    /// (R3c).
    methods: HashMap<DefId, Vec<(String, DefId)>>,
    function_defs: Vec<DefId>,
    functions: Vec<FnOccurrences>,
    spellings: Vec<SpellingEntry>,
    diagnostics: Vec<Diagnostic>,
}

impl<'m> Pass<'m> {
    fn new(module: &'m Module, tokens: &'m [Token]) -> Self {
        Pass {
            module,
            tokens,
            defs: Vec::new(),
            items: HashMap::new(),
            variants: HashMap::new(),
            methods: HashMap::new(),
            function_defs: Vec::new(),
            functions: Vec::new(),
            spellings: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    fn register(&mut self, name: &str, kind: DefKind, range: Range<usize>, owner: Option<DefId>) -> DefId {
        self.register_imported(name, kind, range, owner, false)
    }

    fn register_imported(&mut self, name: &str, kind: DefKind, range: Range<usize>, owner: Option<DefId>, imported: bool) -> DefId {
        let id = DefId(self.defs.len() as u32);
        self.defs.push(DefEntry { id, name: name.to_owned(), kind, range, owner, mutable: false, writable_through: false, inferred: None, imported });
        id
    }

    /// Byte range of the identifier that spells `name` inside `window`.
    /// Declaration ranges start at `pub`/attribute markers, so the name is
    /// the first matching identifier inside the range.
    fn name_range(&self, name: &str, window: Range<usize>) -> Range<usize> {
        for token in self.tokens {
            if token.offset < window.start {
                continue;
            }
            if token.end > window.end {
                break;
            }
            if let Kind::Ident(spelled) = &token.kind
                && spelled == name {
                    return token.offset..token.end;
                }
        }
        window
    }

    /// Phase 1: one def per item, in declaration order — variants after
    /// their enum, methods after their impl. Bodies walk in phase 2, so a
    /// function may use an item declared later in the file.
    fn items(&mut self) {
        // Top-level functions keep source order in `module.functions`, so
        // walking declarations in order and consuming the next unit without
        // an impl owner pairs every `fn` declaration with its unit.
        let mut top_level: Vec<usize> = self.module.functions.iter().enumerate()
            .filter(|(_, unit)| unit.impl_owner.is_none())
            .map(|(index, _)| index)
            .collect();
        let mut methods_of: HashMap<usize, Vec<usize>> = HashMap::new();
        for (unit_index, unit) in self.module.functions.iter().enumerate() {
            if let Some(owner) = unit.impl_owner {
                methods_of.entry(owner).or_default().push(unit_index);
            }
        }
        let mut pending: Vec<Option<DefId>> = vec![None; self.module.functions.len()];
        type InherentMethods = (String, DefId, Vec<(String, DefId)>);
        let mut inherent: Vec<InherentMethods> = Vec::new();
        for declaration_index in 0..self.module.declarations.len() {
            let declaration = &self.module.declarations[declaration_index];
            match &declaration.kind {
                DeclarationKind::Function { .. } => {
                    let id = self.register(&declaration.name, DefKind::Function, self.name_range(&declaration.name, declaration.range.clone()), None);
                    self.claim_item(&declaration.name, id);
                    let unit_index = top_level.remove(0);
                    pending[unit_index] = Some(id);
                }
                DeclarationKind::Struct { .. } => {
                    let id = self.register(&declaration.name, DefKind::Struct, self.name_range(&declaration.name, declaration.range.clone()), None);
                    self.claim_item(&declaration.name, id);
                }
                DeclarationKind::Enum { variants } => {
                    let id = self.register(&declaration.name, DefKind::Enum, self.name_range(&declaration.name, declaration.range.clone()), None);
                    self.claim_item(&declaration.name, id);
                    let mut table = HashMap::new();
                    for variant in variants {
                        let vid = self.register(&variant.name, DefKind::Variant, variant.range.clone(), Some(id));
                        table.insert(variant.name.clone(), vid);
                    }
                    self.variants.insert(id, table);
                }
                DeclarationKind::Trait { .. } => {
                    let id = self.register(&declaration.name, DefKind::Trait, self.name_range(&declaration.name, declaration.range.clone()), None);
                    self.claim_item(&declaration.name, id);
                }
                DeclarationKind::Impl { target } => {
                    let impl_id = self.register(&declaration.name, DefKind::Impl, self.name_range(&declaration.name, declaration.range.clone()), None);
                    let mut inherent_methods: Vec<(String, DefId)> = Vec::new();
                    for &method_index in methods_of.get(&declaration_index).into_iter().flatten() {
                        let method = &self.module.functions[method_index].declaration;
                        let id = self.register(&method.name, DefKind::Function, self.name_range(&method.name, method.range.clone()), Some(impl_id));
                        pending[method_index] = Some(id);
                        inherent_methods.push((method.name.clone(), id));
                    }
                    // An inherent impl is named by its target type, so its
                    // methods answer `Type::method`; a trait impl is named
                    // by the trait and dispatches through types, which the
                    // resolver does not own. The item table is complete only
                    // after the walk, so the pairing happens below.
                    if target.is_none() {
                        inherent.push((declaration.name.clone(), impl_id, inherent_methods));
                    }
                }
            }
        }
        self.function_defs = pending.into_iter().enumerate()
            .map(|(index, slot)| slot.unwrap_or_else(|| {
                let decl = &self.module.functions[index].declaration;
                self.register(&decl.name, DefKind::Function, self.name_range(&decl.name, decl.range.clone()), None)
            }))
            .collect();
        // Pair every inherent impl's methods with its target type's item def.
        for (type_name, _, method_defs) in inherent {
            if let Some(&type_def) = self.items.get(&type_name) {
                self.methods.insert(type_def, method_defs);
            }
        }
    }

    /// Make the imports' `pub` items visible under free names. Ranges come
    /// from the imported module's own source, so each import is lexed once.
    fn seed_imports(&mut self, imports: &[(String, Module)]) {
        for (_, imported) in imports {
            let Ok(tokens) = ae_syntax::lex_diagnostic(&imported.source) else { continue };
            for declaration in &imported.declarations {
                if !declaration.public {
                    continue;
                }
                let visible = match &declaration.kind {
                    DeclarationKind::Function { .. } => DefKind::Function,
                    DeclarationKind::Struct { .. } => DefKind::Struct,
                    DeclarationKind::Enum { variants } => {
                        let id = self.register_imported(&declaration.name, DefKind::Enum, self.import_name_range(&tokens, declaration), None, true);
                        let mut table = HashMap::new();
                        for variant in variants {
                            let vid = self.register_imported(&variant.name, DefKind::Variant, variant.range.clone(), Some(id), true);
                            table.insert(variant.name.clone(), vid);
                        }
                        self.variants.insert(id, table);
                        self.claim_free(&declaration.name, id);
                        continue;
                    }
                    DeclarationKind::Trait { .. } => DefKind::Trait,
                    DeclarationKind::Impl { .. } => continue,
                };
                let id = self.register_imported(&declaration.name, visible, self.import_name_range(&tokens, declaration), None, true);
                self.claim_free(&declaration.name, id);
            }
        }
    }

    /// Imported items claim only free names: the entry module's own items
    /// always win, and two imports colliding is one diagnostic.
    fn claim_free(&mut self, name: &str, id: DefId) {
        if !self.items.contains_key(name) {
            self.items.insert(name.to_owned(), id);
        } else {
            self.diagnostics.push(ae_syntax::diagnostic_range(
                &self.module.file, &self.module.source, self.defs[id.0 as usize].range.clone(),
                &format!("'{name}' is also defined in an imported file"),
            ));
        }
    }

    /// The name token's range inside an imported module's own source.
    fn import_name_range(&self, tokens: &[Token], declaration: &Declaration) -> Range<usize> {
        for token in tokens {
            if token.offset < declaration.range.start {
                continue;
            }
            if token.end > declaration.range.end {
                break;
            }
            if let Kind::Ident(spelled) = &token.kind
                && *spelled == declaration.name {
                    return token.offset..token.end;
                }
        }
        declaration.range.clone()
    }

    fn claim_item(&mut self, name: &str, id: DefId) {
        // The compiler words this per kind ("duplicate function", "duplicate
        // trait", …); the resolver keeps one neutral message and no code,
        // because assigning the compiler's codes here would claim parity the
        // pass does not have on broken programs.
        if self.items.insert(name.to_owned(), id).is_some() {
            self.diagnostics.push(ae_syntax::diagnostic_range(
                &self.module.file, &self.module.source, self.defs[id.0 as usize].range.clone(),
                &format!("duplicate top-level name '{name}'"),
            ));
        }
    }

    /// The type-parameter names a function's spellings see, plus the impl
    /// target when the function sits inside an impl (`Self`).
    fn spelling_context(&self, unit: &crate::module::FnUnit) -> (Vec<String>, Option<String>) {
        let mut params: Vec<String> = unit.declaration.type_parameters.iter().map(|p| p.name.clone()).collect();
        let mut self_name = None;
        if let Some(owner) = unit.impl_owner
            && let Some(impl_declaration) = self.module.declarations.get(owner) {
                params.extend(impl_declaration.type_parameters.iter().map(|p| p.name.clone()));
                self_name = match &impl_declaration.kind {
                    DeclarationKind::Impl { target } => match target {
                        Some(range) => self.first_ident_in(range.clone()),
                        None => Some(impl_declaration.name.clone()),
                    },
                    _ => None,
                };
            }
        (params, self_name)
    }

    /// Phase 1.5: classify every type spelling on declarations. Runs after
    /// the item walk (named spellings resolve through the complete item
    /// map) and before the body walk. Struct fields and variant payloads
    /// first, then functions in module order; a function sees its own type
    /// parameters plus the ones its impl declares, and `Self` resolves to
    /// the impl's target where one is spelled. Closure parameter spellings
    /// stay unclassified for now - they live in statement trees, not on
    /// declarations (ADR-0228 records the gap).
    fn spellings(&mut self) {
        let items = |name: &str| self.items.get(name).copied();
        let mut spellings = Vec::new();
        let mut impl_params: HashMap<usize, Vec<String>> = HashMap::new();
        let mut impl_self: HashMap<usize, Option<String>> = HashMap::new();
        for (declaration_index, declaration) in self.module.declarations.iter().enumerate() {
            match &declaration.kind {
                DeclarationKind::Struct { fields } => {
                    let Some(owner) = self.items.get(&declaration.name).copied() else { continue };
                    let params: Vec<String> = declaration.type_parameters.iter().map(|p| p.name.clone()).collect();
                    for field in fields {
                        spellings.push(SpellingEntry {
                            owner,
                            role: SpellingRole::Field,
                            name: field.name.clone(),
                            spelled: classify(self.tokens, &items, &params, None, field.ty.clone()),
                        });
                    }
                }
                DeclarationKind::Enum { variants } => {
                    let Some(owner) = self.items.get(&declaration.name).copied() else { continue };
                    let params: Vec<String> = declaration.type_parameters.iter().map(|p| p.name.clone()).collect();
                    for variant in variants {
                        for payload in &variant.payload {
                            spellings.push(SpellingEntry {
                                owner,
                                role: SpellingRole::VariantPayload,
                                name: variant.name.clone(),
                                spelled: classify(self.tokens, &items, &params, None, payload.clone()),
                            });
                        }
                    }
                }
                DeclarationKind::Impl { target } => {
                    impl_params.insert(declaration_index, declaration.type_parameters.iter().map(|p| p.name.clone()).collect());
                    // An inherent impl is named by its target; a trait impl
                    // spells the target after `for` - the first identifier
                    // of that range stands for it.
                    let self_name = match target {
                        Some(range) => self.first_ident_in(range.clone()),
                        None => Some(declaration.name.clone()),
                    };
                    impl_self.insert(declaration_index, self_name);
                }
                _ => {}
            }
        }
        for (unit_index, unit) in self.module.functions.iter().enumerate() {
            let fn_def = self.function_defs[unit_index];
            let (params, self_name) = self.spelling_context(unit);
            let self_name = self_name.as_deref();
            if let DeclarationKind::Function { parameters, returns, .. } = &unit.declaration.kind {
                for parameter in parameters {
                    // A bare receiver (`self`, `&self`, `&mut self`) keeps an
                    // empty type range: there is no spelling to classify.
                    if parameter.ty.start == parameter.ty.end {
                        continue;
                    }
                    spellings.push(SpellingEntry {
                        owner: fn_def,
                        role: SpellingRole::Parameter,
                        name: parameter.name.clone(),
                        spelled: classify(self.tokens, &items, &params, self_name, parameter.ty.clone()),
                    });
                }
                if let Some(returns) = returns {
                    spellings.push(SpellingEntry {
                        owner: fn_def,
                        role: SpellingRole::Return,
                        name: String::new(),
                        spelled: classify(self.tokens, &items, &params, self_name, returns.clone()),
                    });
                }
            }
        }
        self.spellings = spellings;
    }

    fn first_ident_in(&self, window: Range<usize>) -> Option<String> {
        self.tokens.iter()
            .filter(|token| token.offset >= window.start && token.end <= window.end)
            .find_map(|token| match &token.kind {
                Kind::Ident(name) => Some(name.clone()),
                _ => None,
            })
    }

    /// Phase 2: per function, parameters then the body walk. Each
    /// function's locals number after everything registered so far —
    /// items first, then the previous functions' locals — so ids stay
    /// unique across the table.
    fn bodies(&mut self) {
        for (index, unit) in self.module.functions.iter().enumerate() {
            let base = self.defs.len() as u32;
            let function_def = self.function_defs[index];
            let (type_params, self_name) = self.spelling_context(unit);
            let declared_return = match &unit.declaration.kind {
                DeclarationKind::Function { returns: Some(range), .. } if range.start != range.end => {
                    let lookup = |name: &str| self.items.get(name).copied();
                    Some(classify(self.tokens, &lookup, &type_params, self_name.as_deref(), range.clone()).spelling)
                }
                _ => None,
            };
            let body_range = unit.declaration.body.clone().unwrap_or(0..0);
            let mut walker = Walker {
                module: self.module,
                tokens: self.tokens,
                items: &self.items,
                variants: &self.variants,
                methods: &self.methods,
                defs: &self.defs,
                type_params: type_params.clone(),
                self_name: self_name.clone(),
                declared_return,
                body_end: None,
                unsafe_depth: 0,
                spellings: Vec::new(),
                base,
                function_def,
                locals: Vec::new(),
                live: HashMap::new(),
                blocks: vec![(body_range.clone(), Vec::new())],
                binding_visibility: Vec::new(),
                occurrences: Vec::new(),
                closure_bodies: Vec::new(),
                specialization_arguments: HashMap::new(),
                pointer_cast_targets: HashMap::new(),
                closure_headers: HashMap::new(),
                expression_types: HashMap::new(),
                scalar_bindings: HashMap::new(),
                diagnostics: Vec::new(),
            };
            if let DeclarationKind::Function { parameters, .. } = &unit.declaration.kind {
                for parameter in parameters {
                    // `&mut T` / `*mut T` parameters accept writes through
                    // the binding; everything else only by `var` (probes
                    // w7-w9). A bare receiver keeps an empty range and
                    // inherits `mutable` from the receiver spelling.
                    let writable_through = if parameter.ty.start == parameter.ty.end {
                        // A bare receiver: `&mut self` writes through, the
                        // by-value and `&self` forms do not (probe w1).
                        parameter.mutable
                    } else {
                        matches!(
                            classify(self.tokens, &|name: &str| self.items.get(name).copied(), &type_params, self_name.as_deref(), parameter.ty.clone()).spelling,
                            crate::TypeSpelling::Borrow { mutable: true, .. }
                                | crate::TypeSpelling::Pointer { mutable: true, .. }
                                // `&mut [T]` stores elements through the slice (ADR-0236 rung 4).
                                | crate::TypeSpelling::Slice { mutable: true, .. },
                        )
                    };
                    let inferred = if parameter.ty.start != parameter.ty.end {
                        Some(classify(self.tokens, &|name: &str| self.items.get(name).copied(), &type_params, self_name.as_deref(), parameter.ty.clone()).spelling)
                    } else if parameter.name == "self" {
                        // A receiver's shape comes from the impl target:
                        // by-value owns it, `&self` borrows, `&mut self`
                        // mutably borrows. The flavor sits in the tokens
                        // before the receiver's name (`&mut` / `&`).
                        let name_token = self.tokens.iter().position(|token| token.offset == parameter.name_range.start);
                        let borrowed = name_token.map(|index| {
                            let mut back = index;
                            if back > 0 && self.tokens[back - 1].kind == Kind::Mut {
                                back -= 1;
                            }
                            back > 0 && self.tokens[back - 1].kind == Kind::Amp
                        }).unwrap_or(false);
                        let target = self_name.as_deref().and_then(|name| self.items.get(name))
                            .map(|def| TypeSpelling::Named { def: *def });
                        match (borrowed, target) {
                            (true, Some(target)) => Some(TypeSpelling::Borrow { mutable: parameter.mutable, inner: Box::new(target) }),
                            (_, target) => target,
                        }
                    } else {
                        None
                    };
                    walker.declare(parameter.name.clone(), DefKind::Parameter, parameter.name_range.clone(), parameter.mutable, writable_through, inferred, body_range.start);
                }
            }
            let body_end = unit.declaration.body.as_ref().map(|body| body.end);
            walker.body_end = body_end;
            if let Some(body) = &unit.body {
                walker.block(body, body_range);
            }
            // A function that declares a return type must return on every
            // path; the compiler's `function may exit without returning a
            // value` sits on the closing brace (probes r1-r5: if/else both,
            // full match arms and unsafe blocks count, `while true` does
            // not).
            let non_unit_return = match &unit.declaration.kind {
                DeclarationKind::Function { returns: Some(range), .. } => {
                    self.module.source.get(range.clone()).map(str::trim) != Some("()")
                }
                _ => false,
            };
            if non_unit_return
                && let Some(body) = &unit.body
                    && !Walker::definitely_returns(body)
                        && let Some(end) = body_end {
                            walker.diagnostics.push(
                                ae_syntax::diagnostic_range(
                                    &self.module.file, &self.module.source, end..end,
                                    "function may exit without returning a value",
                                ),
                            );
                        }
            let Walker { locals, occurrences, binding_visibility, spellings, diagnostics, closure_bodies, specialization_arguments, pointer_cast_targets, closure_headers, expression_types, .. } = walker;
            self.defs.extend(locals);
            self.spellings.extend(spellings);
            self.diagnostics.extend(diagnostics);
            self.functions.push(FnOccurrences { function: index, function_def, occurrences, binding_visibility, closure_bodies, specialization_arguments, pointer_cast_targets, closure_headers, expression_types });
        }
    }
}

/// Why a name is looked up. `LocalOnly` modes come with the compiler's
/// `E0101 unknown variable` when nothing live matches; value positions may
/// legitimately reach builtins/prelude/imports, so an unresolved `Normal`
/// or `ItemsOnly` lookup stays silent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lookup {
    Normal,
    LocalOnly,
    ItemsOnly,
}

struct Walker<'w> {
    module: &'w Module,
    tokens: &'w [Token],
    items: &'w HashMap<String, DefId>,
    variants: &'w HashMap<DefId, HashMap<String, DefId>>,
    methods: &'w HashMap<DefId, Vec<(String, DefId)>>,
    /// The item defs built in phase 1: field reads and call shapes need
    /// def-entry names without a second table.
    defs: &'w [DefEntry],
    /// The enclosing function's type parameters plus its impl's, and the
    /// impl target when there is one: the context closure-parameter
    /// spellings classify in.
    type_params: Vec<String>,
    self_name: Option<String>,
    /// The function's declared return shape, when it declares one and the
    /// spelling classifies. Return statements check against it.
    declared_return: Option<TypeSpelling>,
    /// The offset of the body's closing brace: the compiler points the
    /// missing-return diagnostic there.
    body_end: Option<usize>,
    /// Nesting depth of `unsafe` blocks: the compiler gates pointer
    /// statement calls, `unsafe fn` calls and `extern "C"` calls on it.
    unsafe_depth: usize,
    /// Type spellings met during the walk (closure parameters; the
    /// declarative ones are collected by the spellings phase).
    spellings: Vec<SpellingEntry>,
    /// First local def id of this function.
    base: u32,
    function_def: DefId,
    locals: Vec<DefEntry>,
    /// Live bindings use the compiler's flat name map. Closure parameters
    /// can replace a name temporarily; other live duplicates are E0104.
    live: HashMap<String, DefId>,
    blocks: Vec<ScopeFrame>,
    binding_visibility: Vec<(DefId, Range<usize>)>,
    occurrences: Vec<Occurrence>,
    closure_bodies: Vec<ClosureBody>,
    specialization_arguments: HashMap<usize, Arc<[Range<usize>]>>,
    pointer_cast_targets: HashMap<usize, bool>,
    closure_headers: HashMap<usize, Arc<Expression>>,
    expression_types: HashMap<Range<usize>, ExpressionType>,
    scalar_bindings: HashMap<DefId, Scalar>,
    diagnostics: Vec<Diagnostic>,
}

type ScopeFrame = (Range<usize>, Vec<(String, Option<DefId>)>);

impl Walker<'_> {
    #[allow(clippy::too_many_arguments)]
    fn declare(&mut self, name: String, kind: DefKind, range: Range<usize>, mutable: bool, writable_through: bool, inferred: Option<TypeSpelling>, visible_from: usize) -> DefId {
        let id = DefId(self.base + self.locals.len() as u32);
        self.locals.push(DefEntry { id, name: name.clone(), kind, range: range.clone(), owner: Some(self.function_def), mutable, writable_through, inferred, imported: false });
        if kind != DefKind::ClosureParameter && self.live.contains_key(&name) {
            self.diagnostics.push(
                ae_syntax::diagnostic_range(
                    &self.module.file, &self.module.source, range,
                    &format!("duplicate variable '{name}'"),
                ).with_code("E0104"),
            );
        } else {
            let previous = self.live.insert(name.clone(), id);
            if let Some((scope, bindings)) = self.blocks.last_mut() {
                bindings.push((name, previous));
                self.binding_visibility.push((id, visible_from.max(scope.start)..scope.end));
            }
        }
        id
    }

    fn occurrence(&mut self, name: &str, range: Range<usize>, role: Role, mode: Lookup) -> Option<DefId> {
        let target = match mode {
            Lookup::Normal => self.live.get(name).copied().or_else(|| self.items.get(name).copied()),
            Lookup::LocalOnly => self.live.get(name).copied(),
            Lookup::ItemsOnly => self.items.get(name).copied(),
        };
        if target.is_none() && mode == Lookup::LocalOnly {
            self.diagnostics.push(
                ae_syntax::diagnostic_range(
                    &self.module.file, &self.module.source, range.clone(),
                    &format!("unknown variable '{name}'"),
                ).with_code("E0101"),
            );
        }
        self.occurrences.push(Occurrence { range, name: name.to_owned(), role, target });
        target
    }

    /// The shape an initializer carries, as far as the bounded inference
    /// subset can see (ADR-0229 slice 1). `None` is honest `deferred`:
    /// the initializer needs a rule the subset does not have yet. `root`
    /// is what the walk resolved the initializer's root name to - a
    /// copied binding's def or a called function's def.
    fn infer_expression(&mut self, expression: &Expression) -> Option<TypeSpelling> {
        match &expression.kind {
            ExpressionKind::Atom(Kind::Number(_)) => Some(TypeSpelling::Primitive("i64")),
            ExpressionKind::Atom(Kind::Float(_)) => Some(TypeSpelling::Primitive("f64")),
            ExpressionKind::Atom(Kind::Text(_)) => Some(TypeSpelling::Primitive("string")),
            ExpressionKind::Atom(Kind::Char(_)) => Some(TypeSpelling::Primitive("char")),
            ExpressionKind::Atom(Kind::True) | ExpressionKind::Atom(Kind::False) => Some(TypeSpelling::Primitive("bool")),
            ExpressionKind::Unit => Some(TypeSpelling::Primitive("()")),
            // A name read resolves through the live bindings — the walk is
            // at this statement, so the map answers exactly what the
            // compiler's single pass would see here. Module items and the
            // compiler's prelude types follow.
            ExpressionKind::Atom(Kind::Ident(name)) => {
                if let Some(def) = self.live.get(name) {
                    return self.defs_local(*def).and_then(|entry| entry.inferred.clone());
                }
                if let Some(def) = self.items.get(name) {
                    if let Some(declaration) = self.module.declarations.iter()
                        .find(|d| d.name == *name && matches!(d.kind, DeclarationKind::Function { .. }))
                        && let DeclarationKind::Function { parameters, returns, unsafe_fn, abi } = &declaration.kind
                    {
                            if *unsafe_fn || abi.is_some() || !declaration.type_parameters.is_empty() {
                                return None;
                            }
                            let param_shapes: Vec<TypeSpelling> = parameters.iter()
                                .map(|p| classify(self.tokens, &|n: &str| self.items.get(n).copied(), &[], None, p.ty.clone()).spelling)
                                .collect();
                            let ret_shape = match returns.clone() {
                                Some(range) if range.start != range.end => {
                                    classify(self.tokens, &|n: &str| self.items.get(n).copied(), &[], None, range).spelling
                                }
                                _ => TypeSpelling::Primitive("()"),
                            };
                            return Some(TypeSpelling::FnValue { parameters: param_shapes, returns: Box::new(ret_shape) });
                    }
                    return Some(TypeSpelling::Named { def: *def });
                }
                if let Some(prelude) = crate::types::PRELUDE_TYPES.iter().find(|prelude| **prelude == name) {
                    return Some(TypeSpelling::Prelude { name: prelude, arguments: Vec::new() });
                }
                None
            }
            ExpressionKind::Prefix { operator, operand } if matches!(operator, Kind::Bang | Kind::Minus) => {
                let TypeSpelling::Primitive(name) = self.infer_expression(operand)? else { return None };
                let scalar = crate::typecheck::Scalar::from_spelling(name)?;
                let result = crate::typecheck::prefix(operator, Some(scalar)).ok()?;
                Some(TypeSpelling::Primitive(result.spelling()))
            }
            ExpressionKind::Propagate(inner) | ExpressionKind::Group(inner) => self.infer_expression(inner),
            ExpressionKind::Binary { left, operator, right } => self.infer_binary(left, operator, right),
            // `&v as *mut T` / `expr as *mut T`: the cast target is the
            // shape — the pointee spelling classifies directly.
            ExpressionKind::AddressCast { mutable, pointee, .. } => {
                let mutable = *mutable;
                let shape = classify(self.tokens, &|name: &str| self.items.get(name).copied(), &self.type_params, self.self_name.as_deref(), pointee.clone()).spelling;
                Some(TypeSpelling::Pointer { mutable, pointee: Box::new(shape) })
            }
            ExpressionKind::PointerCast { mutable, pointee, .. } => {
                let mutable = *mutable;
                let shape = classify(self.tokens, &|name: &str| self.items.get(name).copied(), &self.type_params, self.self_name.as_deref(), pointee.clone()).spelling;
                Some(TypeSpelling::Pointer { mutable, pointee: Box::new(shape) })
            }
            ExpressionKind::ScalarCast { target, .. } => {
                let name = self.module.source.get(target.clone())?.trim();
                let scalar = crate::typecheck::Scalar::from_spelling(name)?;
                matches!(scalar, crate::typecheck::Scalar::Int(_) | crate::typecheck::Scalar::Float(_) | crate::typecheck::Scalar::Char)
                    .then(|| TypeSpelling::Primitive(scalar.spelling()))
            }
            // Field and index reads infer through the base's shape.
            ExpressionKind::Field { base, name } => {
                let base_shape = self.infer_expression(base)?;
                self.field_shape(&base_shape, name)
            }
            ExpressionKind::Index { base, .. } => {
                let base_shape = self.infer_expression(base)?;
                match base_shape {
                    TypeSpelling::Composite { family: "vec", arguments } => arguments.into_iter().next(),
                    TypeSpelling::Composite { family: "map", arguments } => arguments.into_iter().nth(1),
                    _ => None,
                }
            }
            // A generic value keeps its instantiated shape
            // (`Option[i64]::Some(3)` is an `Option[i64]`).
            ExpressionKind::Qualified { base, .. } => self.infer_expression(base),
            ExpressionKind::Specialization { base, arguments } => {
                let shape = self.infer_expression(base)?;
                let converted: Vec<TypeSpelling> = arguments.iter()
                    .map(|argument| classify(self.tokens, &|name: &str| self.items.get(name).copied(), &self.type_params, self.self_name.as_deref(), argument.clone()).spelling)
                    .collect();
                Some(match shape {
                    TypeSpelling::Named { def } => TypeSpelling::GenericInstance { def, arguments: converted },
                    TypeSpelling::Prelude { name, .. } => TypeSpelling::Prelude { name, arguments: converted },
                    other => other,
                })
            }
            ExpressionKind::Call { callee, .. } => {
                // A variant-constructor call carries the enum's
                // instantiated shape (`Opt[string]::Some("a")` is an
                // `Option[string]`).
                if let ExpressionKind::Qualified { base, name } = &callee.kind {
                    let base_shape = self.infer_expression(base)?;
                    let is_variant = match &base_shape {
                        TypeSpelling::Named { def } | TypeSpelling::GenericInstance { def, .. } => self.variants.get(def)
                            .is_some_and(|table| table.contains_key(name)),
                        TypeSpelling::Prelude { name: prelude_name, .. } => {
                            prelude_name == &"Option" && matches!(name.as_str(), "Some" | "None")
                        }
                        _ => false,
                    };
                    if is_variant {
                        return Some(base_shape);
                    }
                    return None;
                }
                if let ExpressionKind::Field { base, name } = &callee.kind {
                    if matches!(
                        name.as_str(),
                        "wrapping_add" | "wrapping_sub" | "wrapping_mul" | "wrapping_neg"
                            | "saturating_add" | "saturating_sub" | "saturating_mul"
                    ) {
                        return self.infer_expression(base);
                    }
                    if matches!(
                        name.as_str(),
                        "checked_add" | "checked_sub" | "checked_mul" | "checked_div" | "checked_rem" | "checked_neg"
                    ) {
                        let base_shape = self.infer_expression(base)?;
                        return Some(TypeSpelling::Prelude { name: "Option", arguments: vec![base_shape] });
                    }
                    return None;
                }
                // The root names the module function being called; its
                // return spelling is the shape.
                let ExpressionKind::Atom(Kind::Ident(fn_name)) = &callee.kind else {
                    return None;
                };
                let declaration = self.module.declarations.iter()
                    .find(|declaration| declaration.name == *fn_name && matches!(declaration.kind, DeclarationKind::Function { .. }))?;
                let DeclarationKind::Function { returns, .. } = &declaration.kind else {
                    return None;
                };
                let Some(range) = returns.clone() else {
                    return (declaration.name != "main").then_some(TypeSpelling::Primitive("()"));
                };
                let params: Vec<String> = declaration.type_parameters.iter().map(|parameter| parameter.name.clone()).collect();
                Some(classify(self.tokens, &|name: &str| self.items.get(name).copied(), &params, None, range).spelling)
            }
            ExpressionKind::Vector(items) => {
                // The first typed element decides the vector (ADR-61).
                let element = items.iter().find_map(|item| match &item.kind {
                    ExpressionKind::Atom(Kind::Number(_)) => Some(TypeSpelling::Primitive("i64")),
                    ExpressionKind::Atom(Kind::Float(_)) => Some(TypeSpelling::Primitive("f64")),
                    ExpressionKind::Atom(Kind::Text(_)) => Some(TypeSpelling::Primitive("string")),
                    _ => None,
                })?;
                Some(TypeSpelling::Composite { family: "vec", arguments: vec![element] })
            }
            ExpressionKind::Struct { target, .. } => {
                let ExpressionKind::Atom(Kind::Ident(name)) = &target.kind else {
                    return None;
                };
                Some(TypeSpelling::Named { def: self.items.get(name).copied()? })
            }
            ExpressionKind::Closure { params, return_type, .. } => {
                let lookup = |name: &str| self.items.get(name).copied();
                let mut shapes = Vec::new();
                for param in params {
                    if param.ty.start == param.ty.end {
                        return None;
                    }
                    shapes.push(classify(self.tokens, &lookup, &self.type_params, self.self_name.as_deref(), param.ty.clone()).spelling);
                }
                if return_type.start == return_type.end {
                    return None;
                }
                let returns = classify(self.tokens, &lookup, &self.type_params, self.self_name.as_deref(), return_type.clone()).spelling;
                Some(TypeSpelling::FnValue { parameters: shapes, returns: Box::new(returns) })
            }
            _ => None,
        }
    }

    fn infer_binary(&mut self, left: &Expression, operator: &Kind, right: &Expression) -> Option<TypeSpelling> {
        use crate::typecheck::{Scalar, infix};
        let (TypeSpelling::Primitive(left), TypeSpelling::Primitive(right)) =
            (self.infer_expression(left)?, self.infer_expression(right)?) else { return None };
        if left == "string" && right == "string"
            && matches!(operator, Kind::EqEq | Kind::NotEq | Kind::Less | Kind::LessEq | Kind::Greater | Kind::GreaterEq)
        {
            return Some(TypeSpelling::Primitive("bool"));
        }
        let result = infix(operator, Some(Scalar::from_spelling(left)?), Some(Scalar::from_spelling(right)?)).ok()?;
        Some(TypeSpelling::Primitive(result.spelling()))
    }

    /// The shape of a field read through a base shape: named structs look
    /// their field up in the declaration; borrowed receivers read through
    /// (the compiler auto-borrows `&self` fields).
    fn field_shape(&mut self, base_shape: &TypeSpelling, name: &str) -> Option<TypeSpelling> {
        let def = match base_shape {
            TypeSpelling::Named { def } => Some(*def),
            TypeSpelling::Borrow { inner, .. } => match &**inner {
                TypeSpelling::Named { def } => Some(*def),
                _ => None,
            },
            _ => None,
        }?;
        let DefId(index) = def;
        let struct_name = self.defs.get(index as usize)?.name.clone();
        let declaration = self.module.declarations.iter()
            .find(|declaration| declaration.name == struct_name && matches!(declaration.kind, DeclarationKind::Struct { .. }))?;
        let DeclarationKind::Struct { fields } = &declaration.kind else {
            return None;
        };
        let field = fields.iter().find(|field| field.name == name)?;
        Some(classify(self.tokens, &|lookup_name: &str| self.items.get(lookup_name).copied(), &declaration.type_parameters.iter().map(|parameter| parameter.name.clone()).collect::<Vec<String>>(), None, field.ty.clone()).spelling)
    }

    /// The compiler's value-arity rule at name level: a call whose callee
    /// resolved to a module function supplies exactly that function's
    /// parameter count (`function '{name}' expects {n} arguments, got {m}`).
    /// Method calls through a receiver are Field callees and stay out —
    /// their arity is type-checker material.
    fn call_arity(&mut self, def: DefId, supplied: usize, range: Range<usize>) {
        let Some(entry) = self.defs.get(def.0 as usize) else { return };
        if entry.kind != DefKind::Function || entry.imported {
            return;
        }
        let Some(declaration) = self.module.declarations.iter()
            .find(|declaration| declaration.name == entry.name && matches!(declaration.kind, DeclarationKind::Function { .. }))
        else {
            return;
        };
        let DeclarationKind::Function { parameters, .. } = &declaration.kind else {
            return;
        };
        if parameters.len() != supplied {
            self.diagnostics.push(
                ae_syntax::diagnostic_range(
                    &self.module.file, &self.module.source, range,
                    &format!("function '{}' expects {} arguments, got {}", entry.name, parameters.len(), supplied),
                ),
            );
        }
    }

    /// `unsafe fn` and `extern "C"` declarations of this module require an
    /// enclosing unsafe block at their call sites (the compiler's E0701).
    fn unsafe_call_gate(&mut self, def: DefId, range: Range<usize>) {
        let Some(entry) = self.defs.get(def.0 as usize) else { return };
        if entry.kind != DefKind::Function || entry.imported {
            return;
        }
        let Some(declaration) = self.module.declarations.iter()
            .find(|declaration| declaration.name == entry.name && matches!(declaration.kind, DeclarationKind::Function { .. }))
        else {
            return;
        };
        let DeclarationKind::Function { unsafe_fn, abi, .. } = &declaration.kind else {
            return;
        };
        if *unsafe_fn || abi.is_some() {
            self.diagnostics.push(
                ae_syntax::diagnostic_range(
                    &self.module.file, &self.module.source, range,
                    &format!("call to unsafe function '{}' requires 'unsafe'", entry.name),
                ).with_code("E0701"),
            );
        }
    }

    /// Struct literal fields against the declared struct: unknown names,
    /// names specified twice, and declared fields the literal never
    /// covers, with the compiler's texts (probes u1-u3).
    fn struct_literal_fields(&mut self, def: DefId, fields: &[ae_syntax::expression::FieldInitializer]) {
        let Some(entry) = self.defs.get(def.0 as usize) else { return };
        if entry.imported {
            return;
        }
        let struct_name = entry.name.clone();
        let Some(declaration) = self.module.declarations.iter()
            .find(|declaration| declaration.name == struct_name && matches!(declaration.kind, DeclarationKind::Struct { .. }))
        else {
            return;
        };
        let DeclarationKind::Struct { fields: declared } = &declaration.kind else {
            return;
        };
        let mut covered: Vec<String> = Vec::new();
        for field in fields {
            if !declared.iter().any(|declared_field| declared_field.name == field.name) {
                self.diagnostics.push(
                    ae_syntax::diagnostic_range(
                        &self.module.file, &self.module.source, field.name_range.clone(),
                        &format!("struct '{struct_name}' has no field '{}'", field.name),
                    ),
                );
                continue;
            }
            if covered.contains(&field.name) {
                self.diagnostics.push(
                    ae_syntax::diagnostic_range(
                        &self.module.file, &self.module.source, field.name_range.clone(),
                        &format!("field '{}' specified twice", field.name),
                    ),
                );
            }
            covered.push(field.name.clone());
        }
        for declared_field in declared {
            if !covered.contains(&declared_field.name) {
                self.diagnostics.push(
                    ae_syntax::diagnostic_range(
                        &self.module.file, &self.module.source, declared_field.name_range.clone(),
                        &format!("missing field '{}' in '{} construction'", declared_field.name, struct_name),
                    ),
                );
            }
        }
    }

    /// The compiler's type-arity rule at name level: a specialization of a
    /// module item supplies exactly that item's type-parameter count
    /// (`'{name}' takes {n} type argument(s), found {m}`). Prelude types
    /// are not module items and stay out.
    fn type_argument_arity(&mut self, def: DefId, supplied: usize, range: Range<usize>) {
        let Some(entry) = self.defs.get(def.0 as usize) else { return };
        if entry.imported {
            return;
        }
        let expected = match entry.kind {
            DefKind::Struct | DefKind::Enum | DefKind::Function | DefKind::Impl => {
                self.module.declarations.iter()
                    .find(|declaration| declaration.name == entry.name)
                    .map(|declaration| declaration.type_parameters.len())
                    .unwrap_or(0)
            }
            _ => return,
        };
        if expected != supplied {
            self.diagnostics.push(
                ae_syntax::diagnostic_range(
                    &self.module.file, &self.module.source, range,
                    &format!("'{}' takes {} type argument(s), found {}", entry.name, expected, supplied),
                ),
            );
        }
    }

    /// Whether every path through these statements ends in a return.
    /// A `return` anywhere in the list decides (what follows is
    /// unreachable); an `if` needs both branches; a `match` needs every
    /// arm; an `unsafe` block recurses; a `while` never guarantees —
    /// even `while true` is rejected by the compiler (probe r2).
    fn definitely_returns(statements: &[Statement]) -> bool {
        for statement in statements {
            match &statement.kind {
                StatementKind::Return(_) => return true,
                StatementKind::If { then_body, else_body, .. } => {
                    let then_returns = Walker::definitely_returns(then_body);
                    let else_returns = match else_body {
                        Some(ElseBranch::Block(body)) => Walker::definitely_returns(body),
                        Some(ElseBranch::If(nested)) => Walker::definitely_returns(std::slice::from_ref(nested)),
                        None => false,
                    };
                    if then_returns && else_returns {
                        return true;
                    }
                }
                StatementKind::Match { arms, .. } if !arms.is_empty() => {
                    if arms.iter().all(|arm| Walker::definitely_returns(&arm.body)) {
                        return true;
                    }
                }
                StatementKind::Unsafe(body)
                    if Walker::definitely_returns(body) => {
                        return true;
                    }
                _ => {}
            }
        }
        false
    }

    /// A `&mut binding` / `&mut binding as *mut T` needs a `var` binding:
    /// the compiler's `cannot take a mutable pointer to an immutable
    /// binding; declare it with 'var'` (probe b1).
    fn mutable_borrow_gate(&mut self, target: Option<DefId>, range: Range<usize>) {
        let Some(def) = target.and_then(|id| self.defs_local(id)) else {
            return;
        };
        if !def.mutable {
            self.diagnostics.push(
                ae_syntax::diagnostic_range(
                    &self.module.file, &self.module.source, range,
                    "cannot take a mutable pointer to an immutable binding; declare it with 'var'",
                ),
            );
        }
    }

    /// A write to a binding that permits neither rebinding nor writes
    /// through is the compiler's "cannot assign to immutable variable"
    /// (probes: plain parameters, `let` bindings, `&self` receivers reject;
    /// `var` bindings and `&mut T` parameters accept).
    fn writability(&mut self, target: Option<DefId>, name: &str, range: Range<usize>, through: bool) {
        let Some(def_id) = target else { return };
        let Some(entry) = self.defs_local(def_id) else { return };
        let allowed = if through { entry.writable_through } else { entry.mutable };
        if !allowed {
            self.diagnostics.push(
                ae_syntax::diagnostic_range(
                    &self.module.file, &self.module.source, range,
                    &format!("cannot assign to immutable variable '{name}'"),
                ),
            );
        }
    }

    /// A local's def entry; items have no mutability to check.
    fn defs_local(&self, id: DefId) -> Option<&DefEntry> {
        let offset = id.0 as usize;
        if offset >= self.base as usize {
            self.locals.get(offset - self.base as usize)
        } else {
            None
        }
    }

    fn close_scope(&mut self) {
        let Some((_, bindings)) = self.blocks.pop() else {
            return;
        };
        for (name, previous) in bindings.into_iter().rev() {
            if let Some(previous) = previous {
                self.live.insert(name, previous);
            } else {
                self.live.remove(&name);
            }
        }
    }

    fn block_range(&self, header_end: usize, body: &[Statement]) -> Range<usize> {
        let next = self.tokens.partition_point(|token| token.offset < header_end);
        let Some(rel_open) = self.tokens[next..].iter().position(|token| token.kind == Kind::LBrace) else {
            let end = body.last().map_or(header_end, |last| last.range.end);
            return header_end..end.max(header_end);
        };
        let open = next + rel_open;
        let close = body.last().map_or(open + 1, |last| {
            self.tokens.partition_point(|token| token.offset < last.range.end)
        });
        let end = self.tokens.get(close).or_else(|| self.tokens.get(open)).map_or(header_end, |token| token.end);
        self.tokens[open].end..end.max(self.tokens[open].end)
    }

    fn block(&mut self, body: &[Statement], range: Range<usize>) {
        self.blocks.push((range, Vec::new()));
        for statement in body {
            self.statement(statement);
        }
        self.close_scope();
    }

    /// A loop binding lives exactly for its body: declared into the body's
    /// own block, gone when the block ends (the compiler inserts before the
    /// body and removes after the loop).
    fn scoped_binding(&mut self, binding: &str, binding_range: Range<usize>, body: &[Statement], scope: Range<usize>, inferred: Option<TypeSpelling>) {
        let visible_from = scope.start;
        self.blocks.push((scope, Vec::new()));
        self.declare(binding.to_owned(), DefKind::ForBinding, binding_range, false, false, inferred, visible_from);
        for statement in body {
            self.statement(statement);
        }
        self.close_scope();
    }

    fn statement(&mut self, statement: &Statement) {
        match &statement.kind {
            StatementKind::Binding { mutable, name, name_range, ty, initializer } => {
                self.expr(initializer, Role::Read);
                // A `let v: Type = …` annotation is the binding's declared
                // shape: it wins over the initializer's inference and joins
                // the spelling table (so hover and type dumps show it).
                let annotated = ty.as_ref().map(|range| {
                    let spelled = classify(self.tokens, &|name: &str| self.items.get(name).copied(), &self.type_params, self.self_name.as_deref(), range.clone());
                    let spelling = spelled.spelling.clone();
                    self.spellings.push(SpellingEntry {
                        owner: self.function_def,
                        role: SpellingRole::LocalBinding,
                        name: name.clone(),
                        spelled,
                    });
                    spelling
                });
                let inferred = annotated.or_else(|| self.infer_expression(initializer));
                let binding = self.declare(name.clone(), DefKind::Binding, name_range.clone(), *mutable, *mutable, inferred, statement.range.end);
                if let Some(ExpressionType::Checked(scalar)) = self.expression_types.get(&initializer.range) {
                    self.scalar_bindings.insert(binding, *scalar);
                }
            }
            StatementKind::Assign { target, value, .. } => {
                self.assign_target(target);
                self.expr(value, Role::Read);
            }
            StatementKind::PointerStore { pointer, pointer_range, value } => {
                // Writing through the pointer does not modify the pointer
                // binding, so an immutable `let` pointer stays writable
                // through — the compiler agrees.
                self.occurrence(pointer, pointer_range.clone(), Role::DerefWrite, Lookup::LocalOnly);
                self.expr(value, Role::Read);
            }
            StatementKind::Print(value) | StatementKind::Assert(value) => {
                self.expr(value, Role::Read);
            }
            StatementKind::If { condition, then_body, else_body } => {
                self.expr(condition, Role::Read);
                self.condition_check(condition);
                let then_range = self.block_range(condition.range.end, then_body);
                let then_end = then_range.end;
                self.block(then_body, then_range);
                match else_body {
                    Some(ElseBranch::Block(body)) => self.block(body, self.block_range(then_end, body)),
                    Some(ElseBranch::If(nested)) => self.statement(nested),
                    None => {}
                }
            }
            StatementKind::While { condition, body } => {
                self.expr(condition, Role::Read);
                self.condition_check(condition);
                self.block(body, self.block_range(condition.range.end, body));
            }
            StatementKind::ForRange { binding, binding_range, start, end, body } => {
                self.expr(start, Role::Read);
                self.expr(end, Role::Read);
                self.scoped_binding(binding, binding_range.clone(), body, self.block_range(end.range.end, body), Some(TypeSpelling::Primitive("i64")));
            }
            StatementKind::ForIn { binding, binding_range, iterable, body } => {
                self.expr(iterable, Role::Read);
                // A vector iterable hands its element type to the binding.
                let element = match self.infer_expression(iterable) {
                    Some(TypeSpelling::Composite { family: "vec", arguments }) => arguments.into_iter().next(),
                    _ => None,
                };
                self.scoped_binding(binding, binding_range.clone(), body, self.block_range(iterable.range.end, body), element);
            }
            StatementKind::Match { subject, arms } => {
                self.expr(subject, Role::Read);
                let subject_shape = self.infer_expression(subject);
                self.match_subject_check(subject, &subject_shape);
                self.exhaustiveness(subject, arms);
                for arm in arms {
                    self.blocks.push((self.block_range(arm.pattern.range.end, &arm.body), Vec::new()));
                    self.pattern(&arm.pattern, subject_shape.clone());
                    for statement in &arm.body {
                        self.statement(statement);
                    }
                    self.close_scope();
                }
            }
            StatementKind::Unsafe(body) => {
                self.unsafe_depth += 1;
                self.block(body, self.block_range(statement.range.start, body));
                self.unsafe_depth -= 1;
            }
            StatementKind::Break | StatementKind::Continue => {}
            StatementKind::Expr { value, .. } => {
                self.expr(value, Role::Read);
            }
            StatementKind::Return(value) => {
                self.expr(value, Role::Read);
                // A returned shape must agree with the declaration when
                // both are known; a deferred shape stays silent.
                if let Some(declared) = self.declared_return.clone()
                    && !matches!(declared, TypeSpelling::Unknown(_))
                {
                    let returned = self.infer_expression(value);
                    if let Some(returned) = returned
                        && returned != declared {
                            self.diagnostics.push(
                                ae_syntax::diagnostic_range(
                                    &self.module.file, &self.module.source, statement.range.clone(),
                                    &format!("return type must be {}", declared.describe()),
                                ),
                            );
                        }
                }
            }
        }
    }

    /// A condition position (`if`, `while`) needs a boolean expression —
    /// the compiler's text applies when the shape is known; a deferred
    /// shape stays silent.
    fn condition_check(&mut self, condition: &Expression) {
        let Some(shape) = self.infer_expression(condition) else {
            return;
        };
        if !matches!(shape, TypeSpelling::Primitive("bool")) {
            self.diagnostics.push(
                ae_syntax::diagnostic_range(
                    &self.module.file, &self.module.source, condition.range.clone(),
                    "condition must be a boolean expression",
                ),
            );
        }
    }

    /// The compiler's rule: the match subject is a bare enum variable
    /// (`match needs a bare enum variable as its subject`, probes b2/b3 —
    /// even a field read of an enum value is rejected). A known shape that
    /// is not an enum value fails; unknown and deferred shapes stay silent.
    fn match_subject_check(&mut self, subject: &Expression, subject_shape: &Option<TypeSpelling>) {
        let bare_variable = matches!(&subject.kind, ExpressionKind::Atom(Kind::Ident(_)));
        let is_enum_value = match subject_shape {
            Some(TypeSpelling::Named { def }) => self.defs.get(def.0 as usize)
                .is_some_and(|entry| entry.kind == DefKind::Enum),
            Some(TypeSpelling::GenericInstance { def, .. }) => self.defs.get(def.0 as usize)
                .is_some_and(|entry| entry.kind == DefKind::Enum),
            Some(TypeSpelling::Prelude { .. }) => true,
            _ => false,
        };
        if bare_variable && !is_enum_value && subject_shape.is_some() {
            self.diagnostics.push(
                ae_syntax::diagnostic_range(
                    &self.module.file, &self.module.source, subject.range.clone(),
                    "match needs a bare enum variable as its subject",
                ),
            );
        }
    }

    /// Every variant of the matched enum needs an arm unless an arm binds
    /// the whole value (`Binding` patterns are catch-alls). Generic enums
    /// name the missing variant with their instantiated arguments.
    fn exhaustiveness(&mut self, subject: &Expression, arms: &[ae_syntax::statement::MatchArm]) {
        let subject_shape = self.infer_expression(subject);
        let (enum_def, type_arguments, generic) = match subject_shape {
            Some(TypeSpelling::Named { def }) => (def, Vec::new(), false),
            Some(TypeSpelling::GenericInstance { def, arguments }) => (def, arguments, true),
            _ => return,
        };
        let Some(variants) = self.variants.get(&enum_def) else {
            return;
        };
        let type_arguments = &type_arguments;
        let enum_entry = self.defs.get(enum_def.0 as usize);
        let mut covered: Vec<String> = Vec::new();
        let mut catch_all = false;
        for arm in arms {
            match &arm.pattern.kind {
                PatternKind::Binding(_) => catch_all = true,
                PatternKind::Variant { variant, .. } => covered.push(variant.clone()),
            }
        }
        if catch_all {
            return;
        }
        let enum_name = enum_entry.map(|entry| entry.name.clone()).unwrap_or_default();

        for variant_name in variants.keys() {
            if covered.iter().any(|covered_name| covered_name == variant_name) {
                continue;
            }
            let spelled = if generic {
                let arguments: Vec<String> = type_arguments.iter().map(|argument| argument.describe()).collect();
                format!("{enum_name}[{}]::{variant_name}", arguments.join(", "))
            } else {
                format!("{enum_name}::{variant_name}")
            };
            self.diagnostics.push(
                ae_syntax::diagnostic_range(
                    &self.module.file, &self.module.source, subject.range.clone(),
                    &format!("match is not exhaustive: '{spelled}' is not handled"),
                ),
            );
        }
    }

    /// Match-arm patterns bind names and point at variants. `inferred` is
    /// the shape a top-level binding pattern takes (the subject's); variant
    /// payloads type from the enum declaration's payload spellings, in
    /// payload order.
    fn pattern(&mut self, pattern: &Pattern, subject_shape: Option<TypeSpelling>) {
        match &pattern.kind {
            PatternKind::Binding(name) => {
                // The pattern covers exactly the binding's name token.
                self.declare(name.clone(), DefKind::PatternBinding, pattern.range.clone(), false, false, subject_shape, pattern.range.end);
            }
            PatternKind::Variant { enum_name, variant, variant_range, payload } => {
                if let Some(enum_id) = self.items.get(enum_name)
                    && let Some(table) = self.variants.get(enum_id)
                        && let Some(variant_id) = table.get(variant) {
                            self.occurrences.push(Occurrence {
                                range: variant_range.clone(),
                                name: variant.clone(),
                                role: Role::Read,
                                target: Some(*variant_id),
                            });
                        }
                // The payload bindings carry the variant's payload spellings,
                // classified against the enum's own type parameters.
                let (_enum_params, subject_arguments) = match &subject_shape {
                    Some(TypeSpelling::GenericInstance { def, arguments }) => {
                        let params = self.defs.get(def.0 as usize).map(|entry| entry.name.clone())
                            .and_then(|name| self.module.declarations.iter()
                                .find(|declaration| declaration.name == name))
                            .map(|declaration| declaration.type_parameters.iter().map(|parameter| parameter.name.clone()).collect())
                            .unwrap_or_default();
                        (params, arguments.clone())
                    }
                    // The prelude's `Option[T]` is the one generic whose
                    // payloads types come from the subject.
                    Some(TypeSpelling::Prelude { name, arguments }) if *name == "Option" => {
                        (vec!["T".to_owned()], arguments.clone())
                    }
                    _ => (Vec::new(), Vec::new()),
                };
                let payload_shapes: Vec<Option<TypeSpelling>> = self.module.declarations.iter()
                    .find(|declaration| declaration.name == *enum_name && matches!(declaration.kind, DeclarationKind::Enum { .. }))
                    .and_then(|declaration| match &declaration.kind {
                        DeclarationKind::Enum { variants } => variants.iter()
                            .find(|candidate| candidate.name == *variant)
                            .map(|found| found.payload.clone()),
                        _ => None,
                    })
                    .map(|payload_ranges| {
                        let enum_params: Vec<String> = self.module.declarations.iter()
                            .find(|declaration| declaration.name == *enum_name)
                            .map(|declaration| declaration.type_parameters.iter().map(|parameter| parameter.name.clone()).collect())
                            .unwrap_or_default();
                        payload_ranges.iter()
                            .map(|range| {
                                let shape = classify(self.tokens, &|lookup: &str| self.items.get(lookup).copied(), &enum_params, None, range.clone()).spelling;
                                Some(crate::types::substitute(shape, &enum_params, &subject_arguments))
                            })
                            .collect()
                    })
                    // The compiler's prelude enums have no declaration here:
                    // `Option[T]`'s payloads are the subject's arguments in
                    // order (one for `Some`, none for `None`).
                    .or_else(|| {
                        if subject_arguments.is_empty() {
                            return None;
                        }
                        Some(payload.iter().map(|_| Some(subject_arguments.first().cloned()).flatten()).collect())
                    })
                    .unwrap_or_default();
                if let Some(payload) = payload {
                    for (index, inner) in payload.iter().enumerate() {
                        let shape = payload_shapes.get(index).cloned().flatten();
                        self.pattern(inner, shape);
                    }
                }
            }
        }
    }

    /// Walk one expression and return what its root name resolved to, so a
    /// `Qualified` member can look up its qualifier. Only name-rooted shapes
    /// forward a target; composites return `None`.
    fn expr(&mut self, expression: &Expression, role: Role) -> Option<DefId> {
        let target = self.walk_expression(expression, role);
        let checked = self.check_scalar_expression(expression, target);
        self.expression_types.insert(expression.range.clone(), checked);
        target
    }

    fn check_scalar_expression(&self, expression: &Expression, target: Option<DefId>) -> ExpressionType {
        use ExpressionType::{Checked, Deferred, Rejected};
        let checked_child = |child: &Expression| match self.expression_types.get(&child.range) {
            Some(Checked(scalar)) => Some(*scalar),
            _ => None,
        };
        let result = match &expression.kind {
            ExpressionKind::Atom(Kind::Number(_)) => return Checked(Scalar::Int(ae_ty::IntTy::I64)),
            ExpressionKind::Atom(Kind::Float(_)) => return Checked(Scalar::Float(ae_ty::FloatTy::F64)),
            ExpressionKind::Atom(Kind::Char(_)) => return Checked(Scalar::Char),
            ExpressionKind::Atom(Kind::True | Kind::False) => return Checked(Scalar::Bool),
            ExpressionKind::Atom(Kind::Ident(_)) => {
                let scalar = target.and_then(|id| {
                    if let Some(scalar) = self.scalar_bindings.get(&id) {
                        return Some(*scalar);
                    }
                    let entry = self.defs_local(id)?;
                    let Some(TypeSpelling::Primitive(name)) = entry.inferred.as_ref() else { return None };
                    if matches!(entry.kind, DefKind::Parameter | DefKind::ClosureParameter) {
                        Scalar::from_spelling(name)
                    } else {
                        None
                    }
                });
                return scalar.map_or(Deferred, Checked);
            }
            ExpressionKind::Group(inner) => return checked_child(inner).map_or(Deferred, Checked),
            ExpressionKind::Prefix { operator, operand } if matches!(operator, Kind::Bang | Kind::Minus) => {
                let Some(operand) = checked_child(operand) else { return Deferred };
                typecheck::prefix(operator, Some(operand))
            }
            ExpressionKind::Binary { left, operator, right } => {
                let (Some(left), Some(right)) = (checked_child(left), checked_child(right)) else { return Deferred };
                typecheck::infix(operator, Some(left), Some(right))
            }
            ExpressionKind::ScalarCast { base, target } => {
                let Some(target_scalar) = self.module.source.get(target.clone())
                    .and_then(|name| Scalar::from_spelling(name.trim())) else { return Deferred };
                let Some(base_scalar) = checked_child(base) else { return Deferred };
                if !ae_ty::valid_scalar_cast(base_scalar, target_scalar) { return Deferred; }
                return Checked(target_scalar);
            }
            _ => return Deferred,
        };
        result.map_or_else(Rejected, Checked)
    }

    fn walk_expression(&mut self, expression: &Expression, role: Role) -> Option<DefId> {
        match &expression.kind {
            ExpressionKind::Atom(Kind::Ident(name)) => {
                let mode = match role {
                    Role::Construct | Role::Qualifier => Lookup::ItemsOnly,
                    _ => Lookup::Normal,
                };
                self.occurrence(name, expression.range.clone(), role, mode)
            }
            ExpressionKind::Atom(_) | ExpressionKind::Unit => None,
            ExpressionKind::Group(inner) | ExpressionKind::Propagate(inner) => self.expr(inner, role),
            ExpressionKind::AddressCast { binding, binding_range, mutable, pointee } => {
                self.pointer_cast_targets.insert(pointee.start, *mutable);
                let range = binding_range.clone();
                let mutable = *mutable;
                let target = self.occurrence(binding, range.clone(), Role::Borrow, Lookup::LocalOnly);
                if mutable {
                    self.mutable_borrow_gate(target, range);
                }
                None
            }
            ExpressionKind::PointerCast { base, mutable, pointee } => {
                self.pointer_cast_targets.insert(pointee.start, *mutable);
                self.expr(base, role)
            }
            // A scalar cast names a type, not a value: only the base is walked.
            ExpressionKind::ScalarCast { base, .. } => self.expr(base, role),
            ExpressionKind::BorrowArgument { name, name_range, mutable, index, .. } => {
                let mutable = *mutable;
                let target = self.occurrence(name, name_range.clone(), Role::Borrow, Lookup::LocalOnly);
                if mutable {
                    self.mutable_borrow_gate(target, name_range.clone());
                }
                // `&name[index]` reads the index; `&name[start..end]` reads both
                // bounds (ADR-0236 rung 3). The bracket contents are a byte
                // range in the shared grammar, so parse them back into trees
                // and resolve them like any other read.
                if let Some(range) = index {
                    let parts: Vec<std::ops::Range<usize>> = match ae_syntax::expression::split_range_argument(self.tokens, range) {
                        Some((start, end)) => vec![start, end],
                        None => vec![range.clone()],
                    };
                    for part in parts {
                        let start = self.tokens.partition_point(|token| token.offset < part.start);
                        if let Ok((expression, _)) = ae_syntax::expression::parse_expression_tokens(
                            &self.module.file, &self.module.source, self.tokens, start, false,
                        ) {
                            self.expr(&expression, Role::Read);
                        }
                    }
                }
                None
            }
            ExpressionKind::Array(items) | ExpressionKind::Vector(items) => {
                for item in items {
                    self.expr(item, Role::Read);
                }
                None
            }
            ExpressionKind::Struct { target, fields } => {
                let construct_target = self.expr(target, Role::Construct);
                if let Some(def) = construct_target {
                    self.struct_literal_fields(def, fields);
                }
                for field in fields {
                    self.expr(&field.value, Role::Read);
                }
                construct_target
            }
            ExpressionKind::Call { callee, arguments } => {
                let callee_target = self.expr(callee, Role::Call);
                if self.unsafe_depth == 0 {
                    // Statement-position pointer calls are unsafe on their
                    // own (the compiler's text, no code).
                    if let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind
                        && matches!(name.as_str(), "ptr_write" | "ptr_drop_in_place") {
                            self.diagnostics.push(
                                ae_syntax::diagnostic_range(
                                    &self.module.file, &self.module.source, callee.range.clone(),
                                    &format!("{} is an unsafe operation; wrap it in an 'unsafe {{ ... }}' block", name),
                                ),
                            );
                        }
                    // A module function declared `unsafe fn` or
                    // `extern "C"` needs an enclosing unsafe block (E0701).
                    if let Some(def) = callee_target {
                        self.unsafe_call_gate(def, callee.range.clone());
                    }
                }
                if let Some(def) = callee_target {
                    self.call_arity(def, arguments.len(), callee.range.clone());
                }
                for argument in arguments {
                    self.expr(argument, Role::Read);
                }
                callee_target
            }
            ExpressionKind::Index { base, index } => {
                let base_target = self.expr(base, role);
                self.expr(index, Role::Read);
                base_target
            }
            ExpressionKind::Field { base, .. } => self.expr(base, role),
            // `Name[Types]` behind a call or standalone: the grammar kept
            // the argument ranges precisely so the resolver could decide by
            // the base name (local index vs generic item).
            ExpressionKind::Specialization { base, arguments } => {
                self.specialization_arguments.insert(base.range.start, arguments.as_slice().into());
                let base_target = self.expr(base, role);
                if let Some(def) = base_target {
                    self.type_argument_arity(def, arguments.len(), base.range.clone());
                }
                base_target
            }
            ExpressionKind::BracketedCallTarget { base, arguments } => {
                self.specialization_arguments.insert(base.range.start, arguments.as_slice().into());
                self.expr(base, role)
            }
            ExpressionKind::Qualified { base, name } => {
                let base_target = self.expr(base, Role::Qualifier);
                self.qualified_member(base_target, name, role, expression.range.clone(), base.range.end)
            }
            ExpressionKind::Closure { params, body, return_type } => {
                self.closure_headers.insert(expression.range.start, Arc::new(expression.clone()));
                self.closure(params, body, return_type);
                None
            }
            ExpressionKind::Prefix { operator: _, operand } => self.expr(operand, role),
            ExpressionKind::Binary { left, right, .. } => {
                self.expr(left, Role::Read);
                self.expr(right, Role::Read);
                None
            }
        }
    }

    /// `Name::member` in expression position: a variant of a known enum or
    /// an associated function of a known type's inherent impl. The grammar
    /// keeps the member as a bare string, so its range is scanned out of the
    /// tokens between the qualifier and the end of this expression. An
    /// unknown member is recorded as an occurrence with no target — the type
    /// checker owns the rejection.
    fn qualified_member(&mut self, base: Option<DefId>, name: &str, role: Role, window: Range<usize>, after: usize) -> Option<DefId> {
        let target = base.and_then(|base_def| {
            if let Some(table) = self.variants.get(&base_def) {
                return table.get(name).copied();
            }
            self.methods.get(&base_def).and_then(|list| {
                list.iter().find(|(method_name, _)| method_name == name).map(|(_, id)| *id)
            })
        });
        let range = self.member_range(name, after, window.end);
        self.occurrences.push(Occurrence { range, name: name.to_owned(), role, target });
        target
    }

    /// A closure body is a brace-balanced range the grammar keeps opaque.
    /// The resolver parses it with the shared statement grammar inside a
    /// nested scope: the parameters bind for the body and die with it, and
    /// occurrences inside resolve against parameters, enclosing locals and
    /// items. Capture semantics stay with the type checker (R3c).
    fn closure(&mut self, params: &[ClosureParam], body: &Range<usize>, return_type: &Range<usize>) {
        let diagnostics_start = self.diagnostics.len();
        self.blocks.push((body.start + 1..body.end, Vec::new()));
        for param in params {
            let shape = if param.ty.start == param.ty.end {
                None
            } else {
                let spelled = classify(self.tokens, &|name: &str| self.items.get(name).copied(), &self.type_params, self.self_name.as_deref(), param.ty.clone());
                self.spellings.push(SpellingEntry {
                    owner: self.function_def,
                    role: SpellingRole::ClosureParameter,
                    name: param.name.clone(),
                    spelled: spelled.clone(),
                });
                Some(spelled.spelling)
            };
            self.declare(param.name.clone(), DefKind::ClosureParameter, param.name_range.clone(), false, false, shape, body.start + 1);
        }
        // The closure's return spelling sits between the parameters and the
        // body; an empty range means the spelling carries no return type.
        let closure_return = if return_type.start != return_type.end {
            let spelled = classify(self.tokens, &|name: &str| self.items.get(name).copied(), &self.type_params, self.self_name.as_deref(), return_type.clone());
            let shape = spelled.spelling.clone();
            self.spellings.push(SpellingEntry {
                owner: self.function_def,
                role: SpellingRole::ClosureReturn,
                name: String::new(),
                spelled,
            });
            Some(shape)
        } else {
            None
        };
        if let Some(open) = self.tokens.iter().position(|token| token.offset == body.start) {
            let occurrences_start = self.occurrences.len();
            let locals_start = self.locals.len();
            let prev_return = std::mem::replace(&mut self.declared_return, closure_return);
            // The grammar accepted the literal's header; a body that does
            // not parse stays opaque rather than inventing diagnostics.
            if let Ok((statements, _)) = parse_statements_until_close(&self.module.file, &self.module.source, self.tokens, open + 1) {
                for statement in &statements {
                    self.statement(statement);
                }
                self.declared_return = prev_return;
                let has_binding_conflict = self.diagnostics[diagnostics_start..].iter()
                    .any(|diagnostic| diagnostic.code.as_deref() == Some("E0104"));
                // Captures: reads inside the body whose target is an
                // outer def (declared before this closure's locals
                // began). The compiler owns the capture mode.
                let mut captures: Vec<DefId> = Vec::new();
                for occurrence in &self.occurrences[occurrences_start..] {
                    if occurrence.role != Role::Read { continue; }
                    let Some(target) = occurrence.target else { continue };
                    let outer = (self.base as usize..self.base as usize + locals_start)
                        .contains(&(target.0 as usize));
                    if outer && !captures.contains(&target) {
                        captures.push(target);
                    }
                }
                self.closure_bodies.push(ClosureBody {
                    start: body.start, end: body.end, statements: statements.into(),
                    has_binding_conflict, captures,
                });
            }
        }
        self.close_scope();
    }

    fn member_range(&self, name: &str, after: usize, end: usize) -> Range<usize> {
        for token in self.tokens {
            if token.offset < after {
                continue;
            }
            if token.end > end {
                break;
            }
            if let Kind::Ident(spelled) = &token.kind
                && spelled == name {
                    return token.offset..token.end;
                }
        }
        after..after
    }

    /// The left side of an assignment: the root name must be a live local
    /// (the compiler answers `E0101` otherwise), names nested under index or
    /// field access keep the same rule, everything else stays a plain read
    /// for the type checker to reject.
    fn assign_target(&mut self, target: &Expression) {
        self.assign_target_at(target, false);
    }

    /// `through` is false for the assignment's root name (rebinding, needs
    /// `var`) and true once the name sits under field or index access
    /// (writes through the binding, needs `var` or a `&mut` / `*mut` type).
    fn assign_target_at(&mut self, target: &Expression, through: bool) {
        match &target.kind {
            ExpressionKind::Atom(Kind::Ident(name)) => {
                let resolved = self.occurrence(name, target.range.clone(), Role::Write, Lookup::LocalOnly);
                self.writability(resolved, name, target.range.clone(), through);
            }

            ExpressionKind::Group(inner) => self.assign_target_at(inner, through),
            ExpressionKind::Index { base, index } => {
                self.assign_target_at(base, true);
                self.expr(index, Role::Read);
            }
            ExpressionKind::Field { base, .. } => self.assign_target_at(base, true),
            // A call, a literal, a pointer cast … is not a writable place;
            // keep it as a read so the type checker owns the rejection.
            _ => {
                self.expr(target, Role::Read);
            }
        }
    }
}
