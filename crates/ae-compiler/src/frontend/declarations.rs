//! Function and trait declaration collection and instantiation.
use super::*;
use ae_syntax::declaration::{parse_declaration_attribute, parse_extern_function_prefix, Declaration, DeclarationAttribute, DeclarationKind, TypedName, MAX_FUNCTION_PARAMETERS};
use super::parser::moved_value_hint;

/// How a borrowed argument is passed and whether it needs a call-scoped
/// struct borrow. Classification uses concrete compiler type identities.
#[derive(Clone, Copy)]
pub(super) enum CallBorrow {
    Forward,
    Address,
    Struct { address: bool },
}

impl CallBorrow {
    pub(super) fn classify(expected: Type, actual: Type) -> Option<Self> {
        match (expected, actual) {
            (Type::VecRef, Type::Vec) | (Type::VecFRef, Type::VecF) |
            (Type::StrVecRef, Type::StrVec) => Some(Self::Address),
            (Type::VecRef | Type::VecFRef | Type::StrVecRef | Type::Array(_) | Type::StructRef(_), actual)
                if expected == actual => Some(Self::Forward),
            (Type::StructRef(wanted) | Type::StructMut(wanted), Type::Struct(found) | Type::StructMut(found))
                if wanted == found => Some(Self::Struct { address: matches!(actual, Type::Struct(_)) }),
            _ => None,
        }
    }

    pub(super) fn address(self) -> bool {
        match self {
            Self::Forward => false,
            Self::Address => true,
            Self::Struct { address } => address,
        }
    }
}

