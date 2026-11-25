//! Lowering module: scan (P1.4 parser split).
use super::*;
use super::route_body_stmt::*;
use super::route_marks::*;

impl<'a> Parser<'a> {
    /// Builds the module's resolved-expression index. Separate from the cell
    /// so a caller that also needs `&mut self` can pin the Rc locally and
    /// keep the reference off `self`.
    pub(super) fn build_resolved_expressions(&self) -> ResolvedExpressions {
        let mut expressions = ResolvedExpressions::default();
            let Ok((module, body_errors)) = ae_resolve::module::parse_module_tokens_recovering(&self.file, self.source, &self.tokens) else {
                return expressions;
            };
            expressions.shared_body_errors.extend(body_errors);
            let resolution = ae_resolve::resolve::resolve_with_tokens(&module, &[], &self.tokens);
            expressions.enum_bindings.extend(resolution.defs.iter().filter_map(|entry| {
                use ae_resolve::TypeSpelling;
                if !entry.kind.is_local() { return None }
                let is_enum = match &entry.inferred {
                    // A prelude generic enum (`Option[T]`, `Result[T, E]`):
                    // registered by name, with the definition outside this
                    // module's resolution table.
                    Some(TypeSpelling::Prelude { name, .. }) => self.generics.names.contains_key(*name),
                    Some(TypeSpelling::Named { def }) | Some(TypeSpelling::GenericInstance { def, .. }) => {
                        match resolution.defs.get(def.0 as usize) {
                            Some(item) => item.kind == ae_resolve::DefKind::Enum,
                            // A generic instance whose item is outside this
                            // table: the body lowering still decides the
                            // subject is an enum, and a struct instance falls
                            // back to the typed route there.
                            None => matches!(entry.inferred, Some(TypeSpelling::GenericInstance { .. })),
                        }
                    }
                    _ => false,
                };
                is_enum.then_some(entry.id)
            }));
            for occurrence in resolution.functions.iter().flat_map(|function| &function.occurrences) {
                let Some(definition) = occurrence.target.and_then(|id| resolution.defs.get(id.0 as usize)) else {
                    if occurrence.role == ae_resolve::Role::Call {
                        expressions.unresolved_calls.insert(occurrence.range.start);
                        // No declaration answers this spelling at all: the
                        // lowering reports E0207 from the callee. A call whose
                        // name *is* declared (an import, a trait method, a
                        // builtin) keeps the shaped gates — only identity is
                        // answered here.
                        if !self.call_has_signature(&occurrence.name) {
                            expressions.unknown_calls.insert(occurrence.range.start);
                        }
                    }
                    continue;
                };
                match occurrence.role {
                    ae_resolve::Role::Read | ae_resolve::Role::Borrow if definition.kind.is_local() => {
                        expressions.local_bindings.insert(occurrence.range.start, definition.range.clone());
                        expressions.scalar_reads.insert(occurrence.range.start, definition.id);
                        use ae_resolve::TypeSpelling;
                        let spelling = match &definition.inferred {
                            Some(TypeSpelling::Borrow { mutable: false, inner }) => Some(inner.as_ref()),
                            spelling => spelling.as_ref(),
                        };
                        if let Some(TypeSpelling::Composite { family: "vec", arguments }) = spelling
                            && matches!(arguments.as_slice(), [] | [TypeSpelling::Primitive("i64" | "f64" | "string")]) {
                                expressions.vector_definitions.insert(definition.id);
                                if matches!(definition.inferred, Some(TypeSpelling::Composite { .. })) {
                                    expressions.owned_vectors.insert(definition.id);
                                }
                            }
                        // A parameter bound to a struct borrow (`r: &Rect`): a
                        // value-position read is the typed route's "cannot
                        // store a borrowed struct in a variable" error, so the
                        // scan admits it and the binding arm answers.
                        if matches!(definition.inferred, Some(TypeSpelling::Borrow { .. }))
                            && let Some(TypeSpelling::Named { def } | TypeSpelling::GenericInstance { def, .. }) = spelling
                            && resolution.defs.get(def.0 as usize).is_some_and(|item| item.kind == ae_resolve::DefKind::Struct)
                        {
                            expressions.struct_borrow_reads.insert(definition.id);
                        }
                    }
                    ae_resolve::Role::Write | ae_resolve::Role::DerefWrite if definition.kind.is_local() => {
                        expressions.scalar_writes.insert(occurrence.range.start, definition.id);
                    }
                    ae_resolve::Role::Call if definition.kind == ae_resolve::DefKind::Function
                        && definition.owner.is_none() && definition.name == occurrence.name =>
                    {
                        expressions.direct_calls.insert(occurrence.range.start);
                    }
                    ae_resolve::Role::Call if definition.kind.is_local() => {
                        // The callee offset names the binding being called:
                        // `Expr::FnCall` needs it (a function value).
                        expressions.call_definitions.insert(occurrence.range.start, definition.id);
                        if matches!(definition.inferred.as_ref(),
                            Some(ae_resolve::TypeSpelling::Pointer { pointee, .. })
                                if matches!(pointee.as_ref(), ae_resolve::TypeSpelling::Primitive("i64" | "f64" | "bool")))
                        {
                            expressions.scalar_pointer_receivers.insert(occurrence.range.start);
                            expressions.scalar_reads.insert(occurrence.range.start, definition.id);
                        }
                    }
                    ae_resolve::Role::Construct if definition.kind == ae_resolve::DefKind::Struct
                        && definition.name == occurrence.name =>
                    {
                        expressions.struct_constructors.insert(occurrence.range.start);
                    }
                    _ if definition.kind == ae_resolve::DefKind::Variant => {
                        expressions.enum_variants.insert(occurrence.range.start);
                    }
                    _ => {}
                }
            }
            expressions.specialization_arguments.extend(resolution.functions.iter().flat_map(|function| {
                function.specialization_arguments.iter().map(|(&offset, arguments)| (offset, arguments.clone()))
            }));
            expressions.pointer_cast_targets.extend(resolution.functions.iter().flat_map(|function| {
                function.pointer_cast_targets.iter().map(|(&offset, &mutable)| (offset, mutable))
            }));
            expressions.closure_headers.extend(resolution.functions.iter().flat_map(|function| {
                function.closure_headers.iter().map(|(&offset, header)| (offset, header.clone()))
            }));
            expressions.scalar_expression_types.extend(resolution.functions.iter().flat_map(|function| {
                function.expression_types.iter().filter(|&(_, &fact)| fact != ae_resolve::resolve::ExpressionType::Deferred).map(|(range, &fact)| (range.clone(), fact))
            }));
            expressions.scalar_definitions.extend(resolution.defs.iter()
                .filter(|definition| definition.kind.is_local())
                .map(|definition| (definition.range.start, definition.id)));
            // A local whose spelling is a plain `string` owns one by value:
            // the parameter shape a construction may move into a field.
            expressions.owned_string_bindings.extend(resolution.defs.iter()
                .filter(|entry| entry.kind.is_local()
                    && matches!(entry.inferred, Some(ae_resolve::TypeSpelling::Primitive("string"))))
                .map(|entry| entry.id));
            // A function-typed local: a call through it lowers as `FnCall`.
            expressions.fn_value_bindings.extend(resolution.defs.iter()
                .filter(|entry| entry.kind.is_local()
                    && matches!(entry.inferred, Some(ae_resolve::TypeSpelling::FnValue { .. })))
                .map(|entry| entry.id));
            // A map binding or `&map` parameter: the map builtins and
            // `m[key] = v` stores borrow it for the statement's duration.
            expressions.map_definitions.extend(resolution.defs.iter().filter_map(|entry| {
                use ae_resolve::TypeSpelling;
                if !entry.kind.is_local() { return None }
                let spelling = match &entry.inferred {
                    Some(TypeSpelling::Borrow { mutable: false, inner }) => Some(inner.as_ref()),
                    other => other.as_ref(),
                };
                matches!(spelling, Some(TypeSpelling::Composite { family: "map", .. })).then_some(entry.id)
            }));
            // Struct parameters (borrowed or by value) and local bindings (owned,
            // constructed in-route from a drop-free struct) are field-read
            // receivers for the body route: the resolver's inferred spelling
            // names the struct item, and the compiler's registry supplies
            // field types for the read checks. Marked before the statement
            // walk, whose field-read checks consult this set.
            for definition in resolution.defs.iter().filter(|entry| {
                entry.kind == ae_resolve::DefKind::Parameter || entry.kind == ae_resolve::DefKind::Binding
            }) {
                use ae_resolve::TypeSpelling;
                // The spelling names the struct: a `Named` def of this module
                // resolves through its def table, while an imported type is
                // `Unknown("Name")` and names the registry entry directly.
                let spelling = match &definition.inferred {
                    Some(TypeSpelling::Borrow { inner, .. }) => Some(inner.as_ref()),
                    other => other.as_ref(),
                };
                let name = match spelling {
                    Some(TypeSpelling::Named { def }) => resolution.defs.get(def.0 as usize).map(|item| item.name.clone()),
                    Some(TypeSpelling::Unknown(name)) => Some(name.clone()),
                    _ => None,
                };
                let Some(name) = name else { continue };
                if !self.struct_names.contains_key(&name) { continue }
                expressions.struct_definitions.insert(definition.id);
                expressions.struct_definition_names.insert(definition.id, name);
                // A value binding (spelled `Named`) owns its struct; a
                // borrowed parameter (`&P`, `&mut P`) only views one.
                if matches!(definition.inferred, Some(TypeSpelling::Named { .. })) {
                    expressions.owned_struct_bindings.insert(definition.id);
                }
            }
            // A range-bracket parameter type (`arr: &[i64; 8]`) indexes like a
            // local array; the resolver does not spell bracket types, so read
            // the declaration's source range.
            for function in &module.functions {
                let ae_syntax::declaration::DeclarationKind::Function { parameters, .. } = &function.declaration.kind else { continue };
                for parameter in parameters {
                    let spelling = self.source.get(parameter.ty.clone()).unwrap_or("").trim().trim_start_matches('&').trim();
                    if !spelling.starts_with('[') { continue }
                    if let Some(entry) = resolution.defs.iter().find(|entry| {
                        entry.kind == ae_resolve::DefKind::Parameter && entry.range.start == parameter.name_range.start
                    }) {
                        expressions.array_definitions.insert(entry.id);
                    }
                }
            }
            let closure_statements = resolution.functions.iter()
                .flat_map(|function| function.closure_bodies.iter())
                .flat_map(|body| body.statements.iter());
            let owned_statements = module.functions.iter()
                .filter_map(|function| function.body.as_ref().map(|body| (body, function.impl_owner)));
            for (statement, impl_owner) in owned_statements
                .flat_map(|(body, owner)| body.iter().map(move |statement| (statement, owner)))
                .chain(closure_statements.map(|statement| (statement, None))) {
                ae_syntax::statement::visit_statements(statement, &mut |statement| {
                    use ae_syntax::statement::StatementKind;
                    if let StatementKind::Binding { name_range, initializer, .. } = &statement.kind
                        && matches!(initializer.kind, ae_syntax::expression::ExpressionKind::Array(_))
                            && let Some(def) = expressions.scalar_definitions.get(&name_range.start) {
                                expressions.array_definitions.insert(*def);
                            }
                    if let ae_syntax::statement::StatementKind::PointerStore { pointer, pointer_range, .. } = &statement.kind {
                        expressions.pointer_store_names.insert(pointer_range.start, pointer.clone());
                    }
                    if let ae_syntax::statement::StatementKind::Match { arms, .. } = &statement.kind {
                        for arm in arms {
                            expressions.match_patterns.insert(arm.pattern.range.start, arm.pattern.clone());
                            // A payload binding of a vector payload is an
                            // owned vector: `for name in names` and the vector
                            // statement builtins answer by that mark. The
                            // variant's payload types come from the enum
                            // layout, so a result enum (`DirResult::Ok`) marks
                            // the same way a spelled binding does.
                            let ae_syntax::pattern::PatternKind::Variant { enum_name, variant, payload: Some(bindings), .. } = &arm.pattern.kind else { continue };
                            let payload_types = self.enum_names.get(enum_name.as_str())
                                .and_then(|&id| self.enums[id].variants.iter()
                                    .find(|(name, _)| name == variant).map(|(_, types)| types.clone()));
                            let Some(payload_types) = payload_types else { continue };
                            for (binding, ty) in bindings.iter().zip(payload_types.iter()) {
                                if !matches!(binding.kind, ae_syntax::pattern::PatternKind::Binding(_)) { continue }
                                if !matches!(ty, Type::Vector(_)) { continue }
                                if let Some(&id) = expressions.scalar_definitions.get(&binding.range.start) {
                                    expressions.vector_definitions.insert(id);
                                    expressions.owned_vectors.insert(id);
                                }
                            }
                        }
                    }
                    if let StatementKind::Binding { name_range, initializer, .. } = &statement.kind {
                        use ae_syntax::expression::ExpressionKind;
                        if let Some(&id) = expressions.scalar_definitions.get(&name_range.start) {
                            // A generic struct instance's binding keeps its
                            // template and source type arguments for the field
                            // walk (no layout is instantiated here, so the
                            // index is order-independent).
                            let specialization = match &initializer.kind {
                                ExpressionKind::Struct { target, .. } => Some(target.as_ref()),
                                ExpressionKind::Call { callee, .. } => match &callee.kind {
                                    ExpressionKind::Qualified { base, .. } => Some(base.as_ref()),
                                    _ => None,
                                },
                                _ => None,
                            };
                            if let Some(ExpressionKind::Specialization { base: inner, arguments }) = specialization.map(|target| &target.kind)
                                && let ExpressionKind::Atom(Kind::Ident(struct_name)) = &inner.kind
                                    && let Some(&gid) = self.generics.struct_names.get(struct_name)
                                        && let Some(args) = self.loose_shape_arguments(arguments) {
                                            expressions.generic_struct_bindings.insert(id, (gid, args));
                                        }
                            if let ExpressionKind::Atom(Kind::Ident(_)) = &initializer.kind
                                && let Some(other) = expressions.scalar_reads.get(&initializer.range.start).copied()
                                && let Some(struct_name) = expressions.struct_definition_names.get(&other).cloned()
                            {
                                expressions.struct_definition_names.entry(id).or_insert(struct_name);
                                if expressions.owned_struct_bindings.contains(&other) {
                                    expressions.owned_struct_bindings.insert(id);
                                }
                            }
                            // A plain struct literal types its binding from the
                            // target name: the resolver may leave the spelling
                            // open, and field reads/writes answer by this entry.
                            if let ExpressionKind::Struct { target, .. } = &initializer.kind
                                && let ExpressionKind::Atom(Kind::Ident(struct_name)) = &target.kind
                            {
                                if let Some(&sid) = self.struct_names.get(struct_name) {
                                    expressions.struct_definition_names.entry(id).or_insert_with(|| struct_name.clone());
                                    let constructed = Type::Struct(sid);
                                    if owned_plain_struct(constructed, self.structs)
                                        || owned_field_struct(constructed, self.structs)
                                    {
                                        expressions.owned_struct_bindings.insert(id);
                                    }
                                } else if self.generics.struct_names.contains_key(struct_name) {
                                    expressions.owned_struct_bindings.insert(id);
                                }
                            }
                            // A binding from an explicit generic call (`let x = f[T](..)`)
                            // or a method call (`let x = r.shifted(..)`): the
                            // instantiated return type exists only at lowering
                            // time, so its fields and methods are admitted
                            // permissively there.
                            if let ExpressionKind::Call { callee, .. } = &initializer.kind
                                && matches!(callee.kind, ExpressionKind::BracketedCallTarget { .. }
                                    | ExpressionKind::Field { .. }
                                    // An associated call (`MyVec[Elem]::new()`):
                                    // the instance's type exists only at
                                    // lowering, so the receiver's methods
                                    // resolve lazily.
                                    | ExpressionKind::Qualified { .. })
                                {
                                    expressions.generic_call_bindings.insert(id);
                                }
                            match &initializer.kind {
                                ExpressionKind::Vector(_) => {
                                    expressions.vector_definitions.insert(id);
                                    expressions.owned_vectors.insert(id);
                                }
                                ExpressionKind::Atom(Kind::Text(_)) => {
                                    expressions.owned_text_bindings.insert(id);
                                }
                                ExpressionKind::Closure { .. } => {
                                    expressions.closure_bindings.insert(id);
                                }
                                ExpressionKind::Atom(Kind::Ident(name))
                                    if self.functions.contains_key(name)
                                        || expressions.scalar_reads.get(&initializer.range.start)
                                            .is_some_and(|other| expressions.fn_value_bindings.contains(other)) =>
                                {
                                    expressions.fn_value_bindings.insert(id);
                                }
                                ExpressionKind::Field { base, name: field_name }
                                    if field_owner_struct(base, &expressions, self.struct_names, self.structs)
                                        .and_then(|sid| field_ty_in(sid, field_name, self.structs))
                                        .is_some_and(|ty| matches!(ty, Type::Fn(_))) =>
                                {
                                    expressions.fn_value_bindings.insert(id);
                                }
                                ExpressionKind::Call { callee, .. }
                                    if matches!(&callee.kind, ExpressionKind::Atom(Kind::Ident(fn_name))
                                        if self.functions.get(fn_name).is_some_and(|sig| matches!(sig.ret, Type::Fn(_))
                                            || matches!(sig.ret, Type::Struct(sid) if self.generics.closure_envs.contains_key(&sid)))) =>
                                {
                                    expressions.fn_value_bindings.insert(id);
                                    if let ExpressionKind::Atom(Kind::Ident(fn_name)) = &callee.kind
                                        && let Some(Type::Struct(sid)) = self.functions.get(fn_name).map(|s| s.ret)
                                    {
                                        expressions.owned_struct_bindings.insert(id);
                                        expressions.struct_definition_names.entry(id).or_insert_with(|| self.structs[sid].name.clone());
                                    }
                                }
                                _ => match iterator_constructor(initializer) {
                                    Some(Type::Tokens) => { expressions.token_cursor_definitions.insert(id); }
                                    Some(Type::Vector(_)) => {
                                        expressions.vector_definitions.insert(id);
                                        expressions.owned_vectors.insert(id);
                                    }
                                    _ => {
                                        // A builtin returning a prelude enum
                                        // (parse_int, json_get, find, ...)
                                        // types its binding even when the
                                        // resolver leaves the spelling open;
                                        // `match` needs to know the subject
                                        // is an enum. `keys` returns an owned
                                        // string vector.
                                        if let ExpressionKind::Call { callee, .. } = &initializer.kind
                                            && let ExpressionKind::Atom(Kind::Ident(builtin)) = &callee.kind {
                                                if ENUM_RESULT_BUILTINS.iter().any(|(name, _)| *name == builtin.as_str()) {
                                                    expressions.enum_bindings.insert(id);
                                                }
                                                if matches!(builtin.as_str(), "keys" | "args" | "bytes" | "strvec_new" | "f64vec_new") {
                                                    expressions.vector_definitions.insert(id);
                                                    expressions.owned_vectors.insert(id);
                                                }
                                                if matches!(builtin.as_str(), "map_new" | "f64map_new") {
                                                    // The resolver leaves a
                                                    // builtin-initialized
                                                    // binding's spelling open;
                                                    // the map shape comes from
                                                    // the constructor name.
                                                    expressions.map_definitions.insert(id);
                                                }
                                                // A user function returning an
                                                // enum result: `match` and `?`
                                                // need the binding typed as
                                                // an enum even when the
                                                // resolver leaves the spelling
                                                // open.
                                                if let Some(signature) = self.functions.get(builtin.as_str()) {
                                                    if matches!(signature.ret, Type::Enum(..)) {
                                                        expressions.enum_bindings.insert(id);
                                                    }
                                                    // A map-returning call owns its
                                                    // map: `map_get(counts, ..)` and
                                                    // `return counts;` answer by this
                                                    // mark when the resolver leaves
                                                    // the spelling open.
                                                    if matches!(signature.ret, Type::MapOf(_)) {
                                                        expressions.map_definitions.insert(id);
                                                    }
                                                    // A call returning a struct (often an
                                                    // imported one, e.g. a std constructor)
                                                    // types the binding from the signature:
                                                    // field reads, by-value moves and returns
                                                    // answer from this entry.
                                                    if let Type::Struct(sid) = signature.ret
                                                        && let Some(defn) = self.structs.get(sid) {
                                                            expressions.struct_definition_names.entry(id).or_insert_with(|| defn.name.clone());
                                                            if owned_plain_struct(signature.ret, self.structs)
                                                                || owned_field_struct(signature.ret, self.structs)
                                                            {
                                                                expressions.owned_struct_bindings.insert(id);
                                                            }
                                                        }
                                                }
                                            }
                                    }
                                },
                            }
                        }
                    }
                    for top_level in statement.expressions() {
                        use ae_syntax::expression::ExpressionKind;
                        let target = match &top_level.kind {
                            ExpressionKind::Call { callee, .. } => callee.as_ref(),
                            _ => top_level,
                        };
                        if matches!(&target.kind, ExpressionKind::Qualified { .. }) {
                            let end = self.tokens.partition_point(|token| token.offset < target.range.end);
                            // A variant name recorded by the resolver, or a base that names a known
                            // enum: a generic instance (`Option[i64]::None`)
                            // resolves to the same constructor and must join
                            // the route too. The walk peels any bracket or
                            // specialization wrapper off the base.
                            let mut names_enum = false;
                            let mut names_struct_path = false;
                            if let ExpressionKind::Qualified { base, .. } = &target.kind {
                                let mut node = base.as_ref();
                                loop {
                                    match &node.kind {
                                        ExpressionKind::Atom(Kind::Ident(name)) => {
                                            if name == "Self" {
                                                // The owning declaration answers `Self`
                                                // statelessly: it lives on the raw module,
                                                // so a parser that shares this body's tokens
                                                // (an earlier pass, or an instantiated impl
                                                // method's sub-parser) cannot change the
                                                // answer.
                                                if let Some(owner) = impl_owner
                                                    && let Some(declaration) = module.declarations.get(owner)
                                                        && matches!(declaration.kind, ae_syntax::declaration::DeclarationKind::Impl { target: None }) {
                                                            names_enum = self.enum_names.contains_key(&declaration.name)
                                                                || self.generics.names.contains_key(&declaration.name);
                                                            names_struct_path = self.struct_names.contains_key(&declaration.name)
                                                                || self.generics.struct_names.contains_key(&declaration.name);
                                                        }
                                                break;
                                            }
                                            names_enum = self.enum_names.contains_key(name) || self.generics.names.contains_key(name);
                                            // `Point::new(..)` / `Pair[string, i64]::new(..)`:
                                            // the typed expression parser owns the
                                            // resolution; the mark only lets a
                                            // Return/Binding hand the expression to it.
                                            names_struct_path = self.struct_names.contains_key(name)
                                                || self.generics.struct_names.contains_key(name);
                                            break;
                                        }
                                        ExpressionKind::Specialization { base, .. }
                                        | ExpressionKind::BracketedCallTarget { base, .. } => node = base.as_ref(),
                                        _ => break,
                                    }
                                }
                            }
                            if names_struct_path {
                                expressions.enum_constructors.insert(top_level.range.start);
                            } else if names_enum || (end > 0 && expressions.enum_variants.contains(&self.tokens[end - 1].offset)) {
                                expressions.enum_constructors.insert(top_level.range.start);
                                if let StatementKind::Binding { name_range, .. } = &statement.kind
                                    && let Some(&id) = expressions.scalar_definitions.get(&name_range.start) {
                                        expressions.enum_bindings.insert(id);
                                    }
                            }
                        }
                        expressions.values.insert(top_level.range.start, Rc::new(top_level.clone()));
                        visit_expression_tree(top_level, &mut |expression| {
                            use ae_syntax::expression::ExpressionKind;
                            // A constructor nested in an expression
                            // (`f(Shape::Circle(3))`, `g(Option[i64]::None)`):
                            // the typed expression parser owns the resolution,
                            // so the mark lets the scan and the lowering
                            // delegate it (the same delegation the top-level
                            // statements use for Return/Binding).
                            if let ExpressionKind::Qualified { .. } = &expression.kind {
                                expressions.enum_constructors.insert(expression.range.start);
                            }
                            if let ExpressionKind::Call { callee, .. } = &expression.kind
                                && matches!(callee.kind, ExpressionKind::Qualified { .. }) {
                                    expressions.enum_constructors.insert(expression.range.start);
                                }
                            if let ExpressionKind::AddressCast { pointee, .. } = &expression.kind {
                                // Every spelling the resolver can name is
                                // admitted: the lowering's `lower_address_cast`
                                // answers an owned pointee (a struct, a
                                // string, a vector) with the typed route's own
                                // "pointer to an owned value" diagnostic, and a
                                // non-owned one lowers.
                                let text = self.source.get(pointee.clone());
                                let known = text.is_some_and(|text| {
                                    super::types::scalar_type(text).is_some()
                                        || matches!(text, "string" | "atomic" | "mutex" | "chan" | "exec" | "pool" | "shared" | "task_scope")
                                        || text.starts_with("vec") || text.starts_with("strvec") || text.starts_with("map")
                                        || self.struct_names.contains_key(text) || self.enum_names.contains_key(text)
                                });
                                if known { expressions.scalar_address_casts.insert(expression.range.start); }
                            }
                            if let ExpressionKind::Call { callee, arguments } = &expression.kind
                                && let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind {
                                    let operand = match (name.as_str(), arguments.as_slice()) {
                                        ("spawn", [callable]) => Some(callable),
                                        ("task_spawn" | "scope_spawn" | "pool_spawn", [_, callable]) => Some(callable),
                                        _ => None,
                                    };
                                    if let Some(operand) = operand {
                                        expressions.values.insert(operand.range.start, Rc::new(operand.clone()));
                                    }
                                }
                            if let ExpressionKind::Struct { target, fields } = &expression.kind
                                && matches!(target.kind, ExpressionKind::Specialization { .. }) {
                                    let open = self.tokens.partition_point(|token| token.offset < target.range.end);
                                    expressions.specialized_struct_fields.insert(
                                        self.tokens[open].offset, (expression.range.end, fields.as_slice().into()),
                                    );
                                }
                            if let ExpressionKind::Call { callee, arguments } = &expression.kind
                                && (matches!(callee.kind, ExpressionKind::BracketedCallTarget { .. })
                                    || matches!(&callee.kind, ExpressionKind::Qualified { base, .. }
                                        if matches!(base.kind, ExpressionKind::Specialization { .. })))
                                {
                                    let open = self.tokens.partition_point(|token| token.offset < callee.range.end);
                                    expressions.specialized_call_arguments.insert(self.tokens[open].offset, arguments.as_slice().into());
                                }
                            // A field read on a struct parameter joins the
                            // body route: the owner struct comes from the
                            // parameter's spelling or a nested marked field.
                            // Scalar fields are value-eligible; struct-typed
                            // fields are chain-only receivers (binding one as
                            // a value would move out of the borrowed struct).
                            if let ae_syntax::expression::ExpressionKind::Field { base, name } = &expression.kind {
                                let field_shape = field_owner_struct(base, &expressions, self.struct_names, self.structs)
                                    .and_then(|sid| field_ty_in(sid, name, self.structs))
                                    .map(ShapeArg::Value)
                                    .or_else(|| resolved_field_shape(base, name, &expressions, self.struct_names, self.structs, self.generics));
                                let has_shape = field_shape.is_some();
                                if let Some(field_shape) = field_shape {
                                    match field_shape {
                                        ShapeArg::Value(field_ty) if scalar_type(field_ty) || matches!(field_ty, Type::Fn(_)) => {
                                            expressions.struct_field_reads.insert(expression.range.start);
                                        }
                                        // A struct-typed field (or a nested generic
                                        // instance) is a chain-only receiver.
                                        ShapeArg::Value(Type::Struct(_) | Type::StructRef(_) | Type::StructMut(_))
                                        | ShapeArg::StructInstance { .. } => {
                                            expressions.struct_field_chains.insert(expression.range.start);
                                        }
                                        ShapeArg::Value(Type::String | Type::StrView) => {
                                            expressions.struct_field_views.insert(expression.range.start);
                                        }
                                        ShapeArg::Value(Type::Atomic | Type::Shared(_) | Type::Mutex) => {
                                            // A copy-semantics handle field: only
                                            // a handle consumer may read it.
                                            expressions.struct_field_handles.insert(expression.range.start);
                                        }
                                        _ => {}
                                    }
                                }
                                if !has_shape {
                                    // A chain rooted in a generic-call local: every
                                    // segment's type exists only at lowering, so the
                                    // marks are permissive and the lowering validates
                                    // (scalar read or a further chain).
                                    if let Some(root) = field_chain_root(expression)
                                        && expressions.scalar_reads.get(&root.range.start)
                                            .is_some_and(|id| expressions.generic_call_bindings.contains(id))
                                        {
                                            expressions.struct_field_chains.insert(expression.range.start);
                                            expressions.struct_field_reads.insert(expression.range.start);
                                        }
                                }
                            }
                            // A construction of a file-local, non-generic struct joins the
                            // body route: the drop-free scalar shape as a
                            // value, and the scalar-or-`string` shape as an
                            // owned construction whose string values move in.
                            // Initializers are validated in source order when
                            // the expression is lowered.
                            if let ae_syntax::expression::ExpressionKind::Struct { target, .. } = &expression.kind
                                && let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name)) = &target.kind {
                                    // `Self { .. }` inside an impl resolves through
                                    // the declaration that owns the body (stateless:
                                    // the owner is the declaration index).
                                    let resolved_name = if name == "Self" {
                                        impl_owner
                                            .and_then(|owner| module.declarations.get(owner))
                                            .map(|declaration| declaration.name.clone())
                                    } else {
                                        Some(name.clone())
                                    };
                                    if let Some(&sid) = resolved_name.as_ref().and_then(|struct_name| self.struct_names.get(struct_name)) {
                                        let defn = &self.structs[sid];
                                        {
                                            let constructed = Type::Struct(sid);
                                            if defn.drop_fn.is_none() {
                                                if owned_plain_struct(constructed, self.structs) {
                                                    expressions.drop_free_constructions.insert(expression.range.start);
                                                } else if owned_field_struct(constructed, self.structs) {
                                                    expressions.owned_constructions.insert(expression.range.start);
                                                }
                                            } else if plain_drop_struct(constructed, self.structs) {
                                                expressions.drop_constructions.insert(expression.range.start);
                                            }
                                        }
                                    } else if resolved_name.as_ref().is_some_and(|n| self.generics.struct_names.contains_key(n)) {
                                        expressions.owned_constructions.insert(expression.range.start);
                                    }
                                }
                        });
                    }
                    if let ae_syntax::statement::StatementKind::Assign { target, operator, .. } = &statement.kind {
                        if let ae_syntax::expression::ExpressionKind::Index { index, .. } = &target.kind {
                            // The typed store checks the base and operator, then lowers the index.
                            expressions.values.insert(index.range.start, Rc::new(index.as_ref().clone()));
                        }
                        // `r.n = v` through a `&mut` borrow writes the same
                        // FieldStore an impl method emits for `self.n = v`:
                        // the store needs the base itself in `scalar_writes`
                        // (the lowering indexes that map by the base offset),
                        // and the root's spelling must be a mutable borrow.
                        if (*operator == Kind::Equal || compound_assignment_operator(operator).is_some())
                            && let ae_syntax::expression::ExpressionKind::Field { base, name } = &target.kind {
                                let base_def = expressions.scalar_writes.get(&base.range.start).copied();
                                let mutable_base = matches!(base.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(_)))
                                    && base_def.is_some_and(|def| {
                                        resolution.defs.get(def.0 as usize).is_some_and(|entry| matches!(entry.inferred,
                                            Some(ae_resolve::TypeSpelling::Borrow { mutable: true, .. })))
                                    });
                                // An owned struct local is a writable receiver too
                                // (`plot.w = 40;`): its binding names a struct
                                // (from a spelling, a literal or a call return).
                                let owned_base = matches!(base.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(_)))
                                    && base_def.is_some_and(|def| expressions.struct_definition_names.contains_key(&def));
                                if (mutable_base || owned_base)
                                    && let Some(sid) = field_owner_struct(base, &expressions, self.struct_names, self.structs)
                                        && let Some(field_ty) = field_ty_in(sid, name, self.structs) {
                                            if scalar_type(field_ty) {
                                                expressions.struct_field_writes.insert(target.range.start);
                                            } else if matches!(field_ty, Type::String | Type::Vector(_) | Type::MapOf(_)) {
                                                // An owned field store releases the
                                                // old value and consumes the new one
                                                // (the FieldStore codegen); containers
                                                // release in place like strings.
                                                expressions.owned_field_writes.insert(target.range.start);
                                            }
                                        }
                            }
                    }
                });
            }
            // Array parameters are arrays for the body route too: an array
            // type spelling starts with `[` (after the borrow amp), where
            // vec/map/composite spellings start with an identifier. The
            // resolver keeps the pointee opaque (`& unknown`), so the spelled
            // tokens are the arbiter here.
            for function in &module.functions {
                let ae_syntax::declaration::DeclarationKind::Function { parameters, .. } = &function.declaration.kind else {
                    continue;
                };
                for parameter in parameters {
                    if parameter.ty.start == parameter.ty.end {
                        continue;
                    }
                    let mut index = self.tokens.partition_point(|token| token.offset < parameter.ty.start);
                    if self.tokens.get(index).map(|token| token.kind.clone()) == Some(Kind::Amp) {
                        index += 1;
                    }
                    if self.tokens.get(index).map(|token| token.kind.clone()) == Some(Kind::LBracket)
                        && let Some(def) = expressions.scalar_definitions.get(&parameter.name_range.start) {
                            expressions.array_definitions.insert(*def);
                        }
                }
            }
            for (function, body) in resolution.functions.iter().flat_map(|function| {
                function.closure_bodies.iter().map(move |body| (function, body))
            }) {
                // A captured outer read is legal inside the closure body:
                // the compiler hands the value through a capture slot, so
                // the route re-admits reads whose target is one of this
                // body's captures.
                for occurrence in &function.occurrences {
                    if occurrence.role == ae_resolve::Role::Read
                        && occurrence.range.start >= body.start
                        && occurrence.range.end <= body.end
                        && occurrence.target.is_some_and(|target| {
                            body.captures.contains(&target)
                                && (expressions.owned_text_bindings.contains(&target)
                                    || expressions.owned_vectors.contains(&target))
                        })
                    {
                        expressions.closure_capture_reads.insert(occurrence.range.start);
                    }
                }
                let failing = body.statements.iter().position(|statement| {
                    // The closure lowering re-checks fields against the
                    // parameter slots, so the collection pass is permissive for
                    // an ident-rooted chain (a `&Pair` closure parameter's
                    // field read, an `&[i64; N]` parameter's index).
                    !scalar_body_statement(statement, &expressions, &|_, _| true, &|_, _| true)
                });
                if !body.has_binding_conflict && failing.is_none() {
                    expressions.scalar_bodies.insert(body.start, body.statements.clone());
                } else if body.has_binding_conflict {
                    if std::env::var_os("AE_ROUTE_REASONS").is_some() {
                        eprintln!("route reason closure {}: binding conflict", self.file);
                    }
                } else if let Some(statement) = failing.and_then(|index| body.statements.get(index)) {
                    self.report_route_reason("closure", statement, &|s| !scalar_body_statement(s, &expressions, &|_, _| true, &|_, _| false));
                }
            }
            for function in module.functions {
                let (Some(range), Some(body)) = (function.declaration.body, function.body) else {
                    continue;
                };
                // An impl method keeps its self-rooted chains; any other
                // routed struct receiver answers by the marks the walk above
                // left — a scalar field read names the chain, and a field
                // store names a `&mut` view's target. The lowering re-checks
                // both against the slot types before it commits.
                let supports_field = |expression: &ae_syntax::expression::Expression, role| {
                    if function.impl_owner.is_some() && receiver_field_root(expression).is_some() {
                        return true;
                    }
                    // A match subject: `match self` is a valid enum scrutinee
                    // exactly when the impl's target is an enum. The shared
                    // resolution identifies the target, so the answer does
                    // not depend on any per-body state.
                    if role == ae_resolve::Role::Borrow {
                        if !matches!(&expression.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name)) if name == "self") {
                            return false;
                        }
                        return function.impl_owner.is_some_and(|owner| {
                            resolution.defs.get(owner).is_some_and(|entry| entry.kind == ae_resolve::DefKind::Enum)
                        });
                    }
                    // A method-call callee: permissive at scan time; the
                    // lowering's re-check resolves the method.
                    if role == ae_resolve::Role::Call {
                        return matches!(&expression.kind, ae_syntax::expression::ExpressionKind::Field { base, .. }
                            if matches!(base.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(_))
                                // A method on a temporary/chained result: the
                                // lowering delegates the whole call to the
                                // typed postfix layer.
                                | ae_syntax::expression::ExpressionKind::Call { .. }
                                | ae_syntax::expression::ExpressionKind::Group(_)));
                    }
                    let field_on_ident = role != ae_resolve::Role::Borrow
                        && matches!(expression.kind, ae_syntax::expression::ExpressionKind::Field { .. })
                        && field_chain_root(expression).is_some();
                    match role {
                        ae_resolve::Role::Read => expressions.struct_field_reads.contains(&expression.range.start)
                            || field_on_ident,
                        ae_resolve::Role::Write => expressions.struct_field_writes.contains(&expression.range.start)
                            || expressions.owned_field_writes.contains(&expression.range.start)
                            || field_on_ident,
                        _ => false,
                    }
                };
                let failing = body.iter().position(|statement| {
                    !scalar_body_statement(statement, &expressions, &|_, _| true, &supports_field)
                });
                if failing.is_none() {
                    expressions.scalar_bodies.insert(range.start, body.into());
                } else if let Some(statement) = failing.and_then(|index| body.get(index)) {
                    self.report_route_reason("function", statement, &|s| !scalar_body_statement(s, &expressions, &|_, _| true, &supports_field));
                }
            }
            expressions
    }

    /// Resolve a source-spelled type argument to a concrete type, instantiating
    /// nested generic struct instances as it descends (`Pair[i64, Pair[..]]`).
    pub(super) fn shape_type(&mut self, shape: &ShapeArg, offset: usize) -> Option<Type> {
        match shape {
            ShapeArg::Value(ty) => Some(*ty),
            ShapeArg::StructInstance { gid, args } => {
                let concrete = args.iter().map(|arg| self.shape_type(arg, offset)).collect::<Option<Vec<_>>>()?;
                self.instantiate_struct(*gid, concrete, offset).ok().map(Type::Struct)
            }
        }
    }

    pub(super) fn shared_specialization_arguments(&self, offset: usize) -> Option<std::sync::Arc<[std::ops::Range<usize>]>> {
        self.resolved_expressions().specialization_arguments.get(&offset).cloned()
    }

    pub(super) fn shared_pointer_cast_qualifier(&self) -> Option<bool> {
        let offset = self.tokens.get(self.pos + 2)?.offset;
        self.resolved_expressions().pointer_cast_targets.get(&offset).copied()
    }

    pub(super) fn shared_closure_header(&self) -> Option<std::sync::Arc<ae_syntax::expression::Expression>> {
        let offset = self.tokens.get(self.pos)?.offset;
        self.resolved_expressions().closure_headers.get(&offset).cloned()
    }

}
