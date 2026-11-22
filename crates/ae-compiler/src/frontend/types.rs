//! Type syntax, definitions, constructors, and generic instantiation.
use super::*;

impl EnumDef {
    pub(super) fn variant_payload(&self, name: &str) -> Option<(usize, &[Type])> {
        self.variants.iter().enumerate().find_map(|(index, (variant, payload))| {
            (variant == name).then_some((index, payload.as_slice()))
        })
    }
}

fn enum_instance_key(gid: usize, arguments: &[Type]) -> String {
    format!("{gid}:{}", arguments.iter().map(|ty| ty.name()).collect::<Vec<_>>().join(","))
}

pub(super) fn scalar_type(name: &str) -> Option<Type> {
    match name {
        "i64" => Some(Type::Int),
        "f64" => Some(Type::F64),
        "bool" => Some(Type::Bool),
        _ => None,
    }
}

impl Parser<'_> {
    pub(super) fn read_type_arguments(
        &mut self,
        parse_type: impl FnMut(&mut Self) -> Result<Type, ae_span::Diagnostic>,
    ) -> Result<Vec<Type>, ae_span::Diagnostic> {
        let mut parse_type = parse_type;
        ae_syntax::parse_comma_list(
            self,
            |parser, _| parse_type(parser),
            |parser| {
                if *parser.current() != Kind::Comma {
                    return false;
                }
                parser.pos += 1;
                true
            },
        )
    }

    pub(super) fn named_type(&mut self) -> Result<Type, ae_span::Diagnostic> {
        let name = self.identifier()?;
        // Inside a generic instantiation a type parameter stands for the
        // argument the call site supplied; nothing must reach code generation
        // with a parameter still in it.
        if let Some(&concrete) = self.subst.get(&name) { return Ok(concrete); }
        if let Some(ty) = scalar_type(&name) { return Ok(ty); }
        match name.as_str() {
            "string" => Ok(Type::String),
            // The copy-semantics cell handle (spec/concurrency.md §8): a
            // parameter typed `atomic` shares the caller's cell.
            "atomic" => Ok(Type::Atomic),
            // The copy-semantics mutex handle and owned lock guard (ADR-0216).
            "mutex" => Ok(Type::Mutex),
            "guard" => Ok(Type::Guard),
            // Copy-semantics i64 channel handle (M4.3, spec/concurrency.md §10).
            "chan" => Ok(Type::Chan),
            // Copy-semantics single-worker task executor handle (M4.2, ADR-0218).
            "exec" => Ok(Type::Exec),
            // Owned structured task scope and copy-semantics work-stealing pool
            // (ADR-0220); cancel tokens are ae source since ADR-0224 R2.4.
            "task_scope" => Ok(Type::TaskScope),
            "pool" => Ok(Type::Pool),
            // The copy-semantics shared box handle (ADR-0216, spec/concurrency.md §9):
            // copying the handle shares the process-lifetime box; no handle towers.
            "shared" => {
                if *self.current() != Kind::LBracket {
                    return Err(self.fail("'shared' requires a type argument, e.g. shared[i64]"));
                }
                self.pos += 1;
                let elem = self.named_type()?;
                self.take(Kind::RBracket)?;
                if matches!(elem, Type::Shared(_)) {
                    return Err(self.fail("cannot nest a shared handle inside shared[...] (no handle towers)"));
                }
                if !matches!(elem, Type::Int | Type::Bool | Type::F64 | Type::Atomic) {
                    return Err(self.fail("shared[T] in v1 requires a copy-semantics element type (i64, bool, f64, or atomic)"));
                }
                Ok(Type::Shared(intern_shared(elem)))
            }
            "vec" => {
                // `vec` alone is the i64 vector; `vec[string]` and `vec[f64]`
                // (ADR-61) name the others, so a type annotation can say which
                // it means instead of leaving owned vectors unnameable.
                if *self.current() != Kind::LBracket { return Ok(Type::Vec); }
                self.pos += 1;
                let element = self.identifier()?;
                self.take(Kind::RBracket)?;
                match element.as_str() {
                    "string" => Ok(Type::StrVec),
                    "f64" => Ok(Type::VecF),
                    _ => Err(self.fail("only vec[string] and vec[f64] name an element type; vector elements are i64 by default")),
                }
            }
            "map" => {
                // Two shapes exist (ADR-65); the shape is spelled out so the
                // code says what it means instead of guessing from context.
                self.take(Kind::LBracket)?;
                if self.identifier()? != "string" { return Err(self.fail("map keys must be string")); }
                self.take(Kind::Comma)?;
                let value = self.identifier()?;
                self.take(Kind::RBracket)?;
                match value.as_str() {
                    "i64" => Ok(Type::Map),
                    "f64" => Ok(Type::MapF),
                    _ => Err(self.fail("map values must be i64 or f64")),
                }
            }
            other => {
                if let Some(&gid) = self.generics.names.get(other) {
                    if *self.current() != Kind::LBracket {
                        return Err(self.fail(&format!("'{other}' is a generic enum; write its type arguments, e.g. {other}[string]")));
                    }
                    let offset = self.tokens[self.pos].offset;
                    self.pos += 1;
                    let args = self.read_type_arguments(Self::type_argument)?;
                    self.take(Kind::RBracket)?;
                    let eid = self.instantiate_enum(gid, args, offset)?;
                    let def = &self.enums[eid];
                    self.require_public("enum", def.name, &def.file.clone(), def.public)?;
                    return Ok(Type::Enum(def.name, eid));
                }
                if let Some(&gid) = self.generics.struct_names.get(other) {
                    if *self.current() != Kind::LBracket {
                        return Err(self.fail(&format!("'{other}' is a generic struct; write its type arguments, e.g. {other}[string]")));
                    }
                    let offset = self.tokens[self.pos].offset;
                    self.pos += 1;
                    let args = self.read_type_arguments(Self::named_type)?;
                    self.take(Kind::RBracket)?;
                    let sid = self.instantiate_struct(gid, args, offset)?;
                    let def = &self.structs[sid];
                    self.require_public("struct", &def.name.clone(), &def.file.clone(), def.public)?;
                    return Ok(Type::Struct(sid));
                }
                if other == "Self" {
                    // `Self` (ADR-0186) stands for the type the enclosing impl
                    // block targets; outside one, there is nothing to stand for.
                    return self.impl_self.clone().ok_or_else(|| {
                        self.fail("'Self' is only available inside an impl block")
                    });
                }
                match self.struct_names.get(other) {
                Some(&id) => {
                    let def = &self.structs[id];
                    self.require_public("struct", &def.name.clone(), &def.file.clone(), def.public)?;
                    Ok(Type::Struct(id))
                }
                None => match self.enum_names.get(other) {
                    Some(&id) => {
                        let (name, file, public) = (self.enums[id].name, self.enums[id].file.clone(), self.enums[id].public);
                        self.require_public("enum", name, &file, public)?;
                        Ok(Type::Enum(name, id))
                    }
                    None => Err(self.fail(&format!("unknown type '{}'", other))),
                },
                }
            }
        }
    }
    // `::Variant(payload, ...)` once the enum type is known. Payload values are
    // moved in, so a variant owns whatever it carries.
    pub(super) fn enum_literal(&mut self, eid: usize, offset: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        let (vname, end) = ae_syntax::pattern::parse_variant_suffix(
            &self.file, self.source, &self.tokens, self.pos,
        )?;
        self.pos = end;
        let def = self.enums[eid].clone();
        self.require_public("enum", def.name, &def.file, def.public)?;
        let (vidx, fields) = def.variant_payload(&vname)
            .ok_or_else(|| self.error_at(offset, &format!("enum '{}' has no variant '{}'", def.name, vname)))?;
        let mut values = Vec::new();
        if *self.current() == Kind::LParen {
            self.pos += 1;
            for (index, field) in fields.iter().enumerate() {
                // String payloads are moved into the variant.
                let (value, value_ty) = self.expression(0, *field == Type::String)?;
                if !self.matches_value_type(*field, value_ty, None) {
                    return Err(self.fail(&format!("payload {} of '{}::{}' must be {}", index + 1, def.name, vname, field.name())));
                }
                values.push(value);
                if index + 1 < fields.len() { self.take(Kind::Comma)?; }
            }
            if *self.current() == Kind::Comma {
                return Err(self.fail(&format!("variant '{}::{}' takes {} payload value(s)", def.name, vname, fields.len())));
            }
            self.take(Kind::RParen)?;
        } else if !fields.is_empty() {
            return Err(self.fail(&format!("variant '{}::{}' needs {} payload value(s)", def.name, vname, fields.len())));
        }
        Ok((Expr::EnumLit(def.name, eid, vidx, values), Type::Enum(def.name, eid)))
    }
    pub(super) fn constructor_field(
        &self,
        sid: usize,
        name: &str,
        slots: &[Option<Expr>],
        diagnostic_pos: Option<usize>,
    ) -> Result<(usize, Type), ae_span::Diagnostic> {
        let def = &self.structs[sid];
        let diagnostic = |message: String| match diagnostic_pos {
            Some(offset) => self.error_at(offset, &message),
            None => self.fail(&message),
        };
        let (index, (_, ty)) = def.fields.iter().enumerate().find(|(_, (field, _))| field == name)
            .ok_or_else(|| diagnostic(format!("struct '{}' has no field '{}'", def.name, name)))?;
        if slots[index].is_some() {
            return Err(diagnostic(format!("field '{}' specified twice", name)));
        }
        Ok((index, *ty))
    }

    pub(super) fn finish_struct_literal(
        &self,
        sid: usize,
        offset: usize,
        slots: Vec<Option<Expr>>,
    ) -> Result<(Expr, Type), ae_span::Diagnostic> {
        let def = &self.structs[sid];
        let values = slots.into_iter().enumerate().map(|(i, slot)| {
            slot.ok_or_else(|| self.error_at(offset, &format!("missing field '{}' in '{}' construction", def.fields[i].0, def.name)))
        }).collect::<Result<_, _>>()?;
        Ok((Expr::StructLit(sid, values), Type::Struct(sid)))
    }

    // `Name { field: expr, ... }` once the layout is known. Fields may appear
    // in any order but each exactly once; owned fields are moved in.
    pub(super) fn struct_literal(&mut self, sid: usize, offset: usize) -> Result<(Expr, Type), ae_span::Diagnostic> {
        let name = self.structs[sid].name.clone();
        if std::env::var_os("AE_DEBUG_MIR").is_some() {
            eprintln!("struct literal debug: sid {} name '{}' at token {}", sid, name, self.pos);
        }
        self.take(Kind::LBrace)?;
        let mut slots: Vec<Option<Expr>> = (0..self.structs[sid].fields.len()).map(|_| None).collect();
        while *self.current() != Kind::RBrace {
            let fname = self.identifier()?;
            let (fidx, expected) = self.constructor_field(sid, &fname, &slots, None)?;
            self.take(Kind::Colon)?;
            let (value, vty) = self.expression(0, true)?;
            self.check_field_value_type(&fname, expected, vty, None)?;
            slots[fidx] = Some(value);
            if *self.current() == Kind::Comma { self.pos += 1; }
        }
        self.take(Kind::RBrace)?;
        self.finish_struct_literal(sid, offset, slots)
    }
    // `enum Name { ... }` or `enum Name[T, U] { ... }`. A generic declaration
    // produces no layout at all; only instantiations do (ADR-21).
    pub(super) fn parse_enum_def(
        &mut self,
        public: bool,
        parsed_variants: Option<&[ae_syntax::declaration::EnumVariant]>,
    ) -> Result<(), ae_span::Diagnostic> {
        self.pos += 1;
        let name = self.identifier()?;
        if RESERVED.contains(&name.as_str()) { return Err(self.fail(&format!("'{}' is a built-in name and cannot be an enum", name))); }
        if self.struct_names.contains_key(&name) || self.generics.struct_names.contains_key(&name) { return Err(self.fail(&format!("'{}' is already a struct", name))); }
        if self.enum_names.contains_key(&name) || self.generics.names.contains_key(&name) { return Err(self.fail(&format!("duplicate enum '{}'", name))); }
        let params = self.type_params()?;
        self.take(Kind::LBrace)?;
        let mut variants: Vec<(String, Vec<Payload>)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        while *self.current() != Kind::RBrace {
            let parsed_variant = parsed_variants.and_then(|items| items.get(variants.len()));
            let vname = match parsed_variant {
                Some(variant) if variant.range.start == self.tokens[self.pos].offset => {
                    self.pos += 1;
                    variant.name.clone()
                }
                _ => self.identifier()?,
            };
            if !seen.insert(vname.clone()) { return Err(self.fail(&format!("duplicate variant '{}' in enum '{}'", vname, name))); }
            let mut fields = Vec::new();
            if *self.current() == Kind::LParen {
                self.pos += 1;
                if *self.current() != Kind::RParen {
                    loop {
                        if fields.len() >= 16 { return Err(self.fail("variant payload limit exceeded (16)")); }
                        let fty = self.variant_payload(&params)?;
                        fields.push(fty);
                        if *self.current() != Kind::Comma { break; }
                        self.pos += 1;
                    }
                }
                self.take(Kind::RParen)?;
            }
            variants.push((vname, fields));
            if *self.current() == Kind::Comma { self.pos += 1; }
        }
        self.take(Kind::RBrace)?;
        if variants.is_empty() { return Err(self.fail(&format!("enum '{}' needs at least one variant", name))); }
        if variants.len() > 256 { return Err(self.fail("enum variant limit exceeded (256)")); }
        if params.is_empty() {
            let id = self.enums.len();
            // Type names are interned once per declaration so Type stays Copy and
            // diagnostics can print the declared name.
            let interned: &'static str = Box::leak(name.clone().into_boxed_str());
            self.enum_names.insert(name.clone(), id);
            let concrete = variants.into_iter().map(|(vname, fields)| (vname, fields.into_iter().map(|p| match p {
                Payload::Concrete(ty) => ty,
                Payload::Param(_) | Payload::Nested(_) => unreachable!("a non-generic enum has no type parameters"),
            }).collect())).collect();
            self.enums.push(EnumDef { name: interned, variants: concrete, file: self.file.clone(), public, pattern: name });
        } else {
            let gid = self.generics.defs.len();
            self.generics.names.insert(name.clone(), gid);
            self.generics.defs.push(GenericEnum { name, params, variants, file: self.file.clone(), public });
        }
        Ok(())
    }
    // `[T, U]` after a declaration name; empty for a plain definition. Single
    // uppercase letters keep a parameter from being mistaken for a real type.
    // `[T, U]` or `[T: Add + Ord, U: Copy]`. Constraints are parsed here for
    // every generic declaration, but only generic functions may use them.
    pub(super) fn type_params_with_bounds(&mut self) -> Result<Vec<(String, Vec<Bound>, Vec<usize>)>, ae_span::Diagnostic> {
        use ae_syntax::declaration::{type_bound_tokens, validate_type_parameter, MAX_TYPE_PARAMETERS};

        let mut params: Vec<(String, Vec<Bound>, Vec<usize>)> = Vec::new();
        if *self.current() != Kind::LBracket { return Ok(params); }
        self.pos += 1;
        loop {
            if params.len() >= MAX_TYPE_PARAMETERS { return Err(self.fail("type parameter limit exceeded (8)")); }
            let param = self.identifier()?;
            validate_type_parameter(&param, params.iter().map(|(name, _, _)| name.as_str()))
                .map_err(|message| self.fail(&message))?;
            let mut bounds = Vec::new();
            let mut traits = Vec::new();
            if *self.current() == Kind::Colon {
                self.pos += 1;
                let tokens = Rc::clone(&self.tokens);
                for (index, token) in type_bound_tokens(&tokens, self.pos) {
                    self.pos = index;
                    let at = token.offset;
                    let label = self.identifier()?;
                    let bound = match label.as_str() {
                        "Add" => Some(Bound::Add), "Eq" => Some(Bound::Eq),
                        "Ord" => Some(Bound::Ord), "Copy" => Some(Bound::Copy),
                        _ => None,
                    };
                    match bound {
                        Some(bound) => {
                            if bounds.contains(&bound) { return Err(self.fail(&format!("duplicate constraint '{label}'"))); }
                            bounds.push(bound);
                        }
                        None => {
                            // A user-declared trait. Traits are collected
                            // before anything else in the file and imports
                            // load first, so the name is either here or
                            // nowhere; a private one from another file is
                            // rejected like a private function would be.
                            let tid = *self.generics.trait_names.get(&label).ok_or_else(|| {
                                self.error_at(at, "unknown constraint; allowed: Add, Eq, Ord, Copy, or a trait declared in this program")
                            })?;
                            let trait_file = self.generics.traits[tid].file.clone();
                            if trait_file != self.file {
                                self.require_public("trait", &label, &trait_file, self.generics.traits[tid].public).map_err(|e| self.error_at(at, &e.to_string()))?;
                            }
                            if traits.contains(&tid) { return Err(self.fail(&format!("duplicate constraint '{label}'"))); }
                            traits.push(tid);
                        }
                    }
                }
            }
            params.push((param, bounds, traits));
            if *self.current() != Kind::Comma { break; }
            self.pos += 1;
        }
        self.take(Kind::RBracket)?;
        Ok(params)
    }
    pub(super) fn type_params(&mut self) -> Result<Vec<String>, ae_span::Diagnostic> {
        let params = self.type_params_with_bounds()?;
        if params.iter().any(|(_, bounds, traits)| !bounds.is_empty() || !traits.is_empty()) {
            return Err(self.fail("constraints are only supported on generic functions"));
        }
        Ok(params.into_iter().map(|(name, _, _)| name).collect())
    }
    pub(super) fn variant_payload(&mut self, params: &[String]) -> Result<Payload, ae_span::Diagnostic> {
        if let Kind::Ident(name) = self.current() {
            if let Some(index) = params.iter().position(|p| p == name) {
                self.pos += 1;
                return Ok(Payload::Param(index));
            }
        }
        let fty = self.named_type()?;
        // Payloads stay flat: a variant may carry scalars, strings or a vector,
        // but not a struct or another enum (no layout recursion).
        if !matches!(fty, Type::Int | Type::Bool | Type::F64 | Type::String | Type::Vec | Type::StrVec | Type::VecF) {
            return Err(self.fail("enum variant payloads must be i64, bool, f64, string, vec, vec[string], or a type parameter"));
        }
        Ok(Payload::Concrete(fty))
    }
    // One C layout per (declaration, argument) pair, generated on first use. The
    // budget bounds code growth for a program that instantiates endlessly.
    pub(super) fn instantiate_enum(&mut self, gid: usize, args: Vec<Type>, offset: usize) -> Result<usize, ae_span::Diagnostic> {
        let def = self.generics.defs[gid].clone();
        if args.len() != def.params.len() {
            return Err(self.error_at(offset, &format!("'{}' takes {} type argument(s), found {}", def.name, def.params.len(), args.len())));
        }
        let key = enum_instance_key(gid, &args);
        if let Some(&eid) = self.generics.done.get(&key) { return Ok(eid); }
        let display = self.instantiation_display(&def.name, &args);
        if self.generics.instantiation_trace.iter().any(|(kind, active, _)| *kind == "enum" && *active == key) {
            return Err(self.error_at(offset, &format!("'{}' is already being instantiated with these type arguments; the expansion would never finish (instantiated: {})",
                def.name, self.instantiation_trace(&display))));
        }
        let active_layouts = self.generics.instantiation_trace.iter().filter(|(kind, _, _)| *kind == "enum").count();
        if self.enums.len() + active_layouts >= MAX_ENUM_TYPES {
            return Err(self.error_at(offset, &format!("generic instantiation budget exceeded ({MAX_ENUM_TYPES} enum types) at '{}' (instantiated: {})",
                def.name, self.instantiation_trace(&display))));
        }
        self.generics.instantiation_trace.push(("enum", key.clone(), display));
        let variants = (|| {
            let mut variants: Vec<(String, Vec<Type>)> = Vec::new();
            for (vname, fields) in def.variants.iter() {
                let mut types = Vec::new();
                for p in fields.iter() {
                    types.push(self.resolve_payload(p, &args, offset)?);
                }
                variants.push((vname.clone(), types));
            }
            Ok(variants)
        })();
        self.generics.instantiation_trace.pop();
        let variants = variants?;
        let display = format!("{}[{}]", def.name, args.iter().map(|t| t.name()).collect::<Vec<_>>().join(", "));
        let interned: &'static str = Box::leak(display.into_boxed_str());
        let eid = self.enums.len();
        self.enums.push(EnumDef { name: interned, variants, file: def.file, public: def.public, pattern: def.name });
        self.generics.done.insert(key, eid);
        // Method calls on the instantiation resolve through its generic impl
        // block (same machinery as generic structs, ADR-0187/0196).
        self.generics.enum_origin.insert(eid, (gid, args));
        Ok(eid)
    }

    /// Look up an existing specialization without allocating layouts or
    /// changing the token cursor during speculative lowering.
    pub(super) fn cached_enum_specialization(&self, name: &str, ranges: &[std::ops::Range<usize>]) -> Option<usize> {
        let &gid = self.generics.names.get(name)?;
        let arguments = ranges.iter().map(|range| {
            let start = self.tokens.partition_point(|token| token.offset < range.start);
            let end = self.tokens.partition_point(|token| token.offset < range.end);
            match self.tokens.get(start..end)? {
                [Token { kind: Kind::Ident(name), .. }] => {
                    let ty = self.subst.get(name).copied().or_else(|| match name.as_str() {
                        "string" => Some(Type::String),
                        "vec" => Some(Type::Vec),
                        _ => scalar_type(name),
                    })?;
                    matches!(ty, Type::Int | Type::Bool | Type::F64 | Type::String | Type::Vec | Type::StrVec | Type::VecF).then_some(ty)
                }
                [Token { kind: Kind::Ident(name), .. }, Token { kind: Kind::LBracket, .. },
                 Token { kind: Kind::Ident(element), .. }, Token { kind: Kind::RBracket, .. }]
                    if name == "vec" && !self.subst.contains_key(name) => match element.as_str() {
                        "string" => Some(Type::StrVec),
                        "f64" => Some(Type::VecF),
                        _ => None,
                    },
                _ => None,
            }
        }).collect::<Option<Vec<_>>>()?;
        self.generics.done.get(&enum_instance_key(gid, &arguments)).copied()
    }
    // A concrete type argument. Structs and enums are rejected for the same
    // reason variant payloads are: no recursion and no hidden layout.
    pub(super) fn type_argument(&mut self) -> Result<Type, ae_span::Diagnostic> {
        let ty = self.named_type()?;
        if !matches!(ty, Type::Int | Type::Bool | Type::F64 | Type::String | Type::Vec | Type::StrVec | Type::VecF) {
            return Err(self.fail("type arguments must be i64, bool, f64, string, vec, or vec[string]"));
        }
        Ok(ty)
    }
    // A field type in a generic declaration: a type parameter, a concrete
    // named type, or a *nested instantiation* whose arguments may themselves
    // be the declaration's parameters — `inner: Pair[string, V]` inside
    // `struct Boxed[V]` stays a shape until the outer declaration is
    // instantiated (M1.10).
    pub(super) fn field_type(&mut self, params: &[String]) -> Result<Payload, ae_span::Diagnostic> {
        if *self.current() == Kind::Star {
            return Ok(Payload::Concrete(self.pointer_type()?));
        }
        if let Kind::Ident(name) = self.current() {
            if let Some(index) = params.iter().position(|p| p == name) {
                self.pos += 1;
                return Ok(Payload::Param(index));
            }
            let is_generic_struct = self.generics.struct_names.contains_key(name.as_str());
            let is_generic_enum = self.generics.names.contains_key(name.as_str());
            if (is_generic_struct || is_generic_enum)
                && self.tokens.get(self.pos + 1).map(|t| &t.kind) == Some(&Kind::LBracket) {
                let gid = if is_generic_struct { self.generics.struct_names[name.as_str()] } else { self.generics.names[name.as_str()] };
                self.pos += 1; // the name
                self.pos += 1; // '['
                let mut args = Vec::new();
                loop {
                    args.push(self.payload_argument(params)?);
                    if *self.current() != Kind::Comma { break; }
                    self.pos += 1;
                }
                self.take(Kind::RBracket)?;
                let index = self.generics.nested_shapes.len();
                self.generics.nested_shapes.push(NestedShape { gid, is_struct: is_generic_struct, args });
                return Ok(Payload::Nested(index));
            }
        }
        Ok(Payload::Concrete(self.named_type()?))
    }

    // One argument inside a nested instantiation: the enclosing declaration's
    // parameter, another nested instantiation, or a concrete named type.
    fn payload_argument(&mut self, params: &[String]) -> Result<Payload, ae_span::Diagnostic> {
        if let Kind::Ident(name) = self.current() {
            if let Some(index) = params.iter().position(|p| p == name) {
                self.pos += 1;
                return Ok(Payload::Param(index));
            }
            let is_generic_struct = self.generics.struct_names.contains_key(name.as_str());
            let is_generic_enum = self.generics.names.contains_key(name.as_str());
            if (is_generic_struct || is_generic_enum)
                && self.tokens.get(self.pos + 1).map(|t| &t.kind) == Some(&Kind::LBracket) {
                return self.field_type(params);
            }
        }
        Ok(Payload::Concrete(self.named_type()?))
    }

    // Resolve one payload against an argument list, instantiating nested
    // shapes recursively. Shared by struct fields, enum payloads, and the
    // nested-shape resolver itself.
    fn resolve_payload(&mut self, payload: &Payload, args: &[Type], offset: usize) -> Result<Type, ae_span::Diagnostic> {
        match payload {
            Payload::Concrete(ty) => Ok(*ty),
            Payload::Param(index) => Ok(args[*index]),
            Payload::Nested(index) => {
                let shape = self.generics.nested_shapes[*index].clone();
                let mut resolved = Vec::new();
                for arg in &shape.args {
                    resolved.push(self.resolve_payload(arg, args, offset)?);
                }
                if shape.is_struct {
                    let sid = self.instantiate_struct(shape.gid, resolved, offset)?;
                    Ok(Type::Struct(sid))
                } else {
                    let eid = self.instantiate_enum(shape.gid, resolved, offset)?;
                    Ok(Type::Enum(self.enums[eid].name, eid))
                }
            }
        }
    }
    // One C layout per (declaration, argument) pair, generated on first use.
    pub(super) fn instantiate_struct(&mut self, gid: usize, args: Vec<Type>, offset: usize) -> Result<usize, ae_span::Diagnostic> {
        let def = self.generics.struct_defs[gid].clone();
        if args.len() != def.params.len() {
            return Err(self.error_at(offset, &format!("'{}' takes {} type argument(s), found {}", def.name, def.params.len(), args.len())));
        }
        let key = format!("{gid}:{}", args.iter().map(|t| t.name()).collect::<Vec<_>>().join(","));
        if let Some(&sid) = self.generics.struct_done.get(&key) { return Ok(sid); }
        let display = self.instantiation_display(&def.name, &args);
        if self.generics.instantiation_trace.iter().any(|(kind, active, _)| *kind == "struct" && *active == key) {
            return Err(self.error_at(offset, &format!("'{}' is already being instantiated with these type arguments; the expansion would never finish (instantiated: {})",
                def.name, self.instantiation_trace(&display))));
        }
        let active_layouts = self.generics.instantiation_trace.iter().filter(|(kind, _, _)| *kind == "struct").count();
        if self.structs.len() + active_layouts >= MAX_STRUCT_TYPES {
            return Err(self.error_at(offset, &format!("generic instantiation budget exceeded ({MAX_STRUCT_TYPES} struct types) at '{}' (instantiated: {})",
                def.name, self.instantiation_trace(&display))));
        }
        self.generics.instantiation_trace.push(("struct", key.clone(), display));
        let fields = (|| {
            let mut fields: Vec<(String, Type)> = Vec::new();
            for (fname, payload) in def.fields.iter() {
                let ty = self.resolve_payload(payload, &args, offset)?;
                fields.push((fname.clone(), ty));
            }
            Ok(fields)
        })();
        self.generics.instantiation_trace.pop();
        let fields = fields?;
        let display = format!("{}[{}]", def.name, args.iter().map(|t| t.name()).collect::<Vec<_>>().join(", "));
        let sid = self.structs.len();
        self.structs.push(StructDef { name: display, fields, file: def.file, public: def.public, repr_c: false, drop_fn: None });
        self.generics.struct_done.insert(key, sid);
        // Remember which declaration and arguments produced this layout, so
        // method calls on the instantiation can resolve through its generic
        // impl block (ADR-0187).
        self.generics.struct_origin.insert(sid, (gid, args.clone()));
        if self.generics.generic_impls.get(&def.name).is_some_and(|imp| imp.methods.iter().any(|m| m.name == "drop")) {
            let drop_inst = self.instantiate_impl_method(&def.name, "drop", sid, gid, &args, offset)?;
            self.structs[sid].drop_fn = Some(drop_inst.0);
        }
        Ok(sid)
    }
    pub(super) fn parse_struct_def(
        &mut self,
        public: bool,
        repr_c: bool,
        parsed_fields: Option<&[ae_syntax::declaration::TypedName]>,
    ) -> Result<(), ae_span::Diagnostic> {
        self.pos += 1;
        let name_offset = self.tokens[self.pos].offset;
        let name = self.identifier()?;
        if RESERVED.contains(&name.as_str()) { return Err(self.fail(&format!("'{}' is a built-in name and cannot be a struct", name))); }
        if self.struct_names.contains_key(&name) || self.generics.struct_names.contains_key(&name) { return Err(self.fail(&format!("duplicate struct '{}'", name))); }
        if self.enum_names.contains_key(&name) || self.generics.names.contains_key(&name) { return Err(self.fail(&format!("'{}' is already an enum", name))); }
        let params = self.type_params()?;
        self.take(Kind::LBrace)?;
        let mut fields: Vec<(String, Payload)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        while *self.current() != Kind::RBrace {
            let parsed_field = parsed_fields.and_then(|items| items.get(fields.len()));
            let fname = match parsed_field {
                Some(field) if field.name_range.start == self.tokens[self.pos].offset => {
                    self.pos += 1;
                    field.name.clone()
                }
                _ => self.identifier()?,
            };
            if !seen.insert(fname.clone()) { return Err(self.fail(&format!("duplicate field '{}' in struct '{}'", fname, name))); }
            self.take(Kind::Colon)?;
            let fty = self.field_type(&params)?;
            if repr_c {
                let owned = match &fty {
                    Payload::Concrete(Type::Struct(sid)) if self.structs[*sid].repr_c => false,
                    Payload::Concrete(ty) => ty.owned(),
                    Payload::Nested(_) => false,
                    Payload::Param(_) => false,
                };
                if owned {
                    return Err(self.fail(&format!("field '{}' of '{}' owns storage, so the struct cannot claim a C layout; '#[repr(C)]' structs hold scalars and other '#[repr(C)]' structs", fname, name)));
                }
            }
            fields.push((fname, fty));
            if *self.current() == Kind::Comma { self.pos += 1; }
        }
        self.take(Kind::RBrace)?;
        if repr_c && fields.is_empty() {
            return Err(self.error_at(name_offset, &format!("'{}' must have at least one field to claim a C layout", name)));
        }
        if params.is_empty() {
            // A non-generic struct may still carry nested instantiations
            // (`entries: Pair[i64, Pair[string, f64]]`); their arguments are
            // all concrete, so they resolve immediately with no parameters.
            // Resolution happens BEFORE the struct's own id is reserved: a
            // nested instantiation registers its own layout, and taking the
            // id first would hand Ledger's slot to the inner type.
            let mut concrete = Vec::new();
            for (fname, payload) in fields {
                let ty = match payload {
                    Payload::Concrete(ty) => ty,
                    Payload::Nested(_) => self.resolve_payload(&payload, &[], name_offset)?,
                    Payload::Param(_) => unreachable!("a non-generic struct has no type parameters"),
                };
                if repr_c && ty.owned() && !matches!(ty, Type::Struct(sid) if self.structs[sid].repr_c) {
                    return Err(self.error_at(name_offset, &format!("field '{}' of '{}' owns storage, so the struct cannot claim a C layout", fname, name)));
                }
                concrete.push((fname, ty));
            }
            let id = self.structs.len();
            self.struct_names.insert(name.clone(), id);
            self.structs.push(StructDef { name, fields: concrete, file: self.file.clone(), public, repr_c, drop_fn: None });
        } else {
            if repr_c {
                return Err(self.fail(&format!("'{}' is generic, so it has no single C layout; '#[repr(C)]' belongs on a concrete struct", name)));
            }
            let gid = self.generics.struct_defs.len();
            self.generics.struct_names.insert(name.clone(), gid);
            self.generics.struct_defs.push(GenericStruct { name, params, fields, file: self.file.clone(), public });
        }
        Ok(())
    }
    // `*const T` / `*mut T` in a type annotation. The pointee is any named
    // type the cast grammar accepts; the pointer itself is an untracked
    // address (ADR-0207), so the declaration check stays the C-ABI shape
    // test — the pointee's ownership is the deref-side's problem (U11).
    pub(super) fn pointer_type(&mut self) -> Result<Type, ae_span::Diagnostic> {
        let (mutable, pointee) = self.pointer_pointee(None)?;
        let id = super::intern_pointee(pointee);
        Ok(if mutable { Type::PtrMut(id) } else { Type::Ptr(id) })
    }

    pub(super) fn pointer_cast_pointee(&mut self, required: Option<bool>) -> Result<(bool, Type), ae_span::Diagnostic> {
        if let Some(mutable) = self.shared_pointer_cast_qualifier() {
            if required.is_none() || required == Some(mutable) {
                self.pos += 2;
                return Ok((mutable, self.named_type()?));
            }
        }
        self.pointer_pointee(required)
    }

    fn pointer_pointee(&mut self, required: Option<bool>) -> Result<(bool, Type), ae_span::Diagnostic> {
        self.take(Kind::Star)?;
        let mutable = if let Some(required) = required {
            self.take(if required { Kind::Mut } else { Kind::Const })?;
            required
        } else {
            let mutable = ae_syntax::declaration::pointer_mutability(
                &self.file, self.source, &self.tokens[self.pos],
            )?;
            self.pos += 1;
            mutable
        };
        Ok((mutable, self.named_type()?))
    }

    // Arrays are borrowed read-only; strings and scalars pass by value.
    pub(super) fn callable_parameter_type(&mut self) -> Result<Type, ae_span::Diagnostic> {
        if *self.current() != Kind::Amp {
            return self.named_type();
        }
        let ty = self.parameter_type()?;
        if matches!(ty, Type::StrView | Type::VecRef | Type::StrVecRef | Type::VecFRef | Type::StructRef(_) | Type::StructMut(_) | Type::Array(_)) {
            Ok(ty)
        } else {
            Err(self.fail("unsupported borrowed parameter type for a callable"))
        }
    }

    pub(super) fn parameter_type(&mut self) -> Result<Type, ae_span::Diagnostic> {
        // `*const T` / `*mut T` in a type annotation (M2.12 slice 4's
        // c_abi_shape already advertises the pointer forms; until now pointer
        // types were only born in the cast grammar, so no annotation could
        // spell one — extern "C" signatures return raw pointers, which
        // ADR-0208's allocator bridge needs).
        if *self.current() == Kind::Star {
            return self.pointer_type();
        }
        // `fn(T, U) -> R` (ADR-0189): a function value. Only non-capturing
        // closures can be passed as one; the signature is interned so two
        // closures of the same shape share the type.
        if *self.current() == Kind::Fn {
            self.pos += 1;
            self.take(Kind::LParen)?;
            let mut params = Vec::new();
            if *self.current() != Kind::RParen {
                loop {
                    if params.len() >= 8 { return Err(self.fail("function type parameter limit exceeded (8)")); }
                    params.push(self.callable_parameter_type()?);
                    if *self.current() != Kind::Comma { break; }
                    self.pos += 1;
                }
            }
            self.take(Kind::RParen)?;
            self.take(Kind::Arrow)?;
            let ret = self.return_annotation()?;
            if matches!(ret, Type::StructRef(_) | Type::StructMut(_) | Type::Fn(_)) {
                return Err(self.fail("a function type cannot return a borrow or another function type"));
            }
            let sig = super::closures::intern_fn_sig(params, ret);
            return Ok(Type::Fn(sig));
        }
        if *self.current() == Kind::Amp {
            let amp_offset = self.tokens[self.pos].offset;
            self.pos += 1;
            // `&T` is resolved when a generic function is instantiated: the
            // type parameter already stands for one concrete type, so this is
            // just a spelling for one of the three borrows below. No lifetime
            // or new borrow kind is introduced (ADR-35).
            let parameter: Option<(String, Type)> = match self.current() {
                Kind::Ident(name) => self.subst.get(name).map(|&ty| (name.clone(), ty)),
                _ => None,
            };
            if let Some((name, concrete)) = parameter {
                self.pos += 1;
                return match concrete {
                    Type::String => Ok(Type::StrView),
                    Type::Vec => Ok(Type::VecRef),
                    Type::StrVec => Ok(Type::StrVecRef),
                    Type::VecF => Ok(Type::VecFRef),
                    _ => Err(self.error_at(amp_offset, &format!(
                        "'&{name}' is '{}' here, which has no read-only borrow; only string, vec, vec[string] and vec[f64] can be borrowed",
                        concrete.name()))),
                };
            }
            if matches!(self.current(), Kind::Ident(name) if name == "string") {
                // &string borrows a string read-only for the duration of the
                // call: no move, no copy, and it binds as an ordinary view.
                self.pos += 1;
                return Ok(Type::StrView);
            }
            if matches!(self.current(), Kind::Ident(name) if name == "vec") {
                self.pos += 1;
                // &vec[string] and &vec[f64] (ADR-61) borrow those vectors
                // read-only.
                if *self.current() == Kind::LBracket {
                    self.pos += 1;
                    let element = self.identifier()?;
                    self.take(Kind::RBracket)?;
                    match element.as_str() {
                        "string" => return Ok(Type::StrVecRef),
                        "f64" => return Ok(Type::VecFRef),
                        _ => return Err(self.fail("only &vec[string] and &vec[f64] are supported; vector elements are i64 by default")),
                    }
                }
                return Ok(Type::VecRef);
            }
            // `&mut Type` (ADR-0185) is the exclusive mutable borrow of a
            // user struct: the callee may write fields through the pointer,
            // and the caller may hold no other borrow of the struct while
            // the call runs.
            if *self.current() == Kind::Mut {
                self.pos += 1;
                let borrowed = match self.current() {
                    Kind::Ident(text) => text.clone(),
                    _ => return Err(self.fail("'&mut' must be followed by a struct type")),
                };
                if let Some(&sid) = self.struct_names.get(borrowed.as_str()) {
                    let def = &self.structs[sid];
                    self.require_public("struct", &def.name.clone(), &def.file.clone(), def.public)?;
                    self.pos += 1;
                    return Ok(Type::StructMut(sid));
                }
                if let Some(&gid) = self.generics.struct_names.get(borrowed.as_str()) {
                    let offset = self.tokens[self.pos].offset;
                    self.pos += 1;
                    if *self.current() != Kind::LBracket {
                        return Err(self.fail(&format!("'&mut {borrowed}' is a generic struct; write its type arguments, e.g. &mut {borrowed}[string]")));
                    }
                    self.pos += 1;
                    let args = self.read_type_arguments(Self::named_type)?;
                    self.take(Kind::RBracket)?;
                    let sid = self.instantiate_struct(gid, args, offset)?;
                    let def = &self.structs[sid];
                    self.require_public("struct", &def.name.clone(), &def.file.clone(), def.public)?;
                    return Ok(Type::StructMut(sid));
                }
                return Err(self.error_at(amp_offset, &format!(
                    "'&mut {borrowed}' borrows a struct mutably, but '{borrowed}' is {}; only user structs can be borrowed mutably",
                    if self.struct_names.contains_key(borrowed.as_str()) || self.generics.struct_names.contains_key(borrowed.as_str()) { "a struct" } else { "not a struct" })));
            }
            if matches!(self.current(), Kind::Ident(name) if name == "mut") {
                return Err(self.error_at(amp_offset,
                    "'&mut' does not exist yet: struct borrows are read-only, so a method that needs to mutate takes 'self' by value"));
            }
            if matches!(self.current(), Kind::Ident(_)) {
                // `&Struct` (ADR-0184) borrows a user struct read-only: the
                // callee reads fields through a pointer and the borrow's
                // lifetime is the call, the same contract the container
                // borrows already give. A generic instantiation borrows its
                // own instantiation, e.g. `&Pair[string, i64]`.
                let borrowed = match self.current() {
                    Kind::Ident(text) => text.clone(),
                    _ => unreachable!("the enclosing guard checked for an identifier"),
                };
                if let Some(&sid) = self.struct_names.get(borrowed.as_str()) {
                    let def = &self.structs[sid];
                    self.require_public("struct", &def.name.clone(), &def.file.clone(), def.public)?;
                    self.pos += 1;
                    return Ok(Type::StructRef(sid));
                }
                if let Some(&gid) = self.generics.struct_names.get(borrowed.as_str()) {
                    let offset = self.tokens[self.pos].offset;
                    self.pos += 1;
                    if *self.current() != Kind::LBracket {
                        return Err(self.fail(&format!("'&{borrowed}' is a generic struct; write its type arguments, e.g. &{borrowed}[string]")));
                    }
                    self.pos += 1;
                    let args = self.read_type_arguments(Self::named_type)?;
                    self.take(Kind::RBracket)?;
                    let sid = self.instantiate_struct(gid, args, offset)?;
                    let def = &self.structs[sid];
                    self.require_public("struct", &def.name.clone(), &def.file.clone(), def.public)?;
                    return Ok(Type::StructRef(sid));
                }
                return Err(self.error_at(amp_offset,
                    "unsupported borrow type; write &string, &vec, &vec[string], &vec[f64], &Struct, &[i64; N], or &T inside a generic function"));
            }
            self.take(Kind::LBracket)?;
            if self.identifier()? != "i64" { return Err(self.fail("array element type must be i64")); }
            self.take(Kind::Semi)?;
            let len = match self.current() {
                Kind::Number(n) if *n >= 1 && *n <= 4096 => *n as usize,
                _ => return Err(self.fail("array parameter length must be between 1 and 4096")),
            };
            self.pos += 1; self.take(Kind::RBracket)?;
            Ok(Type::Array(len))
        } else {
            self.named_type()
        }
    }
    pub(super) fn return_annotation(&mut self) -> Result<Type, ae_span::Diagnostic> {
        // No borrow can be returned: a borrow of a parameter would outlive the
        // callee's frame, and a borrow of a local would outlive the local.
        // (ADR-0184) Returning a value is always sound, and callers re-borrow.
        if *self.current() == Kind::Amp {
            return Err(self.fail("a function cannot return a borrow; return an owned value and let callers borrow it themselves"));
        }
        // A raw pointer return is an untracked address (ADR-0207) — the C-ABI
        // shape test, not the borrow rules, decides whether it may cross.
        if *self.current() == Kind::Star {
            return self.pointer_type();
        }
        self.named_type()
    }
    // First pass collects only signatures. Bodies are checked in the second pass.
    // The syntax parser validated the headers; skip them before collecting.
}