impl Parser<'_> {
    // `pub` marks an item as importable from other files. It is consumed by the
    // declaration passes, never by expression parsing.
    // The C-ABI shape of a type crossing an `extern "C"` boundary (M2.12
    // U8): scalars pass directly; a `#[repr(C)]` struct passes by value
    // when every field is itself C-shaped (recursively); raw pointers and
    // borrowed arrays are the spec's pointer forms. An owned type is
    // rejected at the declaration: the C side cannot honor its release
    // logic. The struct's own `#[repr(C)]` attribute is what makes its
    // layout a promise; without it the layout is the compiler's choice.
    #[allow(clippy::only_used_in_recursion)]
    pub(super) fn c_abi_shape(&self, ty: &Type, offset: usize) -> bool {
        match ty {
            Type::Int | Type::Bool | Type::F64 => true,
            Type::Ptr(_) | Type::PtrMut(_) => true,
            Type::Struct(sid) => {
                let def = &self.structs[*sid];
                if !def.repr_c {
                    return false;
                }
                def.fields.iter().all(|(_, fty)| self.c_abi_shape(fty, offset))
            }
            _ => false,
        }
    }

    pub(super) fn take_pub(&mut self) -> bool {
        let is_pub = matches!(self.current(), Kind::Ident(name) if name == "pub");
        if is_pub { self.pos += 1; }
        is_pub
    }
    // Cross-file access check: an item defined in another file must be exported.
    // Prelude names are public by construction, and a file always sees its own.
    pub(super) fn check_export_types(&self, name: &str, params: &[Type], ret: Type) -> Result<(), ae_span::Diagnostic> {
        for ty in params.iter().copied().chain(std::iter::once(ret)) {
            let (kind, type_name, public) = match ty {
                Type::Struct(id) | Type::StructRef(id) | Type::StructMut(id) => {
                    ("struct", self.structs[id].name.clone(), self.structs[id].public)
                }
                Type::Fn(sig) => {
                    let (params, returns) = self.ctx.fn_sig(sig);
                    self.check_export_types(name, &params, returns)?;
                    continue;
                }
                Type::Enum(_, id) => ("enum", self.enums[id].name.to_owned(), self.enums[id].public),
                _ => continue,
            };
            if !public { return Err(self.fail(&format!("public function '{name}' exposes private {kind} '{type_name}'"))); }
        }
        Ok(())
    }
    pub(super) fn require_public(&self, kind: &str, name: &str, file: &str, public: bool) -> Result<(), ae_span::Diagnostic> {
        if public || file == PRELUDE || file == self.file { return Ok(()); }
        Err(self.fail(&format!("{kind} '{name}' is private to {file} (mark it 'pub' to export it)")))
    }
    // Second pass skips struct bodies (already collected, must not re-register).
    pub(super) fn skip_struct_def(&mut self) {
        self.pos += 2; // 'struct' Name
        self.skip_type_params();
        self.skip_collected_body();
    }
    // Enum bodies are collected in the first pass, skipped in the second.
    // Step over `[T, U]` when a declaration is skipped in the second pass.
    pub(super) fn skip_type_params(&mut self) {
        self.pos = ae_syntax::declaration::skip_type_parameter_list(&self.tokens, self.pos);
    }
    // Skip `(params) -> Ret { body }` without interpreting it. Used for a
    // generic declaration, whose body is parsed at each instantiation instead.
    pub(super) fn skip_function_body(&mut self) -> Result<usize, ae_span::Diagnostic> {
        while *self.current() != Kind::LBrace {
            if *self.current() == Kind::Eof { return Err(self.fail("expected '{' before end of file")); }
            self.pos += 1;
        }
        self.skip_balanced_braces()?;
        Ok(self.pos)
    }
    pub(super) fn skip_enum_def(&mut self) {
        self.pos += 2; // 'enum' Name
        self.skip_type_params();
        self.skip_collected_body();
    }

    fn skip_collected_body(&mut self) {
        let boundary = ae_syntax::declaration::brace_body_end(&self.tokens, self.pos + 1);
        self.pos = boundary.unwrap_or_else(|end| end);
    }

    pub(super) fn collect_functions(&mut self, next_id: &mut usize, body_start: usize) -> Result<Vec<Declaration>, ae_span::Diagnostic> {
        // Traits are registered before anything else so a bound in the same
        // file can name a trait declared later; cross-file traits already
        // exist because imports load first.
        use ae_syntax::declaration::parse_declaration_tokens;

        let declarations = parse_declaration_tokens(&self.file, self.source, &self.tokens, body_start);
        match &declarations {
            Ok(declarations) => {
                let mut cursor = body_start;
                for declaration in declarations {
                    if declaration.kind != DeclarationKind::Trait {
                        continue;
                    }
                    // A declaration range includes modifiers such as 'pub'.
                    // The trait parser starts at the trait keyword itself.
                    while self.tokens[cursor].offset < declaration.range.start {
                        cursor += 1;
                    }
                    if self.tokens[cursor].kind != Kind::Trait {
                        cursor += 1;
                    }
                    self.pos = cursor;
                    self.parse_trait_def()?;
                    cursor = self.pos;
                }
            }
            Err(_) => {
                // Keep the compiler's diagnostic precedence for malformed
                // declarations while the syntax parser is being migrated.
                self.pos = body_start;
                while let Some(trait_start) = ae_syntax::declaration::next_top_level_trait(&self.tokens, self.pos) {
                    self.pos = trait_start;
                    self.parse_trait_def()?;
                }
            }
        }
        let function_bodies: Vec<_> = declarations.as_ref().map(|declarations| {
            declarations.iter().filter_map(|declaration| {
                if matches!(declaration.kind, DeclarationKind::Function { .. }) {
                    declaration.body.clone()
                } else {
                    None
                }
            }).collect()
        }).unwrap_or_default();
        self.pos = body_start;
        let mut seen = std::collections::HashSet::new();
        while *self.current() != Kind::Eof {
            let declaration_start = self.tokens[self.pos].offset;
            let parsed_declaration = declarations.as_ref().ok().and_then(|items| {
                let index = items.binary_search_by_key(&declaration_start, |item| item.range.start).ok()?;
                Some(&items[index])
            });
            let parsed_parameters = parsed_declaration.and_then(|item| match &item.kind {
                DeclarationKind::Function { parameters, .. } => Some(parameters.as_slice()),
                _ => None,
            });
            let public = self.take_pub();
            // `#[repr(C)]` is the language's one attribute (M2.12 U8): it
            // pins the struct's layout for the C ABI. The bracket shape is
            // parsed literally, and anything but repr(C) is rejected by
            // name so a typo cannot silently mean "no attribute".
            let (attribute, end) = parse_declaration_attribute(&self.file, self.source, &self.tokens, self.pos)?;
            self.pos = end;
            let repr_c = attribute == Some(DeclarationAttribute::ReprC);
            let no_mangle = attribute == Some(DeclarationAttribute::NoMangle);
            if repr_c && *self.current() != Kind::Struct {
                return Err(self.fail("'#[repr(C)]' applies to a struct declaration"));
            }
            if no_mangle && *self.current() != Kind::Extern {
                return Err(self.fail("'#[no_mangle]' applies to an 'extern \"C\" fn' definition"));
            }
            if *self.current() == Kind::Struct {
                let parsed_fields = parsed_declaration.and_then(|item| match &item.kind {
                    DeclarationKind::Struct { fields } => Some(fields.as_slice()),
                    _ => None,
                });
                self.parse_struct_def(public, repr_c, parsed_fields)?;
                continue;
            }
            if *self.current() == Kind::Enum {
                let parsed_variants = parsed_declaration.and_then(|item| match &item.kind {
                    DeclarationKind::Enum { variants } => Some(variants.as_slice()),
                    _ => None,
                });
                self.parse_enum_def(public, parsed_variants)?;
                continue;
            }
            if *self.current() == Kind::Trait { self.skip_trait_def()?; continue; }
            if *self.current() == Kind::Impl { self.collect_impl(next_id)?; continue; }
            if *self.current() == Kind::Extern {
                // M2.12 slice 4 (spec/unsafe.md 4.1): `extern "C" fn name(params) -> ret;`
                // names a C symbol the program links against. No body, no
                // definition in the generated C — the call site references
                // the symbol by name. Slice 4 restricts the shapes to the
                // scalars the C ABI passes in registers.
                let (_, end) = parse_extern_function_prefix(&self.file, self.source, &self.tokens, self.pos)?;
                self.pos = end;
                let name = self.identifier()?;
                if !seen.insert(name.clone()) { return Err(self.fail(&format!("duplicate function '{name}'"))); }
                self.take(Kind::LParen)?;
                let sig_offset = self.tokens[self.pos.saturating_sub(1)].offset;
                let mut types = Vec::new();
                if *self.current() != Kind::RParen {
                    loop {
                        if types.len() >= MAX_FUNCTION_PARAMETERS { return Err(self.fail(&format!("function parameter limit exceeded ({MAX_FUNCTION_PARAMETERS})"))); }
                        let parsed = parsed_parameters.and_then(|parameters| parameters.get(types.len()));
                        self.parameter_binding(parsed)?;
                        self.take(Kind::Colon)?;
                        types.push(self.parameter_type()?);
                        if *self.current() != Kind::Comma { break; }
                        self.pos += 1;
                    }
                }
                self.take(Kind::RParen)?;
                self.take(Kind::Arrow)?;
                let ret = self.return_annotation()?;
                for ty in types.iter().chain(std::iter::once(&ret)) {
                    if !self.c_abi_shape(ty, sig_offset) {
                        let shown = match ty {
                            Type::Struct(sid) => self.structs[*sid].name.clone(),
                            _ => ty.name(self.ctx),
                        };
                        return Err(self.error_at(sig_offset, &format!("'{shown}' cannot cross the C ABI; scalars, '#[repr(C)]' structs, and '*const T'/'*mut T' can")));
                    }
                }
                if name == "main" { return Err(self.fail("main cannot be an extern declaration")); }
                self.generics.next_fn_id = self.generics.next_fn_id.max(*next_id);
                let id = self.generics.next_fn_id;
                self.generics.next_fn_id += 1;
                *next_id = self.generics.next_fn_id;
                // An exposure's body is safe ae code (spec/unsafe.md 4.2):
                // calls from ae are safe — extern_c stays false. The C name
                // rides the exposure registry, not this flag.
                let signature = if no_mangle {
                    Signature { id, params: types.clone(), ret, file: self.file.clone(), public, extern_c: false }
                } else {
                    Signature { id, params: types.clone(), ret, file: self.file.clone(), public, extern_c: true }
                };
                if self.functions.insert(name.clone(), signature).is_some() {
                    return Err(self.fail(&format!("duplicate function '{name}' (already defined in an imported file)")));
                }
                self.ctx.register_extern(id, name.clone(), types, ret);
                if no_mangle {
                    // An exposure (M2.12 U7): the body parses in the body
                    // pass, under the id this signature registered. The C
                    // name IS the function's emitted name. A bodyless
                    // declaration has nothing to expose — its symbol is the
                    // C name already — so the attribute is rejected.
                    if *self.current() != Kind::LBrace {
                        return Err(self.error_at(sig_offset, "'#[no_mangle]' applies to an 'extern \"C\" fn' definition; a declaration's symbol is its own name already"));
                    }
                    self.ctx.register_exposure(id, name.clone());
                    // The id joins id_order at this position: the body pass
                    // lines up result.len() against it (main included).
                    self.id_order.push(id);
                    self.skip_signature_body(&function_bodies)?;
                } else {
                    self.take(Kind::Semi)?;
                }
                continue;
            }
            // `unsafe fn` (M2.12 slice 5): the body parses like any other
            // function — unchecked memory access inside it still needs its
            // own `unsafe` blocks — but calling it requires `unsafe`.
            // `unsafe fn` (M2.12 slice 5): the body parses like any other
            // function — unchecked memory access inside it still needs its
            // own `unsafe` blocks — but calling it requires `unsafe`. The
            // precondition doc comment is the author’s half of the deal
            // (spec/unsafe.md 6): presence is enforced here, truth cannot be.
            let is_unsafe_fn = if *self.current() == Kind::Unsafe {
                let fn_offset = self.tokens[self.pos].offset;
                if !ae_syntax::has_doc_comment_above(self.source, fn_offset) {
                    return Err(self.fail("an unsafe fn carries a doc comment naming its preconditions (what the caller must guarantee: validity, aliasing, thread context)").with_code("E0704"));
                }
                self.pos += 1;
                self.take(Kind::Fn)?;
                true
            } else { self.take(Kind::Fn)?; false };
            let name = self.identifier()?;
            if !seen.insert(name.clone()) { return Err(self.fail(&format!("duplicate function '{name}'"))); }
            if *self.current() == Kind::LBracket {
                // A generic function has no signature of its own: only its
                // instantiations exist, and each gets an id when constructed.
                if name == "main" { return Err(self.fail("main cannot be generic")); }
                let declared_at = self.tokens[self.pos].offset;
                let declared = self.type_params_with_bounds()?;
                let params: Vec<String> = declared.iter().map(|(n, _, _)| n.clone()).collect();
                let bounds: Vec<Vec<Bound>> = declared.iter().map(|(_, b, _)| b.clone()).collect();
                let trait_bounds: Vec<Vec<usize>> = declared.into_iter().map(|(_, _, t)| t).collect();
                let start = self.pos;
                let end = if let Some(body) = parsed_declaration.and_then(|item| item.body.as_ref()) {
                    self.advance_to_body_end(body.end);
                    self.pos
                } else {
                    self.skip_function_body()?
                };
                audit_generic_fn(&self.file, self.source, self.tokens.as_ref(), start, end, &name, &params, &bounds, declared_at, self.ctx)?;
                let gid = self.generics.fn_defs.len();
                self.generics.fn_names.insert(name.clone(), gid);
                let bindings = parsed_parameters.map(Rc::from);
                self.generics.fn_defs.push(GenericFn { name, bindings, params, bounds, trait_bounds, start, tokens: Rc::clone(&self.tokens), source: Rc::clone(&self.shared_source), file: self.file.clone(), public });
                continue;
            }
            self.take(Kind::LParen)?;
            let mut types = Vec::new();
            if *self.current() != Kind::RParen {
                loop {
                    if types.len() >= MAX_FUNCTION_PARAMETERS { return Err(self.fail(&format!("function parameter limit exceeded ({MAX_FUNCTION_PARAMETERS})"))); }
                    let parsed = parsed_parameters.and_then(|parameters| parameters.get(types.len()));
                    let (parameter, _, _) = self.parameter_binding(parsed)?;
                    if RESERVED.contains(&parameter.as_str()) { return Err(self.fail(&format!("'{parameter}' is a built-in name and cannot be a parameter"))); }
                    self.take(Kind::Colon)?;
                    types.push(self.parameter_type()?);
                    if *self.current() != Kind::Comma { break; }
                    self.pos += 1;
                }
            }
            self.take(Kind::RParen)?;
            if RESERVED.contains(&name.as_str()) { return Err(self.fail(&format!("'{name}' is a built-in and cannot be redefined"))); }
            // Ids are unique across all files of a program, and share one
            // space with generic instantiations, so keep the counter in the
            // shared state and fold the per-file value in.
            self.generics.next_fn_id = self.generics.next_fn_id.max(*next_id);
            let id = self.generics.next_fn_id;
            self.generics.next_fn_id += 1;
            *next_id = self.generics.next_fn_id;
            self.id_order.push(id);
            if name == "main" {
                if !types.is_empty() { return Err(self.fail("main cannot have parameters")); }
            } else {
                self.take(Kind::Arrow)?;
                let ret = self.return_annotation()?;
                // A public signature may not name a type importers cannot see;
                // catching it here beats a puzzling error at every call site.
                if public { self.check_export_types(&name, &types, ret)?; }
                let signature = Signature { id, params: types, ret, file: self.file.clone(), public, extern_c: false };
                if self.functions.insert(name.clone(), signature).is_some() {
                    return Err(self.fail(&format!("duplicate function '{name}' (already defined in an imported file)")));
                }
                if is_unsafe_fn { self.ctx.register_unsafe_fn(id); }
            }
            self.skip_signature_body(&function_bodies)?;
        }
        // main presence is checked once per program, not per file.
        self.pos = body_start;
        Ok(declarations.unwrap_or_default())
    }
    // ---- traits and impls (ADR-47) ------------------------------------------
    // A trait method is only callable inside a generic body whose constrained
    // parameter it dispatches on: resolution maps (trait, method, substituted
    // parameter type) to the impl's mangled signature, and the call emits the
    // impl function's id like any other call — static dispatch with no new
    // runtime mechanism.

    // Parses a trait's payload type: the trait's own `T`, or a concrete type.
    pub(super) fn trait_payload(&mut self, what: &str) -> Result<Payload, ae_span::Diagnostic> {
        if matches!(self.current(), Kind::Amp) {
            return Err(self.fail(&format!("{what} cannot borrow T in this version; take T by value or use a concrete type")));
        }
        if let Kind::Ident(text) = self.current()
            && text == "T" {
                self.pos += 1;
                return Ok(Payload::Param(0));
            }
        Ok(Payload::Concrete(self.parameter_type()?))
    }

    pub(super) fn parse_trait_def(&mut self) -> Result<(), ae_span::Diagnostic> {
        // The token scan landed on the `trait` keyword; a `pub` sits directly
        // before it (`pub` is an identifier-shaped keyword consumed by the
        // declaration passes only).
        let public = self.pos > 0 && matches!(&self.tokens[self.pos - 1].kind, Kind::Ident(name) if name == "pub");
        self.pos += 1; // trait
        let name = self.identifier()?;
        if self.generics.trait_names.contains_key(&name) {
            return Err(self.fail(&format!("duplicate trait '{name}'")));
        }
        if self.generics.traits.len() >= 64 { return Err(self.fail("trait limit exceeded (64 per program)")); }
        self.take(Kind::LBrace)?;
        let mut methods: Vec<TraitMethod> = Vec::new();
        while *self.current() != Kind::RBrace {
            if *self.current() == Kind::Eof { return Err(self.fail("expected '}' before end of file")); }
            self.take(Kind::Fn)?;
            let method = self.identifier()?;
            if method == "main" { return Err(self.fail("a trait method cannot be named main")); }
            if methods.iter().any(|m: &TraitMethod| m.name == method) {
                return Err(self.fail(&format!("duplicate method '{method}' in trait '{name}'")));
            }
            self.take(Kind::LParen)?;
            let mut params = Vec::new();
            if *self.current() != Kind::RParen {
                loop {
                    if params.len() >= 8 { return Err(self.fail("trait method parameter limit exceeded (8)")); }
                    let _param = self.identifier()?;
                    self.take(Kind::Colon)?;
                    params.push(self.trait_payload("trait method parameter")?);
                    if *self.current() != Kind::Comma { break; }
                    self.pos += 1;
                }
            }
            self.take(Kind::RParen)?;
            if params.is_empty() || !matches!(params[0], Payload::Param(0)) {
                return Err(self.fail("the first parameter of a trait method must be the trait's type T"));
            }
            self.take(Kind::Arrow)?;
            let returns = self.trait_payload("trait method return")?;
            self.take(Kind::Semi)?;
            methods.push(TraitMethod { name: method, params, returns });
        }
        self.take(Kind::RBrace)?;
        let tid = self.generics.traits.len();
        self.generics.trait_names.insert(name.clone(), tid);
        self.generics.traits.push(TraitDef { name, methods, file: self.file.clone(), public });
        Ok(())
    }

    pub(super) fn skip_trait_def(&mut self) -> Result<(), ae_span::Diagnostic> {
        while *self.current() != Kind::LBrace {
            if *self.current() == Kind::Eof { return Err(self.fail("expected '{' in trait declaration")); }
            self.pos += 1;
        }
        self.skip_balanced_braces()
    }

    fn skip_signature_body(&mut self, bodies: &[std::ops::Range<usize>]) -> Result<(), ae_span::Diagnostic> {
        let start = self.tokens[self.pos].offset;
        if let Ok(index) = bodies.binary_search_by_key(&start, |body| body.start) {
            let end = bodies[index].end;
            self.advance_to_body_end(end);
            return Ok(());
        }
        self.skip_balanced_braces()
    }

    fn advance_to_body_end(&mut self, end: usize) {
        self.pos += self.tokens[self.pos..].partition_point(|token| token.offset < end);
    }

    pub(super) fn skip_balanced_braces(&mut self) -> Result<(), ae_span::Diagnostic> {
        self.take(Kind::LBrace)?;
        match ae_syntax::declaration::brace_body_end(&self.tokens, self.pos) {
            Ok(end) => {
                self.pos = end;
                Ok(())
            }
            Err(end) => {
                self.pos = end;
                Err(self.fail("expected '}' before end of file"))
            }
        }
    }

    // Pass one for `impl Trait for Type { fn ... }`: register each method's
    // mangled signature and assign its id, so pass two (program) can parse
    // the bodies in file order against `id_order`.
    // Inherent `impl Type { fn m(self) ... }` (M1.2, v1). A method is an
    // ordinary function whose parameter 0 is the receiver, so nothing new is
    // needed downstream: the backend already emits calls and moves. What is new
    // is the (type, method) name space that `value.m(...)` resolves through, and
    // the receiver spelling that lets a body read `self.field`.
    fn collect_inherent_impl(&mut self, type_name: String, next_id: &mut usize) -> Result<(), ae_span::Diagnostic> {
        if self.generics.struct_names.contains_key(&type_name) {
            return Err(self.fail(&format!("'{type_name}' is a generic struct; inherent impls on generic types are not supported yet — write a plain function")));
        }
        if self.generics.names.contains_key(&type_name) {
            return Err(self.fail(&format!("'{type_name}' is a generic enum; inherent impls on generic types are not supported yet — write a plain function")));
        }
        // The impl target is a struct or an enum declared in this file. Both
        // resolve methods the same way — the (type, method) name space — and
        // `Self` inside the block stands for exactly this type (ADR-0186).
        let target = if let Some(&sid) = self.struct_names.get(&type_name) {
            ImplTarget::Struct(sid)
        } else if let Some(&eid) = self.enum_names.get(&type_name) {
            ImplTarget::Enum(eid)
        } else {
            return Err(self.fail(&format!("impl names unknown type '{type_name}'; declare it with 'struct {type_name} {{ ... }}' or 'enum {type_name} {{ ... }}'")));
        };
        // Coherence, same shape as traits: the impl lives with the type, so two
        // files can never give one type two different methods of the same name.
        let owner = match target {
            ImplTarget::Struct(sid) => self.structs[sid].file.clone(),
            ImplTarget::Enum(eid) => self.enums[eid].file.clone(),
        };
        if owner != self.file {
            return Err(self.fail(&format!("an inherent impl for '{type_name}' must be in the file that declares it ({owner})")));
        }
        self.impl_self = Some(match target {
            ImplTarget::Struct(sid) => Type::Struct(sid),
            ImplTarget::Enum(eid) => Type::Enum(self.enums[eid].name, eid),
        });
        self.inherent_impl = true;
        self.take(Kind::LBrace)?;
        while *self.current() != Kind::RBrace {
            if *self.current() == Kind::Eof { return Err(self.fail("expected '}' before end of file")); }
            // `pub` is accepted and means nothing here: a method is exactly as
            // visible as the type it belongs to, so the marker would promise
            // something the type itself does not grant.
            let is_pub = self.take_pub();
            self.take(Kind::Fn)?;
            let method = self.identifier()?;
            if method == "drop" {
                if is_pub {
                    return Err(self.fail("destructor 'drop' cannot be marked 'pub'; only compiler-generated drop glue may invoke it"));
                }
                if !matches!(target, ImplTarget::Struct(_)) {
                    return Err(self.fail("destructor 'drop' is only supported on structs in this version"));
                }
            }
            if method == "main" { return Err(self.fail("a method cannot be named main")); }
            if RESERVED.contains(&method.as_str()) {
                return Err(self.fail(&format!("'{method}' is a built-in name and cannot be a method")));
            }
            if let ImplTarget::Struct(sid) = target
                && self.structs[sid].fields.iter().any(|(name, _)| *name == method) {
                    return Err(self.fail(&format!("method '{method}' collides with the field of the same name on '{type_name}'; 'value.{method}' has to mean one thing, so rename one")));
                }
            let key = format!("{type_name}#{method}");
            if self.generics.inherent.contains_key(&key) {
                return Err(self.fail(&format!("duplicate method '{method}' for '{type_name}'")));
            }
            if self.generics.inherent.len() >= 256 { return Err(self.fail("method limit exceeded (256 per program)")); }
            self.take(Kind::LParen)?;
            let mut types: Vec<Type> = Vec::new();
            let mut receiver: Option<Type> = None;
            if *self.current() != Kind::RParen {
                loop {
                    if types.len() >= 8 { return Err(self.fail("method parameter limit exceeded (8)")); }
                    // A leading `self` is the receiver; everything else is an
                    // ordinary `name: Type` parameter. An impl block may hold
                    // both, and `self` only counts in first position.
                    // `&self` borrows the receiver read-only (ADR-0184): the
                    // body reads fields through a pointer and the caller keeps
                    // ownership, so calling the method no longer moves.
                    let mut borrowed = false;
                    let mut mutable_borrow = false;
                    if types.is_empty() && *self.current() == Kind::Amp {
                        self.pos += 1;
                        if *self.current() == Kind::Mut {
                            self.pos += 1;
                            mutable_borrow = true;
                        }
                        if !matches!(self.current(), Kind::Ident(text) if text.as_str() == "self") {
                            return Err(self.fail("write '&self' or '&mut self' to borrow the receiver; any other borrowed parameter is written 'name: &Type' or 'name: &mut Type'"));
                        }
                        self.pos += 1;
                        if *self.current() == Kind::Colon {
                            return Err(self.fail("write the receiver as '&self' or '&mut self'; it already has the borrowed receiver type"));
                        }
                        borrowed = true;
                    }
                    let is_receiver = types.is_empty() && (borrowed || matches!(self.current(), Kind::Ident(text) if text.as_str() == "self"));
                    if is_receiver {
                        if !borrowed {
                            self.pos += 1;
                            if *self.current() == Kind::Colon {
                                return Err(self.fail(&format!("write the receiver as 'self'; it already has type '{}'", type_name)));
                            }
                            receiver = Some(self.impl_self.expect("an impl block always has a target"));
                        } else if matches!(target, ImplTarget::Enum(_)) {
                            return Err(self.fail(&format!("enum methods take their receiver by value in this version; write 'self', not '{}'", if mutable_borrow { "&mut self" } else { "&self" })));
                        } else if mutable_borrow {
                            receiver = Some(Type::StructMut(sid_of(&target)));
                        } else {
                            receiver = Some(Type::StructRef(sid_of(&target)));
                        }
                        types.push(receiver.unwrap());
                    } else {
                        let _param = self.identifier()?;
                        self.take(Kind::Colon)?;
                        types.push(self.parameter_type()?);
                    }
                    if *self.current() != Kind::Comma { break; }
                    self.pos += 1;
                }
            }
            self.take(Kind::RParen)?;
            let ret = if *self.current() == Kind::Arrow {
                self.pos += 1;
                self.parameter_type()?
            } else {
                Type::Int
            };
            // A borrow may not be returned: the receiver's borrow ends with the
            // call, so anything it points at would be owned by the caller's
            // stack frame and dangle.
            if matches!(ret, Type::StructRef(_) | Type::StructMut(_)) {
                return Err(self.fail("a method cannot return a borrow; return a value that outlives the borrow instead, or take the borrow again at the call site"));
            }
            if method == "drop"
                && (!matches!(receiver, Some(Type::StructMut(_))) || types.len() != 1 || ret != Type::Int) {
                    return Err(self.fail("destructor 'drop' must have signature 'fn drop(&mut self) -> i64'"));
                }
            self.generics.next_fn_id = self.generics.next_fn_id.max(*next_id);
            let id = self.generics.next_fn_id;
            self.generics.next_fn_id += 1;
            *next_id = self.generics.next_fn_id;
            if method == "drop" {
                self.structs[sid_of(&target)].drop_fn = Some(id);
            }
            self.id_order.push(id);
            self.generics.inherent.insert(key, MethodDef {
                signature: Signature { id, params: types, ret, file: self.file.clone(), public: true, extern_c: false },
                receiver,
            });
            self.skip_balanced_braces()?;
        }
        self.take(Kind::RBrace)?;
        self.impl_self = None;
        self.inherent_impl = false;
        Ok(())
    }

    // Signature pass for `impl Pair[K, V] { fn m(self, ...) ... }` (ADR-0187).
    // Nothing here parses bodies or assigns ids: methods are templates, and
    // each (method, argument list) pair gets its function id and body parse
    // at the call site that first needs it, exactly like a generic function.
    fn collect_generic_impl(&mut self, type_name: String) -> Result<(), ae_span::Diagnostic> {
        // Both declaration kinds take the same template shape; the receiver's
        // concrete type comes from the instantiation (enum receivers are by
        // value, so the enum path only ever builds ByValue receivers).
        let is_struct = self.generics.struct_names.contains_key(&type_name);
        // `struct_defs` holds the file for struct declarations; enum
        // declarations record theirs in `defs`, so the lookup follows the
        // kind instead of assuming one table.
        let (gid, def_file) = if is_struct {
            let gid = self.generics.struct_names[&type_name];
            (gid, self.generics.struct_defs[gid].file.clone())
        } else {
            let gid = self.generics.names[&type_name];
            (gid, self.generics.defs[gid].file.clone())
        };
        // Coherence: the impl lives with the declaration, one per type.
        if def_file != self.file {
            return Err(self.fail(&format!("a generic impl for '{type_name}' must be in the file that declares it ({def_file})")));
        }
        if self.generics.generic_impls.contains_key(&type_name) {
            return Err(self.fail(&format!("duplicate generic impl for '{type_name}'")));
        }
        let params = self.type_params()?;
        self.take(Kind::LBrace)?;
        let mut methods: Vec<GenericImplMethod> = Vec::new();
        while *self.current() != Kind::RBrace {
            if *self.current() == Kind::Eof { return Err(self.fail("expected '}' before end of file")); }
            let is_pub = self.take_pub();
            self.take(Kind::Fn)?;
            let method = self.identifier()?;
            if method == "drop" {
                if is_pub {
                    return Err(self.fail("destructor 'drop' cannot be marked 'pub'; only compiler-generated drop glue may invoke it"));
                }
                if !is_struct {
                    return Err(self.fail("destructor 'drop' is only supported on structs in this version"));
                }
            }
            if method == "main" { return Err(self.fail("a method cannot be named main")); }
            if RESERVED.contains(&method.as_str()) {
                return Err(self.fail(&format!("'{method}' is a built-in name and cannot be a method")));
            }
            if methods.iter().any(|m: &GenericImplMethod| m.name == method) {
                return Err(self.fail(&format!("duplicate method '{method}' in the impl for '{type_name}'")));
            }
            // Field collisions: `value.name` must mean one thing, and for an
            // instantiation the fields are exactly the declaration's fields.
            if is_struct && self.generics.struct_defs[gid].fields.iter().any(|(name, _)| *name == method) {
                return Err(self.fail(&format!("method '{method}' collides with the field of the same name on '{type_name}'; 'value.{method}' has to mean one thing, so rename one")));
            }
            // Only the receiver spelling is read here; the rest of the
            // signature and the body are stored as a token range and parsed
            // per instantiation, where the type parameters are known. The
            // stored range starts at the parameter list's '(' so the
            // instantiation re-parses the whole signature.
            let body_start = self.pos;
            let mut receiver: Option<ReceiverKind> = None;
            self.take(Kind::LParen)?;
            if *self.current() != Kind::RParen {
                let mut borrowed = false;
                let mut mutable_borrow = false;
                if *self.current() == Kind::Amp {
                    self.pos += 1;
                    if *self.current() == Kind::Mut { self.pos += 1; mutable_borrow = true; }
                    if !matches!(self.current(), Kind::Ident(text) if text.as_str() == "self") {
                        return Err(self.fail("write '&self' or '&mut self' to borrow the receiver; any other parameter is written 'name: Type'"));
                    }
                    self.pos += 1;
                    borrowed = true;
                }
                if borrowed || matches!(self.current(), Kind::Ident(text) if text.as_str() == "self") {
                    if !borrowed {
                        self.pos += 1;
                        if *self.current() == Kind::Colon {
                            return Err(self.fail(&format!("write the receiver as 'self'; it already has type '{}'", type_name)));
                        }
                        receiver = Some(ReceiverKind::ByValue);
                    } else if !is_struct {
                        return Err(self.fail(&format!("enum methods take their receiver by value in this version; write 'self', not '{}'", if mutable_borrow { "&mut self" } else { "&self" })));
                    } else if mutable_borrow {
                        receiver = Some(ReceiverKind::Mutable);
                    } else {
                        receiver = Some(ReceiverKind::Shared);
                    }
                }
            }
            self.skip_function_body()?;
            methods.push(GenericImplMethod { name: method, receiver, body_start, tokens: Rc::clone(&self.tokens), source: Rc::clone(&self.shared_source) });
        }
        self.take(Kind::RBrace)?;
        self.generics.generic_impls.insert(type_name, GenericImplDef { params, methods, file: self.file.clone() });
        Ok(())
    }

    pub(super) fn collect_impl(&mut self, next_id: &mut usize) -> Result<(), ae_span::Diagnostic> {
        self.pos += 1; // impl
        let first = self.identifier()?;
        // `impl Type { ... }` carries no `for`, so that name is the type itself.
        // A generic struct instantiates its methods per argument list, so its
        // impl is collected as a template instead of signatures (ADR-0187).
        if *self.current() == Kind::LBracket
            && (self.generics.struct_names.contains_key(&first) || self.generics.names.contains_key(&first)) {
            return self.collect_generic_impl(first);
        }
        if *self.current() != Kind::For { return self.collect_inherent_impl(first, next_id); }
        let trait_name = first;
        let tid = *self.generics.trait_names.get(&trait_name)
            .ok_or_else(|| self.fail(&format!("impl names unknown trait '{trait_name}'")))?;
        self.take(Kind::For)?;
        if let Kind::Ident(text) = self.current()
            && text == "T" { return Err(self.fail("an impl names the concrete type, not the trait's parameter T")); }
        let ty = self.parameter_type()?;
        // `Self` inside a trait impl's signatures stands for the concrete
        // type being implemented, exactly as in an inherent impl.
        self.impl_self = Some(ty);
        let trait_file = self.generics.traits[tid].file.clone();
        let type_file = match ty {
            Type::Struct(id) => Some(self.structs[id].file.clone()),
            Type::Enum(_, id) => Some(self.enums[id].file.clone()),
            _ => None,
        };
        // Coherence, v1 shape: the impl lives where the trait or the type
        // lives, so two files can never hand the same (trait, type) pair
        // conflicting behaviour.
        if trait_file != self.file && type_file.as_deref() != Some(self.file.as_str()) {
            let where_must = type_file.unwrap_or_else(|| format!("the file declaring the trait ({trait_file})"));
            return Err(self.fail(&format!(
                "impl of '{trait_name}' for '{}' must be in the file that declares the trait ({trait_file}) or the type ({}); this file may not add it",
                ty.name(self.ctx), where_must)));
        }
        if !self.generics.impls.insert((tid, ty.name(self.ctx))) {
            return Err(self.fail(&format!("duplicate impl of '{trait_name}' for '{}'", ty.name(self.ctx))));
        }
        if self.generics.impls.len() > 256 { return Err(self.fail("impl limit exceeded (256 per program)")); }
        self.take(Kind::LBrace)?;
        while *self.current() != Kind::RBrace {
            if *self.current() == Kind::Eof { return Err(self.fail("expected '}' before end of file")); }
            self.take_pub();
            self.take(Kind::Fn)?;
            let method = self.identifier()?;
            if method == "main" { return Err(self.fail("an impl method cannot be named main")); }
            let mid = self.generics.traits[tid].methods.iter().position(|m| m.name == method)
                .ok_or_else(|| self.fail(&format!("trait '{trait_name}' has no method '{method}'")))?;
            let expected = self.generics.traits[tid].methods[mid].clone();
            let substitute = |payload: &Payload| match payload {
                Payload::Param(0) => ty,
                Payload::Concrete(t) => *t,
                _ => unreachable!("trait payloads only use parameter 0"),
            };
            self.take(Kind::LParen)?;
            let mut types = Vec::new();
            if *self.current() != Kind::RParen {
                loop {
                    if types.len() >= 8 { return Err(self.fail("impl method parameter limit exceeded (8)")); }
                    if let Kind::Ident(text) = self.current()
                        && text == "T" { return Err(self.fail("an impl method signature uses the concrete type, not the trait's parameter T")); }
                    let _param = self.identifier()?;
                    self.take(Kind::Colon)?;
                    types.push(self.parameter_type()?);
                    if *self.current() != Kind::Comma { break; }
                    self.pos += 1;
                }
            }
            self.take(Kind::RParen)?;
            let expected_params: Vec<Type> = expected.params.iter().map(substitute).collect();
            if types != expected_params {
                let want = expected_params.iter().map(|t| t.name(self.ctx)).collect::<Vec<_>>().join(", ");
                let got = types.iter().map(|t| t.name(self.ctx)).collect::<Vec<_>>().join(", ");
                return Err(self.fail(&format!("impl method '{method}' must take ({want}) to match trait '{trait_name}', found ({got})")));
            }
            self.take(Kind::Arrow)?;
            let ret = self.parameter_type()?;
            if ret != substitute(&expected.returns) {
                return Err(self.fail(&format!("impl method '{method}' must return {} to match trait '{trait_name}', found {}", substitute(&expected.returns).name(self.ctx), ret.name(self.ctx))));
            }
            self.generics.next_fn_id = self.generics.next_fn_id.max(*next_id);
            let id = self.generics.next_fn_id;
            self.generics.next_fn_id += 1;
            *next_id = self.generics.next_fn_id;
            self.id_order.push(id);
            let mangled = format!("{}#{}#{}", trait_name, method, ty.name(self.ctx));
            let signature = Signature { id, params: types, ret, file: self.file.clone(), public: true, extern_c: false };
            if self.functions.insert(mangled, signature).is_some() {
                return Err(self.fail(&format!("duplicate impl method '{method}' for this type")));
            }
            self.skip_balanced_braces()?;
        }
        self.take(Kind::RBrace)?;
        self.impl_self = None;
        Ok(())
    }

    // Pass two: parse the impl method bodies in the same file order the
    // signature pass used, so `id_order` lines up exactly like it does for
    // the file's own functions.
    pub(super) fn parse_impl_bodies(&mut self, result: &mut Vec<Function>) -> Result<(), ae_span::Diagnostic> {
        self.pos += 1; // impl
        let first = self.identifier()?;
        // A generic impl's methods are templates: their bodies were stored as
        // token ranges in the signature pass and are parsed per instantiation,
        // so this pass steps over the whole block (ADR-0187).
        if *self.current() == Kind::LBracket
            && (self.generics.struct_names.contains_key(&first) || self.generics.names.contains_key(&first)) {
            self.skip_type_params();
            return self.skip_balanced_braces();
        }
        // The same shape decision the signature pass made, asked again from the
        // tokens: `for` means a trait impl, its absence an inherent one.
        let inherent_type = if *self.current() == Kind::For {
            self.take(Kind::For)?;
            let ty = self.parameter_type()?;
            self.impl_self = Some(ty);
            None
        } else if let Some(&sid) = self.struct_names.get(&first) {
            self.impl_self = Some(Type::Struct(sid));
            Some(first.clone())
        } else if let Some(&eid) = self.enum_names.get(&first) {
            self.impl_self = Some(Type::Enum(self.enums[eid].name, eid));
            Some(first.clone())
        } else {
            return Err(self.fail(&format!("unknown type '{first}'")));
        };
        // The body route may take a receiver-less inherent associated
        // function (`fn circle(r: i64) -> Self`), but a trait-impl body
        // without a receiver stays typed: `Self` there is the trait's
        // implementer, which the resolved route does not model yet.
        self.inherent_impl = inherent_type.is_some();
        self.take(Kind::LBrace)?;
        // The resolver and compiler share the method declaration tree. Keep
        // the token route for syntax the shared grammar cannot recover yet.
        let methods = ae_syntax::declaration::parse_impl_method_tokens(
            &self.file, self.source, &self.tokens, self.pos,
        ).unwrap_or_default();
        let mut methods = methods.iter().peekable();
        while *self.current() != Kind::RBrace {
            if *self.current() == Kind::Eof { return Err(self.fail("expected '}' before end of file")); }
            let start = self.tokens[self.pos].offset;
            let parsed_parameters = methods.next_if(|method| method.range.start == start)
                .and_then(|method| match &method.kind {
                    DeclarationKind::Function { parameters, .. } => Some(parameters.as_slice()),
                    _ => None,
                });
            self.take_pub();
            self.take(Kind::Fn)?;
            let method = self.identifier()?;
            let id = self.id_order[result.len()];
            // The body pass binds `self` to the type the signature pass gave
            // THIS method — by value or as a read-only borrow — so an impl
            // block can mix `self` and `&self` methods. A trait impl has no
            // inherent entry, and its bodies never see `self`.
            self.receiver = inherent_type.as_ref()
                .and_then(|type_name| self.generics.inherent.get(&format!("{type_name}#{method}")))
                .and_then(|def| def.receiver);
            let parsed = self.function_rest_with_parameters(false, id, parsed_parameters);
            self.receiver = None;
            result.push(parsed?);
        }
        self.take(Kind::RBrace)?;
        self.impl_self = None;
        self.inherent_impl = false;
        Ok(())
    }

    // Resolves a call that is not a plain function: a trait method of one of
    // the enclosing generic's constrained parameters. The dispatch type is
    // the parameter's substituted type, fixed when the generic was
    // instantiated, so the impl lookup cannot be ambiguous at runtime.
    pub(super) fn resolve_trait_call(&mut self, method: &str, offset: usize) -> Result<Signature, ae_span::Diagnostic> {
        let mut found: Vec<(String, usize, usize)> = Vec::new();
        for (param, tids) in &self.trait_bounds {
            for &tid in tids {
                if let Some(mid) = self.generics.traits[tid].methods.iter().position(|m| m.name == method) {
                    found.push((param.clone(), tid, mid));
                }
            }
        }
        if found.is_empty() {
            return Err(self.error_at(offset, &format!("unknown function '{method}'")).with_code("E0207"));
        }
        if found.len() > 1 {
            return Err(self.error_at(offset, &format!("method '{method}' is ambiguous; several constrained parameters of this generic declare it")));
        }
        let (param, tid, mid) = found.remove(0);
        let ty = self.subst.get(&param).cloned()
            .ok_or_else(|| self.error_at(offset, &format!("'{method}' dispatches on '{param}', which has no type argument here")))?;
        let key = format!("{}#{}#{}", self.generics.traits[tid].name, self.generics.traits[tid].methods[mid].name, ty.name(self.ctx));
        self.functions.get(&key).cloned().ok_or_else(|| {
            self.error_at(offset, &format!("no impl of trait '{}' for '{}'", self.generics.traits[tid].name, ty.name(self.ctx)))
        })
    }

    pub(super) fn program(&mut self, declarations: &[Declaration]) -> Result<Vec<Function>, ae_span::Diagnostic> {
        let mut result = Vec::new();
        let mut main_found = false;
        let mut declarations = declarations.iter().peekable();
        while *self.current() != Kind::Eof {
            let start = self.tokens[self.pos].offset;
            while declarations.peek().is_some_and(|item| item.range.start < start) {
                declarations.next();
            }
            let declaration = declarations.next_if(|item| item.range.start == start);
            if let Some(item) = declaration {
                // Signatures are already registered. Only executable bodies
                // and impl methods need this pass.
                let already_collected = match item.kind {
                    DeclarationKind::Struct { .. } | DeclarationKind::Enum { .. } | DeclarationKind::Trait => true,
                    DeclarationKind::Function { .. } => !item.type_parameters.is_empty() || item.body.is_none(),
                    DeclarationKind::Impl { .. } => false,
                };
                if already_collected {
                    self.advance_to_body_end(item.range.end);
                    continue;
                }
            }
            let parsed_parameters = declaration.and_then(|item| match &item.kind {
                DeclarationKind::Function { parameters, .. } => Some(parameters.as_slice()),
                _ => None,
            });
            self.take_pub();
            let (_, end) = parse_declaration_attribute(&self.file, self.source, &self.tokens, self.pos)?;
            self.pos = end;
            if *self.current() == Kind::Struct { self.skip_struct_def(); continue; }
            if *self.current() == Kind::Enum { self.skip_enum_def(); continue; }
            if *self.current() == Kind::Trait { self.skip_trait_def()?; continue; }
            if *self.current() == Kind::Impl { self.parse_impl_bodies(&mut result)?; continue; }
            if *self.current() == Kind::Extern {
                // The signature pass registered the extern (M2.12 slice 4).
                // A declaration has no body — skip to its semicolon. An
                // exposure (`#[no_mangle]`, U7) has a body: it parses
                // exactly like a file function, under the id the signature
                // pass registered, so the emitter places it under the C name.
                let (_, end) = parse_extern_function_prefix(&self.file, self.source, &self.tokens, self.pos)?;
                self.pos = end;
                let exposure_name = self.identifier()?;
                // The signature pass decided the shape: an exposure
                // (extern_c false — its body is safe ae code) parses
                // its body here; a declaration skips to its semicolon. The
                // body's own semicolons make a blind skip wrong.
                let exposed = self.functions.get(&exposure_name).map(|sig| !sig.extern_c).unwrap_or(false);
                if exposed {
                    let id = self.id_order[result.len()];
                    result.push(self.function_rest_with_parameters(false, id, parsed_parameters)?);
                    continue;
                }
                while *self.current() != Kind::Semi {
                    if *self.current() == Kind::Eof { return Err(self.fail("expected ';' before the next declaration")); }
                    self.pos += 1;
                }
                self.pos += 1;
                continue;
            }
            if *self.current() == Kind::Unsafe { self.pos += 1; }
            self.take(Kind::Fn)?;
            let name = self.identifier()?;
            // A generic declaration has no body of its own: the signature pass
            // recorded it and an instantiation parses it later (ADR-31).
            if *self.current() == Kind::LBracket { self.skip_function_body()?; continue; }
            let is_main = name == "main";
            if is_main && main_found { return Err(self.fail("duplicate main function")); }
            if is_main { main_found = true; }
            let id = self.id_order[result.len()];
            result.push(self.function_rest_with_parameters(is_main, id, parsed_parameters)?);
        }
        // Instantiations discovered while parsing bodies have no place in the
        // file's own order, but they must still be emitted.
        result.append(&mut self.pending);
        // main presence is checked once per program, not per file.
        Ok(result)
    }
    // `(params) -> Ret { body }` for one function, starting at the parameter
    // list. Shared by the file's own functions and by generic instantiations,
    // which parse the same tokens with type parameters substituted.
    pub(super) fn function_rest(&mut self, is_main: bool, id: usize) -> Result<Function, ae_span::Diagnostic> {
        self.function_rest_with_parameters(is_main, id, None)
    }

    fn parameter_binding(
        &mut self,
        parsed: Option<&TypedName>,
    ) -> Result<(String, bool, std::ops::Range<usize>), ae_span::Diagnostic> {
        if let Some(parameter) = parsed {
            self.advance_to_body_end(parameter.name_range.end);
            return Ok((parameter.name.clone(), parameter.mutable, parameter.name_range.clone()));
        }
        let mutable = *self.current() == Kind::Var;
        if mutable { self.pos += 1; }
        let token = &self.tokens[self.pos];
        let range = token.offset..token.end;
        Ok((self.identifier()?, mutable, range))
    }

    fn function_rest_with_parameters(
        &mut self,
        is_main: bool,
        id: usize,
        parsed_parameters: Option<&[TypedName]>,
    ) -> Result<Function, ae_span::Diagnostic> {
        self.take(Kind::LParen)?;
        // Variable ids restart per function, so move tracking must too.
        self.names.clear(); self.binding_ranges.clear(); self.next_var = 0; self.moved.clear(); self.scopes.clear(); self.borrows.clear(); self.call_borrows.clear(); self.mut_borrows.clear();
        let mut params = Vec::new();
        if *self.current() != Kind::RParen {
            loop {
                if params.len() >= MAX_FUNCTION_PARAMETERS { return Err(self.fail(&format!("function parameter limit exceeded ({MAX_FUNCTION_PARAMETERS})"))); }
                // A borrowed receiver is spelled `&self` in the source. The
                // signature pass already recorded the borrow in the receiver
                // type, so the `&` here is only spelling and is skipped before
                // the name is read (ADR-0184).
                if params.is_empty() && *self.current() == Kind::Amp {
                    self.pos += 1;
                    if *self.current() == Kind::Mut { self.pos += 1; }
                    if !matches!(self.current(), Kind::Ident(text) if text.as_str() == "self") {
                        return Err(self.fail("write '&self' or '&mut self' to borrow the receiver; any other borrowed parameter is written 'name: &Type' or 'name: &mut Type'"));
                    }
                }
                // `var` marks the parameter slot writable. The value was moved
                // in at the call site, so mutating this copy cannot be seen by
                // the caller; that difference is what keeps it safe.
                let parsed = parsed_parameters.and_then(|parameters| parameters.get(params.len()));
                let (param, mutable, binding_range) = self.parameter_binding(parsed)?;
                // A receiver carries its type from the impl, so the parameter
                // list never writes it (M1.2). Everywhere else `self` is just
                // an unknown name, which keeps the spelling from leaking out of
                // method bodies.
                if param == "self" {
                    let Some(receiver) = self.receiver else {
                        return Err(self.fail("'self' is only available as the first parameter of an inherent impl method"));
                    };
                    if !params.is_empty() { return Err(self.fail("'self' must be the first parameter")); }
                    let var_id = self.next_var; self.next_var += 1;
                    // A `&mut self` receiver exists to be written through, so
                    // its slot is mutable without a `var` spelling; a `&self`
                    // or by-value receiver keeps the caller's spelling.
                    let mutable = mutable || matches!(receiver, Type::StructMut(_));
                    self.names.insert(param, (var_id, mutable, receiver));
                    self.binding_ranges.insert(var_id, binding_range);
                    params.push((var_id, receiver));
                    if *self.current() != Kind::Comma { break; }
                    self.pos += 1;
                    continue;
                }
                if RESERVED.contains(&param.as_str()) { return Err(self.fail(&format!("'{param}' is a built-in name and cannot be a parameter"))); }
                if self.names.contains_key(&param) { return Err(self.fail(&format!("duplicate parameter '{param}'"))); }
                self.take(Kind::Colon)?;
                let ty = self.parameter_type()?;
                // A `&mut Type` parameter exists to be written through, so
                // its slot is mutable without a `var` spelling (ADR-0185).
                let mutable = mutable || matches!(ty, Type::StructMut(_));
                let var_id = self.next_var; self.next_var += 1;
                self.names.insert(param, (var_id, mutable, ty));
                self.binding_ranges.insert(var_id, binding_range);
                params.push((var_id, ty));
                if *self.current() != Kind::Comma { break; }
                self.pos += 1;
            }
        }
        self.take(Kind::RParen)?;
        
        let returns_type = if is_main {
            if !params.is_empty() { return Err(self.fail("main cannot have parameters")); }
            Type::Int
        } else {
            self.take(Kind::Arrow)?;
            self.return_annotation()?
        };
        self.in_main = is_main;
        self.return_type = returns_type;
        // Parameter scope: lives for the whole body so return paths can
        // release owned parameters.
        self.scopes.push(params.iter().map(|&(var_id, ty)| (var_id, ty)).collect());
        let body = match self.lower_resolved_scalar_body() {
            Some(body) => body,
            None => {
                // Route-coverage report (P1.1): with AE_ROUTE_REPORT=1 the
                // legacy fallback names each function it takes, so a corpus
                // run measures how much of the front end still depends on
                // the old body parser. Behaviour is unchanged.
                if std::env::var_os("AE_ROUTE_REPORT").is_some() {
                    let offset = self.tokens.get(self.pos).map(|token| token.offset).unwrap_or(0);
                    eprintln!("route legacy function id={id} at {offset}");
                }
                // The endgame switch (P1.4): the resolved route IS the body
                // parser. A body it does not take is a diagnostic, not a
                // silent fallback; the legacy typed block parser stays
                // reachable during its deletion behind the escape hatch
                // AE_ALLOW_LEGACY_ROUTE=1. (AE_DENY_LEGACY_ROUTE is kept as a
                // no-op alias for the gates that still set it.)
                if std::env::var_os("AE_ALLOW_LEGACY_ROUTE").is_none() {
                    let offset = self.tokens.get(self.pos).map(|token| token.offset).unwrap_or(0);
                    // The shared module parse recovered from a statement-level
                    // error in THIS body: its diagnostic is the answer, so it
                    // is reported instead of a route denial.
                    if let Some(diagnostic) = self.resolved_expressions().shared_body_errors.get(&offset).cloned() {
                        return Err(diagnostic);
                    }
                    return Err(self.error_at(offset, "the legacy body parser is disabled (AE_ALLOW_LEGACY_ROUTE=1 re-enables it); this function's body is not on the resolved route yet"));
                }
                self.block()
            }
        };
        let params_scope = self.scopes.pop().unwrap();
        let mut body = body?;
        let param_owned: Vec<(usize, Type)> = params_scope.iter().filter(|(_, ty)| ty.owned()).copied().collect();
        if !param_owned.is_empty() { body.push(Statement::Frees(param_owned)); }
        if !is_main && !returns(&body) { return Err(self.fail("function may exit without returning a value")); }
        let function = Function { id, params, body, is_main, returns: returns_type };
        self.adjudicate_deferred_conflicts(&function)?;
        Ok(function)
    }
    pub(super) fn cached_function_specialization(&self, name: &str, ranges: &[std::ops::Range<usize>]) -> Option<(&GenericFn, &Instance)> {
        let &gid = self.generics.fn_names.get(name)?;
        let def = self.generics.fn_defs.get(gid)?;
        if ranges.len() != def.params.len() || ranges.len() > 8 {
            return None;
        }
        let arguments = self.shared_specialization_types(ranges)?;
        let instance = self.generics.fn_done.get(&super::types::specialization_key(gid, &arguments, self.ctx))?;
        Some((def, instance))
    }

    // Parse a generic function's own tokens once per argument list. The
    // sub-parser shares this program's maps and id space, so a nested
    // instantiation behaves exactly like one written at the top level.
    pub(super) fn instantiate_fn(&mut self, gid: usize, args: Vec<Type>, offset: usize) -> Result<Instance, ae_span::Diagnostic> {
        let def = self.generics.fn_defs[gid].clone();
        if args.len() != def.params.len() {
            return Err(self.error_at(offset, &format!("'{}' takes {} type argument(s), found {}", def.name, def.params.len(), args.len())));
        }
        self.require_public("function", &def.name, &def.file, def.public)?;
        for (index, bounds) in def.bounds.iter().enumerate() {
            for &bound in bounds {
                if !bound.allows(args[index]) {
                    return Err(self.error_at(offset, &format!(
                        "type argument '{}' for '{}' does not satisfy '{}'; that constraint allows {}",
                        args[index].name(self.ctx), def.name, bound.name(), bound.allowed_types())));
                }
            }
        }
        for (index, traits) in def.trait_bounds.iter().enumerate() {
            for &tid in traits {
                let trait_name = self.generics.traits[tid].name.clone();
                if !self.generics.impls.contains(&(tid, args[index].name(self.ctx))) {
                    return Err(self.error_at(offset, &format!(
                        "no impl of trait '{}' for '{}'; required by '{}'",
                        trait_name, args[index].name(self.ctx), def.name)));
                }
            }
        }
        let key = super::types::specialization_key(gid, &args, self.ctx);
        if let Some(instance) = self.generics.fn_done.get(&key) { return Ok(instance.clone()); }
        if self.generics.fn_in_progress.contains(&key) {
            let trace = self.instantiation_trace(&self.instantiation_display(&def.name, &args));
            return Err(self.error_at(offset, &format!("'{}' is already being instantiated with these type arguments; the expansion would never finish (instantiated: {})", def.name, trace)));
        }
        self.generics.fn_in_progress.insert(key.clone());
        self.generics.instantiation_trace.push(("function", key.clone(), self.instantiation_display(&def.name, &args)));
        let id = self.generics.next_fn_id;
        self.generics.next_fn_id += 1;
        let mut subst = HashMap::new();
        let mut trait_bounds = HashMap::new();
        for (index, param) in def.params.iter().enumerate() {
            subst.insert(param.clone(), args[index]);
            trait_bounds.insert(param.clone(), def.trait_bounds[index].clone());
        }
        let mut sub = Parser {
            file: def.file.clone(), source: &def.source, tokens: Rc::clone(&def.tokens), shared_source: Rc::clone(&def.source), pos: def.start,
            names: HashMap::new(), binding_ranges: HashMap::new(), capture_origins: HashMap::new(), resolved_expressions: self.expression_index_for(&def.file, &def.tokens), next_var: 0, expression_depth: 0, expression_nodes: 0, block_depth: 0,
            loop_depth: 0, loop_scopes: Vec::new(), loop_labels: Vec::new(), next_label: 0, unsafe_depth: 0,
            functions: &mut *self.functions, structs: &mut *self.structs, struct_names: &mut *self.struct_names,
            enums: &mut *self.enums, enum_names: &mut *self.enum_names, generics: &mut *self.generics, ctx: &mut *self.ctx,
            id_order: Vec::new(), in_main: false, return_type: Type::Int, moved: std::collections::HashSet::new(),
            borrows: Vec::new(), deferred_write_conflicts: Vec::new(), deferred_borrow_spawns: Vec::new(), call_borrows: Vec::new(), mut_borrows: Vec::new(), plus_operand_depth: 0, scopes: Vec::new(), owned_params: Vec::new(), subst, trait_bounds, receiver: None, impl_self: None, inherent_impl: false, shadow_ok: false, pending: Vec::new(),
        };
        let parsed = sub.function_rest_with_parameters(false, id, def.bindings.as_deref());
        let mut nested = std::mem::take(&mut sub.pending);
        drop(sub);
        self.generics.fn_in_progress.remove(&key);
        self.generics.instantiation_trace.pop();
        let function = parsed?;
        let instance: Instance = (id, function.params.iter().map(|&(_, ty)| ty).collect(), function.returns);
        self.pending.push(function);
        self.pending.append(&mut nested);
        self.generics.fn_done.insert(key.clone(), instance.clone());
        Ok(instance)
    }
    // One instantiation of a generic impl method (ADR-0187). The receiver's
    // type is built from the kind the declaration recorded and the struct
    // instantiation the call arrived on; the body is parsed once per
    // (method, argument list) pair with the impl's parameters substituted.
    pub(super) fn instantiate_impl_method(&mut self, impl_name: &str, method: &str, sid: usize, gid: usize, args: &[Type], offset: usize) -> Result<Instance, ae_span::Diagnostic> {
        let Some(def) = self.generics.generic_impls.get(impl_name).cloned() else {
            return Err(self.error_at(offset, &format!("'{impl_name}' has no generic impl block")));
        };
        let is_enum_target = self.generics.names.contains_key(impl_name);
        if args.len() != def.params.len() {
            return Err(self.error_at(offset, &format!("'{impl_name}' takes {} type argument(s), found {}", def.params.len(), args.len())));
        }
        let Some(mdef) = def.methods.iter().find(|m| m.name == method).cloned() else {
            let mut names: Vec<String> = def.methods.iter().map(|m| m.name.clone()).collect();
            names.sort();
            return Err(self.error_at(offset, &format!("the impl for '{impl_name}' has no method '{method}'; it has {}", names.join(", "))));
        };
        let key = format!("{gid}#{method}#{}", args.iter().map(|t| t.name(self.ctx)).collect::<Vec<_>>().join(","));
        if let Some(instance) = self.generics.generic_impl_done.get(&key) { return Ok(instance.clone()); }
        if self.generics.impl_in_progress.contains(&key) {
            let display = format!("{}::{}", self.instantiation_display(impl_name, args), method);
            let trace = self.instantiation_trace(&display);
            return Err(self.error_at(offset, &format!("'{impl_name}#{method}' is already being instantiated with these type arguments; the expansion would never finish (instantiated: {trace})")));
        }
        self.generics.impl_in_progress.insert(key.clone());
        self.generics.instantiation_trace.push(("method", key.clone(), format!("{}::{}", self.instantiation_display(impl_name, args), method)));
        let id = self.generics.next_fn_id;
        self.generics.next_fn_id += 1;
        // Enum receivers are by value only (ADR-0196): the Shared/Mutable
        // arms cannot arise for an enum target, and the guard makes that
        // explicit rather than silently building a struct-shaped receiver.
        let receiver_ty = mdef.receiver.map(|kind| match (kind, is_enum_target) {
            (ReceiverKind::ByValue, false) => Type::Struct(sid),
            (ReceiverKind::Shared, false) => Type::StructRef(sid),
            (ReceiverKind::Mutable, false) => Type::StructMut(sid),
            (ReceiverKind::ByValue, true) => Type::Enum(self.enums[sid].name, sid),
            (ReceiverKind::Shared, true) | (ReceiverKind::Mutable, true) => unreachable!("enum receivers are by value only"),
        });
        // `Self` inside the body is the instantiation's type: an enum's
        // display name is interned, so the borrow of self.enums ends before
        // the sub-parser takes the shared maps mutably.
        let impl_self_ty = if is_enum_target {
            let display = self.enums[sid].name;
            Type::Enum(display, sid)
        } else {
            Type::Struct(sid)
        };
        let mut subst = HashMap::new();
        for (index, param) in def.params.iter().enumerate() {
            subst.insert(param.clone(), args[index]);
        }
        let mut sub = Parser {
            file: def.file.clone(), source: &mdef.source, tokens: Rc::clone(&mdef.tokens), shared_source: Rc::clone(&mdef.source), pos: mdef.body_start,
            names: HashMap::new(), binding_ranges: HashMap::new(), capture_origins: HashMap::new(), resolved_expressions: self.expression_index_for(&def.file, &mdef.tokens), next_var: 0, expression_depth: 0, expression_nodes: 0, block_depth: 0,
            loop_depth: 0, loop_scopes: Vec::new(), loop_labels: Vec::new(), next_label: 0, unsafe_depth: 0,
            functions: &mut *self.functions, structs: &mut *self.structs, struct_names: &mut *self.struct_names,
            enums: &mut *self.enums, enum_names: &mut *self.enum_names, generics: &mut *self.generics, ctx: &mut *self.ctx,
            id_order: Vec::new(), in_main: false, return_type: Type::Int, moved: std::collections::HashSet::new(),
            borrows: Vec::new(), deferred_write_conflicts: Vec::new(), deferred_borrow_spawns: Vec::new(), call_borrows: Vec::new(), mut_borrows: Vec::new(), plus_operand_depth: 0, scopes: Vec::new(), owned_params: Vec::new(),
            subst, trait_bounds: HashMap::new(), receiver: receiver_ty,
            impl_self: Some(impl_self_ty), inherent_impl: true, shadow_ok: false, pending: Vec::new(),
        };
        let parsed = sub.function_rest(false, id);
        let mut nested = std::mem::take(&mut sub.pending);
        drop(sub);
        self.generics.impl_in_progress.remove(&key);
        self.generics.instantiation_trace.pop();
        let function = parsed?;
        let instance: Instance = (id, function.params.iter().map(|&(_, ty)| ty).collect(), function.returns);
        self.pending.push(function);
        self.pending.append(&mut nested);
        self.generics.generic_impl_done.insert(key.clone(), instance.clone());
        Ok(instance)
    }
    // The argument list of a call, starting at '(' (ADR-31 shares it between
    // ordinary calls and generic instantiations).
    pub(super) fn function_value_argument(&mut self, callee: &str, position: usize, sig: usize) -> Result<Argument, ae_span::Diagnostic> {
        let expected = Type::Fn(sig);
        let (params, returns) = self.ctx.fn_sig(sig);
        if let Kind::Ident(name) = self.current() {
            let ends_argument = self.tokens.get(self.pos + 1)
                .is_some_and(|token| matches!(token.kind, Kind::Comma | Kind::RParen));
            if ends_argument && !self.names.contains_key(name)
                && let Some(function) = self.functions.get(name) {
                    if !function.public && function.file != self.file {
                        return Err(self.fail(&format!("function '{name}' is private to {}", function.file)));
                    }
                    if function.extern_c || self.ctx.is_unsafe_fn(function.id) {
                        return Err(self.fail(&format!("function '{name}' cannot be passed as a safe function value")));
                    }
                    if function.params != params || function.ret != returns {
                        return Err(self.fail(&format!("argument {position} to '{callee}' requires a function of type {}, but '{name}' has a different signature", expected.name(self.ctx))));
                    }
                    let id = function.id;
                    self.pos += 1;
                    return Ok(Argument::FnPtr(id));
                }
        }

        let (value, ty) = self.expression(0, false)?;
        if ty == expected {
            return Ok(Argument::Value(value));
        }
        let cid = match (value, ty) {
            (Expr::ClosureLit(cid), _) | (Expr::Variable(..), Type::Closure(cid)) => cid,
            _ => return Err(self.fail(&format!("argument {position} to '{callee}' requires a function value or non-capturing closure of type {}, found {}", expected.name(self.ctx), ty.name(self.ctx)))),
        };
        let def = &self.generics.closures[cid];
        if !def.value_captures.is_empty() || !def.borrow_captures.is_empty() {
            return Err(self.fail("a closure with captures cannot be passed as a function value; make it parameter-only or bind its inputs as arguments"));
        }
        if def.params != params || def.ret != returns {
            let got = format!("fn({}) -> {}", def.params.iter().map(|t| t.name(self.ctx)).collect::<Vec<_>>().join(", "), def.ret.name(self.ctx));
            return Err(self.fail(&format!("argument {position} to '{callee}' must be a closure of type {}, found {}", expected.name(self.ctx), got)));
        }
        Ok(Argument::FnPtr(def.fn_id))
    }

    fn borrowed_argument_name(&mut self, mutable: bool) -> Result<String, ae_span::Diagnostic> {
        use ae_syntax::expression::{parse_borrow_argument, ExpressionKind};
        let (argument, end) = parse_borrow_argument(
            &self.file, self.source, &self.tokens, self.pos, Some(mutable),
        )?;
        self.pos = end;
        let ExpressionKind::BorrowArgument { name, .. } = argument.kind else {
            unreachable!("borrow argument parser returns a borrowed binding");
        };
        Ok(name)
    }

    pub(super) fn call_arguments(&mut self, name: &str, types: &[Type]) -> Result<Vec<Argument>, ae_span::Diagnostic> {
        self.pos += 1;
        let args = if *self.current() == Kind::RParen {
            Vec::new()
        } else {
            ae_syntax::parse_comma_list(
                self,
                |parser, index| {
                    if index >= 64 {
                        return Err(parser.fail("function argument limit exceeded (64)"));
                    }
                    let expected = *types.get(index)
                        .ok_or_else(|| parser.fail(&format!("too many arguments to '{name}'")))?;
                    parser.typed_call_argument(name, index, expected)
                },
                |parser| {
                    if *parser.current() != Kind::Comma {
                        return false;
                    }
                    parser.pos += 1;
                    true
                },
            )?
        };
        self.take(Kind::RParen)?;
        if args.len() != types.len() { return Err(self.fail(&format!("function '{name}' expects {} arguments, got {}", types.len(), args.len()))); }
        Ok(args)
    }

    fn typed_call_argument(
        &mut self,
        name: &str,
        index: usize,
        expected: Type,
    ) -> Result<Argument, ae_span::Diagnostic> {
        if expected == Type::StrView {
            let (value, ty) = self.expression(0, false)?;
            if !matches!(ty, Type::String | Type::StrView) {
                return Err(self.fail(&format!("argument must be a string or string view, found {}", ty.name(self.ctx))));
            }
            Ok(Argument::View(value))
        } else if expected == Type::VecRef || expected == Type::StrVecRef || expected == Type::VecFRef {
            let arg = self.borrowed_argument_name(false)?;
            let (var, _, actual) = *self.names.get(&arg).ok_or_else(|| self.fail(&format!("unknown variable '{arg}'")).with_code("E0101"))?;
            let owned_shape = match expected {
                Type::VecRef => Type::Vec,
                Type::StrVecRef => Type::StrVec,
                _ => Type::VecF,
            };
            let Some(borrow) = CallBorrow::classify(expected, actual) else {
                return Err(self.fail(&format!("argument must be {}, found {}", owned_shape.name(self.ctx), actual.name(self.ctx))));
            };
            if self.moved.contains(&var) { return Err(moved_value_hint(self.binding_label(self.fail(&format!("cannot borrow '{arg}' because it was moved")).with_code("E0102"), var))); }
            // An owned vector needs its address taken; a parameter that
            // is already `const ae_vec *` is forwarded unchanged.
            // The flag means "take the address", not "is a vector".
            Ok(Argument::Borrow(var, borrow.address()))
        } else if let Type::Array(len) = expected {
            let arg = self.borrowed_argument_name(false)?;
            let (var, _, actual) = *self.names.get(&arg).ok_or_else(|| self.fail(&format!("unknown array '{arg}'")))?;
            let Some(borrow) = CallBorrow::classify(expected, actual) else {
                return Err(self.fail(&format!("argument requires array of length {len}")));
            };
            Ok(Argument::Borrow(var, borrow.address()))
        } else if let Type::Fn(sig) = expected {
            self.function_value_argument(name, index + 1, sig)
        } else if matches!(expected, Type::StructRef(_)) {
            // `&Struct` (ADR-0184) borrows a bare struct variable for
            // the duration of the call, exactly like the container
            // borrows: no move, no copy, and the caller may not write
            // or move the struct while the borrow can be read.
            let arg = self.borrowed_argument_name(false)?;
            let (var, _, actual) = *self.names.get(&arg).ok_or_else(|| self.fail(&format!("unknown variable '{arg}'")).with_code("E0101"))?;
            match CallBorrow::classify(expected, actual) {
                Some(CallBorrow::Struct { address }) => {
                    if self.moved.contains(&var) { return Err(moved_value_hint(self.binding_label(self.fail(&format!("cannot borrow '{arg}' because it was moved")).with_code("E0102"), var))); }
                    if self.mut_borrows.iter().any(|&(bid, _)| bid == var) {
                        return Err(self.fail(&format!("cannot borrow '{arg}' while it is mutably borrowed in this statement")));
                    }
                    // The borrow lives only as long as this call, so it
                    // goes on the statement-scoped register: the next
                    // statement may write the struct again.
                    if !self.is_borrowed(var) {
                        self.call_borrows.push((var, self.scopes.len()));
                    }
                    Ok(Argument::Borrow(var, address))
                }
                // A borrow parameter forwards its pointer unchanged:
                // the callee's reads are already covered by the borrow
                // the caller received. A mutable borrow may shrink to
                // a read-only one; the reverse is not allowed.
                Some(CallBorrow::Forward) => {
                    Ok(Argument::Borrow(var, false))
                }
                _ => Err(self.fail(&format!("argument must be {}, found {}", expected.name(self.ctx), actual.name(self.ctx)))),
            }
        } else if matches!(expected, Type::StructMut(_)) {
            // `&mut Struct` (ADR-0185) lends the struct for writing:
            // spelled `&mut value` at the call site, exclusive for the
            // duration of the call, and the lender must own a mutable
            // variable — the callee writes what the caller owns.
            let arg = self.borrowed_argument_name(true)?;
            let (var, mutable, actual) = *self.names.get(&arg).ok_or_else(|| self.fail(&format!("unknown variable '{arg}'")).with_code("E0101"))?;
            match CallBorrow::classify(expected, actual) {
                Some(CallBorrow::Struct { address }) => {
                    if self.moved.contains(&var) { return Err(moved_value_hint(self.binding_label(self.fail(&format!("cannot borrow '{arg}' because it was moved")).with_code("E0102"), var))); }
                    if !mutable {
                        return Err(self.immutable_binding_error(self.fail(&format!("cannot borrow '{arg}' mutably: it was declared without 'var'")).with_code("E0103"), var));
                    }
                    if self.is_borrowed(var) {
                        // The callee may write any field of the lent
                        // struct, so the call releases the viewed bytes
                        // at the call point (M2.5): with only view
                        // borrows active the conflict defers to the
                        // graph's ordered adjudication, like the write
                        // it is. Same-statement call or mut borrows keep
                        // the on-sight rejection.
                        if !self.only_view_borrows_active(var) {
                            return Err(self.fail(&format!("cannot borrow '{arg}' mutably while it is borrowed in this statement")));
                        }
                        let offset = self.tokens.get(self.pos.saturating_sub(1)).map(|t| t.offset).unwrap_or(0);
                        self.deferred_write_conflicts.push((var, offset, arg.clone(), "mutate", None));
                    }
                    self.mut_borrows.push((var, self.scopes.len()));
                    Ok(Argument::Borrow(var, address))
                }
                _ => Err(self.fail(&format!("argument must be {}, found {}", expected.name(self.ctx), actual.name(self.ctx)))),
            }
        } else {
            // By-value arguments always consume their value:
            // the callee releases the parameter, so the caller
            // must not use the slot afterwards even when the
            // surrounding statement is not a move context.
            let (value, ty) = self.expression(0, true)?;
            self.check_value_argument_type(name, index, expected, ty, None)?;
            Ok(Argument::Value(value))
        }
    }
}
