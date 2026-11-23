//! Parser and single-pass semantic checker: tokens to typed statements.
use super::*;

#[derive(Default)]
struct SharedExpressionEffects {
    moves: Vec<usize>,
    call_borrows: Vec<usize>,
    mut_borrows: Vec<usize>,
    conflicts: Vec<(usize, usize, String, &'static str, Option<String>)>,
}

/// The remedy attached to a use-after-move diagnostic. The move is not tracked
/// to a byte range, so this names both available fixes instead of pointing at a
/// site that may not exist.
pub(super) fn moved_value_hint(diagnostic: ae_span::Diagnostic) -> ae_span::Diagnostic {
    diagnostic.with_suggestion("Owned values move on assignment or when passed by value; read what you need before the move, or build a fresh value.")
}

/// The remedy for `s = f(s)`, where the assignment target is consumed by the
/// call on its own right-hand side.
pub(super) fn moved_self_assignment_hint(diagnostic: ae_span::Diagnostic) -> ae_span::Diagnostic {
    diagnostic.with_suggestion("This call consumes the value being assigned to; bind the result to a new name, or read its fields into locals first.")
}

fn scalar_type(ty: Type) -> bool {
    matches!(ty, Type::Int | Type::F64 | Type::Bool)
}

/// A builtin call that builds a fresh `string`: assigning its result to an
/// owned string releases the old value and takes the new one, exactly like a
/// text literal or another owned string on the right-hand side.
fn string_result_builtin(name: &str) -> bool {
    matches!(name, "slice" | "trim" | "to_string" | "join" | "read_file" | "path_join" | "path_name" | "path_parent" | "utf8_prefix")
}

/// The pure value builtins the resolved body route lowers itself: each reads
/// its arguments (never moves except a string `+`, which the Binary arm
/// handles), so the body needs no borrow-argument machinery for them. Every
/// other reserved name keeps its typed body until its surface is reviewed.
const RESOLVED_BUILTINS: &[&str] = &[
    "len", "to_string", "to_f64", "to_i64", "trim", "slice", "slice_view", "split",
    "contains", "starts_with", "ends_with", "join", "parse_int", "find",
    "map_new", "f64map_new", "map_get", "map_has", "map_remove", "map_inc", "keys",
    "cell_new", "cell_load", "cell_add", "cell_cas",
    // Copy-semantics handle constructors and accessors: each reads or builds a
    // handle value, so the body route owns nothing a release routine walks.
    // The spawn builtins stay typed until closure literals are tracked.
    "shared_new", "shared_get", "mutex_new", "mutex_lock",
    "chan_new", "chan_send", "chan_try_send", "chan_recv", "chan_select",
    "exec_new", "exec_run", "scope_new", "pool_new", "pool_join",
    "json_get", "json_int", "json_bool", "json_len", "json_has",
    // Effectful value builtins: each reads its arguments and returns a value
    // (plain, or a prelude result enum the caller must handle).
    "read_file", "write_file", "now_ms", "now_s", "args", "read_stdin", "cwd", "env",
    "try_read_file", "try_write_file", "try_append_file", "try_list_dir", "try_file_info",
    "try_copy_file", "try_rename_path", "try_remove_file", "try_remove_dir", "try_create_dir",
    "set_cwd", "path_join", "path_name", "path_parent", "path_exists", "path_is_dir",
    "count_lines_with", "run_command", "utf8_len", "utf8_prefix", "lines", "strvec_new", "f64vec_new",
    // The thread pair: `spawn` moves captures into the thread and returns its
    // handle; `wait` consumes the handle. The spawn statement forms
    // (task_spawn/scope_spawn/pool_spawn) stay typed for now.
    "spawn", "wait",
    // Compile-time C layout queries (`c_size_of[Struct]()`, ...), spelled
    // with a type argument like the generic calls.
    "c_size_of", "c_align_of", "c_offset_of",
];

/// Builtins whose result is a prelude enum value: a binding initialized by
/// one is enum-typed even when the resolver leaves its spelling open.
const ENUM_RESULT_BUILTINS: &[(&str, &str)] = &[
    ("parse_int", "IntResult"), ("to_i64", "IntResult"), ("find", "IntResult"),
    ("json_get", "ReadResult"), ("json_int", "IntResult"), ("map_get", "IntResult"),
    ("json_bool", "BoolResult"), ("json_len", "IntResult"),
    ("try_read_file", "ReadResult"), ("try_list_dir", "DirResult"),
    ("try_file_info", "FileResult"), ("env", "ReadResult"),
    ("cwd", "ReadResult"), ("read_stdin", "ReadResult"),
    ("try_write_file", "WriteResult"), ("try_append_file", "WriteResult"),
    ("try_copy_file", "StatusResult"), ("try_rename_path", "StatusResult"),
    ("try_remove_file", "StatusResult"), ("try_remove_dir", "StatusResult"),
    ("try_create_dir", "StatusResult"), ("set_cwd", "StatusResult"),
    ("run_command", "CommandResult"),
];

fn resolved_builtin(name: &str) -> bool {
    RESOLVED_BUILTINS.contains(&name) || MATH_FNS.iter().any(|(math, _, _)| *math == name)
}

/// Intrinsics the typed expression parser dispatches by name and the resolved
/// body route delegates whole to that layer: the pointer pair and `bytes`.
/// The scan admits their calls, and the lowering parses them with the typed
/// postfix code (whose operational checks and shapes are authoritative).
fn typed_intrinsic(name: &str) -> bool {
    matches!(name, "ptr_write" | "ptr_drop_in_place" | "bytes")
}

/// A struct the body route can own outright: no `Drop` impl and only scalar
/// fields. Constructing one allocates nothing a release routine would walk,
/// so a construction, a by-value argument, and a by-value return all move the
/// value without an ownership hook — the same shape the drop-free
/// construction collection marks. Structs with owned fields (`string`,
/// vectors) stay on the typed route until construction moves are tracked.
fn owned_plain_struct(ty: Type, structs: &[StructDef]) -> bool {
    let Type::Struct(sid) = ty else { return false };
    structs.get(sid).is_some_and(|defn| defn.drop_fn.is_none()
        && defn.fields.iter().all(|(_, ty)| scalar_type(*ty) || owned_plain_struct(*ty, structs)))
}

/// A struct whose fields are scalar or `string`: it carries one owned value
/// per string field, and the generated `ae_tN_release` frees them. The body
/// route may build one (each string value moves in) and own it.
/// A struct that declares `fn drop(&mut self)` and whose fields are plain:
/// the body route owns it by value exactly like a drop-free struct — the
/// generated release calls the user's drop and clears the handle, so a
/// parameter or local of this type releases correctly.
fn plain_drop_struct(ty: Type, structs: &[StructDef]) -> bool {
    let Type::Struct(sid) = ty else { return false };
    structs.get(sid).is_some_and(|defn| defn.drop_fn.is_some()
        && defn.fields.iter().all(|(_, ty)| scalar_type(*ty) || owned_plain_struct(*ty, structs)))
}

fn owned_field_struct(ty: Type, structs: &[StructDef]) -> bool {
    let Type::Struct(sid) = ty else { return false };
    structs.get(sid).is_some_and(|defn| defn.drop_fn.is_none()
        && defn.fields.iter().all(|(_, ty)| scalar_type(*ty)
            // Every container the generated release routine knows (it frees a
            // vec/strvec/f64vec/map field in place, `release_member`): the
            // construction moves the value in and the exit free releases it.
            || matches!(ty, Type::String | Type::Atomic | Type::Shared(_) | Type::Mutex
                | Type::Vec | Type::VecF | Type::StrVec | Type::Map | Type::MapF)
            // A nested struct carries its own release routine; the generated
            // release recurses into every struct field (c_output).
            || owned_plain_struct(*ty, structs)
            || owned_field_struct(*ty, structs)))
}

/// The registry struct a field expression reads from: a parameter's
/// immutable borrowed spelling names a file-local struct item; a nested
/// field's struct is the declared type of the field it reads. Generic
/// instances and imported structs have no registry entry here and stay on
/// the typed route.
fn field_owner_struct(
    base: &ae_syntax::expression::Expression,
    resolved: &ResolvedExpressions,
    struct_names: &HashMap<String, usize>,
    structs: &[StructDef],
) -> Option<usize> {
    match &base.kind {
        ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(_)) => {
            // Reads and writes are keyed by occurrence offset, so one merged
            // lookup answers both: a read base names its binding, a store base
            // names the `&mut` view it writes through. The pre-resolved name
            // map answers for this module's structs and imported ones alike.
            let def = resolved.scalar_reads.get(&base.range.start)
                .or_else(|| resolved.scalar_writes.get(&base.range.start))?;
            let name = resolved.struct_definition_names.get(def)?;
            struct_names.get(name.as_str()).copied()
        }
        ae_syntax::expression::ExpressionKind::Field { base: inner_base, name } => {
            let sid = field_owner_struct(inner_base, resolved, struct_names, structs)?;
            let fty = field_ty_in(sid, name, structs)?;
            match fty {
                Type::Struct(sid) | Type::StructRef(sid) | Type::StructMut(sid) => Some(sid),
                _ => None,
            }
        }
        _ => None,
    }
}

fn field_ty_in(sid: usize, field: &str, structs: &[StructDef]) -> Option<Type> {
    let defn = structs.get(sid)?;
    defn.fields.iter().find(|(name, _)| name == field).map(|(_, ty)| *ty)
}

/// The declared shape of one field read in a resolved body, including a
/// generic instance spelled in source (`let p = Pair[i64, Label] { .. };`
/// → `p.left` is `i64`, `p.right.name` walks through `Label`). Nested
/// generic instances (`Pair[string, Pair[string, string]].right.left`) stay
/// shapes until the final field resolves to a value. No layout is
/// instantiated, so the answer is stateless.
fn resolved_field_shape(
    base: &ae_syntax::expression::Expression,
    field: &str,
    resolved: &ResolvedExpressions,
    struct_names: &HashMap<String, usize>,
    structs: &[StructDef],
    generics: &Generics,
) -> Option<ShapeArg> {
    use ae_syntax::expression::ExpressionKind;
    match &base.kind {
        ExpressionKind::Atom(Kind::Ident(_)) => {
            let def = resolved.scalar_reads.get(&base.range.start)
                .or_else(|| resolved.scalar_writes.get(&base.range.start))?;
            if let Some((gid, args)) = resolved.generic_struct_bindings.get(def) {
                let instance = ShapeArg::StructInstance { gid: *gid, args: args.clone() };
                return shape_field(&instance, field, structs, generics);
            }
            let name = resolved.struct_definition_names.get(def)?;
            let sid = struct_names.get(name.as_str())?;
            field_ty_in(*sid, field, structs).map(ShapeArg::Value)
        }
        ExpressionKind::Field { base: inner, name } => {
            let inner_shape = resolved_field_shape(inner, name, resolved, struct_names, structs, generics)?;
            shape_field(&inner_shape, field, structs, generics)
        }
        _ => None,
    }
}

fn shape_field(shape: &ShapeArg, field: &str, structs: &[StructDef], generics: &Generics) -> Option<ShapeArg> {
    match shape {
        ShapeArg::Value(Type::Struct(sid) | Type::StructRef(sid) | Type::StructMut(sid)) => {
            field_ty_in(*sid, field, structs).map(ShapeArg::Value)
        }
        ShapeArg::StructInstance { gid, args } => {
            let defn = generics.struct_defs.get(*gid)?;
            let (_, payload) = defn.fields.iter().find(|(name, _)| name == field)?;
            payload_shape(payload, args, generics)
        }
        _ => None,
    }
}

fn payload_shape(payload: &Payload, args: &[ShapeArg], generics: &Generics) -> Option<ShapeArg> {
    match payload {
        Payload::Concrete(ty) => Some(ShapeArg::Value(*ty)),
        Payload::Param(index) => args.get(*index).cloned(),
        Payload::Nested(index) => {
            let shape = generics.nested_shapes.get(*index)?;
            let resolved: Option<Vec<ShapeArg>> = shape.args.iter().map(|payload| payload_shape(payload, args, generics)).collect();
            resolved.map(|args| ShapeArg::StructInstance { gid: shape.gid, args })
        }
    }
}

fn receiver_field_root(expression: &ae_syntax::expression::Expression) -> Option<usize> {
    use ae_syntax::expression::ExpressionKind;
    match &expression.kind {
        ExpressionKind::Field { base, .. } => receiver_field_root(base),
        ExpressionKind::Atom(Kind::Ident(name)) if name == "self" => Some(expression.range.start),
        _ => None,
    }
}

/// The root identifier of any field chain (`p.x.y` -> the `p` node). The
/// body route's field gate uses it to resolve the chain's type from the
/// slot table, so a plain function's struct parameter works like `self`.
fn field_chain_root(expression: &ae_syntax::expression::Expression) -> Option<&ae_syntax::expression::Expression> {
    use ae_syntax::expression::ExpressionKind;
    match &expression.kind {
        ExpressionKind::Field { base, .. } => field_chain_root(base),
        ExpressionKind::Atom(Kind::Ident(_)) => Some(expression),
        _ => None,
    }
}

/// Whether the expression IS the chain root (no field access on it).
fn field_chain_is_bare(expression: &ae_syntax::expression::Expression) -> bool {
    matches!(expression.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(_)))
}

fn receiver_value(expression: &ae_syntax::expression::Expression) -> Option<&ae_syntax::expression::Expression> {
    use ae_syntax::expression::ExpressionKind;
    match &expression.kind {
        ExpressionKind::Group(inner) => receiver_value(inner),
        ExpressionKind::Atom(Kind::Ident(name)) if name == "self" => Some(expression),
        _ => None,
    }
}

// The type of a field chain walked from `receiver`: the root node is the
// chain's base ident (the caller supplies the root's type from its slot),
// and each step resolves through the field table. `self.a.b` and `r.a.b`
// answer the same way.
fn receiver_field_type(
    expression: &ae_syntax::expression::Expression,
    receiver: Type,
    structs: &[StructDef],
) -> Option<Type> {
    use ae_syntax::expression::ExpressionKind;
    match &expression.kind {
        ExpressionKind::Field { base, name } => {
            let sid = match receiver_field_type(base, receiver, structs)? {
                Type::Struct(sid) | Type::StructRef(sid) | Type::StructMut(sid) => sid,
                _ => return None,
            };
            field_ty_in(sid, name, structs)
        }
        ExpressionKind::Atom(Kind::Ident(_)) => Some(receiver),
        _ => None,
    }
}


/// Every expression in the tree rooted at `expression`. `statement.expressions()`
/// and `visit_expressions` stop at the statement's top-level values; the body
/// route's checks need the nested ones too. Closure bodies stay opaque (the
/// compiler owns capture inference), so their contents are not visited.
fn visit_expression_tree(expression: &ae_syntax::expression::Expression, visit: &mut impl FnMut(&ae_syntax::expression::Expression)) {
    use ae_syntax::expression::ExpressionKind;
    visit(expression);
    match &expression.kind {
        ExpressionKind::Group(inner) | ExpressionKind::Propagate(inner) | ExpressionKind::Prefix { operand: inner, .. } => {
            visit_expression_tree(inner, visit);
        }
        ExpressionKind::PointerCast { base, .. } => visit_expression_tree(base, visit),
        ExpressionKind::Binary { left, right, .. } => {
            visit_expression_tree(left, visit);
            visit_expression_tree(right, visit);
        }
        ExpressionKind::Array(elements) | ExpressionKind::Vector(elements) => {
            elements.iter().for_each(|element| visit_expression_tree(element, visit));
        }
        ExpressionKind::Struct { target, fields } => {
            visit_expression_tree(target, visit);
            fields.iter().for_each(|field| visit_expression_tree(&field.value, visit));
        }
        ExpressionKind::Call { callee, arguments } => {
            visit_expression_tree(callee, visit);
            arguments.iter().for_each(|argument| visit_expression_tree(argument, visit));
        }
        ExpressionKind::Index { base, index } => {
            visit_expression_tree(base, visit);
            visit_expression_tree(index, visit);
        }
        ExpressionKind::Field { base, .. } | ExpressionKind::Specialization { base, .. }
        | ExpressionKind::BracketedCallTarget { base, .. } | ExpressionKind::Qualified { base, .. } => {
            visit_expression_tree(base, visit);
        }
        _ => {}
    }
}

fn scalar_body_expression(expression: &ae_syntax::expression::Expression, resolved: &ResolvedExpressions, supports_call: &impl Fn(&str, usize) -> bool, supports_field: &impl Fn(&ae_syntax::expression::Expression, ae_resolve::Role) -> bool) -> bool {
    use ae_syntax::expression::ExpressionKind;
    match &expression.kind {
        // A name no local answers: the lowering reports the typed route's
        // exact diagnostic (`unknown variable`, E0101, or the struct-name
        // remedy) at the name itself, so the scan admits it. The marked
        // shapes keep their exclusion rules.
        ExpressionKind::Atom(Kind::Ident(name)) if name != "self" => match resolved.scalar_reads.get(&expression.range.start) {
            Some(id) => !resolved.array_definitions.contains(id) && !resolved.struct_definitions.contains(id)
                && !resolved.owned_vectors.contains(id) && !resolved.owned_text_bindings.contains(id)
                && !resolved.enum_bindings.contains(id)
                && !resolved.token_cursor_definitions.contains(id)
                || (resolved.owned_text_bindings.contains(id)
                    && resolved.closure_capture_reads.contains(&expression.range.start))
                // A struct borrow read as a value: the binding arm's own
                // "cannot store a borrowed struct" diagnostic answers.
                || resolved.struct_borrow_reads.contains(id),
            None => true,
        },
        // A construction of a file-local struct: the drop-free scalar
        // shape lowers as a value, and the scalar-or-`string` shape owns its
        // string field values (a literal, a moved owned string, or an
        // expression the scalar arm already admits such as a concat or a
        // `to_string` call). Field types are checked when the literal lowers.
        ExpressionKind::Struct { target, fields } => {
            let marked = resolved.drop_free_constructions.contains(&expression.range.start)
                || resolved.owned_constructions.contains(&expression.range.start)
                // A struct that declares `drop`: the value is owned, and
                // every exit path releases it through the generated routine,
                // which calls the user's `drop` first.
                || resolved.drop_constructions.contains(&expression.range.start);
            // A specialization (`Cell[T] { .. }`) is permissive here: the
            // instance's field types exist only at lowering time and are
            // checked there.
            let specializes = matches!(&target.kind, ExpressionKind::Specialization { base, .. }
                if matches!(base.kind, ExpressionKind::Atom(Kind::Ident(_))));
            let named = matches!(target.kind, ExpressionKind::Atom(Kind::Ident(_)));
            (named || specializes)
                && (marked || specializes)
                && fields.iter().all(|field| {
                    scalar_body_expression(&field.value, resolved, supports_call, supports_field)
                        || matches!(field.value.kind, ExpressionKind::Atom(Kind::Text(_)))
                        || matches!(&field.value.kind, ExpressionKind::Atom(Kind::Ident(name))
                            if name != "self" && resolved.scalar_reads.get(&field.value.range.start)
                                .is_some_and(|id| resolved.owned_text_bindings.contains(id)
                                    || resolved.owned_string_bindings.contains(id)
                                    // An owned struct or enum value moves into
                                    // the field (`SocketAddrV4 { ip: ip, .. }`).
                                    || resolved.struct_definitions.contains(id)
                                    || resolved.owned_struct_bindings.contains(id)
                                    || resolved.enum_bindings.contains(id)))
                })
        }
        // A struct parameter's field read, possibly nested (`o.inner.x`):
        // the chain's base may be a scalar read or a struct-typed chain
        // receiver, but only a scalar field lowers as a value.
        ExpressionKind::Field { base, .. } => {
            if receiver_field_root(expression).is_some() {
                return supports_field(expression, ae_resolve::Role::Read);
            }
            // A field read on a *plain* call result (`f().x`,
            // `compute_mag[Delta](d).offset`): the call must be admissible and
            // the lowering checks the return type is a struct. A field on a
            // method-call result (`p.new(2).x`) is admitted permissively: the
            // method resolution's pinned diagnostics (an associated function
            // called through a value, a missing method) are the lowering's.
            let plain_call_base = match &base.kind {
                ExpressionKind::Call { callee, .. } => {
                    matches!(&callee.kind, ExpressionKind::Atom(Kind::Ident(_)) | ExpressionKind::Qualified { .. })
                        || matches!(&callee.kind, ExpressionKind::BracketedCallTarget { base: inner, .. }
                            if matches!(inner.kind, ExpressionKind::Atom(Kind::Ident(_))))
                }
                _ => false,
            };
            let method_call_base = match &base.kind {
                ExpressionKind::Call { callee, .. } => {
                    matches!(&callee.kind, ExpressionKind::Field { base: inner, .. }
                        if matches!(inner.kind, ExpressionKind::Atom(Kind::Ident(_))))
                }
                _ => false,
            };
            let rooted = match &base.kind {
                ExpressionKind::Atom(Kind::Ident(_)) => true,
                ExpressionKind::Field { .. } => resolved.struct_field_reads.contains(&base.range.start)
                    || resolved.struct_field_chains.contains(&base.range.start),
                ExpressionKind::Call { .. } => (plain_call_base || method_call_base)
                    && scalar_body_expression(base, resolved, supports_call, supports_field),
                _ => false,
            };
            rooted && (resolved.struct_field_reads.contains(&expression.range.start)
                || plain_call_base
                // The lowering's method resolution answers first; a resolved
                // method call's struct result then reads the field.
                || method_call_base
                // A local from an explicit generic call: the instantiated
                // return type exists only at lowering time.
                || matches!(&base.kind, ExpressionKind::Atom(Kind::Ident(_)))
                    && resolved.scalar_reads.get(&base.range.start)
                        .is_some_and(|id| resolved.generic_call_bindings.contains(id))
                // A struct-typed parameter or local whose spelling the resolver
                // leaves open (a generic instance, an imported type): the scan
                // is permissive for an ident-rooted chain and the lowering's
                // re-check answers from the slot table.
                || supports_field(expression, ae_resolve::Role::Read))
        }
        // A name with a bracket that the syntax tree kept as a specialization
        // (`arr[j]` at the end of an expression): the typed expression parser
        // disambiguates it against the known names and builds the index.
        ExpressionKind::Specialization { base, arguments } => {
            matches!(base.kind, ExpressionKind::Atom(Kind::Ident(_)))
                && arguments.len() == 1
                && resolved.scalar_reads.get(&base.range.start).is_some_and(|id| {
                    resolved.array_definitions.contains(id) || resolved.vector_definitions.contains(id)
                })
        }
        ExpressionKind::Index { base, index } => {
            matches!(base.kind, ExpressionKind::Atom(Kind::Ident(_)))
                && resolved.scalar_reads.get(&base.range.start).is_some_and(|id| {
                    resolved.array_definitions.contains(id)
                        // A vector indexes as element copies/views, whether
                        // the binding owns the vector or borrows it
                        // (`values: &vec`); the vector itself never moves.
                        || resolved.vector_definitions.contains(id)
                })
                && scalar_body_expression(index, resolved, supports_call, supports_field)
        }
        ExpressionKind::Atom(Kind::Number(_) | Kind::Float(_) | Kind::True | Kind::False | Kind::Text(_)) => true,
        // A collection literal used as a value (`print(vec[1])`): the scan
        // admits it when every element does, and the lowering builds the same
        // `Expr::VecLit`/array the typed route builds. The print arm's type
        // check then rejects a vector exactly as the typed route does.
        ExpressionKind::Array(elements) | ExpressionKind::Vector(elements) => {
            elements.iter().all(|element| scalar_body_expression(element, resolved, supports_call, supports_field))
        }
        ExpressionKind::AddressCast { binding_range, .. } => {
            resolved.scalar_address_casts.contains(&expression.range.start)
                && resolved.scalar_reads.contains_key(&binding_range.start)
        }
        // A pointer reinterpretation (`raw as *mut f64`): the base is a
        // pointer value and the pointee spelling is the typed parser's
        // business. Nothing moves — a pointer is a copy-semantics handle.
        ExpressionKind::PointerCast { base, .. } => {
            scalar_body_expression(base, resolved, supports_call, supports_field)
        }
        ExpressionKind::Group(inner) | ExpressionKind::Prefix { operand: inner, .. } => {
            scalar_body_expression(inner, resolved, supports_call, supports_field)
        }
        ExpressionKind::Binary { left, right, operator } => {
            // A string-typed field reads as a borrowed view inside a `+`
            // (the typed route's plus-operand rule): the field itself never
            // moves, so the concat may use it even though a bare read of the
            // field is not a value. An owned string variable is consumed by
            // the same `+`, so it is admissible here too.
            let view = |expression: &ae_syntax::expression::Expression| {
                if resolved.struct_field_views.contains(&expression.range.start) {
                    // A borrowed string field reads by `+` and by the
                    // equality/comparison operators; it never moves.
                    return *operator == Kind::Plus
                        || matches!(operator, Kind::EqEq | Kind::NotEq | Kind::Less | Kind::LessEq | Kind::Greater | Kind::GreaterEq);
                }
                // A comparison only reads the string (the typed route's
                // equality does not consume); every other operator keeps the
                // `+`-only rule.
                if *operator != Kind::Plus
                    && !matches!(operator, Kind::EqEq | Kind::NotEq | Kind::Less | Kind::LessEq | Kind::Greater | Kind::GreaterEq)
                {
                    return false;
                }
                matches!(&expression.kind, ExpressionKind::Atom(Kind::Ident(name))
                    if name != "self" && resolved.scalar_reads.get(&expression.range.start)
                        .is_some_and(|id| resolved.owned_text_bindings.contains(id)
                            || resolved.owned_string_bindings.contains(id)))
            };
            (scalar_body_expression(left, resolved, supports_call, supports_field) || view(left))
                && (scalar_body_expression(right, resolved, supports_call, supports_field) || view(right))
        }
        ExpressionKind::Call { callee, arguments } => {
            // A call whose name no declaration answers: the typed route errors
            // at the callee before any argument or return-type question, and
            // the lowering reproduces that E0207 — so the scan admits the
            // shape.
            if let ExpressionKind::Atom(Kind::Ident(_)) = &callee.kind
                && resolved.unknown_calls.contains(&callee.range.start) {
                return true;
            }
            if let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind
                && typed_intrinsic(name) {
                return true;
            }
            // `len` of a bare fixed array is a compile-time constant and of a
            // vector or map a borrow of the slot: the builtin's fast path
            // reads the binding's definition, not a value, so the scan admits
            // the name even where the value-position rules exclude it.
            if let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind
                && name == "len"
                && let [argument] = arguments.as_slice()
                && matches!(&argument.kind, ExpressionKind::Atom(Kind::Ident(_)))
                && resolved.scalar_reads.contains_key(&argument.range.start) {
                return true;
            }
            // An associated call (`Point::new(..)`, `Type[..]::new(..)`): the
            // typed postfix layer owns instance construction, arity, and the
            // pinned associated-function diagnostics; the lowering delegates
            // the whole call.
            if matches!(&callee.kind, ExpressionKind::Qualified { .. }) {
                return true;
            }
            if let ExpressionKind::Field { base, name } = &callee.kind {
                if name == "deref" && arguments.is_empty()
                    && matches!(base.kind, ExpressionKind::Atom(Kind::Ident(_)))
                    && resolved.scalar_pointer_receivers.contains(&base.range.start)
                {
                    return true;
                }
                // A method call: the gate answers `Role::Call` for the callee
                // node. The scan is permissive (a bare-ident receiver with
                // admissible arguments); the lowering's re-check resolves the
                // method and rejects the rest, which falls the body back to
                // the typed route.
                return supports_field(callee, ae_resolve::Role::Call)
                    && arguments.iter().all(|argument| {
                        scalar_body_expression(argument, resolved, supports_call, supports_field)
                            || matches!(argument.kind, ExpressionKind::Atom(Kind::Text(_)))
                            || resolved.struct_field_views.contains(&argument.range.start)
                            || resolved.struct_field_handles.contains(&argument.range.start)
                            || (matches!(argument.kind, ExpressionKind::Field { .. })
                                && supports_field(argument, ae_resolve::Role::Read))
                            || matches!(&argument.kind, ExpressionKind::Atom(Kind::Ident(name))
                                if name != "self" && resolved.scalar_reads.get(&argument.range.start)
                                    .is_some_and(|id| resolved.owned_text_bindings.contains(id)
                                        || resolved.owned_string_bindings.contains(id)
                                        || resolved.owned_vectors.contains(id)
                                        || resolved.map_definitions.contains(id)))
                            // An enum construction as a method argument.
                            || resolved.enum_constructors.contains(&argument.range.start)
                            // An enum value passed by value to a method.
                            || matches!(&argument.kind, ExpressionKind::Atom(Kind::Ident(name))
                                if name != "self" && resolved.scalar_reads.get(&argument.range.start)
                                    .is_some_and(|id| resolved.enum_bindings.contains(id)))
                            // A closure literal passed to a method.
                            || matches!(&argument.kind, ExpressionKind::Closure { body, .. }
                                if resolved.scalar_bodies.contains_key(&body.start))
                    });
            }
            // A call through a function-typed value: the binding names the
            // pointer the caller passed (`Expr::FnCall` on its slot).
            if matches!(callee.kind, ExpressionKind::Atom(Kind::Ident(_)))
                && resolved.call_definitions.get(&callee.range.start)
                    .is_some_and(|id| resolved.fn_value_bindings.contains(id))
            {
                return arguments.iter().all(|argument| {
                    scalar_body_expression(argument, resolved, supports_call, supports_field)
                        || matches!(argument.kind, ExpressionKind::Atom(Kind::Text(_)))
                        || resolved.struct_field_views.contains(&argument.range.start)
                        || resolved.struct_field_handles.contains(&argument.range.start)
                });
            }
            // A call on a closure value: the typed machinery reads the captures
            // at the call site; the scan admits the shape and the lowering
            // validates the callee and the arguments.
            if let ExpressionKind::Atom(Kind::Ident(_)) = &callee.kind
                && resolved.call_definitions.get(&callee.range.start)
                    .is_some_and(|id| resolved.closure_bindings.contains(id))
                {
                    return arguments.iter().all(|argument| {
                        scalar_body_expression(argument, resolved, supports_call, supports_field)
                            || matches!(argument.kind, ExpressionKind::Atom(Kind::Text(_)))
                            || resolved.struct_field_views.contains(&argument.range.start)
                            || resolved.struct_field_handles.contains(&argument.range.start)
                    });
                }
            // An explicit generic call: `identity[i64](41)`. The scan admits
            // the shape permissively; the lowering instantiates the function
            // and re-checks the substituted argument list.
            if let ExpressionKind::BracketedCallTarget { base, .. } = &callee.kind {
                let ExpressionKind::Atom(Kind::Ident(name)) = &base.kind else { return false };
                return supports_call(name, base.range.start)
                    && arguments.iter().all(|argument| scalar_body_expression(argument, resolved, supports_call, supports_field)
                        || matches!(argument.kind, ExpressionKind::Atom(Kind::Text(_)))
                        || resolved.struct_field_views.contains(&argument.range.start)
                        || resolved.struct_field_handles.contains(&argument.range.start)
                        || resolved.struct_field_reads.contains(&argument.range.start)
                        || (matches!(argument.kind, ExpressionKind::Field { .. })
                            && supports_field(argument, ae_resolve::Role::Read))
                        || resolved.enum_constructors.contains(&argument.range.start)
                        || matches!(&argument.kind, ExpressionKind::Closure { body, .. }
                            if resolved.scalar_bodies.contains_key(&body.start))
                        // An owned aggregate passed by value moves into the
                        // instance's substituted parameter.
                        || matches!(&argument.kind, ExpressionKind::Atom(Kind::Ident(name))
                            if name != "self" && resolved.scalar_reads.get(&argument.range.start)
                                .is_some_and(|id| resolved.struct_definitions.contains(id)
                                    || resolved.enum_bindings.contains(id)
                                    || resolved.owned_text_bindings.contains(id)
                                    || resolved.owned_string_bindings.contains(id)
                                    || resolved.owned_vectors.contains(id)
                                    || resolved.map_definitions.contains(id)
                                    || resolved.owned_struct_bindings.contains(id)))
                        || matches!(argument.kind, ExpressionKind::BorrowArgument { .. }));
            }
            // The argument list below needs the callee's identity; a `matches!`
            // guard's bindings do not escape the macro, so the builtin answer is
            // taken once here.
            let callee_builtin = matches!(&callee.kind, ExpressionKind::Atom(Kind::Ident(name)) if resolved_builtin(name));
            matches!(&callee.kind, ExpressionKind::Atom(Kind::Ident(name))
                if (resolved.direct_calls.contains(&callee.range.start)
                    || resolved.unresolved_calls.contains(&callee.range.start))
                    && supports_call(name, callee.range.start))
                && arguments.iter().all(|argument| scalar_body_expression(argument, resolved, supports_call, supports_field)
                    || receiver_value(argument).is_some_and(|atom| supports_field(atom, ae_resolve::Role::Read))
                    // An owned aggregate passed by value is a move, not a
                    // value read: the ident eligibility excludes struct
                    // slots, so the argument position re-admits them.
                    || matches!(&argument.kind, ExpressionKind::Atom(Kind::Ident(name))
                        if name != "self" && resolved.scalar_reads.get(&argument.range.start)
                            .is_some_and(|id| resolved.struct_definitions.contains(id)
                                // An enum value passed by value (`describe(a)`):
                                // the call moves it.
                                || resolved.enum_bindings.contains(id)))
                    // A string-typed field in argument position reads as its
                    // view (builtins and `&string` parameters only ever
                    // borrow it).
                    || resolved.struct_field_views.contains(&argument.range.start)
                    || resolved.struct_field_handles.contains(&argument.range.start)
                    // A scalar field read as an argument (`describe(id.left)`):
                    // scalars copy, nothing moves. The scan is permissive for
                    // an ident-rooted chain; the lowering validates the field
                    // type (view, scalar, or rejected struct chain).
                    || resolved.struct_field_reads.contains(&argument.range.start)
                    || (matches!(argument.kind, ExpressionKind::Field { .. })
                        && supports_field(argument, ae_resolve::Role::Read))
                    // An owned string in argument position: a `&string`
                    // parameter views it, a `string` parameter moves it (the
                    // lowering records which).
                    || matches!(&argument.kind, ExpressionKind::Atom(Kind::Ident(name))
                        if name != "self" && resolved.scalar_reads.get(&argument.range.start)
                            .is_some_and(|id| resolved.owned_text_bindings.contains(id)
                                || resolved.owned_string_bindings.contains(id)
                                // An owned vector in argument position: a
                                // builtin (`len`) or a `&vec` parameter only
                                // borrows it. A map is borrowed the same way.
                                || resolved.owned_vectors.contains(id)
                                || resolved.map_definitions.contains(id)))
                    || matches!(argument.kind, ExpressionKind::BorrowArgument { .. })
                    // An enum construction passed directly (`f(Shape::Circle(3))`).
                    || resolved.enum_constructors.contains(&argument.range.start)
                    // A builtin over a struct field (`map_inc(b.counts, ..)`,
                    // `len(b.counts)`): the builtin's own arm type-checks the
                    // field, so the scan admits the read permissively.
                    || (callee_builtin && matches!(argument.kind, ExpressionKind::Field { .. }))
                    // A closure literal argument (`apply(|x: i64| -> i64
                    // {..}, 21)`) whose body the route collected.
                    || matches!(&argument.kind, ExpressionKind::Closure { body, .. }
                        if resolved.scalar_bodies.contains_key(&body.start)))
        }
        _ => false,
    }
}

fn compound_assignment_operator(operator: &Kind) -> Option<Kind> {
    match operator {
        Kind::PlusEqual => Some(Kind::Plus),
        Kind::MinusEqual => Some(Kind::Minus),
        Kind::StarEqual => Some(Kind::Star),
        Kind::SlashEqual => Some(Kind::Slash),
        Kind::PercentEqual => Some(Kind::Percent),
        _ => None,
    }
}

fn exit_status_operand(statement: &ae_syntax::statement::Statement) -> Option<&ae_syntax::expression::Expression> {
    use ae_syntax::expression::ExpressionKind;
    use ae_syntax::statement::StatementKind;
    let StatementKind::Expr { value, propagates: false } = &statement.kind else { return None };
    let ExpressionKind::Call { callee, arguments } = &value.kind else { return None };
    let [status] = arguments.as_slice() else { return None };
    matches!(&callee.kind, ExpressionKind::Atom(Kind::Ident(name)) if name == "exit").then_some(status)
}

/// A bare statement that only calls a name no declaration answers
/// (`missing_first();`): the typed route reports `unknown function 'name'`
/// (E0207) while parsing the callee, before the statement-level "must end with
/// '?'" check. The scan admits the shape so the lowering can reproduce it.
fn bare_call_statement(statement: &ae_syntax::statement::Statement) -> Option<&ae_syntax::expression::Expression> {
    use ae_syntax::expression::ExpressionKind;
    use ae_syntax::statement::StatementKind;
    let StatementKind::Expr { value, propagates: false } = &statement.kind else { return None };
    let ExpressionKind::Call { callee, .. } = &value.kind else { return None };
    // The statement builtins have their own arms (they precede this one, and
    // a rejected shape of theirs must keep the typed route's message); any
    // other bare call — a function, a builtin value call (`find(..);`) or an
    // unknown name — is evaluated (borrow and move diagnostics included) and
    // then rejected with the typed route's "must end with '?'" rule.
    let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind else { return None };
    (!statement_builtin_name(name)).then_some(value)
}

/// A name whose bare-call statement form has a dedicated arm in the body
/// lowering (`exit`, the vector and handle statement builtins, the spawn
/// forms, the yield). A bare call to one of these is never the generic
/// "must end with '?'" shape.
fn statement_builtin_name(name: &str) -> bool {
    matches!(name,
        "exit" | "push" | "pop" | "sort" | "reverse"
        | "shared_set" | "chan_close" | "cell_store"
        | "task_spawn" | "scope_spawn" | "pool_spawn" | "task_yield")
}

fn propagation_call(statement: &ae_syntax::statement::Statement) -> Option<&ae_syntax::expression::Expression> {
    use ae_syntax::expression::ExpressionKind;
    use ae_syntax::statement::StatementKind;
    let StatementKind::Expr { value, propagates: true } = &statement.kind else { return None };
    let ExpressionKind::Propagate(operand) = &value.kind else { return None };
    let ExpressionKind::Call { callee, .. } = &operand.kind else { return None };
    match &callee.kind {
        ExpressionKind::Atom(Kind::Ident(name)) => (!RESERVED.contains(&name.as_str())
            // A result-returning builtin (`try_copy_file(..)?`) propagates
            // exactly like a user function: its error enum carries `?`.
            || (resolved_builtin(name) && ENUM_RESULT_BUILTINS.iter().any(|(builtin, _)| *builtin == name)))
            .then_some(operand),
        // A variant construction (`Option[i64]::Some(10)?`): the typed
        // expression parser builds the payload, `?` reads the enum result.
        ExpressionKind::Qualified { .. } => Some(operand),
        _ => None,
    }
}

fn iterator_constructor(expression: &ae_syntax::expression::Expression) -> Option<Type> {
    use ae_syntax::expression::ExpressionKind;
    let ExpressionKind::Call { callee, arguments } = &expression.kind else { return None };
    let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind else { return None };
    match (name.as_str(), arguments.len()) {
        ("tokenize", 2) => Some(Type::Tokens),
        ("strvec_new", 0) | ("lines", 1) | ("split", 2) => Some(Type::StrVec),
        _ => None,
    }
}

fn scalar_body_statement(statement: &ae_syntax::statement::Statement, resolved: &ResolvedExpressions, supports_call: &impl Fn(&str, usize) -> bool, supports_field: &impl Fn(&ae_syntax::expression::Expression, ae_resolve::Role) -> bool) -> bool {
    use ae_syntax::statement::{ElseBranch, StatementKind};
    match &statement.kind {
        StatementKind::Binding { name_range, initializer, .. } => {
            resolved.scalar_definitions.contains_key(&name_range.start)
                && match &initializer.kind {
                    ae_syntax::expression::ExpressionKind::Array(elements)
                    | ae_syntax::expression::ExpressionKind::Vector(elements) => elements.iter()
                        .all(|element| scalar_body_expression(element, resolved, supports_call, supports_field)),
                    ae_syntax::expression::ExpressionKind::Atom(Kind::Text(_)) => true,
                    // `let x = foo()?;` — a direct-call propagation binding. A result-returning
                    // builtin (`let a = try_read_file(first)?;`) propagates the
                    // same way; other RESERVED callees stay typed.
                    ae_syntax::expression::ExpressionKind::Propagate(operand) => {
                        matches!(&operand.kind, ae_syntax::expression::ExpressionKind::Call { callee, .. }
                            if matches!(&callee.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                                if !RESERVED.contains(&name.as_str())
                                    || (resolved_builtin(name) && ENUM_RESULT_BUILTINS.iter().any(|(builtin, _)| *builtin == name))))
                            // A variant construction (`let a = Option[i64]::Some(10)?;`).
                            || (matches!(&operand.kind, ae_syntax::expression::ExpressionKind::Call { callee, .. }
                                if matches!(callee.kind, ae_syntax::expression::ExpressionKind::Qualified { .. }))
                                && resolved.enum_constructors.contains(&operand.range.start))
                    }
                    _ => iterator_constructor(initializer).is_some()
                        || resolved.enum_constructors.contains(&initializer.range.start)
                        || scalar_body_expression(initializer, resolved, supports_call, supports_field)
                        // A closure literal whose body the resolved route
                        // already collected: the lowering delegates the literal
                        // to the closure machinery (capture inference and the
                        // generated function).
                        || matches!(&initializer.kind, ae_syntax::expression::ExpressionKind::Closure { body, .. }
                            if resolved.scalar_bodies.contains_key(&body.start))
                        // `let copy = owned;` moves an owned value (the
                        // lowering marks the source moved; the view-after-
                        // last-use case defers through the move graph).
                        || matches!(&initializer.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                            if name != "self" && resolved.scalar_reads.get(&initializer.range.start)
                                .is_some_and(|id| resolved.owned_text_bindings.contains(id)
                                    || resolved.owned_string_bindings.contains(id)
                                    || resolved.owned_vectors.contains(id)
                                    || resolved.owned_struct_bindings.contains(id))),
                }
        }
        StatementKind::Assign { target, operator, value } => {
            match &target.kind {
                ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name)) if name != "self" => {
                    resolved.scalar_writes.get(&target.range.start).is_some_and(|&id| {
                        if resolved.array_definitions.contains(&id)
                            || resolved.token_cursor_definitions.contains(&id)
                        {
                            return false;
                        }
                        // Reassigning an enum releases the old payload first;
                        // the value is a constructor or another owned enum.
                        if resolved.enum_bindings.contains(&id) {
                            return resolved.enum_constructors.contains(&value.range.start)
                                || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(other))
                                    if other != "self" && resolved.scalar_reads.get(&value.range.start)
                                        .is_some_and(|other_id| resolved.enum_bindings.contains(other_id)))
                                // A call returning an enum (`e = swap(e)`): the
                                // callee's signature answers, and the lowering
                                // reports the same-statement move.
                                || scalar_body_expression(value, resolved, supports_call, supports_field);
                        }
                        // Re-initializing an owned vector (after a move) takes
                        // a fresh vector: a literal or a call returning one.
                        if resolved.owned_vectors.contains(&id) {
                            return matches!(value.kind, ae_syntax::expression::ExpressionKind::Vector(_))
                                || scalar_body_expression(value, resolved, supports_call, supports_field);
                        }
                        if resolved.vector_definitions.contains(&id) {
                            return false;
                        }
                        if !resolved.owned_text_bindings.contains(&id) {
                            return scalar_body_expression(value, resolved, supports_call, supports_field);
                        }
                        // Reassigning an owned string: a fresh literal or
                        // another owned string moves in, and the lowering
                        // releases the old value (the Assign codegen's flag).
                        matches!(value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Text(_)))
                            || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(other))
                                if other != "self" && resolved.scalar_reads.get(&value.range.start)
                                    .is_some_and(|other_id| other_id != &id
                                        && (resolved.owned_text_bindings.contains(other_id)
                                            || resolved.owned_string_bindings.contains(other_id))))
                            // A fresh string from a value builtin: the
                            // assignment releases the old value and takes the
                            // new one.
                            || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Call { callee, .. }
                                if matches!(&callee.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                                    if string_result_builtin(name)))
                            // A concat always builds a fresh string, so
                            // `s = a + b` releases the old value and takes
                            // the new one.
                            || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Binary { operator: Kind::Plus, .. })
                            // Any other bare name: the lowering's assignment
                            // type check answers (a string view, a scalar, an
                            // aggregate), positioned at the target name like
                            // the typed route.
                            || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(_)))
                    })
                }
                // `a[i] = v` stores through a resolved array binding. Only
                // the plain operator: a compound store stays on the typed
                // route, which rejects it for arrays.
                ae_syntax::expression::ExpressionKind::Index { base, index }
                    if *operator == Kind::Equal =>
                {
                    matches!(base.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(_)))
                        && resolved.scalar_writes.get(&base.range.start)
                            .or_else(|| resolved.scalar_reads.get(&base.range.start))
                            .is_some_and(|id| resolved.array_definitions.contains(id)
                                || resolved.map_definitions.contains(id)
                                // A vector store (`v[i] = x`): the lowering
                                // checks mutability, the element type, and the
                                // string move for a view/value store.
                                || resolved.vector_definitions.contains(id))
                        // A map key is a string value or view; array indices
                        // are scalars.
                        && (scalar_body_expression(index, resolved, supports_call, supports_field)
                            || matches!(index.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Text(_)))
                            || resolved.struct_field_views.contains(&index.range.start))
                        && (scalar_body_expression(value, resolved, supports_call, supports_field)
                            || matches!(value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Text(_)))
                            || resolved.struct_field_views.contains(&value.range.start)
                            || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                                if name != "self" && resolved.scalar_reads.get(&value.range.start)
                                    .is_some_and(|id| resolved.owned_text_bindings.contains(id)
                                        || resolved.owned_string_bindings.contains(id))))
                }
                ae_syntax::expression::ExpressionKind::Field { base, .. }
                    if *operator == Kind::Equal && matches!(&base.kind,
                        ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(_))) =>
                {
                    supports_field(target, ae_resolve::Role::Write)
                        && scalar_body_expression(value, resolved, supports_call, supports_field)
                }
                _ => false,
            }
        }
        StatementKind::Return(value) => {
            // A returned closure literal has the typed route's own diagnostic
            // (a closure has no storage); every other value keeps the shaped
            // gates.
            matches!(&value.kind, ae_syntax::expression::ExpressionKind::Closure { .. })
                || scalar_body_expression(value, resolved, supports_call, supports_field)
                || resolved.enum_constructors.contains(&value.range.start)
                || receiver_value(value).is_some_and(|atom| supports_field(atom, ae_resolve::Role::Read))
                // `return binding;` moves an owned struct out of the body;
                // the lowering marks the slot moved, so the drops pass trims
                // its Frees instead of releasing a value the caller owns.
                || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                    if name != "self" && resolved.scalar_reads.get(&value.range.start)
                        .is_some_and(|id| resolved.owned_struct_bindings.contains(id)
                            || resolved.owned_vectors.contains(id)
                            // `return owned_string;` moves the string out the
                            // same way.
                            || resolved.owned_text_bindings.contains(id)
                            || resolved.owned_string_bindings.contains(id)
                            // `return counts;` for a map the body owns.
                            || resolved.map_definitions.contains(id)
                            // `return owned_enum;` moves the payload out.
                            || resolved.enum_bindings.contains(id)))
        }
        // A literal print is the typed route's `Printed::Text` (no expression
        // to lower); a string field prints as its view; everything else must
        // resolve as a printable scalar.
        StatementKind::Print(value) => {
            // Any known local reaches the print arm's own type check
            // (`cannot print a value of type ...`); the arm admits the
            // printable shapes and rejects the rest exactly as the typed route.
            matches!(&value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                if name != "self" && resolved.scalar_reads.contains_key(&value.range.start))
                || matches!(value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Text(_)))
                || (matches!(value.kind, ae_syntax::expression::ExpressionKind::Field { .. })
                    && resolved.struct_field_views.contains(&value.range.start))
                // Printing an owned string reads a view of it; nothing moves.
                || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                    if name != "self" && resolved.scalar_reads.get(&value.range.start)
                        .is_some_and(|id| resolved.owned_text_bindings.contains(id)
                            || resolved.owned_string_bindings.contains(id)))
                || scalar_body_expression(value, resolved, supports_call, supports_field)
        }
        StatementKind::Assert(value) => {
            scalar_body_expression(value, resolved, supports_call, supports_field)
        }
        StatementKind::PointerStore { pointer_range, value, .. } => {
            resolved.scalar_writes.contains_key(&pointer_range.start)
                && scalar_body_expression(value, resolved, supports_call, supports_field)
        }
        StatementKind::If { condition, then_body, else_body } => {
            scalar_body_expression(condition, resolved, supports_call, supports_field)
                && then_body.iter().all(|statement| scalar_body_statement(statement, resolved, supports_call, supports_field))
                && match else_body {
                    Some(ElseBranch::Block(body)) => body.iter().all(|statement| scalar_body_statement(statement, resolved, supports_call, supports_field)),
                    Some(ElseBranch::If(branch)) => scalar_body_statement(branch, resolved, supports_call, supports_field),
                    None => true,
                }
        }
        StatementKind::While { condition, body } => {
            scalar_body_expression(condition, resolved, supports_call, supports_field)
                && body.iter().all(|statement| scalar_body_statement(statement, resolved, supports_call, supports_field))
        }
        StatementKind::ForRange { binding_range, start, end, body, .. } => {
            resolved.scalar_definitions.contains_key(&binding_range.start)
                && scalar_body_expression(start, resolved, supports_call, supports_field)
                && scalar_body_expression(end, resolved, supports_call, supports_field)
                && body.iter().all(|statement| scalar_body_statement(statement, resolved, supports_call, supports_field))
        }
        StatementKind::ForIn { binding_range, iterable, body, .. } => {
            resolved.scalar_definitions.contains_key(&binding_range.start)
                && matches!(iterable.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(_)))
                && resolved.scalar_reads.get(&iterable.range.start)
                    .is_some_and(|id| resolved.vector_definitions.contains(id) || resolved.token_cursor_definitions.contains(id))
                && body.iter().all(|statement| scalar_body_statement(statement, resolved, supports_call, supports_field))
        }
        StatementKind::Match { subject, arms } => {
            matches!(subject.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(_)))
                && (resolved.scalar_reads.get(&subject.range.start)
                        .is_some_and(|id| resolved.enum_bindings.contains(id)
                            // A subject bound from a generic or method call:
                            // its concrete enum type exists only at lowering,
                            // which validates the match.
                            || resolved.generic_call_bindings.contains(id))
                    // `match self` in an impl whose target is an enum: the
                    // carrier answers by the impl owner, not by a spelling.
                    || supports_field(subject, ae_resolve::Role::Borrow))
                && arms.iter().all(|arm| arm.body.iter().all(|statement| {
                    scalar_body_statement(statement, resolved, supports_call, supports_field)
                }))
        }
        StatementKind::Unsafe(body) => body.iter().all(|statement| scalar_body_statement(statement, resolved, supports_call, supports_field)),
        StatementKind::Break | StatementKind::Continue => true,
        StatementKind::Expr { .. } => exit_status_operand(statement)
            .is_some_and(|status| scalar_body_expression(status, resolved, supports_call, supports_field))
            || propagation_call(statement).is_some()
            || bare_call_statement(statement).is_some()
            || is_task_yield(statement)
            || handle_statement_builtin(statement).is_some()
            || vector_statement_builtin(statement, resolved)
                .is_some_and(|(builtin, value)| matches!(builtin, "pop" | "sort" | "reverse")
                    || scalar_body_expression(value, resolved, supports_call, supports_field)
                    || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Text(_)))
                    || resolved.struct_field_views.contains(&value.range.start)
                    || matches!(&value.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                        if name != "self" && resolved.scalar_reads.get(&value.range.start)
                            .is_some_and(|id| resolved.owned_text_bindings.contains(id)
                                || resolved.owned_string_bindings.contains(id)))),
    }
}

/// A `push`/`pop`/`sort`/`reverse` statement over an owned vector binding:
/// (the builtin's name, the pushed value expression when it has one). Shape
/// only — the lowering enforces mutability, element types and borrow rules.
fn vector_statement_builtin<'a>(statement: &'a ae_syntax::statement::Statement, resolved: &ResolvedExpressions) -> Option<(&'static str, &'a ae_syntax::expression::Expression)> {
    use ae_syntax::expression::ExpressionKind;
    use ae_syntax::statement::StatementKind;
    let StatementKind::Expr { value, propagates: false } = &statement.kind else { return None };
    let ExpressionKind::Call { callee, arguments } = &value.kind else { return None };
    let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind else { return None };
    let builtin = match name.as_str() {
        "push" => "push", "pop" => "pop", "sort" => "sort", "reverse" => "reverse",
        _ => return None,
    };
    let Some(ExpressionKind::Atom(Kind::Ident(_))) = arguments.first().map(|argument| &argument.kind) else { return None };
    let def = resolved.scalar_reads.get(&arguments[0].range.start)?;
    if !resolved.owned_vectors.contains(def) { return None }
    match (builtin, arguments.len()) {
        ("push", 2) => Some((builtin, &arguments[1])),
        ("pop" | "sort" | "reverse", 1) => Some((builtin, &arguments[0])),
        _ => None,
    }
}

/// A handle builtin written as a statement (`shared_set(m, v);`,
/// `chan_close(ch);`, `cell_store(h, v);`): these lower directly to their
/// statements, so the expression form of the same name is never consulted.
fn handle_statement_builtin(statement: &ae_syntax::statement::Statement) -> Option<(&'static str, &ae_syntax::expression::Expression)> {
    use ae_syntax::expression::ExpressionKind;
    use ae_syntax::statement::StatementKind;
    let StatementKind::Expr { value, propagates: false } = &statement.kind else { return None };
    let ExpressionKind::Call { callee, .. } = &value.kind else { return None };
    let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind else { return None };
    let builtin = match name.as_str() {
        "shared_set" => "shared_set", "chan_close" => "chan_close", "cell_store" => "cell_store",
        "task_spawn" => "task_spawn", "scope_spawn" => "scope_spawn", "pool_spawn" => "pool_spawn",
        _ => return None,
    };
    Some((builtin, value))
}

/// `task_yield();` — a suspension point with no operands. Its statement exists
/// in the lowered form already (`Statement::TaskYield`).
fn is_task_yield(statement: &ae_syntax::statement::Statement) -> bool {
    use ae_syntax::expression::ExpressionKind;
    use ae_syntax::statement::StatementKind;
    let StatementKind::Expr { value, propagates: false } = &statement.kind else { return false };
    matches!(&value.kind, ExpressionKind::Call { callee, arguments }
        if arguments.is_empty()
            && matches!(&callee.kind, ExpressionKind::Atom(Kind::Ident(name)) if name == "task_yield"))
}

impl<'a> Parser<'a> {
    pub(super) fn current(&self) -> &Kind { &self.tokens[self.pos].kind }
    pub(super) fn fail(&self, message: &str) -> ae_span::Diagnostic {
        let token = &self.tokens[self.pos];
        ae_syntax::diagnostic_range(&self.file, self.source, token.offset..token.end, message)
    }
    // The display form of one instantiation in the recursion trace (M1.3.06):
    // names only, rendered never compared — cycles compare the interned key.
    pub(super) fn instantiation_display(&self, name: &str, args: &[Type]) -> String {
        if args.is_empty() { return name.to_owned(); }
        format!("{}[{}]", name, args.iter().map(|t| t.name(self.ctx)).collect::<Vec<_>>().join(", "))
    }
    pub(super) fn instantiation_trace(&self, current: &str) -> String {
        let mut chain: Vec<&str> = self.generics.instantiation_trace.iter().map(|(_, _, display)| display.as_str()).collect();
        chain.push(current);
        chain.join(" -> ")
    }
    pub(super) fn error_at(&self, offset: usize, message: &str) -> ae_span::Diagnostic {
        let end = self.tokens.binary_search_by_key(&offset, |token| token.offset)
            .ok().map_or(offset, |index| self.tokens[index].end);
        ae_syntax::diagnostic_range(&self.file, self.source, offset..end, message)
    }
    pub(super) fn binding_label(&self, diagnostic: ae_span::Diagnostic, id: usize) -> ae_span::Diagnostic {
        let Some(range) = self.binding_ranges.get(&id) else { return diagnostic };
        let source = ae_span::SourceFile { name: Some(&self.file), text: self.source };
        match source.span(range.clone()) {
            Some(span) => diagnostic.with_secondary(span, "binding declared here"),
            None => diagnostic,
        }
    }
    pub(super) fn immutable_binding_error(&self, diagnostic: ae_span::Diagnostic, id: usize) -> ae_span::Diagnostic {
        let diagnostic = self.binding_label(diagnostic, id);
        if diagnostic.secondary.is_empty() { diagnostic } else {
            diagnostic.with_suggestion("If mutation is intended, declare the binding with `var`.")
        }
    }
    pub(super) fn duplicate_binding_error(&self, name: &str) -> ae_span::Diagnostic {
        self.duplicate_binding_error_at(self.tokens[self.pos].offset, name)
    }

    // The tree-driven callers know the offending token's range already, so
    // they name it directly instead of relying on the moving cursor.
    pub(super) fn duplicate_binding_error_at(&self, at: usize, name: &str) -> ae_span::Diagnostic {
        let diagnostic = self.error_at(at, &format!("duplicate variable '{name}'")).with_code("E0104");
        match self.names.get(name) {
            Some(&(id, _, _)) => self.binding_label(diagnostic, id),
            None => diagnostic,
        }
    }
    pub(super) fn take(&mut self, expected: Kind) -> Result<(), ae_span::Diagnostic> {
        ae_syntax::expect_token(&self.file, self.source, &self.tokens[self.pos], expected)?;
        self.pos += 1; Ok(())
    }
    pub(super) fn identifier(&mut self) -> Result<String, ae_span::Diagnostic> {
        let name = ae_syntax::parse_identifier(&self.file, self.source, &self.tokens[self.pos])?;
        self.pos += 1;
        Ok(name.to_owned())
    }
    // Whether an `unsafe` block's body contains any operation the block
    // exists to legalize: a raw dereference, a pointer store, or a call to
    // a declared extern / `unsafe fn`. Nested `unsafe` blocks check their
    // own bodies — the outer block's warning is about ITS body. W0702
    // (spec/unsafe.md U4): a block with none is silent decoration.
    fn block_has_unchecked_operation(&self, statements: &[Statement]) -> bool {
        for statement in statements {
            if self.statement_has_unchecked_operation(statement) { return true; }
        }
        false
    }

    fn statement_has_unchecked_operation(&self, statement: &Statement) -> bool {
        match statement {
            Statement::Unsafe(body) => self.block_has_unchecked_operation(body),
            Statement::PtrStore(..) => true,
            Statement::If(cond, yes, no) => {
                self.expr_has_unchecked_operation(cond)
                    || self.block_has_unchecked_operation(yes)
                    || self.block_has_unchecked_operation(no)
            }
            Statement::While(cond, body) => {
                self.expr_has_unchecked_operation(cond) || self.block_has_unchecked_operation(body)
            }
            Statement::Return(expr, _) => self.expr_has_unchecked_operation(expr),
            Statement::Exit(expr, _) => self.expr_has_unchecked_operation(expr),
            Statement::Print(printed) => match printed {
                Printed::Expr(e) => self.expr_has_unchecked_operation(e),
                Printed::Text(_) => false,
            },
            Statement::Assert(cond, ..) => self.expr_has_unchecked_operation(cond),
            Statement::Assign(_, expr, _) => self.expr_has_unchecked_operation(expr),
            Statement::Declare(_, expr) => self.expr_has_unchecked_operation(expr),
            Statement::For(_, _, body, ..) | Statement::ForTokens(_, _, body, ..) => {
                self.block_has_unchecked_operation(body)
            }
            Statement::Match(_, _, arms) => arms.iter().any(|arm| self.block_has_unchecked_operation(&arm.body)),
            _ => false,
        }
    }

    fn expr_has_unchecked_operation(&self, expr: &Expr) -> bool {
        match expr {
            Expr::DerefPtr(..) => true,
            // Pointer arithmetic is the same class of unchecked operation
            // (it gates on unsafe_depth exactly like the deref).
            Expr::PtrOffset(..) | Expr::PtrDropInPlace(..) | Expr::PtrWrite(..) => true,
            Expr::Call(id, args, _) => {
                // A call to a declared `extern "C" fn` or an `unsafe fn` is
                // exactly what the block legalizes (the same test E0701
                // applies at the call site). Both registries fill during the
                // signature pass, which completes before any body parses, so
                // the id-keyed lookup is order-safe here — including a call
                // that textually precedes its declaration. `Expr::FnCall` is
                // a call through a `fn(T) -> R` pointer whose id comes from
                // the closure-signature interner, a different id space, and
                // closures are never unsafe — no registry check there.
                if self.ctx.is_unsafe_fn(*id) || self.ctx.is_extern_fn(*id) {
                    return true;
                }
                args.iter().any(|arg| match arg {
                    Argument::Value(e) | Argument::View(e) => self.expr_has_unchecked_operation(e),
                    _ => false,
                })
            }
            Expr::FnCall(_, args, ..) => {
                args.iter().any(|arg| match arg {
                    Argument::Value(e) | Argument::View(e) => self.expr_has_unchecked_operation(e),
                    _ => false,
                })
            }
            Expr::Binary(a, _, b, ..) => self.expr_has_unchecked_operation(a) || self.expr_has_unchecked_operation(b),
            Expr::Field(base, ..) => self.expr_has_unchecked_operation(base),
            Expr::Variable(..) | Expr::Number(..) | Expr::FloatLit(..) | Expr::BoolLit(..) | Expr::StrLit(..) => false,
            _ => false,
        }
    }

    pub(super) fn position(&self) -> (usize, usize) {
        let token = &self.tokens[self.pos];
        (token.line, token.column)
    }
    // Prelude result types are always registered, so naming one cannot fail.
    pub(super) fn prelude_type(&self, name: &str) -> Type {
        match self.enum_names.get(name) {
            Some(&id) => Type::Enum(self.enums[id].name, id),
            None => unreachable!("prelude enum {name} is registered for every program"),
        }
    }
    #[inline(never)]
    fn concurrency_stmt(&mut self, name: &str) -> Result<Statement, ae_span::Diagnostic> {
        if matches!(name, "task_spawn" | "scope_spawn" | "pool_spawn") {
            return self.spawn_statement(name);
        }
        self.pos += 2;
        if name == "task_yield" {
            self.take(Kind::RParen)?;
            self.take(Kind::Semi)?;
            return Ok(Statement::TaskYield);
        }
        if name == "chan_close" {
            let (handle, handle_ty) = match self.lower_shared_value(Some(Kind::RParen), false) {
                Some(pair) => pair,
                None => self.expression(0, false)?,
            };
            if handle_ty != Type::Chan {
                return Err(self.fail("chan_close expects a chan handle"));
            }
            self.take(Kind::RParen)?;
            self.take(Kind::Semi)?;
            return Ok(Statement::ChanClose(Box::new(handle)));
        }
        let (handle, handle_ty) = match self.lower_shared_value(Some(Kind::Comma), false) {
            Some(pair) => pair,
            None => self.expression(0, false)?,
        };
        let sid = match handle_ty {
            Type::Shared(sid) => sid,
            _ => return Err(self.fail("shared_set expects a shared handle")),
        };
        self.take(Kind::Comma)?;
        let (value, value_ty) = self.expression(0, false)?;
        let expected_ty = self.ctx.shared_types_get(sid);
        if value_ty != expected_ty {
            return Err(self.fail(&format!("shared_set expects {}, found {}", expected_ty.name(self.ctx), value_ty.name(self.ctx))));
        }
        self.take(Kind::RParen)?;
        self.take(Kind::Semi)?;
        if !self.live_owned().iter().any(|(_, ty)| *ty == Type::Guard) {
            return Err(self.fail("shared_set requires a live mutex guard in scope; call mutex_lock(...) first"));
        }
        Ok(Statement::SharedSet(Box::new(handle), Box::new(value), sid))
    }

    /// The spawn statement forms (`task_spawn(exec, || ..)`,
    /// `scope_spawn(sc, || ..)`, `pool_spawn(pool, || ..)`): the handle first,
    /// then the callable whose captures move into the task. Shared by the
    /// typed statement parser and the resolved route; the cursor sits on the
    /// name.
    pub(super) fn spawn_statement(&mut self, name: &str) -> Result<Statement, ae_span::Diagnostic> {
        let (line, col) = self.position();
        let at = self.tokens[self.pos].offset;
        self.pos += 2;
        let (ex, ex_ty) = match self.lower_shared_value(Some(Kind::Comma), false) {
            Some(pair) => pair,
            None => self.expression(0, false)?,
        };
        let expected_ty = match name {
            "task_spawn" => Type::Exec,
            "scope_spawn" => Type::TaskScope,
            _ => Type::Pool,
        };
        if ex_ty != expected_ty {
            let art = if name == "task_spawn" { "an" } else { "a" };
            return Err(self.fail(&format!("{name} expects {art} {} handle as its first argument", expected_ty.name(self.ctx))));
        }
        self.take(Kind::Comma)?;
        let (callable, callable_ty) = self.spawn_callable()?;
        self.take(Kind::RParen)?;
        self.take(Kind::Semi)?;
        let mut slots = Vec::new();
        let cid = match (&callable, callable_ty) {
            (Expr::ClosureLit(cid), Type::Closure(_)) | (Expr::Variable(_, Type::Closure(cid), ..), _) => {
                let def = self.generics.closures[*cid].clone();
                if !def.params.is_empty() {
                    return Err(self.fail("the spawned task closure must take no parameters"));
                }
                if def.ret != Type::Int {
                    return Err(self.fail("the spawned task closure must return i64"));
                }
                if !def.borrow_captures.is_empty() {
                    return Err(self.fail("task_spawn moves its captures into the task; borrow captures are not allowed"));
                }
                for (outer_id, ty) in &def.value_captures {
                    if !matches!(ty, Type::Int | Type::Bool | Type::F64 | Type::String | Type::Vec | Type::StrVec | Type::VecF | Type::Map | Type::MapF | Type::Struct(_) | Type::Enum(..) | Type::Atomic | Type::Shared(_) | Type::Mutex | Type::Chan | Type::Exec | Type::Pool) {
                        return Err(self.fail("task_spawn moves its captures into the task; this capture's storage cannot cross"));
                    }
                    if self.moved.contains(outer_id) {
                        return Err(moved_value_hint(self.error_at(at, "cannot spawn a task that captures a moved variable").with_code("E0102")));
                    }
                    if self.is_borrowed(*outer_id) {
                        return Err(self.fail("cannot spawn a task while one of the closure's captured variables is borrowed"));
                    }
                    if ty.owned() { self.moved.insert(*outer_id); }
                    slots.push(Expr::Variable(*outer_id, *ty, 0, 0));
                }
                self.ctx.record_spawned(crate::frontend::closures::SpawnedThread::Closure {
                    fn_id: self.ctx.closure_fn_id(*cid),
                    captures: def.value_captures.iter().map(|(_, ty)| *ty).collect(),
                    borrows: Vec::new(),
                    ret: def.ret,
                });
                *cid
            }
            _ => return Err(self.fail("task_spawn expects a zero-parameter closure returning i64")),
        };
        Ok(match name {
            "task_spawn" => Statement::TaskSpawn(Box::new(ex), slots, Box::new(callable), cid, line, col),
            "scope_spawn" => Statement::ScopeSpawn(Box::new(ex), slots, Box::new(callable), cid, line, col),
            _ => Statement::PoolSpawn(Box::new(ex), slots, Box::new(callable), cid, line, col),
        })
    }

    pub(super) fn block(&mut self) -> Result<Vec<Statement>, ae_span::Diagnostic> {
        if self.block_depth >= 64 { return Err(self.fail("block nesting limit exceeded (64)")); }
        self.block_depth += 1;
        self.scopes.push(Vec::new());
        let result = self.block_inner();
        let scope = self.scopes.pop().unwrap();
        self.block_depth -= 1;
        let mut result = result?;
        // Owned values declared in this block are released at its end.
        // Releases are idempotent (slots are zeroed), so overlapping paths
        // stay safe.
        let owned: Vec<(usize, Type)> = scope.iter().filter(|(_, ty)| ty.owned()).copied().collect();
        if !owned.is_empty() { result.push(Statement::Frees(owned)); }
        // Moved markers for names that left scope are no longer meaningful,
        // and borrows recorded in this scope end with it.
        let live: std::collections::HashSet<usize> = self.names.values().map(|&(id, _, _)| id).collect();
        self.moved.retain(|id| live.contains(id));
        let depth_now = self.scopes.len();
        self.borrows.retain(|&(_, depth, _)| depth <= depth_now);
        self.call_borrows.retain(|&(_, depth)| depth <= depth_now);
        self.mut_borrows.retain(|&(_, depth)| depth <= depth_now);
        Ok(result)
    }
    // Is any borrow of this slot live? Two registers feed the answer:
    // `borrows` holds the persistent borrows (views, token cursors, the
    // containers of for-in loops, match subjects) that last until their
    // scope or loop ends, and `call_borrows` holds the borrows a call
    // argument created, which die when the statement that made the call
    // ends -- the callee has returned by then and nothing retains the
    // pointer (ADR-0184).
    // Whether every borrow on `id` is a view-kind borrow: only then can a
    // write conflict be deferred to the NLL adjudication (the view dies
    // after its last use; container borrows run to the scope's end).
    pub(super) fn all_borrows_are_views(&self, id: usize) -> bool {
        self.is_borrowed(id) && !self.borrows.iter().any(|&(bid, _, kind)| bid == id && kind == BorrowKind::Container)
    }

    // Whether the only active borrows on `id` are persistent view borrows:
    // the deferral precondition for a mut lending call (M2.5). Call-argument
    // borrows and earlier mut lends in the same statement are statement-
    // scoped exclusions the on-sight rule still guards (`c.doubled() +
    // c.inc()` stays rejected), and container borrows keep their scope rule.
    pub(super) fn only_view_borrows_active(&self, id: usize) -> bool {
        !self.call_borrows.iter().any(|&(bid, _)| bid == id)
            && !self.mut_borrows.iter().any(|&(bid, _)| bid == id)
            && self.borrows.iter().any(|&(bid, _, kind)| bid == id && kind == BorrowKind::View)
            && !self.borrows.iter().any(|&(bid, _, kind)| bid == id && kind == BorrowKind::Container)
    }

    pub(super) fn is_borrowed(&self, id: usize) -> bool {
        self.borrows.iter().any(|&(bid, _, _)| bid == id)
            || self.call_borrows.iter().any(|&(bid, _)| bid == id)
            || self.mut_borrows.iter().any(|&(bid, _)| bid == id)
    }

    // Adjudicate the write conflicts this function deferred (M2.5 L1): the
    // graph decides whether a view of the written slot may still be read.
    // A surviving conflict is the positioned rejection; everything else was
    // a write after the view's last use, which now parses.
    // Whether the graph's release scan will see a call through this
    // signature id: the scan reads the four tables every `Expr::Call` id
    // can come from. A deferral whose callee is outside them would be an
    // unsound allowance, so the deferral sites reject on sight instead.
    pub(super) fn signature_in_release_tables(&self, callee: Option<usize>) -> bool {
        let Some(id) = callee else { return false; };
        self.functions.values().any(|s| s.id == id)
            || self.generics.inherent.values().any(|m| m.signature.id == id)
            || self.generics.fn_done.values().any(|(iid, _, _)| *iid == id)
            || self.generics.generic_impl_done.values().any(|(iid, _, _)| *iid == id)
    }

    /// The inherent (non-generic) method a receiver type names, if the call
    /// is one the resolved route can lower: a borrowed receiver, a return
    /// the route admits, and value parameters only. The call gate and the
    /// lowering share this, so the route admits exactly what it can emit.
    /// Generic-instance methods need `instantiate_impl_method` (a `&mut self`
    /// operation) and stay on the typed route.
    fn resolve_inherent_method(&self, method_name: &str, base_ty: Type) -> Option<(Signature, Option<Type>, Type)> {
        let type_name = match base_ty {
            Type::Struct(sid) | Type::StructMut(sid) | Type::StructRef(sid) => self.structs.get(sid)?.name.clone(),
            Type::Enum(name, _) => name.to_owned(),
            _ => return None,
        };
        let definition = self.generics.inherent.get(&format!("{type_name}#{method_name}"))?;
        // Any receiver kind is admitted: borrowed receivers keep their
        // bookkeeping and a by-value receiver moves (enum receivers are by
        // value only). A receiver-less associated function is not a method
        // call through a value.
        let receiver = definition.receiver;
        receiver?;
        let signature = definition.signature.clone();
        let ret = signature.ret;
        let ret_ok = scalar_type(ret) || matches!(ret, Type::String | Type::Enum(..))
            || owned_plain_struct(ret, self.structs) || owned_field_struct(ret, self.structs);
        // `params[0]` is the receiver; the value parameters follow it.
        let params_ok = signature.params.iter().skip(1).copied()
            .all(|ty| scalar_type(ty) || matches!(ty, Type::Struct(_) | Type::String));
        if !ret_ok || !params_ok {
            return None;
        }
        Some((signature, receiver, base_ty))
    }

    /// Whether the inherent table declares `method` on this receiver type at
    /// all, receiver-less entries included. The call gate uses it to admit a
    /// method the route's receiver rules refuse (an associated function called
    /// through a value); the lowering then reports the typed route's own
    /// diagnostic instead of a shape rejection.
    fn inherent_entry_exists(&self, method: &str, base_ty: Type) -> bool {
        let type_name = match base_ty {
            Type::Struct(sid) | Type::StructMut(sid) | Type::StructRef(sid) => self.structs.get(sid).map(|defn| defn.name.clone()),
            Type::Enum(name, _) => Some(name.to_owned()),
            _ => None,
        };
        type_name.is_some_and(|name| self.generics.inherent.contains_key(&format!("{name}#{method}")))
    }

    /// The type a method-call chain computes (`start.shifted(1, 2).shifted(0, -6)`).
    /// The precompute walks the chain left to right: the root is a parameter,
    /// `self`, or a binding the scan already typed, and each link asks the
    /// inherent method's declared return type. The pointer surface is answered
    /// the same way (`let v = raw as *mut f64`, `let p = buf.offset(1)`), and a
    /// direct call answers by the callee's declared return (`extern "C" fn
    /// ae_std_alloc(..) -> *mut i64`). Only shapes the resolved route admits
    /// answer (arity included); anything else — a generic instance or a
    /// receiver the scan cannot name — stays typed.
    fn chain_call_type(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        local_structs: &HashMap<String, Type>,
    ) -> Option<Type> {
        use ae_syntax::expression::ExpressionKind;
        match &expression.kind {
            ExpressionKind::Atom(Kind::Ident(name)) => self
                .names
                .get(name)
                .map(|&(_, _, ty)| ty)
                .or_else(|| local_structs.get(name).copied()),
            ExpressionKind::Group(inner) => self.chain_call_type(inner, local_structs),
            ExpressionKind::PointerCast { base, mutable, pointee } => {
                // The address stays; the pointee spelling names the new view.
                if !matches!(self.chain_call_type(base, local_structs)?, Type::Ptr(_) | Type::PtrMut(_)) {
                    return None;
                }
                let name = self.source.get(pointee.clone())?;
                let pointee = self.subst.get(name).copied().or_else(|| super::types::scalar_type(name))?;
                let id = self.ctx.intern_pointee(pointee);
                Some(if *mutable { Type::PtrMut(id) } else { Type::Ptr(id) })
            }
            ExpressionKind::Call { callee, arguments } => match &callee.kind {
                ExpressionKind::Field { base, name } => {
                    let ty = self.chain_call_type(base, local_structs)?;
                    if let Type::Ptr(id) | Type::PtrMut(id) = ty {
                        // A pointer method: `offset`/`byte_offset` keep the
                        // pointer type, `deref` answers with the pointee.
                        return match (name.as_str(), arguments.len()) {
                            ("offset" | "byte_offset", 1) => Some(ty),
                            ("deref", 0) => Some(self.ctx.ptr_pointee(id)),
                            _ => None,
                        };
                    }
                    let (signature, _, _) = self.resolve_inherent_method(name, ty)?;
                    (arguments.len() + 1 == signature.params.len()).then_some(signature.ret)
                }
                ExpressionKind::Atom(Kind::Ident(name)) if !RESERVED.contains(&name.as_str()) => {
                    let signature = self.functions.get(name)?;
                    (arguments.len() == signature.params.len()).then_some(signature.ret)
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Whether any declaration answers `name` as a callable — a function
    /// (file-local, imported, or an `extern`/`unsafe fn`), a generic function,
    /// a trait method of a constrained parameter, or a reserved builtin.
    /// Visibility and signature shape stay the call gate's business; this
    /// answers identity alone, so an unknown call can reach the lowering's
    /// `resolve_trait_call` (the typed route's E0207).
    fn call_has_signature(&self, name: &str) -> bool {
        if RESERVED.contains(&name) { return true; }
        // Intrinsics the typed expression parser dispatches by name and live
        // in neither table: a call to one is known.
        if typed_intrinsic(name) { return true; }
        if self.functions.contains_key(name) || self.generics.fn_names.contains_key(name) { return true; }
        self.trait_bounds.values().flatten().any(|&tid| {
            self.generics.traits.get(tid).is_some_and(|defn| defn.methods.iter().any(|method| method.name == name))
        })
    }

    pub(super) fn adjudicate_deferred_conflicts(&mut self, function: &Function) -> Result<(), ae_span::Diagnostic> {
        let mut_sigs = borrowscope::mut_borrow_positions(self.functions, self.generics);
        let outcome = borrowscope::adjudicate_deferred(function, &self.deferred_write_conflicts, &mut_sigs, self.ctx);
        match outcome {
            Ok(()) => { self.deferred_write_conflicts.clear(); }
            Err(message) => {
                let (_, offset, _, _, _) = self.deferred_write_conflicts[0];
                return Err(self.error_at(offset, &message));
            }
        }
        // ADR-0215: the deferred spawn-escape proofs run on the same graph.
        self.adjudicate_borrow_spawns(function)?;
        Ok(())
    }
    // ADR-0215: the deferred spawn-escape proofs (closure id, the borrowed
    // variable ids, the spawn's byte offset).
    pub(super) fn adjudicate_borrow_spawns(&mut self, function: &Function) -> Result<(), ae_span::Diagnostic> {
        let outcome = borrowscope::adjudicate_borrow_spawns(function, &self.deferred_borrow_spawns, self.ctx);
        match outcome {
            Ok(()) => { self.deferred_borrow_spawns.clear(); Ok(()) }
            Err(message) => {
                let (_, _, offset) = self.deferred_borrow_spawns[0];
                Err(self.error_at(offset, &message))
            }
        }
    }
    // Views borrow their base: element reads and slice_views of variable
    // bases register the owner so it cannot be modified or moved while a
    // view can still be read.
    pub(super) fn record_borrow_bases(&mut self, e: &Expr) {
        match e {
            Expr::StrVecIndex(id, idx, _, _, _) => {
                if !self.is_borrowed(*id) {
                    self.borrows.push((*id, self.scopes.len(), BorrowKind::Container));
                }
                self.record_borrow_bases(idx);
            }
            Expr::SliceView(_, _, a, b, _, _) => { self.record_borrow_bases(a); self.record_borrow_bases(b); }
            Expr::StructLit(_, values) => { for v in values { self.record_borrow_bases(v); } }
            Expr::EnumLit(_, _, _, values) => { for v in values { self.record_borrow_bases(v); } }
            Expr::Binary(a, _, b, _, _, _) => { self.record_borrow_bases(a); self.record_borrow_bases(b); }
            Expr::Neg(a, _, _) | Expr::Not(a) | Expr::Len(a, _, _) => self.record_borrow_bases(a),
            Expr::Index(_, _, a, _, _) => self.record_borrow_bases(a),
            Expr::VecIndex(_, a, _, _, _, _) => self.record_borrow_bases(a),
            Expr::Call(_, args, _) => {
                for arg in args {
                    match arg {
                        Argument::Value(e) => self.record_borrow_bases(e),
                        Argument::View(e) => self.record_borrow_bases(e),
                        Argument::Borrow(_, _) | Argument::FnPtr(_) => {}
                    }
                }
            }
            Expr::Contains(a, b, _, _) | Expr::Split(a, b, _, _) | Expr::StrEq(a, b, _) | Expr::Find(a, b, _, _, _) => { self.record_borrow_bases(a); self.record_borrow_bases(b); }
            Expr::WriteFile(a, b, _, _) | Expr::Slice(_, a, b, _, _) => { self.record_borrow_bases(a); self.record_borrow_bases(b); }
            Expr::MapGet(_, key, _, _, _) | Expr::MapHas(_, key, _, _) | Expr::MapRemove(_, key, _, _) => self.record_borrow_bases(key),
            Expr::Join(_, sep, _, _) => self.record_borrow_bases(sep),
            Expr::Field(base, _, _) => self.record_borrow_bases(base),
            _ => {}
        }
    }
    pub(super) fn live_owned(&self) -> Vec<(usize, Type)> {
        // Parameters are owned by the callee but live outside the block
        // scopes; a `return` must release them exactly like a local unless
        // the body moved them out (the drops pass trims by `moved`). A slot
        // present in both lists (the receiver, which the entry scope already
        // holds) is released once.
        let mut seen = std::collections::HashSet::new();
        self.scopes.iter().flatten().copied()
            .chain(self.owned_params.iter().copied())
            .filter(|(id, ty)| ty.owned() && seen.insert(*id))
            .collect()
    }
    // A loop body runs again, so a value moved out of a variable declared
    // outside the loop must be re-initialized before the body ends; otherwise
    // the next iteration would read a moved-from handle. Without a fixed point
    // over the body (ADR-20) the move is rejected instead of guessed at.
    pub(super) fn close_loop_moves(&mut self, base: usize, before: std::collections::HashSet<usize>, offset: usize) -> Result<(), ae_span::Diagnostic> {
        let after = std::mem::replace(&mut self.moved, before.clone());
        let outer: Vec<usize> = self.scopes[..base.min(self.scopes.len())].iter().flatten().map(|&(id, _)| id).collect();
        for id in &after {
            // Values already moved before the loop are not this body's doing.
            if before.contains(id) || !outer.contains(id) { continue; }
            let name = self.names.iter().find(|(_, (vid, _, _))| vid == id).map(|(n, _)| n.clone()).unwrap_or_else(|| "?".to_owned());
            return Err(self.error_at(offset, &format!("cannot move '{name}' inside a loop body: the next iteration would read it after the move (re-initialize it before the body ends)")));
        }
        // Moves of loop-local values die with the body; nothing to merge.
        Ok(())
    }
    // break/continue leave the loop body: only owned values declared inside it die.
    pub(super) fn loop_local_owned(&self) -> Vec<(usize, Type)> {
        let base = *self.loop_scopes.last().unwrap_or(&0);
        self.scopes[base.min(self.scopes.len())..].iter().flatten().filter(|(_, ty)| ty.owned()).map(|&(id, ty)| (id, ty)).collect()
    }
    pub(super) fn expression_index_for(&self, file: &str, tokens: &Rc<Vec<Token>>) -> Rc<OnceCell<ResolvedExpressions>> {
        if self.file == file && Rc::ptr_eq(&self.tokens, tokens) {
            Rc::clone(&self.resolved_expressions)
        } else {
            Rc::default()
        }
    }

    // Planning aid for the P1.1 slices: with AE_ROUTE_REASONS=1 the first
    // statement that keeps a body off the route is printed with its source
    // line. A compound statement fails because one of the statements inside
    // it fails, so the walk descends into blocks and reports the innermost
    // failing statement — the shape to support next.
    fn report_route_reason(&self, label: &str, statement: &ae_syntax::statement::Statement, fails: &dyn Fn(&ae_syntax::statement::Statement) -> bool) {
        if std::env::var_os("AE_ROUTE_REASONS").is_none() {
            return;
        }
        use ae_syntax::statement::{ElseBranch, StatementKind};
        let mut current = statement;
        loop {
            let nested: Option<&ae_syntax::statement::Statement> = match &current.kind {
                StatementKind::If { then_body, else_body, .. } => then_body.iter().find(|s| fails(s)).or_else(|| match else_body {
                    Some(ElseBranch::Block(body)) => body.iter().find(|s| fails(s)),
                    Some(ElseBranch::If(branch)) => fails(branch).then_some(branch.as_ref()),
                    None => None,
                }),
                StatementKind::While { body, .. } | StatementKind::ForRange { body, .. } | StatementKind::ForIn { body, .. } => {
                    body.iter().find(|s| fails(s))
                }
                StatementKind::Match { arms, .. } => arms.iter().flat_map(|arm| arm.body.iter()).find(|s| fails(s)),
                StatementKind::Unsafe(body) => body.iter().find(|s| fails(s)),
                _ => None,
            };
            match nested {
                Some(next) => current = next,
                None => break,
            }
        }
        let text = self.source.get(current.range.clone()).unwrap_or("").lines().next().unwrap_or("").trim();
        let line = 1 + self.source[..current.range.start.min(self.source.len())].matches('\n').count();
        eprintln!("route reason {label} {}:{line}: {}", self.file, text.chars().take(80).collect::<String>());
    }

    pub(super) fn resolved_expressions(&self) -> &ResolvedExpressions {
        self.resolved_expressions.get_or_init(|| self.build_resolved_expressions())
    }

    /// Builds the module's resolved-expression index. Separate from the cell
    /// so a caller that also needs `&mut self` can pin the Rc locally and
    /// keep the reference off `self`.
    fn build_resolved_expressions(&self) -> ResolvedExpressions {
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
                    let kind = match &statement.kind {
                        StatementKind::Binding { name_range, mutable, .. } => Some(IndexedStatement::Binding {
                            name_start: name_range.start,
                            name_end: name_range.end,
                            mutable: *mutable,
                        }),
                        StatementKind::ForRange { binding_range, .. } => Some(IndexedStatement::For {
                            binding_start: binding_range.start,
                            binding_end: binding_range.end,
                            is_range: true,
                        }),
                        StatementKind::ForIn { binding_range, .. } => Some(IndexedStatement::For {
                            binding_start: binding_range.start,
                            binding_end: binding_range.end,
                            is_range: false,
                        }),
                        StatementKind::Print(argument) => Some(IndexedStatement::Print(argument.range.start)),
                        StatementKind::Assert(argument) => Some(IndexedStatement::Assert(argument.range.start)),
                        StatementKind::Break => Some(IndexedStatement::Break),
                        StatementKind::Continue => Some(IndexedStatement::Continue),
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        expressions.statements.insert(statement.range.start, (kind, statement.range.end));
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
                                if !matches!(ty, Type::Vec | Type::StrVec | Type::VecF) { continue }
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
                            // A plain struct literal types its binding from the
                            // target name: the resolver may leave the spelling
                            // open, and field reads/writes answer by this entry.
                            if let ExpressionKind::Struct { target, .. } = &initializer.kind
                                && let ExpressionKind::Atom(Kind::Ident(struct_name)) = &target.kind
                                    && let Some(&sid) = self.struct_names.get(struct_name) {
                                        expressions.struct_definition_names.entry(id).or_insert_with(|| struct_name.clone());
                                        let constructed = Type::Struct(sid);
                                        if owned_plain_struct(constructed, self.structs)
                                            || owned_field_struct(constructed, self.structs)
                                        {
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
                                    | ExpressionKind::Field { .. })
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
                                _ => match iterator_constructor(initializer) {
                                    Some(Type::Tokens) => { expressions.token_cursor_definitions.insert(id); }
                                    Some(Type::StrVec) => {
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
                                                    if matches!(signature.ret, Type::Map | Type::MapF) {
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
                                        ShapeArg::Value(field_ty) if scalar_type(field_ty) => {
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
                                        if defn.file == self.file || defn.public {
                                            let constructed = Type::Struct(sid);
                                            if defn.drop_fn.is_none() {
                                                if owned_plain_struct(constructed, self.structs) {
                                                    expressions.drop_free_constructions.insert(expression.range.start);
                                                } else if owned_field_struct(constructed, self.structs) {
                                                    expressions.owned_constructions.insert(expression.range.start);
                                                }
                                            } else if defn.fields.iter().all(|(_, ty)| {
                                                // A drop-carrying struct with plain
                                                // fields: the generated release calls
                                                // the user's `drop` exactly where the
                                                // typed route would. Owned fields
                                                // alongside `drop` stay typed (the
                                                // release order is the typed route's).
                                                scalar_type(*ty) || owned_plain_struct(*ty, self.structs)
                                            }) {
                                                expressions.drop_constructions.insert(expression.range.start);
                                            }
                                        }
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
                        if *operator == Kind::Equal
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
                                            } else if matches!(field_ty, Type::String) {
                                                // An owned field store releases the
                                                // old string and consumes the new
                                                // one (the FieldStore codegen).
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
                    !scalar_body_statement(statement, &expressions, &|_, _| true, &|_, _| false)
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

    pub(super) fn shared_specialization_arguments(&self, offset: usize) -> Option<std::sync::Arc<[std::ops::Range<usize>]>> {
        self.resolved_expressions().specialization_arguments.get(&offset).cloned()
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

    pub(super) fn shared_pointer_cast_qualifier(&self) -> Option<bool> {
        let offset = self.tokens.get(self.pos + 2)?.offset;
        self.resolved_expressions().pointer_cast_targets.get(&offset).copied()
    }

    pub(super) fn shared_closure_header(&self) -> Option<std::sync::Arc<ae_syntax::expression::Expression>> {
        let offset = self.tokens.get(self.pos)?.offset;
        self.resolved_expressions().closure_headers.get(&offset).cloned()
    }

    pub(super) fn lower_resolved_scalar_body(&mut self) -> Option<Result<Vec<Statement>, ae_span::Diagnostic>> {
        self.lower_resolved_body_with_captures(&[])
    }

    pub(super) fn lower_resolved_body_with_captures(
        &mut self,
        capture_starts: &[(usize, usize)],
    ) -> Option<Result<Vec<Statement>, ae_span::Diagnostic>> {
        // Array parameters are borrowed views (`&[i64; N]` is the only array
        // parameter form, and `Type::Array` bakes the borrow in): their
        // element reads and stores flow through the same arms as local
        // arrays. File-local struct parameters join as borrowed field
        // receivers: `&P` reads its scalar fields, and `&mut P` also writes
        // them (a FieldStore, nothing moves through the borrow). By-value
        // struct parameters carry ownership effects (moves, drop at scope
        // exit) and stay typed.
        // Borrows of aggregates themselves (`&a`, `&p`) stay typed too — a
        // borrow occurrence never lands in `scalar_reads` — and so do calls
        // to aggregate-taking functions, which `supports_call` excludes.
        // Generic functions arrive here after the instance machinery has
        // substituted their signature. Keep non-scalar substitutions on
        // the typed route until their ownership and layout are covered.
        // A substitution may be any value type the body route handles:
// scalars and `string` both lower through the same arms (owned
// fields, concatenation, comparisons).
        // Every clause is named so `AE_ROUTE_REASONS=1` can say which one kept the
        // body off the route: the guard is the one rejection path with no
        // statement to point at.
        let subst_supported = self.subst.values().copied().all(|ty| {
            scalar_type(ty) || matches!(ty, Type::String | Type::Enum(..) | Type::Vec | Type::StrVec | Type::VecF)
                || owned_plain_struct(ty, self.structs)
                || owned_field_struct(ty, self.structs)
        });
        let receiver_supported = !self.receiver.is_some_and(|ty| !matches!(ty, Type::Struct(_) | Type::StructRef(_) | Type::StructMut(_) | Type::Enum(..)));
        // A receiver-less impl body (an associated function, or a trait impl's
        // concrete method) routes like any function: a body that spells `Self`
        // in a way the marking pass cannot classify falls back at the scan.
        let impl_body_supported = true;
        let return_supported = scalar_type(self.return_type) || matches!(self.return_type, Type::Enum(..) | Type::String)
            || matches!(self.return_type, Type::Vec | Type::StrVec | Type::VecF | Type::Map | Type::MapF)
            || matches!(self.return_type, Type::Atomic | Type::Shared(_) | Type::Mutex | Type::Chan | Type::Exec | Type::Pool)
            || (matches!(self.return_type, Type::Struct(_))
                && (self.receiver == Some(self.return_type)
                    || owned_plain_struct(self.return_type, self.structs)
                    || owned_field_struct(self.return_type, self.structs)));
        let slots_supported = self.names.iter().all(|(name, &(slot, _, ty))| {
            let captured = capture_starts.iter().any(|&(capture, _)| capture == slot);
            // A value capture moves into the closure's slot; the body's
            // own Frees releases it after the call (the FnOnce shape),
            // and a string vector's Frees releases its elements in the
            // same order the typed path emits.
            // A `&mut` struct parameter is a borrowed receiver: its field
            // writes are FieldStore on a StructMut slot (the Checked arm
            // already lowers them), and nothing moves or drops through
            // the borrow.
            // A `string` *parameter* is owned by this body; `live_owned`
            // carries it in every `return`, so the callee releases what it
            // did not move out. A closure's owned string capture stays
            // typed: reading it would move the capture.
            let ok = scalar_type(ty) || matches!(ty, Type::Array(_) | Type::StructRef(_) | Type::StructMut(_) | Type::VecRef | Type::VecFRef | Type::StrVecRef | Type::Vec | Type::VecF | Type::StrVec | Type::Map | Type::MapF | Type::Enum(..) | Type::Fn(_) | Type::Closure(_))
                || (matches!(ty, Type::String) && !captured)
                || matches!(ty, Type::Ptr(id) | Type::PtrMut(id) if scalar_type(self.ctx.ptr_pointee(id)))
                || ty == Type::StrView
                // A handle is a copy-semantics value: passing one never
                // moves or releases anything, so a body whose locals are
                // `atomic`/`shared`/`mutex`/`chan`/`exec`/`pool` values
                // reads and passes them like scalars.
                || matches!(ty, Type::Atomic | Type::Shared(_) | Type::Mutex | Type::Chan | Type::Exec | Type::Pool | Type::Guard)
                // A thread handle (`spawn`) and a task scope (`scope_new`) are
                // owned values the body may bind, pass, and join.
                || matches!(ty, Type::Thread(_) | Type::TaskScope)
                // A by-value plain struct parameter is owned here, but it
                // carries nothing a release routine walks, so the callee
                // needs no exit drop: reads (and moving it back out) are
                // the whole surface.
                || owned_plain_struct(ty, self.structs)
                // A capture of a struct with owned fields is bound by value
                // in the closure's generated function; the caller's move
                // rules already made that capture a value.
                || owned_field_struct(ty, self.structs)
                // A drop-carrying struct with plain fields: the exit Frees
                // runs its generated release, which calls the user's drop.
                || plain_drop_struct(ty, self.structs)
                || (name == "self" && self.receiver == Some(ty) && matches!(ty, Type::Struct(_) | Type::StructMut(_) | Type::Enum(..)));
            if !ok && std::env::var_os("AE_ROUTE_REASONS").is_some() {
                eprintln!("route reason blocked function: guard slot '{name}' of type {}", ty.name(self.ctx));
            }
            ok
        });
        if !(subst_supported && receiver_supported && impl_body_supported && return_supported && slots_supported) {
            if std::env::var_os("AE_ROUTE_REASONS").is_some() {
                let mut failed = Vec::new();
                if !subst_supported { failed.push("substitution"); }
                if !receiver_supported { failed.push("receiver"); }
                if !impl_body_supported { failed.push("impl body"); }
                if !return_supported { failed.push("return type"); }
                if !slots_supported { failed.push("slot"); }
                eprintln!("route reason blocked function at {}: guard ({})",
                    self.tokens.get(self.pos).map(|token| token.offset).unwrap_or(0), failed.join(", "));
            }
            return None;
        }
        let resolved_cell = Rc::clone(&self.resolved_expressions);
        let resolved = resolved_cell.get_or_init(|| self.build_resolved_expressions());
        let Some(body) = resolved.scalar_bodies.get(&self.tokens[self.pos].offset).cloned() else {
            if std::env::var_os("AE_ROUTE_REASONS").is_some() {
                eprintln!("route blocked function at {}: not collected", self.tokens.get(self.pos).map(|t| t.offset).unwrap_or(0));
            }
            return None
        };
        let mut method_calls: HashMap<usize, bool> = HashMap::new();
        let mut local_structs: HashMap<String, Type> = HashMap::new();
        for statement in body.iter() {
            ae_syntax::statement::visit_statements(statement, &mut |statement| {
                if let ae_syntax::statement::StatementKind::Binding { name, initializer, .. } = &statement.kind {
                    if let ae_syntax::expression::ExpressionKind::Struct { target, .. } = &initializer.kind {
                        if let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(struct_name)) = &target.kind
                            && let Some(&sid) = self.struct_names.get(struct_name) {
                                local_structs.insert(name.clone(), Type::Struct(sid));
                            }
                        // A generic struct literal names its instance's type.
                        if let ae_syntax::expression::ExpressionKind::Specialization { base: inner, arguments } = &target.kind
                            && let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(struct_name)) = &inner.kind
                                && let Some(&gid) = self.generics.struct_names.get(struct_name)
                                    && let Some(args) = self.shared_specialization_types(arguments)
                                        && let Ok(sid) = self.instantiate_struct(gid, args, target.range.start) {
                                            local_structs.insert(name.clone(), Type::Struct(sid));
                                        }
                    }
                    // A binding from a struct constructor call (`Point::new(..)`,
                    // `Pair[string, i64]::new(..)`): a layout instantiation only
                    // (no body parse), so it is safe in this precompute.
                    if let ae_syntax::expression::ExpressionKind::Call { callee, .. } = &initializer.kind
                        && let ae_syntax::expression::ExpressionKind::Qualified { base, .. } = &callee.kind {
                            match &base.kind {
                                ae_syntax::expression::ExpressionKind::Specialization { base: inner, arguments } => {
                                    if let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(struct_name)) = &inner.kind
                                        && let Some(args) = self.shared_specialization_types(arguments) {
                                            if let Some(&gid) = self.generics.struct_names.get(struct_name) {
                                                if let Ok(sid) = self.instantiate_struct(gid, args, base.range.start) {
                                                    local_structs.insert(name.clone(), Type::Struct(sid));
                                                }
                                            } else if let Some(&gid) = self.generics.names.get(struct_name)
                                                && let Ok(eid) = self.instantiate_enum(gid, args, base.range.start) {
                                                    let display = self.enums[eid].name;
                                                    local_structs.insert(name.clone(), Type::Enum(display, eid));
                                                }
                                        }
                                }
                                ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(struct_name)) => {
                                    if struct_name == "Self" {
                                        if let Some(impl_ty) = self.impl_self {
                                            local_structs.insert(name.clone(), impl_ty);
                                        }
                                    } else if let Some(&sid) = self.struct_names.get(struct_name) {
                                        local_structs.insert(name.clone(), Type::Struct(sid));
                                    } else if let Some(&eid) = self.enum_names.get(struct_name) {
                                        local_structs.insert(name.clone(), Type::Enum(self.enums[eid].name, eid));
                                    }
                                }
                                _ => {}
                            }
                        }
                    // A bare variant path (`let c = Opt[i64]::None;`) names
                    // the instance's enum type without opening a call.
                    if let ae_syntax::expression::ExpressionKind::Qualified { base, .. } = &initializer.kind {
                        let mut node = base.as_ref();
                        let mut arguments = None;
                        loop {
                            match &node.kind {
                                ae_syntax::expression::ExpressionKind::Specialization { base: inner, arguments: args }
                                | ae_syntax::expression::ExpressionKind::BracketedCallTarget { base: inner, arguments: args } => {
                                    arguments = self.shared_specialization_types(args);
                                    node = inner.as_ref();
                                }
                                ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(type_name)) => {
                                    if type_name == "Self" {
                                        if let Some(impl_ty) = self.impl_self {
                                            local_structs.insert(name.clone(), impl_ty);
                                        }
                                    } else if let Some(&sid) = self.struct_names.get(type_name) {
                                        local_structs.insert(name.clone(), Type::Struct(sid));
                                    } else if let Some(&gid) = self.generics.names.get(type_name) {
                                        if let Some(args) = arguments
                                            && let Ok(eid) = self.instantiate_enum(gid, args, node.range.start) {
                                                let display = self.enums[eid].name;
                                                local_structs.insert(name.clone(), Type::Enum(display, eid));
                                            }
                                    } else if let Some(&eid) = self.enum_names.get(type_name) {
                                        local_structs.insert(name.clone(), Type::Enum(self.enums[eid].name, eid));
                                    }
                                    break;
                                }
                                _ => break,
                            }
                        }
                    }
                    // A binding from a method call whose signature returns a
                    // struct or enum (`let fixed = u2.with_min_radius(1);`,
                    // `let moved = start.shifted(1, 2).shifted(0, -6);`), or a
                    // pointer-valued binding (`let raw = ae_std_alloc(..)`,
                    // `let view = raw as *mut f64`, `let p = buf.offset(1)`):
                    // a parameter or an earlier local roots the chain, so each
                    // link's declared type answers statelessly and the gate can
                    // ask before the lowering declares the binding.
                    if let Some(ret @ (Type::Enum(..) | Type::Struct(_) | Type::Ptr(_) | Type::PtrMut(_))) =
                        self.chain_call_type(initializer, &local_structs)
                    {
                        local_structs.insert(name.clone(), ret);
                    }
                }
                for root in statement.expressions() {
                    visit_expression_tree(root, &mut |expression| {
                        let ae_syntax::expression::ExpressionKind::Call { callee, arguments } = &expression.kind else { return };
                        let ae_syntax::expression::ExpressionKind::Field { base, name } = &callee.kind else { return };
                        let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(receiver)) = &base.kind else { return };
                        // A pointer receiver: the resolver's spelling mark names
                        // the binding before the lowering declares it, so the
                        // verdict does not depend on the slot table.
                        if resolved.scalar_pointer_receivers.contains(&base.range.start) {
                            let ok = match name.as_str() {
                                "deref" => arguments.is_empty(),
                                "offset" | "byte_offset" => arguments.len() == 1,
                                // Any other name reaches the lowering's own
                                // `pointers have no method '...'`, the typed
                                // route's diagnostic.
                                _ => true,
                            };
                            method_calls.insert(callee.range.start, ok);
                            return;
                        }
                        let ty = self.names.get(receiver).map(|&(_, _, ty)| ty)
                            .or_else(|| local_structs.get(receiver).copied());
                        let Some(ty) = ty else { return };
                        let ok = if matches!(ty, Type::Ptr(_) | Type::PtrMut(_)) {
                            name == "deref" && arguments.is_empty()
                        } else if let Some((signature, _, _)) = self.resolve_inherent_method(name, ty) {
                            // Arity is part of the gate: a mismatched call
                            // falls back instead of reaching the lowering.
                            arguments.len() + 1 == signature.params.len()
                        } else {
                            // A method the table knows but the route's receiver
                            // rules refuse (an associated function called
                            // through a value): the lowering's own diagnostic
                            // answers, so the gate admits it.
                            self.inherent_entry_exists(name, ty)
                        };
                        method_calls.insert(callee.range.start, ok);
                    });
                }
            });
        }
        let supports_call = |name: &str, offset: usize| {
            // The pure value builtins lower natively (lower_resolved_builtin);
            // every other reserved name keeps the typed body.
            if resolved_builtin(name) { return true; }
            // A generic function joins the route only when the call spells
            // its type arguments (`identity[i64](41)`); a bare call keeps the
            // typed route's pinned "write its type arguments" diagnostic.
            if self.generics.fn_names.contains_key(name) {
                let index = self.tokens.partition_point(|token| token.offset < offset);
                return self.tokens.get(index + 1).is_some_and(|token| token.kind == Kind::LBracket);
            }
            // A trait method of one of this function's constrained type
            // parameters: the instance's substitution picks the impl, so the
            // call lowers as a plain (monomorphized) call.
            if self.trait_bounds.values().flatten().any(|&tid| {
                self.generics.traits.get(tid).is_some_and(|defn| defn.methods.iter().any(|method| method.name == name))
            }) {
                return true;
            }
            !RESERVED.contains(&name) && self.functions.get(name).is_some_and(|signature| {
                // File-local resolution leaves imports open. The compiler's
                // registry already contains the signatures of loaded modules.
                let visible = if signature.file == self.file {
                    resolved.direct_calls.contains(&offset)
                } else {
                    signature.file != PRELUDE && signature.public && resolved.unresolved_calls.contains(&offset)
                };
                visible
                    // Calling C, or an `unsafe fn`, is the unchecked
                    // operation: the scan admits the shape and the lowering
                    // enforces the `unsafe` block (so the pinned E0701 travels
                    // the same path).
                    && (scalar_type(signature.ret) || owned_plain_struct(signature.ret, self.structs)
                        || matches!(signature.ret, Type::String | Type::Enum(..) | Type::Vec | Type::StrVec | Type::VecF | Type::Map | Type::MapF)
                        || matches!(signature.ret, Type::Atomic | Type::Shared(_) | Type::Mutex | Type::Chan | Type::Exec | Type::Pool)
                        // A scalar-pointee pointer (`extern "C" fn
                        // ae_std_alloc(..) -> *mut i64`): the binding holds a
                        // copy-semantics handle the pointer ops consume, the
                        // same shape the slot guard admits.
                        || matches!(signature.ret, Type::Ptr(id) | Type::PtrMut(id) if scalar_type(self.ctx.ptr_pointee(id)))
                        || owned_field_struct(signature.ret, self.structs))
                    && signature.params.iter().copied().all(|ty| {
                        // Owned-aggregate parameters join the route: the call
                        // moves the argument, tracked in `moved`, and the
                        // drops pass trims the Frees lists. A `&string`
                        // parameter (the resolver's `StrView`) is a read-only
                        // view: the argument lowers as `Argument::View`. The
                        // other borrows take the argument's address
                        // (`Argument::Borrow`) with the typed route's own
                        // classify rules.
                        scalar_type(ty) || matches!(ty, Type::Struct(_) | Type::String | Type::StrView | Type::Array(_)
                            | Type::VecRef | Type::StrVecRef | Type::VecFRef | Type::StructRef(_) | Type::StructMut(_)
                            // A by-value vector or map parameter owns the
                            // argument: the call moves it (tracked in
                            // `moved`).
                            | Type::Vec | Type::StrVec | Type::VecF | Type::Map | Type::MapF
                            // An enum parameter owns a value argument the
                            // same way (a result enum, an Option payload).
                            | Type::Enum(..)
                            // A function-typed parameter takes a closure
                            // literal or a function name (`Argument::FnPtr`).
                            | Type::Fn(_)
                            // A raw-pointer parameter (`extern "C" fn
                            // ae_std_free(block: *mut i64)`): a pointer is a
                            // copy-semantics handle, so the argument passes by
                            // value with the typed route's exact type check.
                            | Type::Ptr(_) | Type::PtrMut(_)
                            // A guard is an owned handle a callee may take by
                            // value (`consume(g)`), the same move rule as the
                            // other handles.
                            | Type::Guard
                            | Type::Atomic | Type::Shared(_) | Type::Mutex | Type::Chan | Type::Exec | Type::Pool)
                    })
            })
        };
        // Slots first: the field gate below answers by slot type, not by the
        // impl receiver.
        let mut slots = HashMap::new();
        for &(slot, _, ty) in self.names.values() {
            let start = self.binding_ranges.get(&slot).map(|range| range.start)
                .or_else(|| capture_starts.iter().find_map(|&(capture, start)| (capture == slot).then_some(start)));
            // These two lookups used to `?` out silently: a body whose slots
            // cannot be mapped fell back with no explanation at all. Name the
            // cause when the reason report is on.
            let Some(start) = start else {
                if std::env::var_os("AE_ROUTE_REASONS").is_some() {
                    eprintln!("route reason blocked function at {}: slot {slot} ({}) has no binding range",
                        self.tokens.get(self.pos).map(|token| token.offset).unwrap_or(0), ty.name(self.ctx));
                }
                return None;
            };
            let Some(&def) = resolved.scalar_definitions.get(&start) else {
                if std::env::var_os("AE_ROUTE_REASONS").is_some() {
                    eprintln!("route reason blocked function at {}: slot {slot} ({}) start {start} is not an indexed binding",
                        self.tokens.get(self.pos).map(|token| token.offset).unwrap_or(0), ty.name(self.ctx));
                }
                return None;
            };
            slots.insert(def, (slot, ty));
        }
        // Method-call verdicts, precomputed with `&self` so the gate closure below
// captures only this map. A receiver is a parameter/`self` (known through
// `names` already) or a local bound to a struct construction; anything
// else — generic-instance receivers included — is rejected here and the
// body falls back whole.
        // Any routed struct slot can root a field chain: the root ident's
        // occurrence names the def, the def names the slot, and the slot's
        // type drives the field walk (a `&mut` struct parameter roots its
        // writes the same way `self` does). Reads and stores lower through
        // the same slot lookups the impl path uses.
        let supports_field = |expression: &ae_syntax::expression::Expression, role| {
            // A match subject: `match self` is valid when this body's
            // receiver is an enum (by-value dispatch or a generic instance).
            if role == ae_resolve::Role::Borrow {
                return matches!(&expression.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name)) if name == "self")
                    && (matches!(self.receiver, Some(Type::Enum(..))) || matches!(self.impl_self, Some(Type::Enum(..))));
            }
            // A method-call callee: the precomputed verdict admits the
            // inherent cases; a struct/enum receiver is admitted permissively
            // and the lowering resolves the generic instance lazily (it owns
            // `&mut self` and emits the typed diagnostics when the method is
            // missing), so the gate never depends on instantiation order.
            if role == ae_resolve::Role::Call {
                if method_calls.get(&expression.range.start).copied().unwrap_or(false) {
                    return true;
                }
                let ae_syntax::expression::ExpressionKind::Field { base, name } = &expression.kind else { return false };
                let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(receiver)) = &base.kind else {
                    // A temporary or chained receiver: the lowering delegates
                    // the whole call to the typed postfix layer.
                    return matches!(base.kind, ae_syntax::expression::ExpressionKind::Call { .. }
                        | ae_syntax::expression::ExpressionKind::Group(_));
                };
                // A body local that the precompute typed (a struct literal or
                // constructor call) answers here even though the lowering has
                // not declared it yet.
                let ty = self.names.get(receiver).map(|&(_, _, ty)| ty)
                    .or_else(|| local_structs.get(receiver).copied());
                if ty.is_none()
                    && resolved.scalar_reads.get(&base.range.start)
                        .is_some_and(|id| resolved.generic_call_bindings.contains(id))
                {
                    // A local from an explicit generic call: the lowering has
                    // its concrete type once the call is instantiated.
                    return true;
                }
                let Some(ty) = ty else { return false };
                if matches!(ty, Type::Ptr(_) | Type::PtrMut(_)) {
                    return name == "deref";
                }
                return matches!(ty, Type::Struct(_) | Type::StructMut(_) | Type::StructRef(_) | Type::Enum(..));
            }
            let Some(root) = field_chain_root(expression) else { return false };
            let occurrences = match role {
                ae_resolve::Role::Read => &resolved.scalar_reads,
                ae_resolve::Role::Write => &resolved.scalar_writes,
                _ => return false,
            };
            let Some(def) = occurrences.get(&root.range.start) else { return false };
            let Some(&(_, ty)) = slots.get(def) else {
                // A body local declared after the entry scope (`var plot = ..;
                // plot.w = 40;`): the lowering owns its slot and validates the
                // write there. A local from an explicit generic call is the
                // same shape for reads — its type exists only at lowering.
                return match role {
                    ae_resolve::Role::Write => resolved.struct_field_writes.contains(&expression.range.start)
                        || resolved.owned_field_writes.contains(&expression.range.start),
                    ae_resolve::Role::Read => resolved.generic_call_bindings.contains(def),
                    _ => false,
                };
            };
            if role == ae_resolve::Role::Write && !matches!(ty, Type::StructMut(_) | Type::Struct(_) | Type::StructRef(_)) {
                return false;
            }
            if field_chain_is_bare(expression) {
                // The whole value: only an owned struct moves as one.
                matches!(ty, Type::Struct(_))
            } else {
                // A string field joins the write surface (the FieldStore
                // codegen releases the old value and consumes the new one);
                // reads stay scalar-only here and reach the scan through the
                // view and read marks.
                receiver_field_type(expression, ty, self.structs)
                    .is_some_and(|field| scalar_type(field) || (role == ae_resolve::Role::Write && matches!(field, Type::String)))
            }
        };
        // Parameters owned by this body, minus capture slots (the closure
        // lowering already accounts for those): `live_owned` releases them at
        // every return unless the body moved one out.
        self.owned_params = self.names.values()
            .filter(|&&(slot, _, ty)| ty.owned() && !capture_starts.iter().any(|&(capture, _)| capture == slot))
            .map(|&(slot, _, ty)| (slot, ty))
            .collect();
        if let Some(statement) = body.iter().position(|statement| {
                !scalar_body_statement(statement, resolved, &supports_call, &supports_field)
            }).and_then(|index| body.get(index)) {
            // The body was collected, so the scan admitted it; this statement
            // fails the lowering's own gates (callee signatures, slot types).
            self.report_route_reason("blocked", statement, &|s| !scalar_body_statement(s, resolved, &supports_call, &supports_field));
            self.owned_params.clear();
            return None;
        }
        let lowered = self.lower_scalar_statements(&body, &mut slots);
        self.owned_params.clear();
        Some(lowered)
    }

    fn lower_scalar_statements(
        &mut self,
        body: &[ae_syntax::statement::Statement],
        slots: &mut HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Vec<Statement>, ae_span::Diagnostic> {
        use ae_syntax::statement::StatementKind;
        self.take(Kind::LBrace)?;
        if self.block_depth >= 64 { return Err(self.fail("block nesting limit exceeded (64)")); }
        self.block_depth += 1;
        self.scopes.push(Vec::new());
        let mut statements = Vec::with_capacity(body.len());
        let lowered: Result<(), ae_span::Diagnostic> = (|| {
            for statement in body {
                // A call argument's borrow covered only the statement that
                // made the call; the next statement may borrow or write the
                // binding again (the typed route's block loop does the same).
                self.call_borrows.clear();
                self.mut_borrows.clear();
                if matches!(statement.kind, StatementKind::ForRange { .. }) {
                    self.lower_scalar_range(statement, slots, &mut statements)?;
                } else if matches!(statement.kind, StatementKind::ForIn { .. }) {
                    self.lower_scalar_for_in(statement, slots, &mut statements)?;
                } else {
                    let statement = self.lower_scalar_statement(statement, slots)?;
                    if matches!(statement, Statement::Break | Statement::Continue(_)) {
                        // Owned values declared inside the loop body die at
                        // the jump; releases are idempotent.
                        let owned = self.loop_local_owned();
                        if !owned.is_empty() { statements.push(Statement::Frees(owned)); }
                    }
                    statements.push(statement);
                }
            }
            Ok(())
        })();
        let scope = self.scopes.pop().unwrap();
        self.block_depth -= 1;
        lowered?;
        self.take(Kind::RBrace)?;
        for statement in body {
            if let StatementKind::Binding { name, name_range, .. } = &statement.kind {
                self.names.remove(name);
                slots.remove(&self.resolved_expressions().scalar_definitions[&name_range.start]);
            }
        }
        let depth = self.scopes.len();
        self.borrows.retain(|&(_, borrow_depth, _)| borrow_depth <= depth);
        // Owned values declared in this block are released at its end,
        // mirroring the typed route's block exit.
        let owned: Vec<(usize, Type)> = scope.iter().filter(|(_, ty)| ty.owned()).copied().collect();
        if !owned.is_empty() { statements.push(Statement::Frees(owned)); }
        Ok(statements)
    }

    fn lower_scalar_statement(
        &mut self,
        statement: &ae_syntax::statement::Statement,
        slots: &mut HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Statement, ae_span::Diagnostic> {
        use ae_syntax::statement::{ElseBranch, StatementKind};
        let lowered = match &statement.kind {
            StatementKind::Binding { name, name_range, mutable, initializer } => {
                self.seek_scalar_offset(name_range.end);
                if RESERVED.contains(&name.as_str()) {
                    return Err(self.fail(&format!("'{name}' is a built-in name and cannot be a variable")));
                }
                if self.struct_names.contains_key(name) {
                    return Err(self.fail(&format!("'{name}' names a struct and cannot be a variable")));
                }
                if self.enum_names.contains_key(name) {
                    return Err(self.fail(&format!("'{name}' names an enum and cannot be a variable")));
                }
                if self.names.contains_key(name) {
                    return Err(self.duplicate_binding_error(name));
                }
                let slot = self.next_var;
                self.next_var += 1;
                let def = self.resolved_expressions().scalar_definitions[&name_range.start];
                let (statement, ty) = if let ae_syntax::expression::ExpressionKind::Propagate(operand) = &initializer.kind {
                    // `let x = foo()?;` binds the Ok payload and hands the
                    // failure on, mirroring the typed route's binding
                    // propagation.
                    let ae_syntax::expression::ExpressionKind::Call { callee, .. } = &operand.kind else {
                        return Err(self.fail("a propagation binding must call a function or construct a result variant"));
                    };
                    match &callee.kind {
                        ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(_))
                        | ae_syntax::expression::ExpressionKind::Qualified { .. } => {}
                        _ => return Err(self.fail("a propagation binding must call a function or construct a result variant")),
                    }
                    self.seek_scalar_offset(operand.range.start);
                    let (value, call_ty) = match self.lower_shared_value(Some(Kind::Question), true) {
                        Some(pair) => pair,
                        None => self.expression(0, true)?,
                    };
                    if *self.current() != Kind::Question {
                        return Err(self.fail("a statement that only calls a function must end with '?' to propagate its result"));
                    }
                    let (statement, bound_ty) = self.propagate_statement(value, call_ty, Some(slot))?;
                    self.pos += 1;
                    self.take(Kind::Semi)?;
                    let bound_ty = bound_ty.expect("a binding propagation always produces a payload type");
                    (statement, bound_ty)
                } else if let ae_syntax::expression::ExpressionKind::Array(elements) = &initializer.kind {
                    if elements.is_empty() {
                        self.seek_scalar_offset(initializer.range.start);
                        self.pos += 1;
                        return Err(self.fail("array must contain at least one element"));
                    }
                    let mut values = Vec::with_capacity(elements.len());
                    for (index, element) in elements.iter().enumerate() {
                        if index == 4096 {
                            self.seek_scalar_offset(element.range.start);
                            return Err(self.fail("array element limit exceeded (4096)"));
                        }
                        let (value, ty) = self.lower_scalar_body_expression(element, slots)?;
                        if ty != Type::Int {
                            self.seek_scalar_offset(element.range.end);
                            return Err(self.fail("array elements must be i64"));
                        }
                        values.push(value);
                    }
                    (Statement::Array(slot, values), Type::Array(elements.len()))
                } else if let ae_syntax::expression::ExpressionKind::Vector(elements) = &initializer.kind {
                    self.seek_scalar_offset(initializer.range.start);
                    let (line, column) = self.position();
                    let mut values = Vec::with_capacity(elements.len());
                    let mut element_type = None;
                    for (index, element) in elements.iter().enumerate() {
                        if index >= 65536 {
                            self.seek_scalar_offset(element.range.start);
                            return Err(self.fail("vector element limit exceeded (65536)"));
                        }
                        let (value, ty) = self.lower_scalar_body_expression(element, slots)?;
                        self.seek_scalar_offset(element.range.end);
                        element_type = Some(self.check_numeric_vector_element(element_type, ty)?);
                        values.push(value);
                    }
                    let ty = if element_type == Some(Type::F64) { Type::VecF } else { Type::Vec };
                    (Statement::Declare(slot, Expr::VecLit(values, ty, line, column)), ty)
                } else {
                    let (value, ty) = if let ae_syntax::expression::ExpressionKind::Closure { .. } = &initializer.kind {
                        // A closure literal: the typed closure machinery owns
                        // capture inference, the generated function, and the
                        // body route (it lowers the body through this same
                        // resolved pass).
                        self.seek_scalar_offset(initializer.range.start);
                        let literal = self.closure_literal()?;
                        self.seek_scalar_offset(initializer.range.end);
                        literal
                    } else if iterator_constructor(initializer).is_some()
                        || self.resolved_expressions().enum_constructors.contains(&initializer.range.start)
                    {
                        self.seek_scalar_offset(initializer.range.start);
                        self.expression(0, false)?
                    } else {
                        let lowered = self.lower_scalar_body_expression(initializer, slots)?;
                        self.seek_scalar_offset(initializer.range.end);
                        lowered
                    };
                    // Binding an owned value moves it: the new slot owns what
                    // the old one did, and later reads of the source fail with
                    // E0102 (the typed route's rule).
                    if ty.owned()
                        && let Expr::Variable(source, ..) = &value {
                            self.check_variable_move(*source)?;
                            self.moved.insert(*source);
                        }
                    // Views borrow their base: `let w = v[0];` registers the vector so a
                    // later `pop(v)`/`push(v, ..)` is rejected while `w` can
                    // still be read (the typed block path's rule).
                    self.record_borrow_bases(&value);
                    self.check_view_binding(*mutable, ty)?;
                    self.take(Kind::Semi)?;
                    self.check_borrowed_binding(ty)?;
                    (Statement::Declare(slot, value), ty)
                };
                slots.insert(def, (slot, ty));
                self.names.insert(name.clone(), (slot, *mutable, ty));
                self.binding_ranges.insert(slot, name_range.clone());
                if ty.owned() {
                    self.scopes.last_mut().expect("block scope").push((slot, ty));
                }
                statement
            }
            StatementKind::Assign { target, operator, value } => {
                if let ae_syntax::expression::ExpressionKind::Field { base, name } = &target.kind {
                    let def = self.resolved_expressions().scalar_writes[&base.range.start];
                    // The store writes through a `&mut` receiver or into an
                    // owned local struct; both reach the same FieldStore, and
                    // codegen picks `.`/`->` from the receiver type.
                    let (slot, receiver) = slots[&def];
                    if let Type::StructRef(_) = receiver {
                        // A read-only struct borrow: neither the pointer nor
                        // anything behind it may be written. The typed route
                        // distinguishes an immutable binding from a mutable one
                        // (`var r: &Rect`), and so does this arm.
                        let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(base_name)) = &base.kind else {
                            unreachable!("borrowed struct store base");
                        };
                        if !self.names.get(base_name).is_some_and(|&(_, mutable, _)| mutable) {
                            self.seek_scalar_offset(target.range.start);
                            let diagnostic = self.fail(&format!("cannot assign to immutable variable '{base_name}'")).with_code("E0103");
                            return Err(self.immutable_binding_error(diagnostic, slot));
                        }
                        self.seek_scalar_offset(base.range.end);
                        return Err(self.fail(&format!("cannot write through '{base_name}': struct borrows are read-only in this version")));
                    }
                    let sid = match receiver {
                        Type::StructMut(sid) | Type::Struct(sid) => sid,
                        _ => unreachable!("resolved mutable receiver field"),
                    };
                    if matches!(receiver, Type::Struct(_)) {
                        let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(base_name)) = &base.kind else {
                            unreachable!("owned struct store base");
                        };
                        if !self.names[base_name].1 {
                            self.seek_scalar_offset(target.range.start);
                            let diagnostic = self.fail(&format!("cannot assign to immutable variable '{base_name}'")).with_code("E0103");
                            return Err(self.immutable_binding_error(diagnostic, slot));
                        }
                    }
                    let field = field_ty_in(sid, name, self.structs).expect("eligible receiver field");
                    let (value, actual) = self.lower_scalar_body_expression(value, slots)?;
                    self.seek_scalar_offset(statement.range.end);
                    self.check_field_value_type(name, field, actual, None)?;
                    return Ok(Statement::FieldStore(slot, name.clone(), field, value, receiver));
                }
                if let ae_syntax::expression::ExpressionKind::Index { base, index } = &target.kind {
                    // An array element store (`a[i] = v`): eligibility
                    // confirmed a resolved array binding and the plain
                    // operator. The checks mirror the typed store: the
                    // binding must be mutable (E0103), the index and the
                    // element must be i64 (E0201 for the index).
                    let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name)) = &base.kind else {
                        unreachable!("scalar store eligibility");
                    };
                    let def = self.resolved_expressions().scalar_writes.get(&base.range.start)
                        .or_else(|| self.resolved_expressions().scalar_reads.get(&base.range.start))
                        .copied().expect("resolved store base");
                    let (slot, ty) = slots[&def];
                    // `m[key] = v` inserts or overwrites in a map: the key is
                    // copied by the runtime, the value must match the map's
                    // element type, and the store is statement-scoped.
                    if matches!(ty, Type::Map | Type::MapF) {
                        let kind = map_kind_of(ty);
                        if !self.names[name].1 {
                            self.seek_scalar_offset(base.range.start);
                            let diagnostic = self.fail(&format!("cannot assign to immutable variable '{name}'")).with_code("E0103");
                            return Err(self.immutable_binding_error(diagnostic, slot));
                        }
                        if self.is_borrowed(slot) {
                            self.seek_scalar_offset(base.range.end);
                            return Err(self.fail(&format!("cannot modify '{name}' while it is borrowed")));
                        }
                        self.seek_scalar_offset(base.range.end);
                        let (line, column) = self.position();
                        let (key, key_ty) = self.lower_scalar_body_expression(index, slots)?;
                        if !matches!(key_ty, Type::String | Type::StrView) {
                            self.seek_scalar_offset(index.range.end);
                            return Err(self.fail("map keys must be strings"));
                        }
                        let (stored, value_ty) = self.lower_scalar_body_expression(value, slots)?;
                        if value_ty != map_element_ty(kind) {
                            self.seek_scalar_offset(value.range.end);
                            return Err(self.fail(&format!("map values must be {}", map_element_ty(kind).name(self.ctx))));
                        }
                        self.seek_scalar_offset(value.range.end);
                        self.take(Kind::Semi)?;
                        return Ok(Statement::MapPut(MapPlace::Var(slot, kind), key, stored, line, column));
                    }
                    if matches!(ty, Type::Vec | Type::StrVec | Type::VecF) {
                        // A vector element store: overwriting a string element
                        // releases the old one, so a live borrow blocks it
                        // (the typed store's rule).
                        if !self.names[name].1 {
                            self.seek_scalar_offset(base.range.start);
                            let diagnostic = self.fail(&format!("cannot assign to immutable variable '{name}'")).with_code("E0103");
                            return Err(self.immutable_binding_error(diagnostic, slot));
                        }
                        if self.is_borrowed(slot) {
                            self.seek_scalar_offset(base.range.end);
                            return Err(self.fail(&format!("cannot modify '{name}' while it is borrowed")));
                        }
                        self.seek_scalar_offset(base.range.end);
                        let (line, column) = self.position();
                        let (element_index, index_ty) = self.lower_scalar_body_expression(index, slots)?;
                        if index_ty != Type::Int {
                            self.seek_scalar_offset(index.range.end);
                            return Err(self.fail("vector index must be an integer"));
                        }
                        let (stored, value_ty) = self.lower_scalar_body_expression(value, slots)?;
                        let expected = match ty {
                            Type::StrVec => "string",
                            Type::VecF => "f64",
                            _ => "i64",
                        };
                        let matches_element = match ty {
                            Type::StrVec => matches!(value_ty, Type::String | Type::StrView),
                            Type::VecF => value_ty == Type::F64,
                            _ => value_ty == Type::Int,
                        };
                        if !matches_element {
                            self.seek_scalar_offset(value.range.end);
                            return Err(self.fail(&format!("vector elements must be {expected}")));
                        }
                        // A value store moves an owned string into the slot;
                        // a view store only borrows it.
                        if value_ty.owned()
                            && let Expr::Variable(source, ..) = &stored {
                                self.moved.insert(*source);
                            }
                        self.seek_scalar_offset(value.range.end);
                        self.take(Kind::Semi)?;
                        return Ok(match ty {
                            Type::StrVec => Statement::StrVecStore(slot, element_index, stored, line, column, value_ty == Type::StrView),
                            Type::VecF => Statement::VecFStore(slot, element_index, stored, line, column),
                            _ => Statement::VecStore(slot, element_index, stored, line, column),
                        });
                    }
                    let Type::Array(len) = ty else {
                        unreachable!("resolved array store");
                    };
                    if !self.names[name].1 {
                        self.seek_scalar_offset(base.range.start);
                        let diagnostic = self.fail(&format!("cannot assign to immutable variable '{name}'")).with_code("E0103");
                        return Err(self.immutable_binding_error(diagnostic, slot));
                    }
                    self.seek_scalar_offset(base.range.end);
                    let (line, column) = self.position();
                    let element_index = self.lower_scalar_array_index(index, slots)?;
                    self.seek_scalar_offset(value.range.start);
                    let (element, element_ty) = self.lower_scalar_body_expression(value, slots)?;
                    if element_ty != Type::Int {
                        self.seek_scalar_offset(value.range.end);
                        return Err(self.fail("array elements must be i64"));
                    }
                    self.seek_scalar_offset(statement.range.end);
                    return Ok(Statement::Store(slot, len, element_index, element, line, column));
                }
                let ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name)) = &target.kind else {
                    unreachable!("scalar assignment eligibility");
                };
                let def = self.resolved_expressions().scalar_writes[&target.range.start];
                let (slot, ty) = slots[&def];
                if !self.names[name].1 {
                    self.seek_scalar_offset(target.range.start);
                    let diagnostic = self.fail(&format!("cannot assign to immutable variable '{name}'")).with_code("E0103");
                    return Err(self.immutable_binding_error(diagnostic, slot));
                }
                // A `&mut` borrow is a view of the caller's storage: writing
                // the binding itself would re-point the view. Fields are the
                // writable surface, exactly as the typed route says.
                if matches!(ty, Type::StructMut(_)) {
                    // The typed route points at the `=` after the name.
                    self.seek_scalar_offset(target.range.end);
                    return Err(self.fail(&format!("cannot assign to '{name}' itself through a '&mut' borrow; write its fields instead")));
                }
                // Writing an owned value while a view of it may still be read
                // frees the bytes the view points at. A string's view
                // conflicts defer to the graph's ordered adjudication (the
                // typed rule); every other owned type rejects on sight.
                if ty.owned() && self.is_borrowed(slot) {
                    if !(matches!(ty, Type::String) && self.all_borrows_are_views(slot)) {
                        self.seek_scalar_offset(target.range.end);
                        return Err(self.fail(&format!("cannot assign to '{name}' while it is borrowed")));
                    }
                    self.deferred_write_conflicts.push((slot, target.range.start, name.clone(), "assign to", None));
                }
                self.seek_scalar_offset(target.range.end);
                let compound = compound_assignment_operator(operator);
                if let Some(op) = &compound {
                    self.check_compound_assignment(ty, op)?;
                }
                self.seek_scalar_offset(value.range.start);
                let (line, column) = self.position();
                // The right-hand side may not consume the target itself.
                // Assigning to an already-moved variable is a legal
                // re-initialization, so the flag clears either way.
                let moved_before = self.moved.contains(&slot);
                let (rhs, rhs_ty) = if self.resolved_expressions().enum_constructors.contains(&value.range.start) {
                    // An enum construction on the right-hand side
                    // (`current = Message::Number(9);`): the typed expression
                    // parser owns the variant layout and payload lowering.
                    self.seek_scalar_offset(value.range.start);
                    self.expression(0, true)?
                } else {
                    self.lower_scalar_body_expression(value, slots)?
                };
                // An owned right-hand side moves into the target: the source
                // slot's Frees is trimmed, and assigning the target to itself
                // fails as a same-statement move (the typed rule).
                if rhs_ty.owned()
                    && let Expr::Variable(source, ..) = &rhs {
                        self.seek_scalar_offset(value.range.end);
                        self.check_variable_move(*source)?;
                        self.moved.insert(*source);
                    }
                self.seek_scalar_offset(statement.range.end);
                self.check_assignment_type(name, ty, rhs_ty, Some(&value.range))?;
                if self.moved.contains(&slot) && !moved_before {
                    return Err(moved_self_assignment_hint(self.binding_label(
                        self.error_at(target.range.start, &format!("cannot assign to '{name}' because it was moved in the same statement")).with_code("E0102"), slot)));
                }
                self.moved.remove(&slot);
                let value = if let Some(op) = compound {
                    let lhs = Expr::Variable(slot, ty, line, column);
                    Expr::Binary(Box::new(lhs), op, Box::new(rhs), line, column, ty)
                } else {
                    rhs
                };
                // Overwriting an owned value releases the old one first (the
                // Assign codegen's flag); scalars need no release.
                Statement::Assign(slot, value, ty.owned())
            }
            StatementKind::PointerStore { pointer, pointer_range, value } => {
                self.seek_scalar_offset(statement.range.start);
                let (line, column) = self.position();
                let def = self.resolved_expressions().scalar_writes[&pointer_range.start];
                let (slot, ty) = slots[&def];
                self.seek_scalar_offset(pointer_range.end);
                let pointee = self.pointer_store_pointee(pointer, Some(ty), line)?;
                let (lowered, actual) = self.lower_scalar_body_expression(value, slots)?;
                self.seek_scalar_offset(value.range.end);
                self.check_pointer_store_value(actual, pointee)?;
                self.take(Kind::Semi)?;
                Statement::PtrStore(slot, lowered, line, column)
            }
            StatementKind::Print(argument) => {
                // A literal prints as `Printed::Text`, the same statement
                // the typed route emits for it (byte parity).
                if let ae_syntax::expression::ExpressionKind::Atom(Kind::Text(text)) = &argument.kind {
                    self.seek_scalar_offset(statement.range.end);
                    return Ok(Statement::Print(Printed::Text(text.clone())));
                }
                let (value, ty) = self.lower_scalar_body_expression(argument, slots)?;
                if !matches!(ty, Type::Int | Type::Bool | Type::F64 | Type::String | Type::StrView) {
                    self.seek_scalar_offset(argument.range.end);
                    return Err(self.fail(&format!("cannot print a value of type {}", ty.name(self.ctx))).with_code("E0206"));
                }
                Statement::Print(Printed::Expr(value))
            }
            StatementKind::Assert(argument) => {
                self.seek_scalar_offset(statement.range.start);
                let (line, column) = self.position();
                let (condition, ty) = self.lower_scalar_body_expression(argument, slots)?;
                if ty != Type::Bool {
                    self.seek_scalar_offset(argument.range.end);
                    return Err(self.fail("assert requires a boolean condition"));
                }
                Statement::Assert(condition, line, column)
            }
            StatementKind::Return(value) => {
                if self.in_main {
                    self.seek_scalar_offset(statement.range.start);
                    return Err(self.fail("return is not supported in main"));
                }
                if matches!(&value.kind, ae_syntax::expression::ExpressionKind::Closure { .. }) {
                    self.seek_scalar_offset(statement.range.end);
                    return Err(self.fail("a closure cannot be returned: it has no storage, and its captures live in this frame"));
                }
                let (lowered, ty) = if self.resolved_expressions().enum_constructors.contains(&value.range.start) {
                    self.seek_scalar_offset(value.range.start);
                    let result = self.expression(0, true)?;
                    self.take(Kind::Semi)?;
                    result
                } else {
                    self.lower_scalar_body_expression(value, slots)?
                };
                // Returning an owned value moves it out of the body: the
                // caller owns it, so the drops pass must trim this slot's
                // Frees and later reads are rejected (E0102). This covers
                // `return self;` and `return binding;` for structs, strings
                // and vectors alike.
                if let Expr::Variable(slot, ty, ..) = &lowered
                    && ty.owned() {
                        self.seek_scalar_offset(value.range.end);
                        self.check_variable_move(*slot)?;
                        self.moved.insert(*slot);
                    }
                self.seek_scalar_offset(statement.range.end);
                self.check_return_type(ty, Some(&value.range))?;
                Statement::Return(lowered, self.live_owned())
            }
            StatementKind::If { condition, then_body, else_body } => {
                let value = self.lower_scalar_condition(condition, slots)?;
                let before = self.moved.clone();
                let yes = self.lower_scalar_statements(then_body, slots)?;
                let after_yes = std::mem::replace(&mut self.moved, before);
                let no = if let Some(branch) = else_body {
                    self.pos += 1;
                    match branch {
                        ElseBranch::Block(body) => self.lower_scalar_statements(body, slots)?,
                        ElseBranch::If(statement) => vec![self.lower_scalar_statement(statement, slots)?],
                    }
                } else {
                    Vec::new()
                };
                self.moved.extend(after_yes);
                Statement::If(value, yes, no)
            }
            StatementKind::While { condition, body } => {
                let condition = self.lower_scalar_condition(condition, slots)?;
                if self.loop_depth >= 64 {
                    return Err(self.fail("loop nesting limit exceeded (64)"));
                }
                let base = self.scopes.len();
                let moved_before = self.moved.clone();
                let loop_body = self.lower_scalar_loop_body(body, slots, None)?;
                self.close_loop_moves(base, moved_before, statement.range.start)?;
                Statement::While(condition, loop_body)
            }
            StatementKind::Unsafe(body) => {
                // An `unsafe { ... }` block is a scoping container; mirror
                // the typed route's depth tracking and the W0702 note for
                // blocks with no unchecked operation.
                self.seek_scalar_offset(statement.range.start);
                self.pos += 1;
                let block_line = self.tokens[self.pos].line;
                let block_column = self.tokens[self.pos].column;
                self.unsafe_depth += 1;
                let inner = self.lower_scalar_statements(body, slots);
                self.unsafe_depth -= 1;
                let inner = inner?;
                if !self.block_has_unchecked_operation(&inner) {
                    eprintln!("ae: {}:{}: warning: unnecessary unsafe block (W0702): the body contains no unchecked operation", block_line, block_column);
                }
                Statement::Unsafe(inner)
            }
            StatementKind::Expr { .. } if is_task_yield(statement) => {
                let StatementKind::Expr { value, .. } = &statement.kind else { unreachable!("task_yield statement") };
                self.seek_scalar_offset(value.range.end);
                self.take(Kind::Semi)?;
                return Ok(Statement::TaskYield);
            }
            StatementKind::Expr { propagates: false, .. }
                if vector_statement_builtin(statement, self.resolved_expressions()).is_some() =>
            {
                return self.lower_vector_builtin_statement(statement, slots);
            }
            StatementKind::Expr { propagates: false, .. } if handle_statement_builtin(statement).is_some() => {
                return self.lower_handle_builtin_statement(statement, slots);
            }
            StatementKind::Expr { propagates: true, .. } => {
                let operand = propagation_call(statement).expect("resolved propagation eligibility");
                self.seek_scalar_offset(operand.range.start);
                self.lower_propagating_call()?
            }
            StatementKind::Expr { .. } if bare_call_statement(statement).is_some() => {
                let StatementKind::Expr { value, .. } = &statement.kind else { unreachable!("unknown call statement") };
                self.seek_scalar_offset(value.range.start);
                // The name answers nowhere, so the callee resolution fails
                // with the typed route's E0207 (the error precedes the
                // statement-level "must end with '?'" check). If it somehow
                // resolves, fall through to that statement rule.
                let _ = self.lower_scalar_body_expression(value, slots)?;
                return Err(self.fail("a statement that only calls a function must end with '?' to propagate its result"));
            }
            StatementKind::Expr { .. } => {
                let status = exit_status_operand(statement).expect("resolved exit eligibility");
                let (code, ty) = self.lower_scalar_body_expression(status, slots)?;
                self.seek_scalar_offset(status.range.end);
                self.finish_exit(code, ty)?
            }
            StatementKind::Match { subject, arms } => {
                let (value, ty) = self.lower_scalar_body_expression(subject, slots)?;
                self.seek_scalar_offset(subject.range.end);
                self.lower_match(value, ty, |parser, index, names| {
                    let definitions = names.iter().map(|(_, offset)| {
                        parser.resolved_expressions().scalar_definitions.get(offset).copied()
                    }).collect::<Option<Vec<_>>>();
                    let (Some(arm), Some(definitions)) = (arms.get(index), definitions) else {
                        return parser.block();
                    };
                    for ((name, _), &def) in names.iter().zip(&definitions) {
                        let (slot, _, ty) = parser.names[name];
                        slots.insert(def, (slot, ty));
                    }
                    let body = parser.lower_scalar_statements(&arm.body, slots);
                    for def in definitions { slots.remove(&def); }
                    body
                })?
            }
            StatementKind::Break | StatementKind::Continue => {
                let is_break = matches!(statement.kind, StatementKind::Break);
                if self.loop_depth == 0 {
                    self.seek_scalar_offset(statement.range.start);
                    let keyword = if is_break { "break" } else { "continue" };
                    return Err(self.fail(&format!("'{keyword}' outside of a loop")));
                }
                if is_break {
                    Statement::Break
                } else {
                    Statement::Continue(self.loop_labels.last().copied().flatten())
                }
            }
            _ => unreachable!("scalar body eligibility"),
        };
        self.seek_scalar_offset(statement.range.end);
        Ok(lowered)
    }

    fn lower_scalar_range(
        &mut self,
        statement: &ae_syntax::statement::Statement,
        slots: &mut HashMap<ae_resolve::DefId, (usize, Type)>,
        statements: &mut Vec<Statement>,
    ) -> Result<(), ae_span::Diagnostic> {
        let ae_syntax::statement::StatementKind::ForRange { binding, binding_range, start, end, body } = &statement.kind else {
            unreachable!("scalar range eligibility");
        };
        self.seek_scalar_offset(binding_range.end);
        if RESERVED.contains(&binding.as_str()) {
            return Err(self.fail(&format!("'{binding}' is a built-in name and cannot be a loop variable")));
        }
        if self.names.contains_key(binding) {
            return Err(self.duplicate_binding_error(binding));
        }
        let start = self.lower_scalar_range_bound(start, slots)?;
        let end = self.lower_scalar_range_bound(end, slots)?;
        if self.loop_depth >= 64 {
            return Err(self.fail("loop nesting limit exceeded (64)"));
        }
        let (line, column) = self.position();
        let end_slot = self.next_var;
        let slot = end_slot + 1;
        self.next_var += 2;
        let label = self.next_label;
        self.next_label += 1;
        let def = self.resolved_expressions().scalar_definitions[&binding_range.start];
        slots.insert(def, (slot, Type::Int));
        self.names.insert(binding.clone(), (slot, false, Type::Int));
        let base = self.scopes.len();
        let moved_before = self.moved.clone();
        let body = self.lower_scalar_loop_body(body, slots, Some(label));
        slots.remove(&def);
        self.names.remove(binding);
        let body = body?;
        // The body runs again: a value moved out of an outer variable must be
        // re-initialized before the body ends (the typed route's ADR-20 rule,
        // the same close the while loop applies).
        self.close_loop_moves(base, moved_before, statement.range.start)?;
        statements.push(Statement::Declare(slot, start));
        statements.push(Statement::Declare(end_slot, end));
        statements.push(Statement::For(slot, end_slot, body, label, line, column));
        Ok(())
    }

    fn lower_scalar_for_in(
        &mut self,
        statement: &ae_syntax::statement::Statement,
        slots: &mut HashMap<ae_resolve::DefId, (usize, Type)>,
        result: &mut Vec<Statement>,
    ) -> Result<(), ae_span::Diagnostic> {
        let ae_syntax::statement::StatementKind::ForIn { binding, binding_range, iterable, body } = &statement.kind else {
            unreachable!("resolved for-in eligibility");
        };
        self.seek_scalar_offset(binding_range.end);
        if RESERVED.contains(&binding.as_str()) {
            return Err(self.fail(&format!("'{binding}' is a built-in name and cannot be a loop variable")));
        }
        if self.names.contains_key(binding) {
            return Err(self.duplicate_binding_error(binding));
        }
        let (iterable_value, ty) = self.lower_scalar_body_expression(iterable, slots)?;
        self.seek_scalar_offset(iterable.range.end);
        let def = self.resolved_expressions().scalar_definitions[&binding_range.start];
        let lower_body = |parser: &mut Self, slot, elem_ty| {
            slots.insert(def, (slot, elem_ty));
            let result = parser.lower_scalar_statements(body, slots);
            slots.remove(&def);
            result
        };
        if ty == Type::Tokens {
            self.lower_token_loop(binding, iterable_value, statement.range.start, result, lower_body)
        } else {
            self.lower_vector_loop(binding, iterable_value, ty, statement.range.start, result, lower_body)
        }
    }

    fn lower_token_loop(
        &mut self, var_name: &str, first: Expr, loop_offset: usize,
        result: &mut Vec<Statement>,
        lower_body: impl FnOnce(&mut Self, usize, Type) -> Result<Vec<Statement>, ae_span::Diagnostic>,
    ) -> Result<(), ae_span::Diagnostic> {
        // `for token in cursor` streams the tokens as read-only
        // views: no vector, no per-token allocation, and the
        // loop body sees one token at a time (ADR-53). Each
        // loop restarts at the beginning of the base text.
        let cursor = match &first {
            Expr::Variable(id, Type::Tokens, _, _) => *id,
            _ => return Err(self.fail("for-in over a token cursor needs a bare cursor variable: bind it first, e.g. let toks = tokens(text, \" \");")),
        };
        if self.loop_depth >= 64 { return Err(self.fail("loop nesting limit exceeded (64)")); }
        let (line, col) = self.position();
        let var = self.next_var; self.next_var += 1;
        let label = self.next_label; self.next_label += 1;
        self.loop_depth += 1;
        let base = self.scopes.len();
        let moved_before = self.moved.clone();
        self.scopes.push(vec![(var, Type::StrView)]);
        self.loop_scopes.push(self.scopes.len());
        self.loop_labels.push(Some(label));
        self.names.insert(var_name.to_owned(), (var, false, Type::StrView));
        let body = lower_body(self, var, Type::StrView);
        self.loop_labels.pop();
        self.loop_scopes.pop();
        self.scopes.pop().unwrap();
        self.loop_depth -= 1;
        self.names.remove(var_name);
        let body = body?;
        self.close_loop_moves(base, moved_before, loop_offset)?;
        result.push(Statement::ForTokens(cursor, var, body, label, line, col));
        Ok(())
    }

    fn lower_vector_loop(
        &mut self, var_name: &str, first: Expr, first_ty: Type, loop_offset: usize,
        result: &mut Vec<Statement>,
        lower_body: impl FnOnce(&mut Self, usize, Type) -> Result<Vec<Statement>, ae_span::Diagnostic>,
    ) -> Result<(), ae_span::Diagnostic> {
        // `for x in vec-variable` iterates owned elements by
        // value: desugars to a hidden range loop plus an
        // immutable element declaration per iteration.
        // Borrowed string vectors iterate the same way, with
        // element access going through the pointer.
        let (v_id, elem_ty, by_ref) = match (&first, first_ty) {
            (Expr::Variable(id, Type::Vec, _, _), Type::Vec) => (*id, Type::Int, false),
            (Expr::Variable(id, Type::StrVec, _, _), Type::StrVec) => (*id, Type::StrView, false),
            (Expr::Variable(id, Type::StrVecRef, _, _), Type::StrVecRef) => (*id, Type::StrView, true),
            (Expr::Variable(id, Type::VecRef, _, _), Type::VecRef) => (*id, Type::Int, false),
            (Expr::Variable(id, Type::VecF, _, _), Type::VecF) => (*id, Type::F64, false),
            (Expr::Variable(id, Type::VecFRef, _, _), Type::VecFRef) => (*id, Type::F64, false),
            _ => return Err(self.fail("for-in needs a bare vec variable or an int range (start..end)")),
        };
        if self.loop_depth >= 64 { return Err(self.fail("loop nesting limit exceeded (64)")); }
        let (line, col) = self.position();
        let end_var = self.next_var; self.next_var += 1;
        let counter = self.next_var; self.next_var += 1;
        let var = self.next_var; self.next_var += 1;
        let label = self.next_label; self.next_label += 1;
        self.loop_depth += 1;
        let base = self.scopes.len();
        let moved_before = self.moved.clone();
        self.scopes.push(vec![(var, elem_ty), (counter, Type::Int), (end_var, Type::Int)]);
        self.loop_scopes.push(self.scopes.len());
        self.loop_labels.push(Some(label));
        self.names.insert(var_name.to_owned(), (var, false, elem_ty));
        // The container is borrowed for the whole loop.
        let container_depth = self.scopes.len();
        self.borrows.push((v_id, container_depth, BorrowKind::Container));
        let body = lower_body(self, var, elem_ty);
        self.loop_labels.pop();
        self.loop_scopes.pop();
        self.scopes.pop().unwrap();
        self.loop_depth -= 1;
        self.names.remove(var_name);
        self.borrows.retain(|&(id, _, _)| id != v_id);
        let mut body = body?;
        if self.moved.contains(&v_id) { return Err(self.fail("cannot move the vector being iterated (a move of it happens in this loop)")); }
        self.close_loop_moves(base, moved_before, loop_offset)?;
        // Per-iteration element binding, then the user's body.
        let element = match elem_ty {
            Type::StrView => Expr::StrVecIndex(v_id, Box::new(Expr::Variable(counter, Type::Int, line, col)), line, col, by_ref),
            _ => Expr::VecIndex(v_id, Box::new(Expr::Variable(counter, Type::Int, line, col)), line, col, first_ty == Type::VecRef || first_ty == Type::VecFRef, elem_ty),
        };
        body.insert(0, Statement::Declare(var, element));
        result.push(Statement::Declare(end_var, Expr::Len(Box::new(first), line, col)));
        result.push(Statement::Declare(counter, Expr::Number(0)));
        result.push(Statement::For(counter, end_var, body, label, line, col));
        Ok(())
    }

    fn lower_scalar_loop_body(
        &mut self,
        body: &[ae_syntax::statement::Statement],
        slots: &mut HashMap<ae_resolve::DefId, (usize, Type)>,
        label: Option<usize>,
    ) -> Result<Vec<Statement>, ae_span::Diagnostic> {
        self.loop_depth += 1;
        self.loop_labels.push(label);
        let base = self.scopes.len();
        self.loop_scopes.push(base);
        let result = self.lower_scalar_statements(body, slots);
        self.loop_scopes.pop();
        self.loop_labels.pop();
        self.loop_depth -= 1;
        result
    }

    fn lower_scalar_range_bound(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Expr, ae_span::Diagnostic> {
        let (value, ty) = self.lower_scalar_body_expression(expression, slots)?;
        self.seek_scalar_offset(expression.range.end);
        if ty != Type::Int {
            return Err(self.fail("range bounds must be i64"));
        }
        Ok(value)
    }

    fn lower_scalar_condition(
        &mut self,
        condition: &ae_syntax::expression::Expression,
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Expr, ae_span::Diagnostic> {
        let (value, ty) = self.lower_scalar_body_expression(condition, slots)?;
        self.seek_scalar_offset(condition.range.end);
        if ty != Type::Bool {
            return Err(self.fail("condition must be a boolean expression").with_code("E0205"));
        }
        Ok(value)
    }

    pub(super) fn seek_scalar_offset(&mut self, offset: usize) {
        self.pos = self.tokens.partition_point(|token| token.offset < offset);
    }

    fn field_ty_checked(&self, sid: usize, name: &str) -> Result<Type, ae_span::Diagnostic> {
        let defn = &self.structs[sid];
        defn.fields.iter().position(|(n, _)| n == name)
            .map(|fidx| defn.fields[fidx].1)
            .ok_or_else(|| self.fail(&format!("struct '{}' has no field '{}'", defn.name, name)))
    }

    fn lower_scalar_body_expression(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<(Expr, Type), ae_span::Diagnostic> {
        use ae_syntax::expression::ExpressionKind;
        Ok(match &expression.kind {
            ExpressionKind::Atom(Kind::Number(value)) => (Expr::Number(*value), Type::Int),
            ExpressionKind::Atom(Kind::Float(value)) => (Expr::FloatLit(*value), Type::F64),
            ExpressionKind::Atom(Kind::True) => (Expr::BoolLit(true), Type::Bool),
            ExpressionKind::Atom(Kind::False) => (Expr::BoolLit(false), Type::Bool),
            ExpressionKind::Atom(Kind::Text(value)) => {
                self.seek_scalar_offset(expression.range.start);
                let (line, column) = self.position();
                (Expr::StrLit(value.clone(), line, column), Type::String)
            }
            ExpressionKind::Atom(Kind::Ident(name)) => {
                // A name the resolver did not record is a diagnostic, never a
                // panic: a permissive scan can hand us a malformed tree. The
                // classification mirrors the typed ident arm: a struct name
                // gets its construction remedy (the cursor sits after the
                // name, where the typed arm's `fail` lands), everything else is
                // an unknown variable at the name itself.
                let Some(def) = self.resolved_expressions().scalar_reads.get(&expression.range.start).copied() else {
                    // A generic enum template in value position has the typed
                    // route's own reminder; a plain enum name has none (the
                    // typed arm reports it as an unknown variable).
                    if self.generics.names.contains_key(name) {
                        return Err(self.error_at(expression.range.start, &format!("'{name}' is a generic enum; write '{name}[string]::Variant'")));
                    }
                    if self.struct_names.contains_key(name) {
                        self.seek_scalar_offset(expression.range.end);
                        return Err(self.fail(&format!("'{name}' is a struct name; construct it with '{name} {{ ... }}'")));
                    }
                    return Err(self.error_at(expression.range.start, &format!("unknown variable '{name}'")).with_code("E0101"));
                };
                let &(slot, ty) = slots.get(&def).ok_or_else(|| {
                    self.error_at(expression.range.start, &format!("unknown variable '{name}'")).with_code("E0101")
                })?;
                self.seek_scalar_offset(expression.range.start);
                if self.moved.contains(&slot) {
                    return Err(moved_value_hint(self.binding_label(
                        self.fail(&format!("cannot use '{name}' because it was moved")).with_code("E0102"),
                        slot,
                    )));
                }
                let (line, column) = self.position();
                (Expr::Variable(slot, ty, line, column), ty)
            }
            ExpressionKind::Group(inner) => self.lower_scalar_body_expression(inner, slots)?,
            ExpressionKind::AddressCast { binding, binding_range, mutable, .. } => {
                self.seek_scalar_offset(expression.range.start);
                let position = self.position();
                let def = self.resolved_expressions().scalar_reads[&binding_range.start];
                let (slot, ty) = slots[&def];
                self.seek_scalar_offset(binding_range.start);
                if self.moved.contains(&slot) {
                    return Err(moved_value_hint(self.binding_label(
                        self.fail(&format!("cannot use '{binding}' because it was moved")).with_code("E0102"), slot,
                    )));
                }
                let (line, column) = self.position();
                let operand = Expr::Variable(slot, ty, line, column);
                self.seek_scalar_offset(binding_range.end);
                self.check_address_binding(slot, *mutable)?;
                self.take(Kind::As)?;
                let (_, pointee) = self.pointer_cast_pointee(Some(*mutable))?;
                self.lower_address_cast(operand, ty, pointee, *mutable, position)?
            }
            // `raw as *mut f64` (ADR-0209 step A): the address stays, the
            // pointee changes. The base must already be a pointer — value
            // conversions are `to_f64`/`to_i64`'s territory — and the pointee
            // spelling comes from the shared node's range.
            ExpressionKind::PointerCast { base, mutable, pointee } => {
                self.seek_scalar_offset(expression.range.start);
                let (operand, base_type) = self.lower_scalar_body_expression(base, slots)?;
                if !matches!(base_type, Type::Ptr(_) | Type::PtrMut(_)) {
                    self.seek_scalar_offset(expression.range.start);
                    return Err(self.fail(&format!("'as' retags a raw pointer, and {} is not a pointer", base_type.name(self.ctx))));
                }
                let name = self.source.get(pointee.clone())
                    .ok_or_else(|| self.error_at(pointee.start, "cannot read the pointer's pointee type"))?;
                let pointee_type = self.subst.get(name).copied().or_else(|| super::types::scalar_type(name))
                    .ok_or_else(|| self.error_at(pointee.start, "a pointer cast retags to a scalar pointee here"))?;
                let end = self.tokens.partition_point(|token| token.offset < expression.range.end);
                let token = self.tokens.get(end).ok_or_else(|| self.fail("expected the end of the pointer cast"))?;
                let id = self.ctx.intern_pointee(pointee_type);
                let ty = if *mutable { Type::PtrMut(id) } else { Type::Ptr(id) };
                (Expr::PtrCast(Box::new(operand), *mutable, id, token.line, token.column), ty)
            }
            ExpressionKind::Struct { target, fields } => {
                // Validate in source order and emit in layout order.
                let sid = match &target.kind {
                    ExpressionKind::Atom(Kind::Ident(name)) => {
                        if name == "Self" {
                            // `Self { .. }` inside a concrete impl body builds
                            // the impl target's struct layout.
                            match self.impl_self {
                                Some(Type::Struct(sid)) => sid,
                                _ => return Err(self.fail("'Self' does not name a struct here; write the struct's name")),
                            }
                        } else {
                            *self.struct_names.get(name).expect("scalar construction target")
                        }
                    }
                    // A generic instance (`Cell[T] { .. }`): the body was parsed
                    // with `T` substituted, so the registry already holds the
                    // concrete `StructDef` under the instance name.
                    ExpressionKind::Specialization { base, arguments } => {
                        let ExpressionKind::Atom(Kind::Ident(name)) = &base.kind else {
                            unreachable!("scalar construction eligibility");
                        };
                        if let Some(sid) = self.cached_struct_specialization(name, arguments) {
                            sid
                        } else {
                            // The typed route instantiates a generic struct
                            // literal on first sight; do the same here.
                            let &gid = self.generics.struct_names.get(name)
                                .ok_or_else(|| self.fail(&format!("'{name}' is not a generic struct")))?;
                            // A struct literal may name structs (even nested
                            // generic instances) as type arguments, which the
                            // strict call-site reader rejects by design.
                            let shapes = self.loose_shape_arguments(arguments)
                                .ok_or_else(|| self.error_at(target.range.start, &format!("cannot read the type arguments of '{name}'")))?;
                            let args = shapes.iter().map(|shape| self.shape_type(shape, target.range.start))
                                .collect::<Option<Vec<_>>>()
                                .ok_or_else(|| self.error_at(target.range.start, &format!("cannot read the type arguments of '{name}'")))?;
                            self.instantiate_struct(gid, args, target.range.start)?
                        }
                    }
                    _ => unreachable!("scalar construction eligibility"),
                };
                let mut values: Vec<Option<Expr>> = (0..self.structs[sid].fields.len()).map(|_| None).collect();
                for field in fields {
                    let next = self.tokens.partition_point(|token| token.offset < field.name_range.end);
                    let (slot, expected) = self.constructor_field(sid, &field.name, &values, Some(self.tokens[next].offset))?;
                    let (value, actual) = self.lower_scalar_body_expression(&field.value, slots)?;
                    self.check_field_value_type(&field.name, expected, actual, Some(&field.value.range))?;
                    // An owned field value moves into the struct: the literal
                    // owns it, so the source slot's Frees is trimmed (the same
                    // rule the typed struct literal applies).
                    if actual.owned()
                        && let Expr::Variable(slot, ..) = &value {
                            self.seek_scalar_offset(field.value.range.end);
                            self.check_variable_move(*slot)?;
                            self.moved.insert(*slot);
                        }
                    values[slot] = Some(value);
                }
                self.finish_struct_literal(sid, target.range.start, values)?
            }
            ExpressionKind::Field { base, name } => {
                if let ExpressionKind::Atom(Kind::Ident(base_name)) = &base.kind {
                    // A struct parameter's scalar field read: the slot carries
                    // the struct type with its registry id, so the field type
                    // comes straight from the compiler's own table. A
                    // permissive scan can hand the lowering a chain the
                    // resolver never marked (a mutant, or a name the body
                    // never bound): that is a diagnostic, never a panic.
                    let Some(&def) = self.resolved_expressions().scalar_reads.get(&base.range.start) else {
                        return Err(self.error_at(expression.range.start, &format!("unknown variable '{base_name}'")).with_code("E0101"));
                    };
                    let Some(&(slot, struct_ty)) = slots.get(&def) else {
                        return Err(self.error_at(expression.range.start, &format!("unknown variable '{base_name}'")).with_code("E0101"));
                    };
                    let sid = match struct_ty {
                        Type::Struct(sid) | Type::StructRef(sid) | Type::StructMut(sid) => sid,
                        other => return Err(self.error_at(expression.range.start, &format!("type {} has no fields or methods", other.name(self.ctx)))),
                    };
                    self.seek_scalar_offset(base.range.start);
                    if self.moved.contains(&slot) {
                        return Err(moved_value_hint(self.binding_label(
                            self.fail(&format!("cannot use '{base_name}' because it was moved")).with_code("E0102"),
                            slot,
                        )));
                    }
                    let (line, column) = self.position();
                    let fty = self.field_ty_checked(sid, name)?;
                    (Expr::Field(Box::new(Expr::Variable(slot, struct_ty, line, column)), name.clone(), fty), fty)
                } else {
                    // A nested read (`o.inner.x`): the marked base lowers
                    // recursively and its declared type supplies the owner.
                    let (base_expr, base_ty) = self.lower_scalar_body_expression(base, slots)?;
                    let sid = match base_ty {
                        Type::Struct(sid) | Type::StructRef(sid) | Type::StructMut(sid) => sid,
                        // A permissive scan can hand the lowering a chain whose
                        // base is not a struct; the typed route's diagnostics
                        // stay authoritative.
                        Type::Enum(enum_name, _) => {
                            self.seek_scalar_offset(expression.range.start);
                            return Err(self.fail(&format!("enum '{enum_name}' has no fields; bind its variant payloads with 'match'")));
                        }
                        other => {
                            self.seek_scalar_offset(expression.range.start);
                            return Err(self.fail(&format!("type {} has no fields or methods", other.name(self.ctx))));
                        }
                    };
                    let fty = self.field_ty_checked(sid, name)?;
                    (Expr::Field(Box::new(base_expr), name.clone(), fty), fty)
                }
            }
            ExpressionKind::Prefix { operator, operand } => {
                self.seek_scalar_offset(expression.range.start);
                let position = self.position();
                let operand = self.lower_scalar_body_expression(operand, slots)?;
                self.seek_scalar_offset(expression.range.end);
                let fact = self.resolved_expressions().scalar_expression_types.get(&expression.range).copied();
                self.lower_prefix(operator.clone(), operand, position, fact)?
            }
            ExpressionKind::Binary { left, operator, right } => {
                let lhs = self.lower_scalar_body_expression(left, slots)?;
                // A string `+` consumes its left operand before the right
                // side is even read: the typed route's postfix layer marks it
                // when the operator follows, so `s + s` fails on the second
                // read with E0102 (the source's move is already in effect).
                if *operator == Kind::Plus && matches!(lhs.1, Type::String | Type::StrView)
                    && let Expr::Variable(id, Type::String, ..) = &lhs.0 {
                        self.check_variable_move(*id)?;
                        self.moved.insert(*id);
                    }
                self.seek_scalar_offset(left.range.end);
                let position = self.position();
                let rhs = self.lower_scalar_body_expression(right, slots)?;
                self.seek_scalar_offset(expression.range.end);
                // The right operand is consumed by the same `+`; the left
                // one's move is already recorded above.
                let concat = *operator == Kind::Plus
                    && matches!(lhs.1, Type::String | Type::StrView)
                    && matches!(rhs.1, Type::String | Type::StrView);
                if concat
                    && let Expr::Variable(id, Type::String, ..) = &rhs.0 {
                        self.check_variable_move(*id)?;
                        self.moved.insert(*id);
                    }
                let fact = self.resolved_expressions().scalar_expression_types.get(&expression.range).copied();
                self.lower_infix(lhs, operator.clone(), rhs, position, fact)?
            }
            ExpressionKind::Call { callee, arguments } => {
                // A typed-only intrinsic (`ptr_write`, `ptr_drop_in_place`,
                // `bytes`): the typed expression parser owns the operational
                // checks and the shape. Bounded by the following operator.
                if let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind
                    && typed_intrinsic(name) {
                    let end = self.tokens.partition_point(|token| token.offset < expression.range.end);
                    let min = self.tokens.get(end)
                        .and_then(|token| ae_syntax::operator::infix_binding_power(&token.kind))
                        .map_or(0, |(left, _)| left + 1);
                    self.seek_scalar_offset(expression.range.start);
                    let parsed = self.expression(min, true)?;
                    self.seek_scalar_offset(expression.range.end);
                    return Ok(parsed);
                }
                // An associated call (`Point::new(1)`): the typed postfix layer
                // builds the instance and owns the pinned diagnostics. Bounded
                // by the following operator, like a chained method receiver.
                if let ExpressionKind::Qualified { .. } = &callee.kind {
                    let end = self.tokens.partition_point(|token| token.offset < expression.range.end);
                    let min = self.tokens.get(end)
                        .and_then(|token| ae_syntax::operator::infix_binding_power(&token.kind))
                        .map_or(0, |(left, _)| left + 1);
                    self.seek_scalar_offset(expression.range.start);
                    let parsed = self.expression(min, true)?;
                    self.seek_scalar_offset(expression.range.end);
                    return Ok(parsed);
                }
                if let ExpressionKind::Field { base, name: method_name } = &callee.kind {
                    // A method on a temporary or chained result
                    // (`start.shifted(1, 2).shifted(0, -6)`): the typed postfix
                    // layer owns the receiver semantics and the diagnostics;
                    // delegate the whole call. The binding power comes from the
                    // operator that follows this operand in the shared tree, so
                    // the typed parse stops at the operand's end and the
                    // enclosing operator (`... .value() == 11`) stays with the
                    // shared lowering.
                    if !matches!(base.kind, ExpressionKind::Atom(Kind::Ident(_))) {
                        let end = self.tokens.partition_point(|token| token.offset < expression.range.end);
                        let min = self.tokens.get(end)
                            .and_then(|token| ae_syntax::operator::infix_binding_power(&token.kind))
                            .map_or(0, |(left, _)| left + 1);
                        self.seek_scalar_offset(expression.range.start);
                        let parsed = self.expression(min, true)?;
                        // The typed parse consumed exactly this operand; the
                        // surrounding shared nodes own what follows.
                        self.seek_scalar_offset(expression.range.end);
                        return Ok(parsed);
                    }
                    // A method receiver is a borrow, not a value read: its
                    // slot comes from `names`. The re-check admitted only
                    // calls `resolve_inherent_method` answers, so this path
                    // and the gate agree.
                    if let ExpressionKind::Atom(Kind::Ident(receiver)) = &base.kind
                        && let Some(&(slot, _, ty)) = self.names.get(receiver)
                            && !matches!(ty, Type::Ptr(_) | Type::PtrMut(_)) {
                                if let Some((signature, _, _)) = self.resolve_inherent_method(method_name, ty) {
                                    return self.lower_method_call(method_name, slot, ty, signature, expression, arguments, slots);
                                }
                                // A generic-instance receiver resolves lazily
                                // here (the gate is permissive and stateless);
                                // a missing method reproduces the typed route's
                                // pinned diagnostics.
                                let sid = match ty {
                                    Type::Struct(sid) | Type::StructMut(sid) | Type::StructRef(sid) => Some((sid, false)),
                                    Type::Enum(_, sid) => Some((sid, true)),
                                    _ => None,
                                };
                                if let Some((sid, is_enum)) = sid {
                                    let origin = if is_enum {
                                        self.generics.enum_origin.get(&sid).cloned()
                                    } else {
                                        self.generics.struct_origin.get(&sid).cloned()
                                    };
                                    if let Some((gid, origin_args)) = origin {
                                        let template = if is_enum {
                                            self.generics.defs[gid].name.clone()
                                        } else {
                                            self.generics.struct_defs[gid].name.clone()
                                        };
                                        let Some(gimpl) = self.generics.generic_impls.get(&template).cloned() else {
                                            return Err(self.error_at(callee.range.end, &format!("the impl for '{template}' has no method '{method_name}'")));
                                        };
                                        let mut names: Vec<String> = gimpl.methods.iter().map(|m| m.name.clone()).collect();
                                        names.sort();
                                        let Some(mdef) = gimpl.methods.iter().find(|m| m.name == *method_name) else {
                                            return Err(self.error_at(callee.range.end, &format!("the impl for '{template}' has no method '{method_name}'; it has {}", names.join(", "))));
                                        };
                                        if mdef.receiver.is_some() {
                                            let (id, params, ret) = self.instantiate_impl_method(&template, method_name, sid, gid, &origin_args, expression.range.start)?;
                                            let signature = Signature { id, params, ret, file: self.file.clone(), public: true, extern_c: false };
                                            return self.lower_method_call(method_name, slot, ty, signature, expression, arguments, slots);
                                        }
                                        return Err(self.error_at(callee.range.end, &format!("'{template}::{method_name}' is an associated function, so it takes no receiver")));
                                    }
                                    let type_name = match ty {
                                        Type::Enum(name, _) => name.to_owned(),
                                        _ => self.structs[sid].name.clone(),
                                    };
                                    // The table knows the method but its entry
                                    // declares no receiver: a value call is the
                                    // typed route's associated-function error,
                                    // not a missing method.
                                    if self.generics.inherent.get(&format!("{type_name}#{method_name}"))
                                        .is_some_and(|definition| definition.receiver.is_none())
                                    {
                                        return Err(self.error_at(callee.range.end, &format!("'{type_name}::{method_name}' is an associated function, so it takes no receiver; call it as '{type_name}::{method_name}(...)'")));
                                    }
                                    let mut names: Vec<String> = self.generics.inherent.keys()
                                        .filter(|key| key.starts_with(&format!("{type_name}#")))
                                        .filter_map(|key| key.split('#').nth(1).map(|name| name.to_owned()))
                                        .collect();
                                    names.sort();
                                    if names.is_empty() {
                                        return Err(self.error_at(callee.range.end, &format!("type '{type_name}' has no methods; declare one in 'impl {type_name} {{ ... }}'")));
                                    }
                                    return Err(self.error_at(callee.range.end, &format!("type '{type_name}' has no method '{method_name}'; it has {}", names.join(", "))));
                                }
                                return Err(self.fail(&format!("type {} has no fields or methods", ty.name(self.ctx))));
                            }
                    // The receiver's slot comes from `names`: a pointer binding
                    // the resolver could not spell yet (the result of
                    // `buf.offset(1)`, say) still has an authoritative slot by
                    // the time this statement lowers.
                    let (operand, ty) = if let ExpressionKind::Atom(Kind::Ident(receiver)) = &base.kind {
                        let &(slot, _, ty) = self.names.get(receiver).ok_or_else(|| {
                            self.fail(&format!("unknown variable '{receiver}'")).with_code("E0101")
                        })?;
                        self.seek_scalar_offset(base.range.start);
                        let (line, column) = self.position();
                        (Expr::Variable(slot, ty, line, column), ty)
                    } else {
                        self.lower_scalar_body_expression(base, slots)?
                    };
                    let (Type::Ptr(id) | Type::PtrMut(id)) = ty else {
                        return Err(self.fail(&format!("type {} has no fields or methods", ty.name(self.ctx))));
                    };
                    let method = self.tokens.partition_point(|token| token.offset < callee.range.end) - 1;
                    let method_offset = self.tokens[method].offset;
                    // `offset(n)` / `byte_offset(n)` (ADR-0209 step A): the
                    // address moves by n elements or n bytes, the result is
                    // the same pointer type. The gate's marks admitted only
                    // these three names, so anything else is `deref`'s
                    // diagnostic below.
                    if method_name == "offset" || method_name == "byte_offset" {
                        if self.unsafe_depth == 0 {
                            return Err(self.error_at(method_offset, "pointer arithmetic requires 'unsafe'"));
                        }
                        let [count] = arguments.as_slice() else {
                            self.seek_scalar_offset(callee.range.end);
                            return Err(self.fail(&format!("{method_name} takes an i64 count")));
                        };
                        let (count_value, count_ty) = self.lower_scalar_body_expression(count, slots)?;
                        if count_ty != Type::Int {
                            self.seek_scalar_offset(count.range.end);
                            return Err(self.fail(&format!("{method_name} takes an i64 count")));
                        }
                        self.seek_scalar_offset(expression.range.end);
                        let (line, column) = self.position();
                        let bytewise = method_name == "byte_offset";
                        return Ok((Expr::PtrOffset(Box::new(operand), Box::new(count_value), bytewise, line, column), ty));
                    }
                    if method_name != "deref" {
                        // The typed route consumes the name and fails on the
                        // token that follows it (the `(`), so the span lands
                        // there.
                        self.seek_scalar_offset(callee.range.end);
                        return Err(self.fail(&format!("pointers have no method '{method_name}'")));
                    }
                    self.check_pointer_deref(method_offset)?;
                    self.seek_scalar_offset(expression.range.end);
                    let (line, column) = self.position();
                    return Ok((Expr::DerefPtr(Box::new(operand), line, column), self.ctx.ptr_pointee(id)));
                }
                if let ExpressionKind::BracketedCallTarget { base, .. } = &callee.kind {
                    let ExpressionKind::Atom(Kind::Ident(name)) = &base.kind else {
                        return Err(self.fail("this call form is not supported by the body route"));
                    };
                    // The C layout queries are spelled like a generic call but read a
                    // repr(C) layout: cursor on the `[` for the shared reader.
                    if matches!(name.as_str(), "c_size_of" | "c_align_of" | "c_offset_of") {
                        self.seek_scalar_offset(base.range.start);
                        self.pos += 1;
                        let (value, ty) = self.c_layout_expr(name)?;
                        self.seek_scalar_offset(expression.range.end);
                        return Ok((value, ty));
                    }
                    let Some(&gid) = self.generics.fn_names.get(name) else {
                        return Err(self.error_at(base.range.start, &format!("'{name}' is not a generic function")));
                    };
                    // An explicit generic call: the type arguments choose the
                    // instance. Instantiate first (the same instance map the
                    // typed route fills), then lower the argument list against
                    // the substituted signature.
                    self.seek_scalar_offset(base.range.start);
                    // `seek_scalar_offset` lands on the name; the type
                    // argument reader starts at the `[` that follows.
                    self.pos += 1;
                    let type_args = self.generic_call_type_arguments(base.range.start)?;
                    let (id, params, ret) = self.instantiate_fn(gid, type_args, base.range.start)?;
                    // The argument list follows the type arguments; lower it
                    // against the substituted signature with the shared
                    // argument rules (borrows, views, moves).
                    let budget = 128usize.checked_sub(self.expression_nodes)
                        .ok_or_else(|| self.fail("expression node limit exceeded"))?;
                    let remaining = 128usize.checked_sub(self.expression_depth).map(|depth| depth.min(64))
                        .ok_or_else(|| self.fail("expression depth limit exceeded"))?;
                    let mut nodes_left = budget;
                    let mut effects = SharedExpressionEffects::default();
                    let shared = self.lower_shared_call_arguments(arguments, &params, remaining, &mut nodes_left, &mut effects);
                    let args = match shared {
                        Some(args) => {
                            self.commit_shared_effects(effects);
                            self.expression_nodes += budget - nodes_left;
                            args
                        }
                        // The generic template's own argument list may not be
                        // indexed in the shared tree; the typed argument
                        // reader still owns the exact diagnostics (and its
                        // error falls the whole body back to the typed route).
                        None => {
                            let open = self.tokens.partition_point(|token| token.offset < callee.range.end);
                            self.pos = open;
                            self.call_arguments(name, &params)?
                        }
                    };
                    self.seek_scalar_offset(expression.range.end);
                    return Ok((Expr::Call(id, args, ret), ret));
                }
                if let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind {
                    if resolved_builtin(name) {
                        return self.lower_resolved_builtin(name, expression, callee, arguments, slots);
                    }
                    // A call on a closure value: the typed machinery reads the
                    // captured slots at the call site (the leading arguments)
                    // and parses the visible argument list.
                    if matches!(self.names.get(name), Some(&(_, _, Type::Closure(_)))) {
                        self.seek_scalar_offset(callee.range.end);
                        let (value, ty) = self.closure_call(name, callee.range.start)?;
                        self.seek_scalar_offset(expression.range.end);
                        return Ok((value, ty));
                    }
                    // A call through a function-typed value: the binding's
                    // slot names the pointer the caller passed (ADR-0189).
                    if let Some(&(slot, _, Type::Fn(sig))) = self.names.get(name) {
                        let (params, ret) = self.ctx.fn_sig(sig);
                        let mut args = Vec::with_capacity(arguments.len());
                        for (index, argument) in arguments.iter().enumerate() {
                            self.seek_scalar_offset(argument.range.start);
                            let Some(expected) = params.get(index).copied() else {
                                return Err(self.fail(&format!("too many arguments to '{name}'")));
                            };
                            let (value, actual) = self.lower_scalar_body_expression(argument, slots)?;
                            // A `&string` parameter takes a view (the typed
                            // `typed_call_argument` rule); everything else is
                            // a value, and an owned one moves.
                            if matches!(expected, Type::StrView) {
                                if !matches!(actual, Type::String | Type::StrView) {
                                    self.seek_scalar_offset(argument.range.end);
                                    return Err(self.fail(&format!("argument must be a string or string view, found {}", actual.name(self.ctx))));
                                }
                                args.push(Argument::View(value));
                                continue;
                            }
                            self.check_value_argument_type(name, index, expected, actual, Some(&argument.range))?;
                            if actual.owned()
                                && let Expr::Variable(source, ..) = &value {
                                    self.moved.insert(*source);
                                }
                            args.push(Argument::Value(value));
                        }
                        if args.len() != params.len() {
                            return Err(self.fail(&format!("function '{name}' expects {} arguments, got {}", params.len(), args.len())));
                        }
                        self.seek_scalar_offset(expression.range.end);
                        return Ok((Expr::FnCall(slot, args, ret, sig), ret));
                    }
                }
                let ExpressionKind::Atom(Kind::Ident(name)) = &callee.kind else {
                    // Only a bare name or a field receiver reaches this arm;
                    // anything else is a call form the route does not take.
                    return Err(self.fail("this call form is not supported by the body route"));
                };
                let signature = match self.functions.get(name).cloned() {
                    Some(signature) => signature,
                    // Not a plain function: a trait method of one of this
                    // instance's constrained parameters. The substitution picks
                    // the concrete impl, so the call stays monomorphized.
                    None => self.resolve_trait_call(name, callee.range.start)?,
                };
                if (signature.extern_c || self.ctx.is_unsafe_fn(signature.id)) && self.unsafe_depth == 0 {
                    return Err(self.error_at(callee.range.start, &format!("call to unsafe function '{name}' requires 'unsafe'")).with_code("E0701"));
                }
                let mut args = Vec::with_capacity(arguments.len());
                for (index, argument) in arguments.iter().enumerate() {
                    self.seek_scalar_offset(argument.range.start);
                    if index >= 64 {
                        return Err(self.fail("function argument limit exceeded (64)"));
                    }
                    let &expected = signature.params.get(index)
                        .ok_or_else(|| self.fail(&format!("too many arguments to '{name}'")))?;
                    // A function-typed parameter takes a closure literal (or
                    // a named function) as `Argument::FnPtr`; the typed reader
                    // owns the shape check and the generated function.
                    if let Type::Fn(callable) = expected {
                        self.seek_scalar_offset(argument.range.start);
                        let converted = self.function_value_argument(name, index + 1, callable)?;
                        args.push(converted);
                        continue;
                    }
                    // The shared tree records `&x` as a borrow argument.
                    // Only a borrow parameter consumes it; anywhere else the
                    // typed route's expression parser rejects it (a pointer
                    // cast is the only legal use of `&expr`).
                    if matches!(argument.kind, ExpressionKind::BorrowArgument { .. })
                        && !matches!(expected, Type::VecRef | Type::StrVecRef | Type::VecFRef | Type::Array(_) | Type::StructRef(_) | Type::StructMut(_))
                    {
                        self.seek_scalar_offset(argument.range.start);
                        return Err(self.fail("a borrow expression must be cast to a pointer"));
                    }
                    // A container or struct borrow takes the binding's
                    // address (`Argument::Borrow`); the same classify rules,
                    // messages and statement-scoped borrow registers the typed
                    // route uses.
                    if matches!(expected, Type::VecRef | Type::StrVecRef | Type::VecFRef | Type::Array(_) | Type::StructRef(_) | Type::StructMut(_)) {
                        let ExpressionKind::BorrowArgument { name: borrowed, mutable: argument_mutability, name_range, .. } = &argument.kind else {
                            self.seek_scalar_offset(argument.range.end);
                            return Err(self.fail("expected Amp, found the argument"));
                        };
                        if matches!(expected, Type::StructMut(_)) && !*argument_mutability {
                            self.seek_scalar_offset(name_range.start);
                            return Err(self.fail("this parameter borrows mutably; write the argument as '&mut name'"));
                        }
                        self.seek_scalar_offset(argument.range.end);
                        let &(slot, binding_mutable, actual) = self.names.get(borrowed).ok_or_else(|| {
                            self.fail(&format!("unknown variable '{borrowed}'")).with_code("E0101")
                        })?;
                        let Some(borrow) = super::declarations::CallBorrow::classify(expected, actual) else {
                            let shape = match expected {
                                Type::VecRef => Type::Vec, Type::StrVecRef => Type::StrVec, Type::VecFRef => Type::VecF, _ => expected,
                            };
                            return Err(self.fail(&format!("argument must be {}, found {}", shape.name(self.ctx), actual.name(self.ctx))));
                        };
                        if self.moved.contains(&slot) {
                            return Err(moved_value_hint(self.binding_label(
                                self.fail(&format!("cannot borrow '{borrowed}' because it was moved")).with_code("E0102"), slot)));
                        }
                        match (expected, borrow) {
                            (Type::StructRef(_), super::declarations::CallBorrow::Struct { address }) => {
                                if self.mut_borrows.iter().any(|&(bid, _)| bid == slot) {
                                    return Err(self.fail(&format!("cannot borrow '{borrowed}' while it is mutably borrowed in this statement")));
                                }
                                // The borrow lives only as long as this call.
                                if !self.is_borrowed(slot) {
                                    self.call_borrows.push((slot, self.scopes.len()));
                                }
                                args.push(Argument::Borrow(slot, address));
                            }
                            (Type::StructRef(_), super::declarations::CallBorrow::Forward) => {
                                args.push(Argument::Borrow(slot, false));
                            }
                            (Type::StructMut(_), super::declarations::CallBorrow::Struct { address }) => {
                                if !binding_mutable {
                                    return Err(self.immutable_binding_error(self.fail(&format!("cannot borrow '{borrowed}' mutably: it was declared without 'var'")).with_code("E0103"), slot));
                                }
                                if self.is_borrowed(slot) {
                                    // The callee may write any field, so the
                                    // call releases the viewed bytes at the
                                    // call point; with only view borrows
                                    // active the conflict defers to the
                                    // graph's ordered adjudication.
                                    if !self.only_view_borrows_active(slot) {
                                        return Err(self.fail(&format!("cannot borrow '{borrowed}' mutably while it is borrowed in this statement")));
                                    }
                                    // The conflict points at the borrowed name, the token the
                                    // typed parser's cursor ends on (col of `d`
                                    // in `&mut d`, not the `&`).
                                    self.deferred_write_conflicts.push((slot, name_range.start, borrowed.clone(), "mutate", None));
                                }
                                self.mut_borrows.push((slot, self.scopes.len()));
                                args.push(Argument::Borrow(slot, address));
                            }
                            (Type::VecRef | Type::VecFRef | Type::StrVecRef | Type::Array(_), borrow) => {
                                // An owned vector takes the address; a
                                // parameter already spelled `&vec` forwards
                                // its pointer unchanged.
                                args.push(Argument::Borrow(slot, borrow.address()));
                            }
                            _ => {
                                let shape = match expected {
                                    Type::VecRef => Type::Vec, Type::StrVecRef => Type::StrVec, Type::VecFRef => Type::VecF, _ => expected,
                                };
                                return Err(self.fail(&format!("argument must be {}, found {}", shape.name(self.ctx), actual.name(self.ctx))));
                            }
                        }
                        continue;
                    }
                    let (value, actual) = if self.resolved_expressions().enum_constructors.contains(&argument.range.start) {
                        // An enum construction as an argument (`area(Shape::Circle(3))`):
                        // the typed expression parser owns the variant layout.
                        self.seek_scalar_offset(argument.range.start);
                        self.expression(0, true)?
                    } else {
                        self.lower_scalar_body_expression(argument, slots)?
                    };
                    // A `&string` parameter is a read-only view: the argument
                    // lowers as `Argument::View` and nothing moves (the typed
                    // route's `typed_call_argument` for a `StrView`).
                    if matches!(expected, Type::StrView) {
                        if !matches!(actual, Type::String | Type::StrView) {
                            self.seek_scalar_offset(argument.range.end);
                            return Err(self.fail(&format!("argument must be a string or string view, found {}", actual.name(self.ctx))));
                        }
                        args.push(Argument::View(value));
                        continue;
                    }
                    if let (Some(atom), Expr::Variable(slot, ..)) = (receiver_value(argument), &value) {
                        self.seek_scalar_offset(atom.range.end);
                        self.check_variable_move(*slot)?;
                    }
                    self.check_value_argument_type(name, index, expected, actual, Some(&argument.range))?;
                    // An owned aggregate passed by value moves: later uses
                    // are rejected (E0102), and the drops pass trims the
                    // Frees lists for this slot.
                    if actual.owned()
                        && let Expr::Variable(slot, ..) = &value {
                            self.moved.insert(*slot);
                        }
                    args.push(Argument::Value(value));
                }
                self.seek_scalar_offset(expression.range.end);
                if args.len() != signature.params.len() {
                    return Err(self.fail(&format!("function '{name}' expects {} arguments, got {}", signature.params.len(), args.len())));
                }
                (Expr::Call(signature.id, args, signature.ret), signature.ret)
            }
            ExpressionKind::Specialization { .. } => {
                // A bracket the syntax tree kept as a specialization on a
                // value name (`arr[j]`): the typed expression parser
                // disambiguates it against the known names and indexes.
                self.seek_scalar_offset(expression.range.start);
                self.expression(0, false)?
            }
            ExpressionKind::Index { base, index } => {
                let def = self.resolved_expressions().scalar_reads[&base.range.start];
                let (slot, ty) = slots[&def];
                self.seek_scalar_offset(base.range.end);
                let (line, column) = self.position();
                match ty {
                    Type::Array(len) => {
                        let value = self.lower_scalar_array_index(index, slots)?;
                        (Expr::Index(slot, len, Box::new(value), line, column), Type::Int)
                    }
                    Type::VecRef | Type::VecFRef | Type::Vec | Type::VecF => {
                        let (value, index_ty) = self.lower_scalar_body_expression(index, slots)?;
                        if index_ty != Type::Int {
                            self.seek_scalar_offset(index.range.end);
                            return Err(self.fail("vector index must be an integer"));
                        }
                        let element = if matches!(ty, Type::VecFRef | Type::VecF) { Type::F64 } else { Type::Int };
                        let borrowed = matches!(ty, Type::VecRef | Type::VecFRef);
                        (Expr::VecIndex(slot, Box::new(value), line, column, borrowed, element), element)
                    }
                    Type::StrVecRef | Type::StrVec => {
                        // A borrowed string vector's element is a view; an
                        // owned one hands the element's view too.
                        let (value, index_ty) = self.lower_scalar_body_expression(index, slots)?;
                        if index_ty != Type::Int {
                            self.seek_scalar_offset(index.range.end);
                            return Err(self.fail("vector index must be an integer"));
                        }
                        let by_ref = matches!(ty, Type::StrVecRef);
                        (Expr::StrVecIndex(slot, Box::new(value), line, column, by_ref), Type::StrView)
                    }
                    _ => unreachable!("resolved array, borrowed or owned numeric/string vector binding"),
                }
            }
            ExpressionKind::Vector(elements) => {
                // A vector literal used as a value (`a = vec[9];`): the same
                // construction the Declare arm builds.
                self.seek_scalar_offset(expression.range.start);
                let (line, column) = self.position();
                let mut values = Vec::with_capacity(elements.len());
                let mut element_type = None;
                for (index, element) in elements.iter().enumerate() {
                    if index >= 65536 {
                        self.seek_scalar_offset(element.range.start);
                        return Err(self.fail("vector element limit exceeded (65536)"));
                    }
                    let (value, ty) = self.lower_scalar_body_expression(element, slots)?;
                    self.seek_scalar_offset(element.range.end);
                    element_type = Some(self.check_numeric_vector_element(element_type, ty)?);
                    values.push(value);
                }
                let ty = if element_type == Some(Type::F64) { Type::VecF } else { Type::Vec };
                (Expr::VecLit(values, ty, line, column), ty)
            }
            _ => unreachable!("scalar expression eligibility"),
        })
    }

    /// The pure value builtins, lowered with the same `Expr` variants and the
    /// same argument rules the typed route applies (positions included). The
    /// resolved Call arm dispatches here for `resolved_builtin` names; no
    /// `Argument::Borrow` machinery is involved because every one of these
    /// builtins only reads its arguments.
    fn lower_resolved_builtin(
        &mut self,
        name: &str,
        expression: &ae_syntax::expression::Expression,
        callee: &ae_syntax::expression::Expression,
        arguments: &[ae_syntax::expression::Expression],
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<(Expr, Type), ae_span::Diagnostic> {
        use ae_syntax::expression::ExpressionKind;
        (|| -> Result<(Expr, Type), ae_span::Diagnostic> {
            self.seek_scalar_offset(callee.range.start);
            let (line, column) = self.position();
            match name {
                "len" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let argument = &arguments[0];
                    // The typed route's fast path: a bare local's array length
                    // is a compile-time constant, and a vector or map length
                    // borrows the slot. Either way the variable must not be
                    // moved.
                    if let ExpressionKind::Atom(Kind::Ident(argument_name)) = &argument.kind
                        && let Some(&def) = self.resolved_expressions().scalar_reads.get(&argument.range.start)
                        && let Some(&(slot, ty)) = slots.get(&def)
                    {
                                self.seek_scalar_offset(argument.range.start);
                                if self.moved.contains(&slot) {
                                    return Err(moved_value_hint(self.binding_label(
                                        self.fail(&format!("cannot use '{argument_name}' because it was moved")).with_code("E0102"), slot)));
                                }
                                if let Type::Array(n) = ty {
                                    self.seek_scalar_offset(expression.range.end);
                                    return Ok((Expr::Number(n as i64), Type::Int));
                                }
                                if matches!(ty, Type::Vec | Type::StrVec | Type::VecRef | Type::StrVecRef | Type::Map) {
                                    // The typed fast path stamps the operand with
                                    // the `len` call's position, not the variable's.
                                    let value = Expr::Variable(slot, ty, line, column);
                                    self.seek_scalar_offset(expression.range.end);
                                    return Ok((Expr::Len(Box::new(value), line, column), Type::Int));
                                }
                    }
                    let (value, ty) = self.lower_scalar_body_expression(argument, slots)?;
                    if ty == Type::Tokens {
                        // The typed route consumes len's `)` before it checks
                        // the operand type, so the cursor is one token past it.
                        self.seek_scalar_offset(expression.range.end);
                        return Err(self.fail("a token cursor has no len: counting would consume the stream, so use split(text, sep) when the count is needed"));
                    }
                    if !matches!(ty, Type::String | Type::StrView | Type::Vec | Type::StrVec | Type::StrVecRef | Type::VecF | Type::VecRef | Type::VecFRef | Type::Map | Type::MapF) {
                        self.seek_scalar_offset(expression.range.end);
                        return Err(self.fail("len expects a string, vec, map, or array variable"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::Len(Box::new(value), line, column), Type::Int))
                }
                "to_string" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let argument = &arguments[0];
                    let (value, actual) = self.lower_scalar_body_expression(argument, slots)?;
                    let value = match actual {
                        Type::Int => Expr::ToString(Box::new(value), line, column),
                        Type::F64 => Expr::FloatToString(Box::new(value), line, column),
                        _ => {
                            self.seek_scalar_offset(argument.range.end);
                            return Err(self.fail("to_string expects an i64 or f64"));
                        }
                    };
                    self.seek_scalar_offset(expression.range.end);
                    Ok((value, Type::String))
                }
                "to_f64" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let argument = &arguments[0];
                    let (value, actual) = self.lower_scalar_body_expression(argument, slots)?;
                    if actual != Type::Int {
                        self.seek_scalar_offset(argument.range.end);
                        return Err(self.fail("to_f64 expects an i64"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::ToF64(Box::new(value), line, column), Type::F64))
                }
                "trim" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let argument = &arguments[0];
                    let (value, actual) = self.lower_scalar_body_expression(argument, slots)?;
                    if !matches!(actual, Type::String | Type::StrView) {
                        self.seek_scalar_offset(argument.range.end);
                        return Err(self.fail("trim expects a string or string view argument"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::Trim(Box::new(value), line, column), Type::String))
                }
                "slice" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 3)?;
                    let (text, start, end) = (&arguments[0], &arguments[1], &arguments[2]);
                    let (first, first_ty) = self.lower_scalar_body_expression(text, slots)?;
                    if !matches!(first_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(text.range.end);
                        return Err(self.fail("slice expects a string or string view argument"));
                    }
                    self.seek_scalar_offset(start.range.start);
                    let (from, from_ty) = self.lower_scalar_body_expression(start, slots)?;
                    if from_ty != Type::Int {
                        self.seek_scalar_offset(start.range.end);
                        return Err(self.fail("slice start must be i64"));
                    }
                    self.seek_scalar_offset(end.range.start);
                    let (to, to_ty) = self.lower_scalar_body_expression(end, slots)?;
                    if to_ty != Type::Int {
                        self.seek_scalar_offset(end.range.end);
                        return Err(self.fail("slice end must be i64"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::Slice(Box::new(first), Box::new(from), Box::new(to), line, column), Type::String))
                }
                "split" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let (text, separator) = (&arguments[0], &arguments[1]);
                    let (first, first_ty) = self.lower_scalar_body_expression(text, slots)?;
                    if !matches!(first_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(text.range.end);
                        return Err(self.fail("split expects a string or string view argument"));
                    }
                    self.seek_scalar_offset(separator.range.start);
                    let (second, second_ty) = self.lower_scalar_body_expression(separator, slots)?;
                    if !matches!(second_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(separator.range.end);
                        return Err(self.fail("split expects a string or string view separator"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::Split(Box::new(first), Box::new(second), line, column), Type::StrVec))
                }
                "contains" | "starts_with" | "ends_with" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let (text, pattern) = (&arguments[0], &arguments[1]);
                    let (first, first_ty) = self.lower_scalar_body_expression(text, slots)?;
                    if !matches!(first_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(text.range.end);
                        return Err(self.fail(&format!("{name} expects a string or string view argument")));
                    }
                    self.seek_scalar_offset(pattern.range.start);
                    let (second, second_ty) = self.lower_scalar_body_expression(pattern, slots)?;
                    if !matches!(second_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(pattern.range.end);
                        return Err(self.fail(&format!("{name} expects a string or string view pattern")));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    let value = match name {
                        "contains" => Expr::Contains(Box::new(first), Box::new(second), line, column),
                        "starts_with" => Expr::StartsWith(Box::new(first), Box::new(second)),
                        _ => Expr::EndsWith(Box::new(first), Box::new(second)),
                    };
                    Ok((value, Type::Bool))
                }
                "map_new" | "f64map_new" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 0)?;
                    let (kind, ty) = if name == "map_new" { (MapKind::Int, Type::Map) } else { (MapKind::Float, Type::MapF) };
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::MapNew(line, column, kind), ty))
                }
                "map_get" | "map_has" | "map_remove" | "map_inc" | "keys" => {
                    let expected = if matches!(name, "map_inc") { 3 } else if matches!(name, "keys") { 1 } else { 2 };
                    self.expect_resolved_builtin_arity(expression, arguments, expected)?;
                    // The map argument is a bare map variable, or a map field of
                    // a struct *variable* (the borrow stays statement-scoped).
                    let (map, map_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    let kind = map_kind_of(map_ty);
                    let place = match (&map, map_ty) {
                        (Expr::Variable(id, inner, ..), t) if *inner == t && matches!(t, Type::Map | Type::MapF) => MapPlace::Var(*id, kind),
                        (Expr::Field(base, fname, t), _) if matches!(*t, Type::Map | Type::MapF) => match **base {
                            Expr::Variable(id, ..) => MapPlace::Field(id, fname.clone(), kind),
                            _ => {
                                self.seek_scalar_offset(arguments[0].range.end);
                                return Err(self.fail(&format!("{name} needs a map variable or a field of one")));
                            }
                        },
                        _ => {
                            self.seek_scalar_offset(arguments[0].range.end);
                            return Err(self.fail(&format!("{name} expects a map variable or a field of one")));
                        }
                    };
                    if self.moved.contains(&place.slot()) {
                        return Err(self.binding_label(self.fail(&format!("cannot use '{name}' on a moved map")).with_code("E0102"), place.slot()));
                    }
                    if matches!(name, "keys") {
                        self.seek_scalar_offset(expression.range.end);
                        return Ok((Expr::MapKeys(place, line, column), Type::StrVec));
                    }
                    let (key, key_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if !matches!(key_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        let message = if matches!(name, "map_inc") { "map_inc expects a string key".to_owned() } else { format!("{name} expects a string key") };
                        return Err(self.fail(&message));
                    }
                    if matches!(name, "map_inc") {
                        let (delta, delta_ty) = self.lower_scalar_body_expression(&arguments[2], slots)?;
                        if delta_ty != map_element_ty(kind) {
                            self.seek_scalar_offset(arguments[2].range.end);
                            return Err(self.fail(&format!("map_inc expects a {} delta", map_element_ty(kind).name(self.ctx))));
                        }
                        self.seek_scalar_offset(expression.range.end);
                        return Ok((Expr::MapInc(place, Box::new(key), Box::new(delta), line, column), map_element_ty(kind)));
                    }
                    let ty = match name {
                        "map_get" => self.prelude_type(if kind == MapKind::Float { "FloatResult" } else { "IntResult" }),
                        _ => Type::Bool,
                    };
                    let value = match name {
                        "map_get" => Expr::MapGet(place, Box::new(key), ty, line, column),
                        "map_has" => Expr::MapHas(place, Box::new(key), line, column),
                        _ => Expr::MapRemove(place, Box::new(key), line, column),
                    };
                    self.seek_scalar_offset(expression.range.end);
                    Ok((value, ty))
                }
                "cell_new" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (seed, seed_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if seed_ty != Type::Int {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("cell_new expects an i64 seed"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::AtomicNew(Box::new(seed)), Type::Atomic))
                }
                "cell_load" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (handle, handle_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if handle_ty != Type::Atomic {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("cell_load expects an atomic handle"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::AtomicLoad(Box::new(handle)), Type::Int))
                }
                "cell_add" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let (handle, handle_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if handle_ty != Type::Atomic {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("cell_add expects an atomic handle"));
                    }
                    let (value, value_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if value_ty != Type::Int {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail("cell_add expects an i64 delta"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::AtomicAdd(Box::new(handle), Box::new(value)), Type::Int))
                }
                "cell_cas" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 3)?;
                    let (handle, handle_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if handle_ty != Type::Atomic {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("cell_cas expects an atomic handle"));
                    }
                    let (expect, expect_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if expect_ty != Type::Int {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail("cell_cas expects an i64 expected value"));
                    }
                    let (new, new_ty) = self.lower_scalar_body_expression(&arguments[2], slots)?;
                    if new_ty != Type::Int {
                        self.seek_scalar_offset(arguments[2].range.end);
                        return Err(self.fail("cell_cas expects an i64 new value"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::AtomicCas(Box::new(handle), Box::new(expect), Box::new(new)), Type::Bool))
                }
                "shared_new" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (value, value_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if matches!(value_ty, Type::Shared(_)) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("shared_new cannot wrap another shared handle (no handle towers)"));
                    }
                    if !matches!(value_ty, Type::Int | Type::Bool | Type::F64 | Type::Atomic) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("shared_new in v1 expects a copy-semantics value (i64, bool, f64, or atomic)"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    let sid = self.ctx.intern_shared(value_ty);
                    Ok((Expr::SharedNew(Box::new(value), sid), Type::Shared(sid)))
                }
                "shared_get" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (handle, handle_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    let sid = match handle_ty {
                        Type::Shared(sid) => sid,
                        _ => {
                            self.seek_scalar_offset(arguments[0].range.end);
                            return Err(self.fail("shared_get expects a shared handle"));
                        }
                    };
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::SharedGet(Box::new(handle), sid), self.ctx.shared_types_get(sid)))
                }
                "mutex_new" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 0)?;
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::MutexNew, Type::Mutex))
                }
                "mutex_lock" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (handle, handle_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if handle_ty != Type::Mutex {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("mutex_lock expects a mutex handle"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::MutexLock(Box::new(handle)), Type::Guard))
                }
                "chan_new" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (capacity, capacity_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if capacity_ty != Type::Int {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("chan_new expects an i64 capacity"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::ChanNew(Box::new(capacity)), Type::Chan))
                }
                "chan_send" | "chan_try_send" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let (channel, channel_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if channel_ty != Type::Chan {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail(&format!("{name} expects a chan handle")));
                    }
                    let (value, value_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if value_ty != Type::Int {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail(&format!("{name} expects an i64 value")));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok(if name == "chan_send" {
                        (Expr::ChanSend(Box::new(channel), Box::new(value)), Type::Bool)
                    } else {
                        (Expr::ChanTrySend(Box::new(channel), Box::new(value)), Type::Bool)
                    })
                }
                "chan_recv" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (channel, channel_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if channel_ty != Type::Chan {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("chan_recv expects a chan handle"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::ChanRecv(Box::new(channel)), Type::Int))
                }
                "chan_select" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let (first, first_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if first_ty != Type::Chan {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("chan_select expects chan handles"));
                    }
                    let (second, second_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if second_ty != Type::Chan {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail("chan_select expects chan handles"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::ChanSelect(Box::new(first), Box::new(second)), Type::Int))
                }
                "exec_new" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 0)?;
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::ExecNew, Type::Exec))
                }
                "exec_run" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (handle, handle_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if handle_ty != Type::Exec {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("exec_run expects an exec handle"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::ExecRun(Box::new(handle)), Type::Int))
                }
                "scope_new" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 0)?;
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::ScopeNew, Type::TaskScope))
                }
                "pool_new" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (workers, workers_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if workers_ty != Type::Int {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("pool_new expects an i64 worker count"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::PoolNew(Box::new(workers)), Type::Pool))
                }
                "pool_join" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (handle, handle_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if handle_ty != Type::Pool {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("pool_join expects a pool handle"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::PoolJoin(Box::new(handle)), Type::Int))
                }
                "c_size_of" | "c_align_of" | "c_offset_of" => {
                    // Only the bracketed form exists; the bracket path is
                    // intercepted before this arm, so a plain call lands here.
                    Err(self.fail(&format!("{name} is a layout query; write {name}[Type](..)")))
                }
                "spawn" => {
                    // Cursor after the name: the typed spawn helper parses the
                    // callable, moves its captures, and interns the thread.
                    self.pos += 1;
                    let (value, ty) = self.spawn_expr(line, column)?;
                    self.seek_scalar_offset(expression.range.end);
                    Ok((value, ty))
                }
                "wait" => {
                    self.pos += 1;
                    let (value, ty) = self.wait_expr(line, column)?;
                    self.seek_scalar_offset(expression.range.end);
                    Ok((value, ty))
                }
                "now_ms" | "now_s" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 0)?;
                    self.seek_scalar_offset(expression.range.end);
                    Ok((if name == "now_ms" { Expr::NowMs } else { Expr::NowS }, Type::Int))
                }
                "args" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 0)?;
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::Args(line, column), Type::StrVec))
                }
                "read_stdin" | "cwd" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 0)?;
                    let ty = self.prelude_type("ReadResult");
                    self.seek_scalar_offset(expression.range.end);
                    Ok((if name == "read_stdin" { Expr::ReadStdin(ty, line, column) } else { Expr::Cwd(ty, line, column) }, ty))
                }
                "read_file" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (path, path_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(path_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("read_file expects a string or string view argument"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::ReadFile(Box::new(path), line, column), Type::String))
                }
                "write_file" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let (path, path_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(path_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("write_file expects a string or string view argument"));
                    }
                    let (content, content_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if !matches!(content_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail("write_file expects a string or string view content"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::WriteFile(Box::new(path), Box::new(content), line, column), Type::Int))
                }
                "path_exists" | "path_is_dir" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (path, path_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(path_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail(&format!("{name} expects a string path")));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((if name == "path_exists" {
                        Expr::PathExists(Box::new(path), line, column)
                    } else {
                        Expr::PathIsDir(Box::new(path), line, column)
                    }, Type::Bool))
                }
                "path_join" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let (directory, directory_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(directory_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("path_join expects a string directory"));
                    }
                    let (part, part_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if !matches!(part_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail("path_join expects a string name"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::PathJoin(Box::new(directory), Box::new(part), line, column), Type::String))
                }
                "path_name" | "path_parent" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (path, path_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(path_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail(&format!("{name} expects a string path")));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((if name == "path_name" {
                        Expr::PathName(Box::new(path), line, column)
                    } else {
                        Expr::PathParent(Box::new(path), line, column)
                    }, Type::String))
                }
                "try_read_file" | "try_list_dir" | "try_file_info" | "env" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (ty, message) = match name {
                        "try_read_file" => (self.prelude_type("ReadResult"), "try_read_file expects a string path"),
                        "try_list_dir" => (self.prelude_type("DirResult"), "try_list_dir expects a string path"),
                        "try_file_info" => (self.prelude_type("FileResult"), "try_file_info expects a string path"),
                        _ => (self.prelude_type("ReadResult"), "env expects a string variable name"),
                    };
                    let (operand, operand_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(operand_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail(message));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((match name {
                        "try_read_file" => Expr::TryReadFile(Box::new(operand), ty, line, column),
                        "try_list_dir" => Expr::TryListDir(Box::new(operand), ty, line, column),
                        "try_file_info" => Expr::TryFileInfo(Box::new(operand), ty, line, column),
                        _ => Expr::Env(Box::new(operand), ty, line, column),
                    }, ty))
                }
                "try_write_file" | "try_append_file" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let ty = self.prelude_type("WriteResult");
                    let (path, path_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(path_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail(&format!("{name} expects a string path")));
                    }
                    let (content, content_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if !matches!(content_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail(&format!("{name} expects a string or string view content")));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((if name == "try_write_file" {
                        Expr::TryWriteFile(Box::new(path), Box::new(content), ty, line, column)
                    } else {
                        Expr::TryAppendFile(Box::new(path), Box::new(content), ty, line, column)
                    }, ty))
                }
                "try_copy_file" | "try_rename_path" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let ty = self.prelude_type("StatusResult");
                    let (from, from_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(from_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail(&format!("{name} expects a string source path")));
                    }
                    let (to, to_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if !matches!(to_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail(&format!("{name} expects a string destination path")));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((if name == "try_copy_file" {
                        Expr::TryCopyFile(Box::new(from), Box::new(to), ty, line, column)
                    } else {
                        Expr::TryRenamePath(Box::new(from), Box::new(to), ty, line, column)
                    }, ty))
                }
                "try_remove_file" | "try_remove_dir" | "try_create_dir" | "set_cwd" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let ty = self.prelude_type("StatusResult");
                    let (path, path_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(path_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail(&format!("{name} expects a string path")));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((match name {
                        "try_remove_file" => Expr::TryRemoveFile(Box::new(path), ty, line, column),
                        "try_remove_dir" => Expr::TryRemoveDir(Box::new(path), ty, line, column),
                        "try_create_dir" => Expr::TryCreateDir(Box::new(path), ty, line, column),
                        _ => Expr::SetCwd(Box::new(path), ty, line, column),
                    }, ty))
                }
                "utf8_len" | "utf8_prefix" => {
                    let arity = if name == "utf8_len" { 1 } else { 2 };
                    self.expect_resolved_builtin_arity(expression, arguments, arity)?;
                    let (text, text_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(text_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail(&format!("{name} expects a string or string view")));
                    }
                    if name == "utf8_len" {
                        self.seek_scalar_offset(expression.range.end);
                        return Ok((Expr::Utf8Len(Box::new(text), line, column), Type::Int));
                    }
                    let (count, count_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if count_ty != Type::Int {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail("utf8_prefix expects an i64 character count"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::Utf8Prefix(Box::new(text), Box::new(count), line, column), Type::String))
                }
                "count_lines_with" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let (text, text_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(text_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("count_lines_with expects a string or string view"));
                    }
                    let (pattern, pattern_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if !matches!(pattern_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail("count_lines_with expects a string or string view pattern"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::CountLinesWith(Box::new(text), Box::new(pattern), line, column), Type::Int))
                }
                "run_command" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let ty = self.prelude_type("CommandResult");
                    let (program, program_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(program_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("run_command expects a string program name"));
                    }
                    let (arguments_value, arguments_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    let vector = match (&arguments_value, arguments_ty) {
                        (Expr::Variable(id, Type::StrVec, ..), Type::StrVec) => *id,
                        _ => {
                            self.seek_scalar_offset(arguments[1].range.end);
                            return Err(self.fail("run_command expects a bare vec[string] variable for its arguments"));
                        }
                    };
                    if self.moved.contains(&vector) {
                        return Err(self.binding_label(self.fail("cannot use a moved vector in run_command").with_code("E0102"), vector));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::RunCommand(Box::new(program), vector, ty, line, column), ty))
                }
                "strvec_new" | "f64vec_new" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 0)?;
                    self.seek_scalar_offset(expression.range.end);
                    Ok(if name == "f64vec_new" {
                        (Expr::F64VecNew(line, column), Type::VecF)
                    } else {
                        (Expr::StrVecNew(line, column), Type::StrVec)
                    })
                }
                "lines" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (text, text_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(text_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("lines expects a string or string view"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::Lines(Box::new(text), line, column), Type::StrVec))
                }
                "join" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    // join borrows a bare string-vector variable.
                    let (parts, parts_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    let vid = match (&parts, parts_ty) {
                        (Expr::Variable(id, Type::StrVec, ..), Type::StrVec) => *id,
                        _ => {
                            self.seek_scalar_offset(arguments[0].range.end);
                            return Err(self.fail("join expects a bare vec[string] variable"));
                        }
                    };
                    if self.moved.contains(&vid) {
                        return Err(self.binding_label(self.fail("cannot use a moved vector in join").with_code("E0102"), vid));
                    }
                    let (sep, sep_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if !matches!(sep_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail("join expects a string or string view separator"));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::Join(vid, Box::new(sep), line, column), Type::String))
                }
                "slice_view" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 3)?;
                    let (base, base_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    // The view borrows its base: a bare string variable, or a
                    // string field of a struct *variable* (the whole variable
                    // is what the borrow protects — ADR-52). Mirrors the typed
                    // route's checks and messages.
                    let (vid, field) = match (&base, base_ty) {
                        (Expr::Variable(id, Type::String, ..), Type::String) => (*id, None),
                        (Expr::Field(inner, fname, Type::String), Type::String) => match **inner {
                            Expr::Variable(id, Type::Struct(_), ..) => (id, Some(fname.clone())),
                            _ => {
                                self.seek_scalar_offset(arguments[0].range.end);
                                return Err(self.fail("slice_view can follow a string field only on a struct variable, not on a nested expression"));
                            }
                        },
                        _ => {
                            self.seek_scalar_offset(arguments[0].range.end);
                            return Err(self.fail("slice_view expects a string variable or a string field of a struct variable (views borrow their base)"));
                        }
                    };
                    if self.moved.contains(&vid) {
                        return Err(self.binding_label(self.fail("cannot use a moved string in slice_view").with_code("E0102"), vid));
                    }
                    let (start, start_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if start_ty != Type::Int {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail("slice_view start must be i64"));
                    }
                    let (end, end_ty) = self.lower_scalar_body_expression(&arguments[2], slots)?;
                    if end_ty != Type::Int {
                        self.seek_scalar_offset(arguments[2].range.end);
                        return Err(self.fail("slice_view end must be i64"));
                    }
                    let depth = self.scopes.len();
                    if !self.borrows.iter().any(|&(id, _, _)| id == vid) {
                        self.borrows.push((vid, depth, BorrowKind::View));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::SliceView(vid, field, Box::new(start), Box::new(end), line, column), Type::StrView))
                }
                "to_i64" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (value, actual) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if actual != Type::F64 {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("to_i64 expects an f64"));
                    }
                    let ty = self.prelude_type("IntResult");
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::ToI64(Box::new(value), ty, line, column), ty))
                }
                "parse_int" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 1)?;
                    let (value, actual) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(actual, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("parse_int expects a string or string view"));
                    }
                    let ty = self.prelude_type("IntResult");
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::ParseInt(Box::new(value), ty, line, column), ty))
                }
                "find" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let (doc, doc_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(doc_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail("find expects a string to search in"));
                    }
                    let (needle, needle_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if !matches!(needle_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail("find expects a string to search for"));
                    }
                    let ty = self.prelude_type("IntResult");
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::Find(Box::new(doc), Box::new(needle), ty, line, column), ty))
                }
                "json_get" | "json_int" | "json_bool" | "json_len" | "json_has" => {
                    self.expect_resolved_builtin_arity(expression, arguments, 2)?;
                    let (doc, doc_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
                    if !matches!(doc_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[0].range.end);
                        return Err(self.fail(&format!("{name} expects a JSON document string")));
                    }
                    let (path, path_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
                    if !matches!(path_ty, Type::String | Type::StrView) {
                        self.seek_scalar_offset(arguments[1].range.end);
                        return Err(self.fail(&format!("{name} expects a string path")));
                    }
                    let ty = match name {
                        "json_get" => self.prelude_type("ReadResult"),
                        "json_has" => Type::Bool,
                        "json_bool" => self.prelude_type("BoolResult"),
                        _ => self.prelude_type("IntResult"),
                    };
                    let value = match name {
                        "json_get" => Expr::JsonGet(Box::new(doc), Box::new(path), ty, line, column),
                        "json_int" => Expr::JsonInt(Box::new(doc), Box::new(path), ty, line, column),
                        "json_bool" => Expr::JsonBool(Box::new(doc), Box::new(path), ty, line, column),
                        "json_len" => Expr::JsonLen(Box::new(doc), Box::new(path), ty, line, column),
                        _ => Expr::JsonHas(Box::new(doc), Box::new(path), line, column),
                    };
                    self.seek_scalar_offset(expression.range.end);
                    Ok((value, ty))
                }
                _ => {
                    // A math builtin: fixed arity, every argument already f64.
                    let Some((index, _, arity)) = MATH_FNS.iter().enumerate().find_map(|(index, (math, _, arity))| (*math == name).then_some((index, *math, *arity))) else {
                        unreachable!("resolved builtin dispatch");
                    };
                    if arguments.len() < arity {
                        self.seek_scalar_offset(expression.range.end);
                        self.pos = self.pos.saturating_sub(1);
                        return Err(self.fail(&format!("{name} expects {arity} f64 argument(s)")));
                    }
                    if arguments.len() > arity {
                        self.seek_scalar_offset(arguments[arity - 1].range.end);
                        return Err(self.fail(&format!("{name} takes {arity} argument(s)")));
                    }
                    let mut values = Vec::with_capacity(arity);
                    for argument in arguments {
                        self.seek_scalar_offset(argument.range.start);
                        let (value, ty) = self.lower_scalar_body_expression(argument, slots)?;
                        if ty != Type::F64 {
                            self.seek_scalar_offset(argument.range.end);
                            return Err(self.fail(&format!("{name} expects an f64 argument; convert integers explicitly with to_f64")));
                        }
                        values.push(Box::new(value));
                    }
                    self.seek_scalar_offset(expression.range.end);
                    Ok((Expr::MathCall(index as u8, values, line, column), Type::F64))
                }
            }
        })()
    }

    /// The typed route's own arity behaviour for a builtin that parses
    /// `(` expression (`(, expression)*`) `)`: a missing argument produces the
    /// Pratt parser's `expected expression` (E0011) at the closing
    /// parenthesis, and an extra one produces `take(RParen)`'s `expected
    /// RParen, found Comma` (E0010) at the comma before it. The pinned
    /// diagnostics stay identical because the same rules are applied.
    fn expect_resolved_builtin_arity(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        arguments: &[ae_syntax::expression::Expression],
        expected: usize,
    ) -> Result<(), ae_span::Diagnostic> {
        if arguments.len() < expected {
            self.seek_scalar_offset(expression.range.end);
            self.pos = self.pos.saturating_sub(1);
            return Err(self.fail("expected expression").with_code("E0011"));
        }
        if arguments.len() > expected {
            self.seek_scalar_offset(arguments[expected].range.start);
            self.pos = self.pos.saturating_sub(1);
            self.take(Kind::RParen)?;
        }
        Ok(())
    }

    fn lower_handle_builtin_statement(
        &mut self,
        statement: &ae_syntax::statement::Statement,
        slots: &mut HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Statement, ae_span::Diagnostic> {
        use ae_syntax::expression::ExpressionKind;
        let (builtin, call) = handle_statement_builtin(statement).expect("handle builtin eligibility");
        // The spawn statement forms share the typed parser's helper (cursor on
        // the name); it consumes the arguments and the semicolon.
        if matches!(builtin, "task_spawn" | "scope_spawn" | "pool_spawn") {
            self.seek_scalar_offset(call.range.start);
            return self.spawn_statement(builtin);
        }
        let ExpressionKind::Call { callee, arguments } = &call.kind else { unreachable!("handle builtin call") };
        self.seek_scalar_offset(callee.range.start);
        if builtin == "chan_close" {
            let Some(argument) = arguments.first() else {
                self.seek_scalar_offset(call.range.end);
                self.pos = self.pos.saturating_sub(1);
                return Err(self.fail("expected expression").with_code("E0011"));
            };
            let (handle, handle_ty) = self.lower_scalar_body_expression(argument, slots)?;
            if handle_ty != Type::Chan {
                self.seek_scalar_offset(argument.range.end);
                return Err(self.fail("chan_close expects a chan handle"));
            }
            self.seek_scalar_offset(call.range.end);
            self.take(Kind::Semi)?;
            return Ok(Statement::ChanClose(Box::new(handle)));
        }
        if arguments.len() < 2 {
            self.seek_scalar_offset(call.range.end);
            self.pos = self.pos.saturating_sub(1);
            return Err(self.fail("expected expression").with_code("E0011"));
        }
        let (handle, handle_ty) = self.lower_scalar_body_expression(&arguments[0], slots)?;
        if builtin == "cell_store" {
            if handle_ty != Type::Atomic {
                self.seek_scalar_offset(arguments[0].range.end);
                return Err(self.fail("cell_store expects an atomic handle"));
            }
            let (value, value_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
            if value_ty != Type::Int {
                self.seek_scalar_offset(arguments[1].range.end);
                return Err(self.fail("cell_store expects an i64 value"));
            }
            self.seek_scalar_offset(call.range.end);
            self.take(Kind::Semi)?;
            return Ok(Statement::AtomicStore(Box::new(handle), Box::new(value)));
        }
        // `shared_set(m, v);` — the typed route's seq_cst store, guarded by a
        // live mutex guard from `mutex_lock(...)` in the same body.
        let sid = match handle_ty {
            Type::Shared(sid) => sid,
            _ => {
                self.seek_scalar_offset(arguments[0].range.end);
                return Err(self.fail("shared_set expects a shared handle"));
            }
        };
        let (value, value_ty) = self.lower_scalar_body_expression(&arguments[1], slots)?;
        let expected = self.ctx.shared_types_get(sid);
        if value_ty != expected {
            self.seek_scalar_offset(arguments[1].range.end);
            return Err(self.fail(&format!("shared_set expects {}, found {}", expected.name(self.ctx), value_ty.name(self.ctx))));
        }
        self.seek_scalar_offset(call.range.end);
        self.take(Kind::Semi)?;
        if !self.live_owned().iter().any(|(_, ty)| *ty == Type::Guard) {
            return Err(self.fail("shared_set requires a live mutex guard in scope; call mutex_lock(...) first"));
        }
        Ok(Statement::SharedSet(Box::new(handle), Box::new(value), sid))
    }

    fn lower_vector_builtin_statement(
        &mut self,
        statement: &ae_syntax::statement::Statement,
        slots: &mut HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Statement, ae_span::Diagnostic> {
        use ae_syntax::expression::ExpressionKind;
        let resolved_cell = Rc::clone(&self.resolved_expressions);
        let resolved = resolved_cell.get_or_init(|| self.build_resolved_expressions());
        let (builtin, value) = vector_statement_builtin(statement, resolved).expect("vector builtin eligibility");
        let ae_syntax::statement::StatementKind::Expr { value: call, .. } = &statement.kind else { unreachable!("vector builtin statement") };
        let ExpressionKind::Call { callee, arguments } = &call.kind else { unreachable!("vector builtin call") };
        let ExpressionKind::Atom(Kind::Ident(target)) = &arguments[0].kind else { unreachable!("vector builtin target") };
        self.seek_scalar_offset(callee.range.start);
        let (line, column) = self.position();
        let def = resolved.scalar_reads[&arguments[0].range.start];
        let (slot, ty) = slots[&def];
        if self.moved.contains(&slot) {
            return Err(moved_value_hint(self.binding_label(
                self.fail(&format!("cannot use '{target}' because it was moved")).with_code("E0102"), slot)));
        }
        if !matches!(ty, Type::Vec | Type::StrVec | Type::VecF) {
            self.seek_scalar_offset(arguments[0].range.end);
            return Err(self.fail(&format!("{builtin} expects a vec, found {}", ty.name(self.ctx))));
        }
        if self.is_borrowed(slot) {
            self.seek_scalar_offset(arguments[0].range.end);
            return Err(self.fail(&format!("cannot modify '{target}' while it is borrowed")));
        }
        // The immutability diagnostic is emitted per branch: the typed route
        // reports a `push` target at the value's token (the shared expression
        // path lowers the value first) and the others at the closing paren.
        // Ordering happens in place (qsort for i64, byte order for strings);
        // f64 has no total order, so sort/reverse reject it.
        if matches!(builtin, "sort" | "reverse") {
            if !self.names[target].1 {
                self.seek_scalar_offset(arguments[0].range.end);
                return Err(self.immutable_binding_error(
                    self.fail(&format!("cannot modify immutable variable '{target}'")).with_code("E0103"), slot));
            }
            if ty == Type::VecF {
                self.seek_scalar_offset(arguments[0].range.end);
                return Err(self.fail(&format!("{builtin} is not supported for vec[f64]; NaN has no total order")));
            }
            self.seek_scalar_offset(call.range.end);
            self.take(Kind::Semi)?;
            let strings = ty == Type::StrVec;
            return Ok(if builtin == "sort" { Statement::Sort(slot, strings, line, column) } else { Statement::Reverse(slot, strings, line, column) });
        }
        if builtin == "push" {
            // The immutability diagnostic points just past the target name
            // (the `,`), the token the typed parser's cursor ends on.
            if !self.names[target].1 {
                self.seek_scalar_offset(arguments[0].range.end);
                return Err(self.immutable_binding_error(
                    self.fail(&format!("cannot modify immutable variable '{target}'")).with_code("E0103"), slot));
            }
            let (lowered, value_ty) = self.lower_scalar_body_expression(value, slots)?;
            let ok = match ty {
                Type::Vec => value_ty == Type::Int,
                Type::VecF => value_ty == Type::F64,
                _ => matches!(value_ty, Type::String | Type::StrView),
            };
            if !ok {
                self.seek_scalar_offset(value.range.end);
                return Err(self.fail(&format!("vector elements must be {}", match ty { Type::Vec => "i64", Type::VecF => "f64", _ => "string" })));
            }
            self.seek_scalar_offset(call.range.end);
            self.take(Kind::Semi)?;
            return Ok(if ty == Type::StrVec {
                Statement::StrVecPush(slot, lowered, line, column, value_ty == Type::StrView)
            } else if ty == Type::VecF {
                Statement::VecFPush(slot, lowered, line, column)
            } else {
                Statement::VecPush(slot, lowered, line, column)
            });
        }
        if !self.names[target].1 {
            self.seek_scalar_offset(arguments[0].range.end);
            return Err(self.immutable_binding_error(
                self.fail(&format!("cannot modify immutable variable '{target}'")).with_code("E0103"), slot));
        }
        self.seek_scalar_offset(call.range.end);
        self.take(Kind::Semi)?;
        Ok(if ty == Type::StrVec {
            Statement::StrVecPop(slot, line, column)
        } else if ty == Type::VecF {
            Statement::VecFPop(slot, line, column)
        } else {
            Statement::VecPop(slot, line, column)
        })
    }

    /// A method call on a bare struct variable (`self.m(...)` included).
    /// `signature.params[0]` is the receiver; the argument list follows it.
    /// The receiver bookkeeping mirrors the typed postfix path, so the
    /// borrow graph and the pinned diagnostics stay identical.
    #[allow(clippy::too_many_arguments)]
    fn lower_method_call(
        &mut self,
        method_name: &str,
        slot: usize,
        base_ty: Type,
        signature: Signature,
        expression: &ae_syntax::expression::Expression,
        arguments: &[ae_syntax::expression::Expression],
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<(Expr, Type), ae_span::Diagnostic> {
        let wants_mut = matches!(signature.params.first(), Some(Type::StructMut(_)));
        let wants_ref = matches!(signature.params.first(), Some(Type::StructRef(_)));
        self.seek_scalar_offset(expression.range.end);
        if (wants_mut || wants_ref) && self.moved.contains(&slot) {
            return Err(moved_value_hint(self.binding_label(self.fail(&format!("cannot borrow the receiver of '{method_name}' because it was moved")).with_code("E0102"), slot)));
        }
        if wants_mut {
            // A read-only receiver cannot grant a mutable borrow; the pinned
            // diagnostic reads exactly like the typed route's.
            if matches!(base_ty, Type::StructRef(_)) {
                return Err(self.fail(&format!("'{method_name}' borrows its receiver mutably, which a read-only borrow cannot grant; call it on the owned variable")));
            }
            let mutable = self.names.iter().find(|(_, (id, _, _))| *id == slot).map(|(_, (_, mutable, _))| *mutable).unwrap_or(false);
            if !mutable {
                return Err(self.fail(&format!("'{method_name}' borrows its receiver mutably, but the variable was declared without 'var'")));
            }
            if self.is_borrowed(slot) {
                if !self.only_view_borrows_active(slot) {
                    return Err(self.fail(&format!("cannot borrow the receiver of '{method_name}' mutably while it is borrowed in this statement")));
                }
                let receiver_name = self.names.iter().find(|(_, (id, _, _))| *id == slot).map(|(name, _)| name.clone()).unwrap_or_else(|| format!("v{slot}"));
                // The typed route records the conflict at the last token
                // before the statement's end (the call's closing paren).
                let conflict_at = self.tokens.partition_point(|token| token.offset < expression.range.end).saturating_sub(1);
                let conflict_at = self.tokens.get(conflict_at).map(|token| token.offset).unwrap_or(expression.range.start);
                self.deferred_write_conflicts.push((slot, conflict_at, receiver_name, "mutate", None));
            }
            self.mut_borrows.push((slot, self.scopes.len()));
        } else if wants_ref {
            if self.mut_borrows.iter().any(|&(bid, _)| bid == slot) {
                return Err(self.fail(&format!("cannot borrow the receiver of '{method_name}' while it is mutably borrowed in this statement")));
            }
            if !self.is_borrowed(slot) {
                self.call_borrows.push((slot, self.scopes.len()));
            }
        } else {
            // A by-value receiver moves: the typed postfix path's rules.
            if self.is_borrowed(slot) && !self.all_borrows_are_views(slot) {
                return Err(self.fail(&format!("cannot move the receiver of '{method_name}' while it is borrowed")));
            }
            if self.is_borrowed(slot) {
                let receiver_name = self.names.iter().find(|(_, (id, _, _))| *id == slot).map(|(name, _)| name.clone()).unwrap_or_else(|| format!("v{slot}"));
                let conflict_at = self.tokens.partition_point(|token| token.offset < expression.range.end).saturating_sub(1);
                let conflict_at = self.tokens.get(conflict_at).map(|token| token.offset).unwrap_or(expression.range.start);
                self.deferred_write_conflicts.push((slot, conflict_at, receiver_name, "move", None));
            }
            if self.moved.contains(&slot) {
                return Err(moved_value_hint(self.binding_label(self.fail(&format!("cannot use a moved value as the receiver of '{method_name}'")).with_code("E0102"), slot)));
            }
            self.moved.insert(slot);
        }
        let mut args = if wants_mut || wants_ref {
            vec![Argument::Borrow(slot, matches!(base_ty, Type::Struct(_)))]
        } else {
            vec![Argument::Value(Expr::Variable(slot, base_ty, 0, 0))]
        };
        for (index, argument) in arguments.iter().enumerate() {
            self.seek_scalar_offset(argument.range.start);
            let Some(expected) = signature.params.get(index + 1).copied() else {
                return Err(self.fail(&format!("too many arguments to '{method_name}'")));
            };
            let (value, actual) = self.lower_scalar_body_expression(argument, slots)?;
            self.check_value_argument_type(method_name, index, expected, actual, Some(&argument.range))?;
            if actual.owned()
                && let Expr::Variable(source, ..) = &value {
                    self.moved.insert(*source);
                }
            args.push(Argument::Value(value));
        }
        self.seek_scalar_offset(expression.range.end);
        Ok((Expr::Call(signature.id, args, signature.ret), signature.ret))
    }

    fn lower_scalar_array_index(
        &mut self,
        index: &ae_syntax::expression::Expression,
        slots: &HashMap<ae_resolve::DefId, (usize, Type)>,
    ) -> Result<Expr, ae_span::Diagnostic> {
        let (value, ty) = self.lower_scalar_body_expression(index, slots)?;
        let scalar = self.checked_scalar(Some(&index.range)).or_else(|| super::expression::operator_scalar(ty));
        self.check_array_index(scalar, Some(index.range.end))?;
        Ok(value)
    }

    fn lower_propagating_call(&mut self) -> Result<Statement, ae_span::Diagnostic> {
        let (value, ty) = match self.lower_shared_value(Some(Kind::Question), true) {
            Some(value) => value,
            None => self.expression(0, true)?,
        };
        if *self.current() != Kind::Question {
            return Err(self.fail("a statement that only calls a function must end with '?' to propagate its result"));
        }
        let (statement, _) = self.propagate_statement(value, ty, None)?;
        self.pos += 1;
        self.take(Kind::Semi)?;
        Ok(statement)
    }

    fn finish_exit(&mut self, code: Expr, ty: Type) -> Result<Statement, ae_span::Diagnostic> {
        if ty != Type::Int {
            return Err(self.fail("exit expects an i64 status code"));
        }
        self.take(Kind::RParen)?;
        self.take(Kind::Semi)?;
        Ok(Statement::Exit(code, self.live_owned()))
    }

    fn check_view_binding(&self, mutable: bool, ty: Type) -> Result<(), ae_span::Diagnostic> {
        if mutable && matches!(ty, Type::StrView | Type::Tokens) {
            return Err(self.fail("a view binding cannot be mutable; declare it with 'let'"));
        }
        Ok(())
    }

    fn check_borrowed_binding(&self, ty: Type) -> Result<(), ae_span::Diagnostic> {
        if matches!(ty, Type::VecRef | Type::StrVecRef) {
            return Err(self.fail("cannot store a borrowed vector in a variable; pass it directly to a parameter that borrows"));
        }
        if matches!(ty, Type::StructRef(_) | Type::StructMut(_)) {
            return Err(self.fail("cannot store a borrowed struct in a variable; pass it directly to a parameter that borrows"));
        }
        Ok(())
    }

    fn pointer_store_pointee(&self, name: &str, ty: Option<Type>, line: usize) -> Result<Type, ae_span::Diagnostic> {
        let Some(Type::PtrMut(id)) = ty else {
            return Err(self.fail(&format!("'*{name}' requires a *mut pointer variable")));
        };
        if self.unsafe_depth == 0 {
            return Err(self.error_at(line, "writing through a raw pointer requires 'unsafe'").with_code("E0700"));
        }
        Ok(self.ctx.ptr_pointee(id))
    }

    pub(super) fn check_pointer_deref(&self, offset: usize) -> Result<(), ae_span::Diagnostic> {
        if self.unsafe_depth == 0 {
            return Err(self.error_at(offset, "dereferencing a raw pointer requires 'unsafe'").with_code("E0700"));
        }
        Ok(())
    }

    fn check_pointer_store_value(&self, actual: Type, pointee: Type) -> Result<(), ae_span::Diagnostic> {
        if actual != pointee {
            return Err(self.fail(&format!("cannot write a {} value through *mut {}", actual.name(self.ctx), pointee.name(self.ctx))));
        }
        if pointee.owned() {
            return Err(self.fail("writing an owned value through a pointer is not supported yet; the pointer would take over its release"));
        }
        Ok(())
    }

    fn checked_scalar(&self, range: Option<&std::ops::Range<usize>>) -> Option<ae_resolve::typecheck::Scalar> {
        match self.resolved_expressions().scalar_expression_types.get(range?)? {
            ae_resolve::resolve::ExpressionType::Checked(scalar) => Some(*scalar),
            _ => None,
        }
    }

    fn check_compound_assignment(&self, target: Type, operator: &Kind) -> Result<(), ae_span::Diagnostic> {
        if (target == Type::String && *operator == Kind::Plus)
            || ae_resolve::typecheck::scalar_compound_assignment(super::expression::operator_scalar(target), operator)
        {
            return Ok(());
        }
        Err(self.fail(&format!("compound assignment is not supported for type '{}'", target.name(self.ctx))))
    }

    pub(super) fn matches_value_type(&self, expected: Type, actual: Type, range: Option<&std::ops::Range<usize>>) -> bool {
        match self.checked_scalar(range) {
            Some(scalar) => super::expression::operator_scalar(expected) == Some(scalar),
            None => actual == expected,
        }
    }

    fn check_return_type(&self, value: Type, range: Option<&std::ops::Range<usize>>) -> Result<(), ae_span::Diagnostic> {
        if self.matches_value_type(self.return_type, value, range) {
            return Ok(());
        }
        Err(self.fail(&format!("return type must be {}", self.return_type.name(self.ctx))).with_code("E0204"))
    }

    pub(super) fn check_value_argument_type(
        &mut self,
        name: &str,
        index: usize,
        expected: Type,
        actual: Type,
        range: Option<&std::ops::Range<usize>>,
    ) -> Result<(), ae_span::Diagnostic> {
        if self.matches_value_type(expected, actual, range) {
            return Ok(());
        }
        if let Some(range) = range {
            self.seek_scalar_offset(range.end);
        }
        Err(self.fail(&format!("argument {} to '{name}' must be {}", index + 1, expected.name(self.ctx))))
    }

    pub(super) fn check_field_value_type(
        &mut self,
        name: &str,
        expected: Type,
        actual: Type,
        range: Option<&std::ops::Range<usize>>,
    ) -> Result<(), ae_span::Diagnostic> {
        if self.matches_value_type(expected, actual, range) {
            return Ok(());
        }
        if let Some(range) = range {
            self.seek_scalar_offset(range.end);
        }
        Err(self.fail(&format!("field '{name}' must be {}", expected.name(self.ctx))))
    }

    fn check_assignment_type(
        &self,
        name: &str,
        target: Type,
        value: Type,
        range: Option<&std::ops::Range<usize>>,
    ) -> Result<(), ae_span::Diagnostic> {
        if self.matches_value_type(target, value, range) {
            return Ok(());
        }
        Err(self.fail(&format!("cannot assign a {} value to {} variable '{name}'", value.name(self.ctx), target.name(self.ctx))))
    }

    fn lower_resolved_local(&mut self, name: &str, offset: usize) -> Option<(Expr, Type)> {
        let (id, _, ty) = *self.names.get(name)?;
        if self.moved.contains(&id) || self.shadow_ok || !self.subst.is_empty() {
            return None;
        }
        let definition = self.resolved_expressions().local_bindings.get(&offset)?;
        // Match the resolver's source definition to the active compiler slot.
        if self.binding_ranges.get(&id)? != definition {
            return None;
        }
        let token = self.tokens.get(self.tokens.partition_point(|token| token.offset < offset))?;
        Some((Expr::Variable(id, ty, token.line, token.column), ty))
    }

    fn lower_shared_argument(
        &mut self,
        argument: &ae_syntax::expression::Expression,
        expected: Type,
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<(Expr, Type)> {
        use ae_syntax::expression::ExpressionKind;

        if (expected.owned() || expected == Type::StrView)
            && let ExpressionKind::Atom(Kind::Ident(name)) = &argument.kind {
                remaining.checked_sub(1)?;
                *nodes_left = nodes_left.checked_sub(1)?;
                let (value, ty) = self.lower_resolved_local(name, argument.range.start)?;
                let Expr::Variable(slot, ..) = value else { unreachable!("resolved local") };
                if effects.moves.contains(&slot) {
                    return None;
                }
                if expected == ty && ty.owned() {
                    if self.variable_move_needs_deferral(slot)?
                        || effects.call_borrows.contains(&slot)
                        || effects.mut_borrows.contains(&slot)
                    {
                        effects.conflicts.push((slot, argument.range.start, name.clone(), "move", None));
                    }
                    effects.moves.push(slot);
                }
                return Some((value, ty));
            }
        if expected == Type::StrView {
            self.lower_shared_read(argument, remaining, nodes_left, effects)
        } else {
            self.lower_shared_expression(argument, remaining, nodes_left, effects)
        }
    }

    fn lower_shared_field(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<(Expr, Type)> {
        use ae_syntax::expression::ExpressionKind;

        let ExpressionKind::Field { base, name } = &expression.kind else {
            return None;
        };
        let remaining = remaining.checked_sub(1)?;
        let (value, ty) = match &base.kind {
            ExpressionKind::Atom(Kind::Ident(binding)) => {
                *nodes_left = nodes_left.checked_sub(1)?;
                let (value, ty) = self.lower_resolved_local(binding, base.range.start)?;
                let Expr::Variable(slot, ..) = value else { unreachable!("resolved local") };
                if effects.moves.contains(&slot) {
                    return None;
                }
                (value, ty)
            }
            ExpressionKind::Field { .. } => self.lower_shared_field(base, remaining, nodes_left, effects)?,
            // The typed parser requires parentheses around a literal receiver.
            ExpressionKind::Struct { .. } => return None,
            _ => self.lower_shared_expression(base, remaining, nodes_left, effects)?,
        };
        let sid = match ty {
            Type::Struct(sid) | Type::StructRef(sid) | Type::StructMut(sid) => sid,
            _ => return None,
        };
        let (_, ty) = self.structs.get(sid)?.fields.iter().find(|(field, _)| field == name)?;
        Some((Expr::Field(Box::new(value), name.clone(), *ty), *ty))
    }

    fn lower_shared_read(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<(Expr, Type)> {
        use ae_syntax::expression::ExpressionKind;

        if matches!(expression.kind, ExpressionKind::Field { .. }) {
            let (value, ty) = self.lower_shared_field(expression, remaining, nodes_left, effects)?;
            return matches!(ty, Type::Int | Type::F64 | Type::Bool | Type::String | Type::StrView)
                .then_some((value, ty));
        }
        self.lower_shared_expression(expression, remaining, nodes_left, effects)
    }

    fn lower_shared_variant(
        &mut self,
        target: &ae_syntax::expression::Expression,
        arguments: &[ae_syntax::expression::Expression],
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<(Expr, Type)> {
        use ae_syntax::expression::ExpressionKind;

        let ExpressionKind::Qualified { base, name: variant } = &target.kind else {
            return None;
        };
        let member = self.tokens.partition_point(|token| token.offset < target.range.end).checked_sub(1)?;
        let offset = self.tokens[member].offset;
        if !self.resolved_expressions().enum_variants.contains(&offset) {
            return None;
        }
        let eid = match &base.kind {
            ExpressionKind::Atom(Kind::Ident(name)) => *self.enum_names.get(name)?,
            ExpressionKind::Specialization { base, arguments } => {
                let ExpressionKind::Atom(Kind::Ident(name)) = &base.kind else { return None };
                self.cached_enum_specialization(name, arguments)?
            }
            _ => return None,
        };
        let def = self.enums.get(eid)?;
        self.require_public("enum", def.name, &def.file, def.public).ok()?;
        let (variant_id, payload) = def.variant_payload(variant)?;
        let (enum_name, payload) = (def.name, payload.to_vec());
        let values = self.lower_shared_enum_payload(arguments, &payload, remaining, nodes_left, effects)?;
        Some((Expr::EnumLit(enum_name, eid, variant_id, values), Type::Enum(enum_name, eid)))
    }

    fn lower_shared_enum_payload(
        &mut self,
        arguments: &[ae_syntax::expression::Expression],
        payload: &[Type],
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<Vec<Expr>> {
        if arguments.len() != payload.len() {
            return None;
        }
        let mut values = Vec::with_capacity(payload.len());
        for (argument, &expected) in arguments.iter().zip(payload) {
            let (value, actual) = if expected == Type::String {
                self.lower_shared_argument(argument, expected, remaining, nodes_left, effects)?
            } else {
                self.lower_shared_expression(argument, remaining, nodes_left, effects)?
            };
            if !self.matches_value_type(expected, actual, Some(&argument.range)) {
                return None;
            }
            values.push(value);
        }
        Some(values)
    }

    fn lower_shared_expression(
        &mut self,
        expression: &ae_syntax::expression::Expression,
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<(Expr, Type)> {
        use ae_syntax::expression::ExpressionKind;

        if matches!(expression.kind, ExpressionKind::Field { .. }) {
            let (value, ty) = self.lower_shared_field(expression, remaining, nodes_left, effects)?;
            // Owned fields need the typed parser's context-sensitive move checks.
            return matches!(ty, Type::Int | Type::F64 | Type::Bool).then_some((value, ty));
        }
        let remaining = remaining.checked_sub(1)?;
        // The typed Pratt parser charges operands, groups and prefixes.
        // Infix and postfix cast nodes add no charge beyond their operands.
        if !matches!(expression.kind, ExpressionKind::Binary { .. } | ExpressionKind::PointerCast { .. }) {
            *nodes_left = nodes_left.checked_sub(1)?;
        }
        match &expression.kind {
            ExpressionKind::Atom(Kind::Number(value)) => Some((Expr::Number(*value), Type::Int)),
            ExpressionKind::Atom(Kind::Float(value)) => Some((Expr::FloatLit(*value), Type::F64)),
            ExpressionKind::Atom(Kind::True) => Some((Expr::BoolLit(true), Type::Bool)),
            ExpressionKind::Atom(Kind::False) => Some((Expr::BoolLit(false), Type::Bool)),
            ExpressionKind::Atom(Kind::Text(value)) => {
                let offset = expression.range.start;
                let token = self.tokens.get(self.tokens.partition_point(|token| token.offset < offset))?;
                Some((Expr::StrLit(value.clone(), token.line, token.column), Type::String))
            }
            ExpressionKind::Atom(Kind::Ident(name)) => {
                let (_, _, ty) = self.names.get(name)?;
                if !matches!(ty, Type::Int | Type::F64 | Type::Bool | Type::Closure(_) | Type::Ptr(_) | Type::PtrMut(_)) {
                    return None;
                }
                self.lower_resolved_local(name, expression.range.start)
            }
            ExpressionKind::Index { base, index } => {
                let ExpressionKind::Atom(Kind::Ident(name)) = &base.kind else {
                    return None;
                };
                let (Expr::Variable(slot, _, _, _), ty) = self.lower_resolved_local(name, base.range.start)? else {
                    return None;
                };
                if effects.moves.contains(&slot) {
                    return None;
                }
                let bracket = self.tokens.get(self.tokens.partition_point(|token| token.offset < base.range.end))?;
                if bracket.kind != Kind::LBracket {
                    return None;
                }
                let (line, column) = (bracket.line, bracket.column);
                let (index, Type::Int) = self.lower_shared_expression(index, remaining, nodes_left, effects)? else {
                    return None;
                };
                let index = Box::new(index);
                match ty {
                    Type::Array(len) => Some((Expr::Index(slot, len, index, line, column), Type::Int)),
                    Type::Vec | Type::VecRef | Type::VecF | Type::VecFRef => {
                        let borrowed = matches!(ty, Type::VecRef | Type::VecFRef);
                        let element = if matches!(ty, Type::VecF | Type::VecFRef) { Type::F64 } else { Type::Int };
                        Some((Expr::VecIndex(slot, index, line, column, borrowed, element), element))
                    }
                    _ => None,
                }
            }
            ExpressionKind::Struct { target, fields } => {
                if std::env::var_os("AE_DEBUG_MIR").is_some()
                    || !self.resolved_expressions().struct_constructors.contains(&target.range.start)
                {
                    return None;
                }
                let sid = match &target.kind {
                    ExpressionKind::Atom(Kind::Ident(name)) => *self.struct_names.get(name)?,
                    ExpressionKind::Specialization { base, arguments } => {
                        let ExpressionKind::Atom(Kind::Ident(name)) = &base.kind else { return None };
                        self.cached_struct_specialization(name, arguments)?
                    }
                    _ => return None,
                };
                self.lower_shared_struct_fields(sid, fields, remaining, nodes_left, effects)
            }
            ExpressionKind::Vector(elements) => {
                let token = self.tokens.get(self.tokens.partition_point(|token| token.offset < expression.range.start))?;
                let (line, column) = (token.line, token.column);
                let mut vector_type = None;
                let mut values = Vec::new();
                for element in elements {
                    let (value, element_type) = self.lower_shared_expression(element, remaining, nodes_left, effects)?;
                    let ty = match element_type {
                        Type::Int => Type::Vec,
                        Type::F64 => Type::VecF,
                        _ => return None,
                    };
                    if vector_type.is_some_and(|previous| previous != ty) {
                        return None;
                    }
                    vector_type = Some(ty);
                    values.push(value);
                }
                let ty = vector_type.unwrap_or(Type::Vec);
                Some((Expr::VecLit(values, ty, line, column), ty))
            }
            ExpressionKind::AddressCast { binding, binding_range, mutable, pointee } => {
                remaining.checked_sub(1)?;
                *nodes_left = nodes_left.checked_sub(1)?;
                let (operand, ty) = self.lower_resolved_local(binding, binding_range.start)?;
                let name = self.source.get(pointee.clone())?;
                if super::types::scalar_type(name)? != ty || (*mutable && !self.names.get(binding)?.1) {
                    return None;
                }
                let start = self.tokens.partition_point(|token| token.offset < expression.range.start);
                let token = self.tokens.get(start)?;
                let id = self.ctx.intern_pointee(ty);
                let ty = if *mutable { Type::PtrMut(id) } else { Type::Ptr(id) };
                Some((Expr::BorrowAs(Box::new(operand), *mutable, token.line, token.column), ty))
            }
            ExpressionKind::PointerCast { base, mutable, pointee } => {
                let (operand, base_type) = self.lower_shared_expression(base, remaining, nodes_left, effects)?;
                if !matches!(base_type, Type::Ptr(_) | Type::PtrMut(_)) {
                    return None;
                }
                let name = self.source.get(pointee.clone())?;
                let pointee = self.subst.get(name).copied().or_else(|| super::types::scalar_type(name))?;
                let end = self.tokens.partition_point(|token| token.offset < expression.range.end);
                let token = self.tokens.get(end)?;
                let id = self.ctx.intern_pointee(pointee);
                let ty = if *mutable { Type::PtrMut(id) } else { Type::Ptr(id) };
                Some((Expr::PtrCast(Box::new(operand), *mutable, id, token.line, token.column), ty))
            }
            ExpressionKind::Group(inner) => self.lower_shared_expression(inner, remaining, nodes_left, effects),
            ExpressionKind::Qualified { .. } => {
                self.lower_shared_variant(expression, &[], remaining, nodes_left, effects)
            }
            ExpressionKind::Call { callee, arguments } => {
                if matches!(callee.kind, ExpressionKind::Qualified { .. }) {
                    if let Some(last) = arguments.last() {
                        let next = self.tokens.partition_point(|token| token.offset < last.range.end);
                        if self.tokens.get(next)?.kind != Kind::RParen {
                            return None;
                        }
                    }
                    return self.lower_shared_variant(callee, arguments, remaining, nodes_left, effects);
                }
                let (base, types) = match &callee.kind {
                    ExpressionKind::BracketedCallTarget { base, arguments } => (base.as_ref(), Some(arguments.as_slice())),
                    _ => (callee.as_ref(), None),
                };
                let ExpressionKind::Atom(Kind::Ident(name)) = &base.kind else {
                    return None;
                };
                if self.names.contains_key(name) || RESERVED.contains(&name.as_str()) {
                    return None;
                }
                let resolved = self.resolved_expressions();
                let offset = base.range.start;
                let (id, params, ret) = if let Some(types) = types {
                    let (definition, (id, params, ret)) = self.cached_function_specialization(name, types)?;
                    let visible = if definition.file == self.file {
                        resolved.direct_calls.contains(&offset)
                    } else {
                        definition.file != PRELUDE && definition.public && resolved.unresolved_calls.contains(&offset)
                    };
                    if !visible {
                        return None;
                    }
                    (*id, params, *ret)
                } else {
                    let signature = self.functions.get(name)?;
                    if signature.file != self.file || signature.extern_c || !resolved.direct_calls.contains(&offset) {
                        return None;
                    }
                    (signature.id, &signature.params, signature.ret)
                };
                if self.ctx.is_unsafe_fn(id) || arguments.len() != params.len() || arguments.len() > 64
                    || !params.iter().all(|ty| {
                        matches!(ty, Type::Int | Type::Bool | Type::F64 | Type::String | Type::StrView
                            | Type::Vec | Type::VecF | Type::VecRef | Type::VecFRef | Type::StrVecRef | Type::Array(_) | Type::Struct(_) | Type::Enum(..) | Type::StructRef(_) | Type::StructMut(_))
                    })
                    || !matches!(ret, Type::Int | Type::Bool | Type::F64 | Type::String | Type::Vec | Type::VecF | Type::Struct(_) | Type::Enum(..))
                {
                    return None;
                }
                let params = params.clone();
                let args = self.lower_shared_call_arguments(arguments, &params, remaining, nodes_left, effects)?;
                Some((Expr::Call(id, args, ret), ret))
            }
            ExpressionKind::Prefix { operator, operand } => {
                let token = self.tokens.get(self.tokens.partition_point(|token| token.offset < expression.range.start))?;
                let position = (token.line, token.column);
                let operand = self.lower_shared_expression(operand, remaining, nodes_left, effects)?;
                self.lower_prefix(operator.clone(), operand, position, None).ok()
            }
            ExpressionKind::Binary { left, operator, right } => {
                let token = self.tokens.get(self.tokens.partition_point(|token| token.offset < left.range.end))?;
                if &token.kind != operator {
                    return None;
                }
                let position = (token.line, token.column);
                let lower_operand = if matches!(operator, Kind::Plus | Kind::EqEq | Kind::NotEq) {
                    Self::lower_shared_read
                } else {
                    Self::lower_shared_expression
                };
                let left = lower_operand(self, left, remaining, nodes_left, effects)?;
                let right = lower_operand(self, right, remaining, nodes_left, effects)?;
                self.lower_infix(left, operator.clone(), right, position, None).ok()
            }
            _ => None,
        }
    }

    fn lower_shared_struct_fields(
        &mut self,
        sid: usize,
        fields: &[ae_syntax::expression::FieldInitializer],
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<(Expr, Type)> {
        let def = self.structs.get(sid)?;
        self.require_public("struct", &def.name, &def.file, def.public).ok()?;
        if fields.len() != def.fields.len() {
            return None;
        }
        let declared_fields = def.fields.clone();
        let mut values: Vec<_> = (0..fields.len()).map(|_| None).collect();
        for field in fields {
            let slot = declared_fields.iter().position(|(name, _)| name == &field.name)?;
            if values[slot].is_some() {
                return None;
            }
            let expected = declared_fields[slot].1;
            let (value, actual) = self.lower_shared_argument(&field.value, expected, remaining, nodes_left, effects)?;
            if !self.matches_value_type(expected, actual, Some(&field.value.range)) {
                return None;
            }
            values[slot] = Some(value);
        }
        Some((Expr::StructLit(sid, values.into_iter().collect::<Option<_>>()?), Type::Struct(sid)))
    }

    pub(super) fn lower_committed_specialized_struct(&mut self, sid: usize) -> Option<(Expr, Type)> {
        if *self.current() != Kind::LBrace || std::env::var_os("AE_DEBUG_MIR").is_some() {
            return None;
        }
        let offset = self.tokens[self.pos].offset;
        let (end, fields) = self.resolved_expressions().specialized_struct_fields.get(&offset)?.clone();
        let cursor = self.tokens.partition_point(|token| token.offset < end);
        let close = self.tokens.get(cursor.checked_sub(1)?)?;
        if close.kind != Kind::RBrace || close.end != end {
            return None;
        }
        let depth = 128usize.checked_sub(self.expression_depth)?.min(64);
        let budget = 128usize.checked_sub(self.expression_nodes)?;
        let mut nodes_left = budget;
        let mut effects = SharedExpressionEffects::default();
        let value = self.lower_shared_struct_fields(sid, &fields, depth, &mut nodes_left, &mut effects)?;
        self.commit_shared_effects(effects);
        self.expression_nodes += budget - nodes_left;
        self.pos = cursor;
        Some(value)
    }

    fn lower_shared_call_arguments(
        &mut self,
        arguments: &[ae_syntax::expression::Expression],
        params: &[Type],
        remaining: usize,
        nodes_left: &mut usize,
        effects: &mut SharedExpressionEffects,
    ) -> Option<Vec<Argument>> {
        use ae_syntax::expression::ExpressionKind;
        let mut args = Vec::with_capacity(arguments.len());
        for (argument, expected) in arguments.iter().zip(params.iter().copied()) {
            if matches!(expected, Type::VecRef | Type::VecFRef | Type::StrVecRef | Type::Array(_) | Type::StructRef(_) | Type::StructMut(_)) {
                let ExpressionKind::BorrowArgument { name, name_range, mutable } = &argument.kind else {
                    return None;
                };
                if *mutable != matches!(expected, Type::StructMut(_)) {
                    return None;
                }
                let (Expr::Variable(slot, ..), actual) = self.lower_resolved_local(name, name_range.start)? else {
                    return None;
                };
                if effects.moves.contains(&slot) {
                    return None;
                }
                use super::declarations::CallBorrow;
                let borrow = CallBorrow::classify(expected, actual)?;
                if let CallBorrow::Struct { .. } = borrow {
                    if !*mutable {
                        if effects.mut_borrows.contains(&slot)
                            || self.mut_borrows.iter().any(|&(borrowed, _)| borrowed == slot)
                        {
                            return None;
                        }
                        if !self.is_borrowed(slot) && !effects.call_borrows.contains(&slot) {
                            effects.call_borrows.push(slot);
                        }
                    } else {
                        if !self.names.get(name)?.1 || effects.call_borrows.contains(&slot)
                            || effects.mut_borrows.contains(&slot)
                        {
                            return None;
                        }
                        if self.is_borrowed(slot) {
                            if !self.only_view_borrows_active(slot) {
                                return None;
                            }
                            effects.conflicts.push((slot, name_range.start, name.clone(), "mutate", None));
                        }
                        effects.mut_borrows.push(slot);
                    }
                }
                args.push(Argument::Borrow(slot, borrow.address()));
                continue;
            }
            let value = self.lower_shared_argument(argument, expected, remaining, nodes_left, effects)?;
            args.push(match (expected, value) {
                (Type::StrView, (value, Type::String | Type::StrView)) => Argument::View(value),
                (expected, (value, actual)) if self.matches_value_type(expected, actual, Some(&argument.range)) => Argument::Value(value),
                _ => return None,
            });
        }
        Some(args)
    }

    pub(super) fn lower_committed_specialized_arguments(&mut self, params: &[Type]) -> Option<Vec<Argument>> {
        self.lower_indexed_argument_list(params, Self::lower_shared_call_arguments)
    }

    pub(super) fn lower_committed_enum_payload(&mut self, payload: &[Type]) -> Option<Vec<Expr>> {
        self.lower_indexed_argument_list(payload, Self::lower_shared_enum_payload)
    }

    fn lower_indexed_argument_list<T>(
        &mut self,
        params: &[Type],
        lower: impl FnOnce(&mut Self, &[ae_syntax::expression::Expression], &[Type], usize, &mut usize, &mut SharedExpressionEffects) -> Option<T>,
    ) -> Option<T> {
        if *self.current() != Kind::LParen {
            return None;
        }
        let offset = self.tokens[self.pos].offset;
        let arguments = self.resolved_expressions().specialized_call_arguments.get(&offset)?.clone();
        if arguments.len() != params.len() || arguments.len() > 64 {
            return None;
        }
        let mut cursor = self.pos + 1;
        for (index, argument) in arguments.iter().enumerate() {
            if self.tokens.get(cursor)?.offset != argument.range.start {
                return None;
            }
            cursor = self.tokens.partition_point(|token| token.offset < argument.range.end);
            if index + 1 < arguments.len() {
                if self.tokens.get(cursor)?.kind != Kind::Comma {
                    return None;
                }
                cursor += 1;
            }
        }
        if self.tokens.get(cursor)?.kind != Kind::RParen {
            return None;
        }
        let depth = 128usize.checked_sub(self.expression_depth)?.min(64);
        let budget = 128usize.checked_sub(self.expression_nodes)?;
        let mut nodes_left = budget;
        let mut effects = SharedExpressionEffects::default();
        let args = lower(self, &arguments, params, depth, &mut nodes_left, &mut effects)?;
        self.commit_shared_effects(effects);
        self.expression_nodes += budget - nodes_left;
        self.pos = cursor + 1;
        Some(args)
    }

    fn commit_shared_effects(&mut self, effects: SharedExpressionEffects) {
        self.call_borrows.extend(effects.call_borrows.into_iter().map(|slot| (slot, self.scopes.len())));
        self.mut_borrows.extend(effects.mut_borrows.into_iter().map(|slot| (slot, self.scopes.len())));
        self.deferred_write_conflicts.extend(effects.conflicts);
        self.moved.extend(effects.moves);
    }

    fn shared_expression_start(kind: &Kind) -> bool {
        matches!(kind, Kind::Number(_) | Kind::Float(_) | Kind::True | Kind::False |
            Kind::Text(_) | Kind::LParen | Kind::Ident(_) | Kind::Amp)
            || ae_syntax::operator::prefix_binding_power(kind).is_some()
    }

    pub(super) fn lower_shared_value(&mut self, boundary: Option<Kind>, moving: bool) -> Option<(Expr, Type)> {
        if !Self::shared_expression_start(self.current()) {
            return None;
        }
        let offset = self.tokens.get(self.pos)?.offset;
        let root = self.resolved_expressions().values.get(&offset).cloned()?;
        let expression = match (boundary.as_ref(), &root.kind) {
            (Some(Kind::Question), ae_syntax::expression::ExpressionKind::Propagate(operand)) => operand.as_ref(),
            (Some(Kind::Question), _) => return None,
            _ => root.as_ref(),
        };
        let end = self.tokens.partition_point(|token| token.offset < expression.range.end);
        let next = &self.tokens.get(end)?.kind;
        // Leave the delimiter for the surrounding typed statement.
        // A propagation request lowers only the operand before '?'.
        let complete = match boundary.as_ref() {
            Some(expected) => next == expected,
            None => matches!(next, Kind::Semi | Kind::Comma | Kind::RParen | Kind::RBracket | Kind::DotDot),
        };
        if !complete {
            return None;
        }
        let (depth, budget) = if boundary.is_none() {
            (128usize.checked_sub(self.expression_depth)?.min(64),
             128usize.checked_sub(self.expression_nodes)?)
        } else {
            (64, 128)
        };
        let mut nodes_left = budget;
        let mut effects = SharedExpressionEffects::default();
        let value = match &expression.kind {
            ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(name))
                if self.names.contains_key(name) =>
            {
                nodes_left = nodes_left.checked_sub(1)?;
                let value = self.lower_resolved_local(name, expression.range.start)?;
                let Expr::Variable(id, ty, ..) = &value.0 else { unreachable!("resolved local") };
                // Owned locals move on `moving`; copy-semantics handles
                // (Exec, Pool, Chan, Atomic, Mutex, Guard, TaskScope) and
                // views never move, so their reads route the same way the
                // typed parse reads them.
                if moving && ty.owned() {
                    let id = *id;
                    let start = self.pos;
                    self.pos = end;
                    if self.check_variable_move(id).is_err() {
                        self.pos = start;
                        return None;
                    }
                    self.moved.insert(id);
                }
                value
            }
            _ => self.lower_shared_expression(expression, depth, &mut nodes_left, &mut effects)?,
        };
        self.commit_shared_effects(effects);
        if boundary.is_none() {
            self.expression_nodes += budget - nodes_left;
        }
        self.pos = end;
        Some(value)
    }

    pub(super) fn spawn_callable(&mut self) -> Result<(Expr, Type), ae_span::Diagnostic> {
        if let Some(header) = self.shared_closure_header() {
            let end = self.tokens.partition_point(|token| token.offset < header.range.end);
            if self.tokens.get(end).is_some_and(|token| token.kind == Kind::RParen)
                && self.expression_depth < 128
            {
                if self.expression_depth == 0 {
                    self.expression_nodes = 0;
                }
                self.expression_depth += 1;
                self.expression_nodes += 1;
                let result = if self.expression_nodes > 128 {
                    Err(self.fail("expression complexity limit exceeded (128)"))
                } else {
                    self.closure_literal()
                };
                self.expression_depth -= 1;
                return result;
            }
        }
        self.expression(0, false)
    }

    fn lower_shared_array(&mut self) -> Option<Vec<Expr>> {
        use ae_syntax::expression::ExpressionKind;

        let start = self.tokens.get(self.pos)?.offset;
        let root = self.resolved_expressions().values.get(&start).cloned()?;
        let ExpressionKind::Array(elements) = &root.kind else {
            return None;
        };
        let end = self.tokens.partition_point(|token| token.offset < root.range.end);
        if elements.is_empty() || elements.len() > 4096 || self.tokens.get(end)?.kind != Kind::Semi {
            return None;
        }
        let mut effects = SharedExpressionEffects::default();
        let mut values = Vec::with_capacity(elements.len());
        for element in elements {
            let mut nodes_left = 128;
            let (value, Type::Int) = self.lower_shared_expression(element, 64, &mut nodes_left, &mut effects)? else {
                return None;
            };
            values.push(value);
        }
        self.pos = end + 1;
        self.commit_shared_effects(effects);
        Some(values)
    }

    fn lower_shared_iterable(&mut self) -> Option<(Expr, Type)> {
        use ae_syntax::expression::ExpressionKind;

        let offset = self.tokens.get(self.pos)?.offset;
        let root = self.resolved_expressions().values.get(&offset).cloned()?;
        let ExpressionKind::Atom(Kind::Ident(name)) = &root.kind else {
            return None;
        };
        let end = self.tokens.partition_point(|token| token.offset < root.range.end);
        if self.tokens.get(end)?.kind != Kind::LBrace || name == "Self" || self.struct_names.contains_key(name) {
            return None;
        }
        let value = self.lower_resolved_local(name, offset)?;
        if !matches!(value.1, Type::Tokens | Type::Vec | Type::VecRef | Type::VecF | Type::VecFRef | Type::StrVec | Type::StrVecRef) {
            return None;
        }
        self.pos = end;
        Some(value)
    }

    fn condition(&mut self) -> Result<Expr, ae_span::Diagnostic> {
        let (condition, ty) = match self.lower_shared_value(Some(Kind::LBrace), false) {
            Some(value) => value,
            None => self.expression(0, false)?,
        };
        if ty != Type::Bool {
            return Err(self.fail("condition must be a boolean expression").with_code("E0205"));
        }
        Ok(condition)
    }

    fn store_field_name(&mut self, target_offset: usize) -> Result<String, ae_span::Diagnostic> {
        use ae_syntax::expression::ExpressionKind;

        self.take(Kind::Dot)?;
        if let Some(root) = self.resolved_expressions().values.get(&target_offset) {
            let field = match &root.kind {
                ExpressionKind::Index { base, .. } => base.as_ref(),
                _ => root.as_ref(),
            };
            if let ExpressionKind::Field { base, name } = &field.kind
                && matches!(base.kind, ExpressionKind::Atom(Kind::Ident(_)))
                    && matches!(self.current(), Kind::Ident(token_name) if token_name == name)
                {
                    let name = name.clone();
                    self.pos += 1;
                    return Ok(name);
                }
        }
        self.identifier()
    }

    fn indexed_statement(&self) -> Option<(IndexedStatement, usize)> {
        let start = self.tokens.get(self.pos)?.offset;
        let &(kind, end) = self.resolved_expressions().statements.get(&start)?;
        Some((kind, self.tokens.partition_point(|token| token.offset < end)))
    }

    /// The loop binding's name from the shared for statement, when the
    /// resolver tree indexed this `for` (ADR-0229 slice 5): the binding
    /// name and whether the iterable is a range. Falls back to the token
    /// reads when the index misses.
    fn for_header(&self, loop_offset: usize) -> Option<(String, bool)> {
        let Some((IndexedStatement::For { binding_start, binding_end, is_range }, _)) =
            self.resolved_expressions().statements.get(&loop_offset) else {
            return None;
        };
        let (binding_start, binding_end, is_range) = (*binding_start, *binding_end, *is_range);
        let name = self.source.get(binding_start..binding_end)?.to_owned();
        Some((name, is_range))
    }

    fn binding_header(&self) -> Result<ae_syntax::statement::BindingHeader<'_>, ae_span::Diagnostic> {
        let offset = self.tokens[self.pos].offset;
        if let Some(&(IndexedStatement::Binding { name_start, name_end, mutable }, _)) =
            self.resolved_expressions().statements.get(&offset)
        {
            let name_range = name_start..name_end;
            if let Some(name) = self.source.get(name_range.clone()) {
                return Ok(ae_syntax::statement::BindingHeader { name, name_range, mutable });
            }
        }
        ae_syntax::statement::parse_binding_header(&self.file, self.source, &self.tokens, self.pos)
    }

    fn lower_output_from_syntax(&mut self) -> Option<Statement> {
        use ae_syntax::expression::ExpressionKind;

        let [_, open, argument, ..] = &self.tokens[self.pos..] else {
            return None;
        };
        if open.kind != Kind::LParen || !Self::shared_expression_start(&argument.kind) {
            return None;
        }
        let (print, argument, end) = if let Some((kind, end)) = self.indexed_statement() {
            let (print, offset) = match kind {
                IndexedStatement::Print(offset) => (true, offset),
                IndexedStatement::Assert(offset) => (false, offset),
                _ => return None,
            };
            let argument = self.resolved_expressions().values.get(&offset).cloned()?;
            (print, argument, end)
        } else {
            use ae_syntax::statement::{parse_statement_tokens, StatementKind};

            let (statement, end) = parse_statement_tokens(&self.file, self.source, &self.tokens, self.pos).ok()?;
            let (print, argument) = match statement.kind {
                StatementKind::Print(value) => (true, value),
                StatementKind::Assert(value) => (false, value),
                _ => return None,
            };
            (print, Rc::new(argument), end)
        };

        // Commit argument effects and advance the cursor only after the entire
        // statement lowers successfully.
        let mut nodes_left = 128;
        let mut effects = SharedExpressionEffects::default();
        let lowered = if print {
            match &argument.kind {
                ExpressionKind::Atom(Kind::Text(value)) => Statement::Print(Printed::Text(value.clone())),
                _ => {
                    let (value, ty) = self.lower_shared_read(&argument, 64, &mut nodes_left, &mut effects)?;
                    if !matches!(ty, Type::Int | Type::Bool | Type::F64 | Type::String | Type::StrView) {
                        return None;
                    }
                    Statement::Print(Printed::Expr(value))
                }
            }
        } else {
            let (condition, Type::Bool) = self.lower_shared_expression(&argument, 64, &mut nodes_left, &mut effects)? else {
                return None;
            };
            let (line, column) = self.position();
            Statement::Assert(condition, line, column)
        };
        self.pos = end;
        self.commit_shared_effects(effects);
        Some(lowered)
    }

    pub(super) fn block_inner(&mut self) -> Result<Vec<Statement>, ae_span::Diagnostic> {
        self.take(Kind::LBrace)?;
        let mut declared = Vec::new();
        let mut result = Vec::new();
        while *self.current() != Kind::RBrace {
            // A call argument's borrow covered only the statement that made
            // the call; the next statement may write the struct again.
            self.call_borrows.clear();
            self.mut_borrows.clear();
            match self.current().clone() {
                Kind::Let | Kind::Var => {
                    let header = self.binding_header()?;
                    let name = header.name.to_owned();
                    let binding_range = header.name_range;
                    let mutable = header.mutable;
                    self.pos += 2;
                    if RESERVED.contains(&name.as_str()) { return Err(self.fail(&format!("'{name}' is a built-in name and cannot be a variable"))); }
                    if self.struct_names.contains_key(&name) { return Err(self.fail(&format!("'{name}' names a struct and cannot be a variable"))); }
                    if self.enum_names.contains_key(&name) { return Err(self.fail(&format!("'{name}' names an enum and cannot be a variable"))); }
                    if self.names.contains_key(&name) { return Err(self.duplicate_binding_error(&name)); }
                    self.take(Kind::Equal)?;
                    let id = self.next_var; self.next_var += 1;
                    let shared_array = if *self.current() == Kind::LBracket { self.lower_shared_array() } else { None };
                    let (stmt, ty) = if let Some(values) = shared_array {
                        let len = values.len();
                        (Statement::Array(id, values), Type::Array(len))
                    } else if *self.current() == Kind::LBracket {
                        self.pos += 1;
                        if *self.current() == Kind::RBracket { return Err(self.fail("array must contain at least one element")); }
                        let values = ae_syntax::parse_comma_list(
                            self,
                            |parser, count| {
                                if count == 4096 {
                                    return Err(parser.fail("array element limit exceeded (4096)"));
                                }
                                let (element, element_ty) = parser.expression(0, false)?;
                                if element_ty != Type::Int {
                                    return Err(parser.fail("array elements must be i64"));
                                }
                                Ok(element)
                            },
                            |parser| {
                                if *parser.current() != Kind::Comma {
                                    return false;
                                }
                                parser.pos += 1;
                                true
                            },
                        )?;
                        self.take(Kind::RBracket)?; self.take(Kind::Semi)?;
                        let len = values.len(); (Statement::Array(id, values), Type::Array(len))
                    } else {
                        // The typed initializer consumes string values.
                        let (value, ty) = match self.lower_shared_value(Some(Kind::Question), true) {
                            Some(value) => value,
                            None => self.expression(0, true)?,
                        };
                        // `let x = call()?;` binds the Ok payload and returns the
                        // error to the caller instead of continuing.
                        if *self.current() == Kind::Question {
                            let (statement, bound_ty) = self.propagate_statement(value, ty, Some(id))?;
                            self.pos += 1;
                            self.take(Kind::Semi)?;
                            let bound_ty = bound_ty.expect("a binding propagation always produces a payload type");
                            self.names.insert(name.clone(), (id, mutable, bound_ty));
                            self.binding_ranges.insert(id, binding_range);
                            self.scopes.last_mut().unwrap().push((id, bound_ty));
                            declared.push(name);
                            result.push(statement);
                            continue;
                        }
                        // A view binds once (M2.5): the NLL regions treat view
                        // slots as declare-only — a mutable binding would let a
                        // reassignment re-point the view at different bytes
                        // with no kill in the region math, so it is rejected
                        // before it can exist.
                        self.check_view_binding(mutable, ty)?;
                        self.take(Kind::Semi)?;
                        // A borrow has no storage of its own: it lives only as a
                        // parameter or a pattern binding, never as a variable.
                        self.check_borrowed_binding(ty)?;
                        (Statement::Declare(id, value), ty)
                    };
                    self.names.insert(name.clone(), (id, mutable, ty));
                    self.binding_ranges.insert(id, binding_range);
                    self.scopes.last_mut().unwrap().push((id, ty));
                    declared.push(name);
                    if let Statement::Declare(_, init) = &stmt { self.record_borrow_bases(init); }
                    result.push(stmt);
                }
                Kind::Ident(name) => {
                    // Kept for diagnostics that must point at the statement's
                    // own name rather than at whatever token follows it.
                    let name_offset = self.tokens[self.pos].offset;
                    // `exit(code);` terminates the process. It releases the
                    // live owned values first, so the allocation account stays
                    // balanced and buffered output still flushes.
                    if name == "exit" && self.tokens.get(self.pos + 1).map(|t| t.kind.clone()) == Some(Kind::LParen) {
                        self.pos += 2;
                        let (code, code_ty) = match self.lower_shared_value(Some(Kind::RParen), false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        };
                        result.push(self.finish_exit(code, code_ty)?);
                        continue;
                    }
                    // Built-in mutation statements come before assignment so
                    // they stay unambiguous: push(v, x); pop(v);
                    // atomic_store(handle, value); — the seq_cst release
                    // half of the handoff (spec/concurrency.md §8).
                    if name == "cell_store" && self.tokens.get(self.pos + 1).map(|t| t.kind.clone()) == Some(Kind::LParen) {
                        self.pos += 2;
                        let (handle, handle_ty) = match self.lower_shared_value(Some(Kind::Comma), false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        };
                        if handle_ty != Type::Atomic { return Err(self.fail("cell_store expects an atomic handle")); }
                        self.take(Kind::Comma)?;
                        let (value, value_ty) = match self.lower_shared_value(Some(Kind::RParen), false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        };
                        if value_ty != Type::Int { return Err(self.fail("cell_store expects an i64 value")); }
                        self.take(Kind::RParen)?; self.take(Kind::Semi)?;
                        result.push(Statement::AtomicStore(Box::new(handle), Box::new(value)));
                        continue;
                    }
                    if matches!(name.as_str(), "shared_set" | "chan_close" | "task_spawn" | "task_yield" | "scope_spawn" | "pool_spawn") && self.tokens.get(self.pos + 1).map(|t| t.kind.clone()) == Some(Kind::LParen) {
                        result.push(self.concurrency_stmt(&name)?);
                        continue;
                    }
                    if matches!(name.as_str(), "push" | "pop" | "sort" | "reverse") && self.tokens.get(self.pos + 1).map(|t| t.kind.clone()) == Some(Kind::LParen) {
                        let (line, col) = self.position();
                        self.pos += 2;
                        let target = self.identifier()?;
                        let (id, mutable, ty) = *self.names.get(&target).ok_or_else(|| self.fail(&format!("unknown variable '{target}'")).with_code("E0101"))?;
                        if self.moved.contains(&id) { return Err(moved_value_hint(self.binding_label(self.fail(&format!("cannot use '{target}' because it was moved")).with_code("E0102"), id))); }
                        if !matches!(ty, Type::Vec | Type::StrVec | Type::VecF) { return Err(self.fail(&format!("{name} expects a vec, found {}", ty.name(self.ctx)))); }
                        if !mutable { return Err(self.immutable_binding_error(self.fail(&format!("cannot modify immutable variable '{target}'")).with_code("E0103"), id)); }
                        if self.is_borrowed(id) { return Err(self.fail(&format!("cannot modify '{target}' while it is borrowed"))); }
                        // Ordering in place: qsort for i64, byte order for
                        // strings, so sorted output is identical on both
                        // platforms. The vector keeps its length.
                        if name == "sort" || name == "reverse" {
                            if ty == Type::VecF { return Err(self.fail(&format!("{name} is not supported for vec[f64]; NaN has no total order"))); }
                            self.take(Kind::RParen)?; self.take(Kind::Semi)?;
                            let strings = ty == Type::StrVec;
                            result.push(if name == "sort" { Statement::Sort(id, strings, line, col) } else { Statement::Reverse(id, strings, line, col) });
                            continue;
                        }
                        if name == "push" {
                            self.take(Kind::Comma)?;
                            let (value, value_ty) = match self.lower_shared_value(Some(Kind::RParen), ty == Type::StrVec) {
                                Some(pair) => pair,
                                None => self.expression(0, ty == Type::StrVec)?,
                            };
                            // A borrow is fine: the element is a copy, so a view
                            // can be stored without outliving its source.
                            let ok = match ty {
                                Type::Vec => value_ty == Type::Int,
                                Type::VecF => value_ty == Type::F64,
                                _ => matches!(value_ty, Type::String | Type::StrView),
                            };
                            if !ok { return Err(self.fail(&format!("vector elements must be {}", match ty { Type::Vec => "i64", Type::VecF => "f64", _ => "string" }))); }
                            self.take(Kind::RParen)?; self.take(Kind::Semi)?;
                            if ty == Type::StrVec {
                                result.push(Statement::StrVecPush(id, value, line, col, value_ty == Type::StrView));
                            } else if ty == Type::VecF {
                                result.push(Statement::VecFPush(id, value, line, col));
                            } else {
                                result.push(Statement::VecPush(id, value, line, col));
                            }
                        } else {
                            self.take(Kind::RParen)?; self.take(Kind::Semi)?;
                            if ty == Type::StrVec {
                                result.push(Statement::StrVecPop(id, line, col));
                            } else if ty == Type::VecF {
                                result.push(Statement::VecFPop(id, line, col));
                            } else {
                                result.push(Statement::VecPop(id, line, col));
                            }
                        }
                        continue;
                    }
                    if !self.names.contains_key(&name) {
                        // A call used only for its effect. The value must be
                        // propagated with '?', so a failure is never dropped.
                        result.push(self.lower_propagating_call()?);
                        continue;
                    }
                    let (id, mutable, ty) = *self.names.get(&name).ok_or_else(|| self.fail(&format!("unknown variable '{name}'")).with_code("E0101"))?;
                    if !mutable { return Err(self.immutable_binding_error(self.fail(&format!("cannot assign to immutable variable '{name}'")).with_code("E0103"), id)); }
                    self.pos += 1;
                    if (ty == Type::Map || ty == Type::MapF) && *self.current() == Kind::LBracket {
                        // `m[key] = value;` inserts or overwrites. The key is
                        // copied by the runtime, so a view or temporary is fine.
                        if self.is_borrowed(id) {
                            return Err(self.fail(&format!("cannot modify '{name}' while it is borrowed")));
                        }
                        let (line, col) = self.position();
                        self.take(Kind::LBracket)?;
                        let (key, key_ty) = match self.lower_shared_value(Some(Kind::RBracket), false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        };
                        if !matches!(key_ty, Type::String | Type::StrView) { return Err(self.fail("map keys must be strings")); }
                        self.take(Kind::RBracket)?; self.take(Kind::Equal)?;
                        let (value, value_ty) = match self.lower_shared_value(None, false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        }; self.take(Kind::Semi)?;
                        if value_ty != map_element_ty(map_kind_of(ty)) { return Err(self.fail(&format!("map values must be {}", map_element_ty(map_kind_of(ty)).name(self.ctx)))); }
                        result.push(Statement::MapPut(MapPlace::Var(id, map_kind_of(ty)), key, value, line, col));
                    } else if let Type::Array(len) = ty {
                        let (line, col) = self.position(); self.take(Kind::LBracket)?;
                        let (index, index_ty) = match self.lower_shared_value(Some(Kind::RBracket), false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        };
                        self.check_array_index(super::expression::operator_scalar(index_ty), None)?;
                        self.take(Kind::RBracket)?; self.take(Kind::Equal)?;
                        let (value, value_ty) = match self.lower_shared_value(None, false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        }; self.take(Kind::Semi)?;
                        if value_ty != Type::Int { return Err(self.fail("array elements must be i64")); }
                        result.push(Statement::Store(id, len, index, value, line, col));
                    } else if ty == Type::StrVec && *self.current() == Kind::LBracket {
                        // Overwriting an element releases the old string, so a
                        // live borrow of any element blocks it.
                        if self.is_borrowed(id) {
                            return Err(self.fail(&format!("cannot modify '{name}' while it is borrowed")));
                        }
                        let (line, col) = self.position(); self.take(Kind::LBracket)?;
                        let (index, index_ty) = match self.lower_shared_value(Some(Kind::RBracket), false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        };
                        if index_ty != Type::Int { return Err(self.fail("vector index must be an integer")); }
                        self.take(Kind::RBracket)?; self.take(Kind::Equal)?;
                        let (value, value_ty) = match self.lower_shared_value(None, true) {
                            Some(pair) => pair,
                            None => self.expression(0, true)?,
                        }; self.take(Kind::Semi)?;
                        if !matches!(value_ty, Type::String | Type::StrView) { return Err(self.fail("vector elements must be string")); }
                        result.push(Statement::StrVecStore(id, index, value, line, col, value_ty == Type::StrView));
                    } else if (ty == Type::Vec || ty == Type::VecF) && *self.current() == Kind::LBracket {
                        if self.is_borrowed(id) {
                            return Err(self.fail(&format!("cannot modify '{name}' while it is borrowed")));
                        }
                        let (line, col) = self.position(); self.take(Kind::LBracket)?;
                        let (index, index_ty) = match self.lower_shared_value(Some(Kind::RBracket), false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        };
                        if index_ty != Type::Int { return Err(self.fail("vector index must be an integer")); }
                        self.take(Kind::RBracket)?; self.take(Kind::Equal)?;
                        let (value, value_ty) = match self.lower_shared_value(None, false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        }; self.take(Kind::Semi)?;
                        if ty == Type::VecF {
                            if value_ty != Type::F64 { return Err(self.fail("vector elements must be f64")); }
                            result.push(Statement::VecFStore(id, index, value, line, col));
                        } else {
                            if value_ty != Type::Int { return Err(self.fail("vector elements must be i64")); }
                            result.push(Statement::VecStore(id, index, value, line, col));
                        }
                    } else if let Type::Enum(_, _) = ty {
                        // Assigning an enum releases the old payload first.
                        if self.is_borrowed(id) {
                            return Err(self.fail(&format!("cannot assign to '{name}' while it is borrowed")));
                        }
                        self.take(Kind::Equal)?;
                        let moved_before = self.moved.contains(&id);
                        let (value, value_ty) = match self.lower_shared_value(None, true) {
                            Some(pair) => pair,
                            None => self.expression(0, true)?,
                        }; self.take(Kind::Semi)?;
                        if value_ty != ty { return Err(self.fail(&format!("cannot assign a {} value to {} variable", value_ty.name(self.ctx), ty.name(self.ctx)))); }
                        if self.moved.contains(&id) && !moved_before {
                            return Err(moved_self_assignment_hint(self.binding_label(self.error_at(name_offset, &format!("cannot assign to '{name}' because it was moved in the same statement")).with_code("E0102"), id)));
                        }
                        self.moved.remove(&id);
                        result.push(Statement::Assign(id, value, true));
                    } else if let Type::StructRef(_) = ty {
                        // A struct borrow is read-only (ADR-0184): neither the
                        // pointer nor anything behind it may be written, so
                        // both spellings get their own diagnostic instead of
                        // falling into the scalar-assignment path.
                        if *self.current() == Kind::Dot {
                            return Err(self.fail(&format!("cannot write through '{name}': struct borrows are read-only in this version")));
                        }
                        return Err(self.fail(&format!("cannot assign to '{name}': struct borrows are read-only in this version")));
                    } else if let Type::StructMut(sid) = ty {
                        // A mutable borrow exists to write through (ADR-0185):
                        // field stores go through the pointer. Whole-struct
                        // assignment would have to release the old pointee
                        // before storing, which needs a dedicated statement —
                        // rejected for now rather than guessed at.
                        if *self.current() == Kind::Dot {
                            let fname = self.store_field_name(name_offset)?;
                            let fty = {
                                let def = self.structs[sid].clone();
                                let fidx = def.fields.iter().position(|(n, _)| *n == fname)
                                    .ok_or_else(|| self.fail(&format!("struct '{}' has no field '{}'", def.name, fname)))?;
                                def.fields[fidx].1
                            };
                            if matches!(fty, Type::Map | Type::MapF) {
                                return Err(self.fail("map fields cannot be written through a '&mut' borrow yet; copy the map out with a by-value receiver instead"));
                            }
                            self.take(Kind::Equal)?;
                            // An owned field write consumes a string/vec
                            // right side — the field owns the bytes after
                            // the write, so the source must forfeit them
                            // (the same rule the scalar assignment uses).
                            let (value, value_ty) = match self.lower_shared_value(None, true) {
                            Some(pair) => pair,
                            None => self.expression(0, true)?,
                        }; self.take(Kind::Semi)?;
                            self.check_field_value_type(&fname, fty, value_ty, None)?;
                            result.push(Statement::FieldStore(id, fname, fty, value, ty));
                            continue;
                        }
                        return Err(self.fail(&format!("cannot assign to '{name}' itself through a '&mut' borrow; write its fields instead")));
                    } else if let Type::Struct(sid) = ty {
                        if *self.current() == Kind::Dot {
                            let fname = self.store_field_name(name_offset)?;
                            let fty = {
                                let def = self.structs[sid].clone();
                                let fidx = def.fields.iter().position(|(n, _)| *n == fname)
                                    .ok_or_else(|| self.fail(&format!("struct '{}' has no field '{}'", def.name, fname)))?;
                                def.fields[fidx].1
                            };
                            if (fty == Type::Map || fty == Type::MapF) && *self.current() == Kind::LBracket {
                                // `s.counts[key] = n;` writes through the field,
                                // so the record itself stays in place.
                                if self.is_borrowed(id) {
                                    return Err(self.fail(&format!("cannot modify '{name}' while it is borrowed")));
                                }
                                let (line, col) = self.position();
                                self.take(Kind::LBracket)?;
                                let (key, key_ty) = match self.lower_shared_value(Some(Kind::RBracket), false) {
                                    Some(pair) => pair,
                                    None => self.expression(0, false)?,
                                };
                                if !matches!(key_ty, Type::String | Type::StrView) { return Err(self.fail("map keys must be strings")); }
                                self.take(Kind::RBracket)?; self.take(Kind::Equal)?;
                                let (value, value_ty) = match self.lower_shared_value(None, false) {
                            Some(pair) => pair,
                            None => self.expression(0, false)?,
                        }; self.take(Kind::Semi)?;
                                if value_ty != map_element_ty(map_kind_of(fty)) { return Err(self.fail(&format!("map values must be {}", map_element_ty(map_kind_of(fty)).name(self.ctx)))); }
                                result.push(Statement::MapPut(MapPlace::Field(id, fname, map_kind_of(fty)), key, value, line, col));
                                continue;
                            }
                            // A view into one of the fields points at the bytes
                            // this write would release, so the write is only
                            // allowed once the view is gone (ADR-52) — the
                            // view-kind case is deferred to the graph's NLL
                            // adjudication at the variable granularity the
                            // borrow uses; container borrows reject on sight.
                            if self.is_borrowed(id) && !self.all_borrows_are_views(id) {
                                return Err(self.fail(&format!("cannot modify '{name}' while it is borrowed")));
                            }
                            if self.is_borrowed(id) {
                                self.deferred_write_conflicts.push((id, name_offset, name.clone(), "modify", Some(fname.clone())));
                            }
                            let compound_op = match *self.current() {
                                Kind::PlusEqual => Some(Kind::Plus),
                                Kind::MinusEqual => Some(Kind::Minus),
                                Kind::StarEqual => Some(Kind::Star),
                                Kind::SlashEqual => Some(Kind::Slash),
                                Kind::PercentEqual => Some(Kind::Percent),
                                _ => None,
                            };
                            if let Some(op) = compound_op {
                                let allowed = matches!(fty, Type::Int)
                                    || (matches!(fty, Type::F64) && op != Kind::Percent)
                                    || (fty == Type::String && op == Kind::Plus);
                                if !allowed {
                                    return Err(self.fail(&format!("compound assignment is not supported for field type '{}'", fty.name(self.ctx))));
                                }
                                self.pos += 1;
                                let (line, col) = self.position();
let (rhs, rhs_ty) = match self.lower_shared_value(None, false) {
    Some(pair) => pair,
    None => self.expression(0, false)?,
};
                                self.take(Kind::Semi)?;
                                self.check_field_value_type(&fname, fty, rhs_ty, None)?;
                                let base_var = Expr::Variable(id, ty, line, col);
                                let lhs_field = Expr::Field(Box::new(base_var), fname.clone(), fty);
                                let value = Expr::Binary(Box::new(lhs_field), op, Box::new(rhs), line, col, fty);
                                result.push(Statement::FieldStore(id, fname, fty, value, ty));
                                continue;
                            }
                            self.take(Kind::Equal)?;
                            // Same consuming rule as the `&mut` write: the
                            // field owns the bytes afterwards.
                            let (value, value_ty) = match self.lower_shared_value(None, true) {
                            Some(pair) => pair,
                            None => self.expression(0, true)?,
                        }; self.take(Kind::Semi)?;
                            self.check_field_value_type(&fname, fty, value_ty, None)?;
                            result.push(Statement::FieldStore(id, fname, fty, value, ty));
                            continue;
                        }
                        // Whole-struct assignment moves a fresh value in.
                        if self.is_borrowed(id) {
                            return Err(self.fail(&format!("cannot assign to '{name}' while it is borrowed")));
                        }
                        self.take(Kind::Equal)?;
                        let moved_before = self.moved.contains(&id);
                        let (value, value_ty) = match self.lower_shared_value(None, true) {
                            Some(pair) => pair,
                            None => self.expression(0, true)?,
                        }; self.take(Kind::Semi)?;
                        if value_ty != ty { return Err(self.fail(&format!("cannot assign a {} value to {} variable", value_ty.name(self.ctx), ty.name(self.ctx)))); }
                        if self.moved.contains(&id) && !moved_before {
                            return Err(moved_self_assignment_hint(self.binding_label(self.error_at(name_offset, &format!("cannot assign to '{name}' because it was moved in the same statement")).with_code("E0102"), id)));
                        }
                        self.moved.remove(&id);
                        result.push(Statement::Assign(id, value, true));
                    } else {
                        // A string's borrows can be view-kind borrows, whose
                        // region dies after the view's last use (M2.5 L1):
                        // the conflict is deferred to the graph's
                        // adjudication instead of rejected on sight, so a
                        // write after the last view use parses. Container
                        // borrows keep the on-sight rejection.
                        if self.is_borrowed(id) && !self.all_borrows_are_views(id) {
                            return Err(self.fail(&format!("cannot assign to '{name}' while it is borrowed")));
                        }
                        if self.is_borrowed(id) {
                            self.deferred_write_conflicts.push((id, name_offset, name.clone(), "assign to", None));
                        }
                        let compound_op = compound_assignment_operator(self.current());
                        if let Some(op) = compound_op {
                            self.check_compound_assignment(ty, &op)?;
                            self.pos += 1;
                            let (line, col) = self.position();
                            let (rhs, rhs_ty) = match self.lower_shared_value(None, false) {
                                Some(pair) => pair,
                                None => self.expression(0, false)?,
                            };
                            self.take(Kind::Semi)?;
                            self.check_assignment_type(&name, ty, rhs_ty, None)?;
                            let lhs_var = Expr::Variable(id, ty, line, col);
                            let value = Expr::Binary(Box::new(lhs_var), op, Box::new(rhs), line, col, ty);
                            result.push(Statement::Assign(id, value, false));
                            continue;
                        }
                        self.take(Kind::Equal)?;
                        // A view slot never re-points (M2.5): the region math
                        // has no kills, so a reassigned view would both
                        // overstate the old region and silently borrow a base
                        // the recorded introduction never names. Bind a new
                        // view with 'let' instead.
                        if matches!(ty, Type::StrView | Type::Tokens) {
                            return Err(self.fail("a view binding cannot be reassigned; declare a new view with 'let'"));
                        }
                        let moved_before = self.moved.contains(&id);
                        let (value, value_ty) = match self.lower_shared_value(None, true) {
                            Some(pair) => pair,
                            None => self.expression(0, true)?,
                        }; self.take(Kind::Semi)?;
                        self.check_assignment_type(&name, ty, value_ty, None)?;
                        // The right-hand side may not consume the target itself;
                        // assigning to an already-moved variable is a legal
                        // re-initialization, so the move flag clears either way.
                        if self.moved.contains(&id) && !moved_before {
                            return Err(moved_self_assignment_hint(self.binding_label(self.error_at(name_offset, &format!("cannot assign to '{name}' because it was moved in the same statement")).with_code("E0102"), id)));
                        }
                        self.moved.remove(&id);
                        // Every owned slot releases its old value before the new
                        // one moves in; scalars have nothing to release.
                        result.push(Statement::Assign(id, value, ty.owned()));
                    }
                }
                Kind::Return => {
                    if self.in_main { return Err(self.fail("return is not supported in main")); }
                    self.pos += 1;
                    let (value, ty) = match self.lower_shared_value(None, true) {
                        Some(pair) => pair,
                        None => self.expression(0, true)?,
                    };
                    self.take(Kind::Semi)?;
                    if matches!(ty, Type::Closure(_)) { return Err(self.fail("a closure cannot be returned: it has no storage, and its captures live in this frame")); }
                    self.check_return_type(ty, None)?;
                    // Every live owned value that is not returned is released.
                    result.push(Statement::Return(value, self.live_owned()));
                }
                Kind::Star => {
                    // `*p = value;` (M2.12 slice 3): a write through a `*mut`
                    // pointer, inside `unsafe` only. The pointer is a local
                    // variable; the pointee's current storage is whatever the
                    // pointer names.
                    let (line, column) = self.position();
                    self.pos += 1;
                    let offset = self.tokens[self.pos].offset;
                    let name = match self.resolved_expressions().pointer_store_names.get(&offset).cloned() {
                        Some(name) => {
                            self.pos += 1;
                            name
                        }
                        None => self.identifier()?,
                    };
                    let target = self.names.get(&name).copied();
                    let pointee = self.pointer_store_pointee(&name, target.map(|(_, _, ty)| ty), line)?;
                    let (var_id, _, _) = target.expect("validated pointer store target");
                    self.take(Kind::Equal)?;
                    let (value, value_ty) = match self.lower_shared_value(None, false) {
                        Some(pair) => pair,
                        None => self.expression(0, false)?,
                    };
                    self.check_pointer_store_value(value_ty, pointee)?;
                    self.take(Kind::Semi)?;
                    result.push(Statement::PtrStore(var_id, value, line, column));
                }
                Kind::Unsafe => {
                    // M2.12 slice 1 (spec/unsafe.md 3.1): an `unsafe { ... }`
                    // block at statement position. The block is a scoping
                    // container; the unchecked operations (raw deref, pointer
                    // store, extern/unsafe calls) gate on unsafe_depth. W0702
                    // (spec/unsafe.md U4): a block containing none of them is
                    // silent decoration, and says so on stderr without failing
                    // the compile.
                    self.pos += 1;
                    let block_line = self.tokens[self.pos].line;
                    let block_column = self.tokens[self.pos].column;
                    self.unsafe_depth += 1;
                    let inner = self.block()?;
                    self.unsafe_depth -= 1;
                    if !self.block_has_unchecked_operation(&inner) {
                        eprintln!("ae: {}:{}: warning: unnecessary unsafe block (W0702): the body contains no unchecked operation", block_line, block_column);
                    }
                    result.push(Statement::Unsafe(inner));
                }
                Kind::If => result.push(self.if_statement()?),
                Kind::Match => result.push(self.match_statement()?),
                Kind::While => {
                    let loop_offset = self.tokens[self.pos].offset;
                    self.pos += 1;
                    let condition = self.condition()?;
                    if self.loop_depth >= 64 { return Err(self.fail("loop nesting limit exceeded (64)")); }
                    self.loop_depth += 1;
                    let base = self.scopes.len();
                    let moved_before = self.moved.clone();
                    self.loop_scopes.push(base);
                    self.loop_labels.push(None);
                    let body = self.block();
                    self.loop_labels.pop();
                    self.loop_scopes.pop();
                    self.loop_depth -= 1;
                    let body = body?;
                    self.close_loop_moves(base, moved_before, loop_offset)?;
                    result.push(Statement::While(condition, body));
                }
                Kind::For => {
                    let loop_offset = self.tokens[self.pos].offset;
                    self.pos += 1;
                    let shared_for = self.for_header(loop_offset);
                    let var_name = match &shared_for {
                        Some((name, _)) => {
                            self.pos += 1;
                            name.clone()
                        }
                        None => self.identifier()?,
                    };
                    if RESERVED.contains(&var_name.as_str()) { return Err(self.fail(&format!("'{var_name}' is a built-in name and cannot be a loop variable"))); }
                    if self.names.contains_key(&var_name) { return Err(self.duplicate_binding_error(&var_name)); }
                    self.take(Kind::In)?;
                    let (first, first_ty) = match self.lower_shared_iterable() {
                        Some(value) => value,
                        None => self.expression(0, false)?,
                    };
                    if first_ty == Type::Tokens {
                        self.lower_token_loop(&var_name, first, loop_offset, &mut result, |parser, _, _| parser.block())?;
                        continue;
                    }
                    if *self.current() != Kind::DotDot {
                        self.lower_vector_loop(&var_name, first, first_ty, loop_offset, &mut result, |parser, _, _| parser.block())?;
                        continue;
                    }
                    if first_ty != Type::Int { return Err(self.fail("range bounds must be i64")); }
                    self.take(Kind::DotDot)?;
                    let (end, end_ty) = match self.lower_shared_value(Some(Kind::LBrace), false) {
                        Some(value) => value,
                        None => self.expression(0, false)?,
                    };
                    if end_ty != Type::Int { return Err(self.fail("range bounds must be i64")); }
                    if self.loop_depth >= 64 { return Err(self.fail("loop nesting limit exceeded (64)")); }
                    let (line, col) = self.position();
                    // Hidden storage: bounds evaluate once, in order.
                    let end_var = self.next_var; self.next_var += 1;
                    let var = self.next_var; self.next_var += 1;
                    let label = self.next_label; self.next_label += 1;
                    self.loop_depth += 1;
                    let base = self.scopes.len();
                    let moved_before = self.moved.clone();
                    self.scopes.push(vec![(var, Type::Int)]);
                    self.loop_scopes.push(self.scopes.len());
                    self.loop_labels.push(Some(label));
                    self.names.insert(var_name.clone(), (var, false, Type::Int));
                    let body = self.block();
                    self.loop_labels.pop();
                    self.loop_scopes.pop();
                    let for_scope = self.scopes.pop().unwrap();
                    self.loop_depth -= 1;
                    self.names.remove(&var_name);
                    let body = body?;
                    self.close_loop_moves(base, moved_before, loop_offset)?;
                    debug_assert!(for_scope.iter().all(|(_, ty)| *ty != Type::String));
                    // Bounds evaluate once, left to right, before any iteration.
                    result.push(Statement::Declare(var, first));
                    result.push(Statement::Declare(end_var, end));
                    result.push(Statement::For(var, end_var, body, label, line, col));
                }
                Kind::Break | Kind::Continue => {
                    let keyword = if *self.current() == Kind::Break { "break" } else { "continue" };
                    if self.loop_depth == 0 { return Err(self.fail(&format!("'{keyword}' outside of a loop"))); }
                    let (is_break, end) = match self.indexed_statement() {
                        Some((IndexedStatement::Break, end)) => (true, end),
                        Some((IndexedStatement::Continue, end)) => (false, end),
                        _ => {
                            let (statement, end) = ae_syntax::statement::parse_statement_tokens(
                                &self.file, self.source, &self.tokens, self.pos,
                            )?;
                            (matches!(statement.kind, ae_syntax::statement::StatementKind::Break), end)
                        }
                    };
                    self.pos = end;
                    let owned = self.loop_local_owned();
                    if !owned.is_empty() { result.push(Statement::Frees(owned)); }
                    result.push(if is_break {
                        Statement::Break
                    } else {
                        Statement::Continue(self.loop_labels.last().copied().flatten())
                    });
                }
                Kind::Assert => {
                    if let Some(statement) = self.lower_output_from_syntax() {
                        result.push(statement);
                        continue;
                    }
                    let (line, col) = self.position();
                    self.pos += 1; self.take(Kind::LParen)?;
                    let (condition, ty) = match self.lower_shared_value(Some(Kind::RParen), false) {
                        Some(pair) => pair,
                        None => self.expression(0, false)?,
                    };
                    if ty != Type::Bool { return Err(self.fail("assert requires a boolean condition")); }
                    self.take(Kind::RParen)?; self.take(Kind::Semi)?;
                    result.push(Statement::Assert(condition, line, col));
                }
                Kind::Print => {
                    if let Some(statement) = self.lower_output_from_syntax() {
                        result.push(statement);
                        continue;
                    }
                    self.pos += 1; self.take(Kind::LParen)?;
                    // print borrows its value; only `+` consumes strings.
                    let (value, ty) = match self.lower_shared_value(Some(Kind::RParen), false) {
                        Some(pair) => pair,
                        None => self.expression(0, false)?,
                    };
                    if !matches!(ty, Type::Int | Type::Bool | Type::F64 | Type::String | Type::StrView) {
                        return Err(self.fail(&format!("cannot print a value of type {}", ty.name(self.ctx))).with_code("E0206"));
                    }
                    self.take(Kind::RParen)?; self.take(Kind::Semi)?;
                    result.push(Statement::Print(Printed::Expr(value)));
                }
                _ => return Err(self.fail("expected declaration, assignment, print, or '}'")),
            };
        }
        self.take(Kind::RBrace)?;
        for name in declared { self.names.remove(&name); }
        Ok(result)
    }
    pub(super) fn if_statement(&mut self) -> Result<Statement, ae_span::Diagnostic> {
        self.pos += 1;
        let condition = self.condition()?;
        let snapshot = self.moved.clone();
        let yes = self.block()?;
        let after_yes = std::mem::replace(&mut self.moved, snapshot.clone());
        let no = if *self.current() == Kind::Else {
            self.pos += 1;
            // `else if` chains recurse instead of nesting braces.
            if *self.current() == Kind::If { vec![self.if_statement()?] } else { self.block()? }
        } else { Vec::new() };
        // A variable moved in either branch is treated as moved after the if.
        for id in after_yes { self.moved.insert(id); }
        Ok(Statement::If(condition, yes, no))
    }
    // `expr?` propagates the error variant of a result enum: on `Err` every
    // live owned value is released and the enum itself is returned, so a caller
    // never observes a half-finished result. On `Ok` the payload is bound (or
    // released when the statement only wanted the side effect).
    pub(super) fn propagate_statement(&mut self, value: Expr, ty: Type, binding: Option<usize>) -> Result<(Statement, Option<Type>), ae_span::Diagnostic> {
        let eid = match ty {
            Type::Enum(_, eid) => eid,
            _ => return Err(self.fail(&format!("'?' needs a result or option value, found {}", ty.name(self.ctx)))),
        };
        let def = self.enums[eid].clone();
        // Both two-variant shapes can be propagated: a result hands its Err
        // back, an Option hands its None back. Neither variant has to be a
        // prelude enum - a user enum with the same shape behaves identically.
        let result_shaped = def.variants.len() == 2
            && def.variants[0].0 == "Ok" && def.variants[1].0 == "Err"
            && def.variants[1].1 == [Type::String];
        let option_shaped = def.variants.len() == 2
            && def.variants[0].0 == "Some" && def.variants[1].0 == "None"
            && def.variants[1].1.is_empty() && def.variants[0].1.len() == 1;
        if !result_shaped && !option_shaped {
            return Err(self.fail(&format!("'?' needs an enum shaped {{ Ok(...), Err(string) }} or {{ Some(T), None }}, and '{}' is not one", def.name)));
        }
        if self.in_main || self.return_type != ty {
            return Err(self.fail(&format!("'?' needs the enclosing function to return {}, so the failure can be handed on", def.name)));
        }
        let fields = def.variants[0].1.clone();
        let bound = match binding {
            Some(id) => {
                if fields.len() != 1 {
                    return Err(self.fail(&format!("'{}::{}' carries {} values; a binding `let` needs exactly one (use a bare statement to discard)", def.name, def.variants[0].0, fields.len())));
                }
                Some((id, fields[0]))
            }
            None => None,
        };
        let bound_ty = bound.map(|(_, ty)| ty);
        Ok((Statement::Propagate(value, ty, fields, bound, self.live_owned()), bound_ty))
    }
    // `match subject { Enum::Variant(a, b) => { ... } ... }` is a statement.
    // Coverage must be exhaustive and each variant may appear once, so there is
    // no fall-through path to reason about. The subject is borrowed for the
    // whole match: arms can read payloads but never move or overwrite it.
    //
    // The arm pattern itself comes from the shared syntax parser, so both
    // parsers agree on the grammar and its byte ranges; the checks below are
    // the semantic layer (declaration shape, binding kinds, exhaustiveness).
    pub(super) fn match_statement(&mut self) -> Result<Statement, ae_span::Diagnostic> {
        self.pos += 1;
        let bare_enum = match self.current() {
            Kind::Ident(name) => name != "Self" && !self.struct_names.contains_key(name)
                && matches!(self.names.get(name), Some((_, _, Type::Enum(..))))
                && self.tokens.get(self.pos + 1).is_some_and(|token| token.kind == Kind::LBrace),
            _ => false,
        };
        let shared = if bare_enum { self.lower_shared_value(Some(Kind::LBrace), false) } else { None };
        let (subject, subject_ty) = match shared {
            Some(value) => value,
            None => self.expression(0, false)?,
        };
        self.lower_match(subject, subject_ty, |parser, _, _| parser.block())
    }

    fn lower_match(
        &mut self,
        subject: Expr,
        subject_ty: Type,
        mut lower_body: impl FnMut(&mut Self, usize, &[(String, usize)]) -> Result<Vec<Statement>, ae_span::Diagnostic>,
    ) -> Result<Statement, ae_span::Diagnostic> {
        let (sid, eid) = match (&subject, subject_ty) {
            (Expr::Variable(id, Type::Enum(_, _), _, _), Type::Enum(_, k)) => (*id, k),
            _ => return Err(self.fail("match needs a bare enum variable as its subject")),
        };
        if self.moved.contains(&sid) { return Err(self.fail("cannot match a value that was moved")); }
        let def = self.enums[eid].clone();
        self.take(Kind::LBrace)?;
        let mut arms: Vec<MatchArm> = Vec::new();
        let mut seen = vec![false; def.variants.len()];
        self.borrows.push((sid, self.scopes.len(), BorrowKind::Container));
        while *self.current() != Kind::RBrace {
            // A bare name cannot reach a variant: keep the specific remedy
            // instead of the shared parser's generic separator error.
            if matches!(self.current(), Kind::Ident(_))
                && self.tokens.get(self.pos + 1).is_some_and(|token| token.kind != Kind::ColonColon)
            {
                return Err(self.fail(&format!("pattern must name the matched enum ('{}::Variant')", def.pattern)));
            }
            let offset = self.tokens[self.pos].offset;
            let (tree, end) = if let Some(pattern) = self.resolved_expressions().match_patterns.get(&offset).cloned() {
                let end = self.tokens.partition_point(|token| token.offset < pattern.range.end);
                (pattern, end)
            } else {
                ae_syntax::pattern::parse_pattern_tokens(&self.file, self.source, &self.tokens, self.pos)?
            };
            self.pos = end;
            let ae_syntax::pattern::PatternKind::Variant {
                enum_name, variant: vname, variant_range, payload,
            } = tree.kind
            else {
                return Err(ae_syntax::diagnostic_range(
                    &self.file, self.source, tree.range, "expected a qualified variant pattern",
                ));
            };
            // An instantiated enum is matched by its declaration name:
            // `Option::Some(v)` covers Option[string] and Option[i64] alike.
            if enum_name != def.pattern {
                return Err(self.error_at(tree.range.start,
                    &format!("pattern must name the matched enum ('{}::Variant')", def.pattern)));
            }
            let vidx = def.variants.iter().position(|(n, _)| *n == vname)
                .ok_or_else(|| self.error_at(variant_range.start,
                    &format!("enum '{}' has no variant '{}'", def.name, vname)))?;
            if seen[vidx] {
                return Err(self.error_at(variant_range.start,
                    &format!("variant '{}::{}' is matched twice", def.name, vname)));
            }
            seen[vidx] = true;
            let fields = def.variants[vidx].1.clone();
            // The declaration decides the payload shape; `()` is a parse-level
            // empty list, so an empty or mismatched list lands here.
            let payloads = match payload {
                Some(_) if fields.is_empty() => {
                    return Err(self.error_at(variant_range.end,
                        &format!("variant '{}::{}' has no payload", def.name, vname)));
                }
                None if !fields.is_empty() => {
                    return Err(self.error_at(variant_range.start,
                        &format!("variant '{}::{}' needs {} binding(s)", def.name, vname, fields.len())));
                }
                Some(patterns) if patterns.len() > fields.len() => {
                    return Err(self.error_at(patterns[fields.len()].range.start,
                        &format!("variant '{}::{}' binds {} payload value(s)", def.name, vname, fields.len())));
                }
                Some(patterns) if patterns.len() < fields.len() => {
                    return Err(self.error_at(variant_range.start,
                        &format!("variant '{}::{}' needs {} binding(s)", def.name, vname, fields.len())));
                }
                Some(patterns) => patterns,
                None => Vec::new(),
            };
            self.scopes.push(Vec::new());
            let mut bindings = Vec::new();
            let mut bound = Vec::new();
            for (index, (field, payload_pattern)) in fields.iter().copied().zip(payloads).enumerate() {
                let bname = match payload_pattern.kind {
                    ae_syntax::pattern::PatternKind::Binding(name) => name,
                    ae_syntax::pattern::PatternKind::Variant { .. } => {
                        return Err(self.error_at(payload_pattern.range.start,
                            "pattern payloads must be plain names"));
                    }
                };
                let name_at = payload_pattern.range.start;
                if RESERVED.contains(&bname.as_str()) {
                    return Err(self.error_at(name_at,
                        &format!("'{bname}' is a built-in name and cannot be a pattern binding")));
                }
                if self.names.contains_key(&bname) {
                    return Err(self.duplicate_binding_error_at(name_at, &bname));
                }
                let id = self.next_var; self.next_var += 1;
                // String payloads bind as read-only views and vec payloads
                // as read-only borrows: nothing can be moved out of the
                // matched enum.
                let bty = match field {
                    Type::String => Type::StrView,
                    Type::Vec => Type::VecRef,
                    Type::StrVec => Type::StrVecRef,
                    Type::VecF => Type::VecFRef,
                    other => other,
                };
                self.names.insert(bname.clone(), (id, false, bty));
                self.scopes.last_mut().unwrap().push((id, bty));
                bound.push((bname, name_at));
                bindings.push((id, field, index));
            }
            self.take(Kind::FatArrow)?;
            let body = lower_body(self, arms.len(), &bound)?;
            self.scopes.pop().unwrap();
            for (name, _) in bound { self.names.remove(&name); }
            arms.push(MatchArm { variant: vidx, bindings, body });
        }
        self.take(Kind::RBrace)?;
        self.borrows.pop();
        if let Some(missing) = seen.iter().position(|covered| !covered) {
            return Err(self.fail(&format!("match is not exhaustive: '{}::{}' is not handled", def.name, def.variants[missing].0)));
        }
        Ok(Statement::Match(sid, subject_ty, arms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_output_matches_resolver_definition_to_live_slot() {
        let source = "struct Point { x: i64, y: i64 } struct Inner { x: i64 } struct Outer { inner: Inner, tag: i64 } struct Holder { ready: bool, text: string } struct Guard { id: i64 } fn scalar() -> bool { let local = (2 + 3) * 4; return local == 20; } fn mismatch() -> i64 { return true; } fn branches() -> i64 { if false { let same = 1; return same; } else if true { let same = 2; return same; } else { let same = 3; return same; } } fn loops() -> i64 { while true { while false { continue; } break; } return 7; } fn bad_loop() -> i64 { continue; return 0; } fn ranges() -> i64 { for i in 2..5 { while false { continue; } if i == 3 { continue; } } let i = 7; return i; } fn bad_range() -> i64 { for j in false..5 {} return 0; } fn countdown() -> i64 { var n = 4; n = 2; while n > 0 { n -= 1; } return n; } fn frozen() -> i64 { let n = 1; n = 2; return n; } fn output() -> i64 { print(2); assert(2 == 2); return 0; } fn bad_assert() -> i64 { assert(5); return 0; } fn entry_shape() -> i64 { print(23); } fn calls() -> i64 { return compute(compute(2)); } fn wrong_call() -> i64 { return compute(false); } fn cross_module() -> i64 { return imported_scalar(2); } fn private_module() -> i64 { return imported_private(3); } fn local_array() -> i64 { let a = [4, 5]; return a[1]; } fn bad_array() -> i64 { let a = [4, 5]; return a[false]; } fn local_array_store() -> i64 { var a = [4, 5]; a[1] = 9; return a[1]; } fn bad_store() -> i64 { var a = [4, 5]; a[false] = 1; return a[0]; } fn frozen_store() -> i64 { let a = [4, 5]; a[0] = 9; return a[0]; } fn bad_store_value() -> i64 { var a = [4, 5]; a[0] = false; return a[0]; } fn compound_store() -> i64 { var a = [4, 5]; a[0] += 1; return a[0]; } fn array_param(a: &[i64; 2]) -> i64 { return a[1]; } fn array_param_store(a: &[i64; 2]) -> i64 { a[0] = 3; return 0; } fn field_read(p: &Point) -> i64 { return p.x + p.y; } fn field_text(h: &Holder) -> i64 { return h.text; } fn nested_read(o: &Outer) -> i64 { return o.inner.x; } fn make_point() -> i64 { let p = Point { x: 1, y: 2 }; return p.x + p.y; } fn make_guard() -> i64 { let g = Guard { id: 1 }; return g.id; } fn consume_point(p: Point) -> i64 { return p.x; } fn pass_point() -> i64 { let q = Point { x: 3, y: 4 }; let r = consume_point(q); return r + 1; } fn pass_twice() -> i64 { let q = Point { x: 5, y: 6 }; let r = consume_point(q); let s = consume_point(q); return r + s; } fn loop_move(b: bool) -> i64 { let q = Point { x: 7, y: 8 }; while b { let r = consume_point(q); } return 0; } fn unsafe_wrap() -> i64 { unsafe { let a = 1; } return 2; } fn compute(x: i64) -> i64 { return x; } fn consume(s: string) -> i64 { return 1; } fn main() { let text = \"hello\"; let flag = true; var value = flag; if flag {} print(flag); print(compute(2)); assert(consume(text)); print(consume(text)); let v = vec[1.0, 2.0]; print(vec[1]); print(v[0]); let raw = &value as *const bool; let view = raw as *mut f64; let write = &mut value as *mut bool; print(compute(choose[i64, vec[f64]](2, vec[1.0]))); let pair = Pair[i64, vec[f64]] { first: 1, second: vec[1.0] }; let made = Pair[i64, vec[f64]]::new(); let variant = Choice[i64, vec[f64]]::Some(1); let empty = Choice[i64, vec[f64]]::empty(); let compound = raw as *const shared[i64]; task_spawn(ex, || -> i64 { return 0; }); let closure = |x: i64| -> bool { return x == 0; }; }";
        let source = format!("{source} fn generic_scalar[T](generic_value: T) -> T {{ return generic_value; }}");
        let source = format!("{source} fn closure_owner(captured_scalar: i64) -> i64 {{ let f = || -> i64 {{ return captured_scalar; }}; return f(); }} fn closure_text() -> i64 {{ let keep = \"world\"; let g = || -> i64 {{ print(keep); return 0; }}; return g(); }} fn closure_vector() -> i64 {{ let numbers = vec[10, 20]; let get = || -> i64 {{ print(numbers[1]); return 0; }}; return get(); }} fn closure_words() -> i64 {{ let labels = vec[\"aa\", \"bb\"]; let pick = || -> i64 {{ print(labels[0]); return 0; }}; return pick(); }}");
        let source = format!("{source} struct ScalarBox[T] {{ item: T }} impl ScalarBox[T] {{ fn scalar_identity(method_value: T) -> T {{ return method_value; }} fn get(&self) -> T {{ return self.item; }} fn set(&mut self, new_value: T) -> T {{ self.item = new_value; return self.item; }} fn consume_box(self) -> T {{ return self.item; }} }}");
        let source = format!("{source} impl ScalarBox[T] {{ fn round_trip(self, route: bool) -> Self {{ if route {{ return self; }} else {{ return (self); }} }} }}");
        let source = format!("{source} fn take_box(taken: ScalarBox[i64]) -> i64 {{ return taken.item; }} impl ScalarBox[T] {{ fn send(self) -> i64 {{ return take_box((self)); }} fn send_twice(self) -> i64 {{ let sent = take_box(self); return self.item; }} }}");
        let source = format!("{source} fn bad_point() -> i64 {{ let wrong = Point {{ x: false, y: 1 }}; return wrong.y; }}");
        let source = format!("{source} fn bad_fields() -> i64 {{ let extra = Point {{ z: 1 }}; return 0; }} fn dup_fields() -> i64 {{ let dup = Point {{ x: 1, x: false }}; return 0; }} fn missing_fields() -> i64 {{ let missing = Point {{ x: 1 }}; return 0; }}");
        let source = source.as_str();
        let source = format!("{source} enum CachedBox[T] {{ Item(T) }} fn pack(specialized_text: string) -> CachedBox[string] {{ return CachedBox[string]::Item(specialized_text); }}");
        let source = format!("{source} struct CachedRecord[T] {{ value: T, count: i64 }} fn record(record_text: string) -> CachedRecord[string] {{ return CachedRecord[string] {{ count: 2, value: record_text }}; }}");
        let source = format!("{source} fn relay(call_text: string) -> string {{ return generic_scalar[string](call_text); }}");
        let source = format!("{source} fn use_import() -> i64 {{ return imported_generic[i64](7); }}");
        let source = format!("{source} fn write_ptr(raw_param: *mut i64) -> i64 {{ unsafe {{ *raw_param = 7; }} return 0; }}");
        let source = format!("{source} fn read_ptr(read_param: *const i64) -> i64 {{ unsafe {{ return read_param.deref(); }} }}");
        let source = format!("{source} fn local_pointer() -> i64 {{ var pointee = 1; let local_ptr = &mut pointee as *mut i64; unsafe {{ *local_ptr = 7; return local_ptr.deref(); }} }}");
        let source = format!("{source} fn vector_sum(vector_arg: &vec[f64]) -> f64 {{ var total = 0.0; for item in vector_arg {{ total += item; }} return total; }}");
        let source = format!("{source} fn owned_loop() -> i64 {{ let empty_vec = vec[]; for unused in empty_vec {{ print(unused); }} return 0; }}");
        let source = format!("{source} fn moved_vec() -> i64 {{ spawn(|| -> i64 {{ return 0; }}); return 0; }}");
        let source = format!("{source} fn print_words(words: &vec[string]) -> i64 {{ for word in words {{ print(word); }} return 0; }}");
        let source = format!("{source} fn scan_words() -> i64 {{ let text_base = \"a b\"; let cursor = tokenize(text_base, \" \"); for token in cursor {{ print(token); }} return 0; }}");
        let source = format!("{source} fn local_words() -> i64 {{ let pieces = split(\"a,b\", \",\"); for piece in pieces {{ print(piece); }} return 0; }}");
        let source = format!("{source} enum Reading {{ Value(i64), Empty }} fn read_match(input: Reading) -> i64 {{ match input {{ Reading::Value(value) => {{ return value; }} Reading::Empty => {{ return 0; }} }} }}");
        let source = format!("{source} fn local_match() -> i64 {{ let reading = Reading::Value(42); match reading {{ Reading::Value(value) => {{ return value; }} Reading::Empty => {{ return 0; }} }} }}");
        let source = format!("{source} fn exits() -> i64 {{ exit(7); return 0; }} fn bad_exit() -> i64 {{ exit(false); return 0; }}");
        let source = format!("{source} enum Maybe {{ Some(i64), None }} fn step() -> Maybe {{ return Maybe::None; }} fn propagate() -> Maybe {{ step()?; return Maybe::Some(7); }} fn bind() -> Maybe {{ let value = step()?; return Maybe::Some(value); }}");
        let source = source.as_str();
        let tokens = Rc::new(ae_syntax::lex_diagnostic(source).unwrap());
        let print_token = tokens.partition_point(|token| token.offset < source.find("print(flag)").unwrap());
        let call_token = tokens.iter().position(|token| token.offset == source.find("print(compute").unwrap()).unwrap();
        let condition_token = tokens.partition_point(|token| token.offset < source.find("if flag").unwrap() + 3);
        let initializer = source.find("flag;").unwrap();
        let initializer_token = tokens.iter().position(|token| token.offset == initializer).unwrap();
        let binding = source.find("flag").unwrap();
        let mut functions = SharedFunctions::default();
        functions.insert("compute".into(), Signature {
            id: 9, params: vec![Type::Int], ret: Type::Int, file: "case.ae".into(),
            public: false, extern_c: false,
        });
        functions.insert("consume_point".into(), Signature {
            id: 12, params: vec![Type::Struct(0)], ret: Type::Int, file: "case.ae".into(),
            public: false, extern_c: false,
        });
        functions.insert("imported_scalar".into(), Signature {
            id: 10, params: vec![Type::Int], ret: Type::Int, file: "imported.ae".into(),
            public: true, extern_c: false,
        });
        functions.insert("imported_private".into(), Signature {
            id: 11, params: vec![Type::Int], ret: Type::Int, file: "imported.ae".into(),
            public: false, extern_c: false,
        });
        let mut structs = vec![
            StructDef { name: "Point".into(), fields: vec![("x".into(), Type::Int), ("y".into(), Type::Int)], file: "case.ae".into(), public: false, repr_c: false, drop_fn: None },
            StructDef { name: "Inner".into(), fields: vec![("x".into(), Type::Int)], file: "case.ae".into(), public: false, repr_c: false, drop_fn: None },
            StructDef { name: "Outer".into(), fields: vec![("inner".into(), Type::Struct(1)), ("tag".into(), Type::Int)], file: "case.ae".into(), public: false, repr_c: false, drop_fn: None },
            StructDef { name: "Holder".into(), fields: vec![("ready".into(), Type::Bool), ("text".into(), Type::String)], file: "case.ae".into(), public: false, repr_c: false, drop_fn: None },
            StructDef { name: "Guard".into(), fields: vec![("id".into(), Type::Int)], file: "case.ae".into(), public: false, repr_c: false, drop_fn: Some(0) },
        ];
        let mut struct_names = HashMap::new();
        struct_names.insert("Point".into(), 0);
        struct_names.insert("Holder".into(), 3);
        struct_names.insert("Inner".into(), 1);
        struct_names.insert("Outer".into(), 2);
        struct_names.insert("Guard".into(), 4);
        let mut enums = vec![EnumDef {
            name: "Reading", variants: vec![("Value".into(), vec![Type::Int]), ("Empty".into(), vec![])],
            file: "case.ae".into(), public: false, pattern: "Reading".into(),
        }];
        let mut enum_names = HashMap::new();
        enum_names.insert("Reading".into(), 0);
        enums.push(EnumDef {
            name: "Maybe", variants: vec![("Some".into(), vec![Type::Int]), ("None".into(), vec![])],
            file: "case.ae".into(), public: false, pattern: "Maybe".into(),
        });
        enum_names.insert("Maybe".into(), 1);
        functions.insert("step".into(), Signature {
            id: 19, params: vec![], ret: Type::Enum("Maybe", 1), file: "case.ae".into(),
            public: false, extern_c: false,
        });
        let mut generics = Generics::default();
        let mut ctx = CompileContext::new();
        let mut parser = make_parser(
            "case.ae", source, tokens, &mut functions, &mut structs,
            &mut struct_names, &mut enums, &mut enum_names, &mut generics, &mut ctx,
        );
        let next_var = parser.next_var;
        let input = source.find("input: Reading").unwrap();
        parser.names.insert("input".into(), (89, false, Type::Enum("Reading", 0)));
        parser.binding_ranges.insert(89, input..input + "input".len());
        parser.seek_scalar_offset(source.find("{ match input").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved enum match").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Match(89, Type::Enum(_, 0), arms)]
            if matches!(arms[0].body.as_slice(), [Statement::Return(Expr::Variable(slot, Type::Int, ..), _)]
                if *slot == arms[0].bindings[0].0)));
        parser.names.remove("input");
        parser.binding_ranges.remove(&89);
        parser.seek_scalar_offset(source.find("{ exit(7)").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved exit").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Exit(Expr::Number(7), _), Statement::Return(..)]));
        parser.seek_scalar_offset(source.find("{ exit(false)").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved exit diagnostic").err().unwrap();
        assert_eq!(error.message, "exit expects an i64 status code");
        assert_eq!(*parser.current(), Kind::RParen);
        parser.return_type = Type::Enum("Maybe", 1);
        parser.seek_scalar_offset(source.find("{ step()?").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved bare propagation").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Propagate(Expr::Call(19, _, Type::Enum(_, 1)), Type::Enum(_, 1), _, None, _),
            Statement::Return(Expr::EnumLit(_, 1, 0, _), _)]));
        parser.seek_scalar_offset(source.find("{ let value = step").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved binding propagation").unwrap();
        let [Statement::Propagate(prop_value, prop_ty, prop_fields, prop_bound, _), Statement::Return(ret_expr, _)] = body.as_slice() else {
            panic!("unexpected binding-propagation body: {body:?}");
        };
        assert!(matches!(prop_value, Expr::Call(19, args, Type::Enum(_, 1)) if args.is_empty()), "{prop_value:?}");
        assert_eq!(prop_ty, &Type::Enum("Maybe", 1));
        assert_eq!(prop_fields, &[Type::Int]);
        let Some((payload_slot, payload_ty)) = prop_bound else {
            panic!("payload binding missing: {prop_bound:?}");
        };
        assert_eq!(payload_ty, &Type::Int);
        assert!(matches!(ret_expr, Expr::EnumLit(_, _, 0, vals)
            if matches!(&**vals, [Expr::Variable(vs, Type::Int, ..)] if vs == payload_slot)), "{ret_expr:?}");
        parser.return_type = Type::Int;
        parser.seek_scalar_offset(source.find("{ let reading =").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved local enum match").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Declare(slot, Expr::EnumLit(_, 0, 0, _)),
            Statement::Match(subject, Type::Enum(_, 0), arms), ..] if slot == subject && arms.len() == 2));
        parser.next_var = next_var;
        let start = source.find("raw_param:").unwrap();
        parser.names.insert("raw_param".into(), (85, false, Type::PtrMut(parser.ctx.intern_pointee(Type::Int))));
        parser.binding_ranges.insert(85, start..start + "raw_param".len());
        parser.seek_scalar_offset(source.find("{ unsafe { *raw_param").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved pointer store").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Unsafe(inner), Statement::Return(..)]
            if matches!(inner.as_slice(), [Statement::PtrStore(85, Expr::Number(7), ..)])));
        parser.names.remove("raw_param");
        parser.binding_ranges.remove(&85);
        let start = source.find("read_param:").unwrap();
        parser.names.insert("read_param".into(), (86, false, Type::Ptr(parser.ctx.intern_pointee(Type::Int))));
        parser.binding_ranges.insert(86, start..start + "read_param".len());
        parser.seek_scalar_offset(source.find("{ unsafe { return read_param").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved pointer read").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Unsafe(inner)]
            if matches!(inner.as_slice(), [Statement::Return(Expr::DerefPtr(..), _)])));
        parser.names.remove("read_param");
        parser.binding_ranges.remove(&86);
        parser.seek_scalar_offset(source.find("{ var pointee = 1").unwrap());
        let next_var = parser.next_var;
        let body = parser.lower_resolved_scalar_body().expect("resolved local pointer body").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Declare(_, Expr::Number(1)), Statement::Declare(pointer, Expr::BorrowAs(_, true, ..)), Statement::Unsafe(inner)]
            if matches!(inner.as_slice(), [Statement::PtrStore(slot, Expr::Number(7), ..), Statement::Return(Expr::DerefPtr(..), _)] if slot == pointer)));
        parser.next_var = next_var;
        let start = source.find("vector_arg:").unwrap();
        parser.names.insert("vector_arg".into(), (87, false, Type::VecFRef));
        parser.binding_ranges.insert(87, start..start + "vector_arg".len());
        parser.return_type = Type::F64;
        parser.seek_scalar_offset(source.find("{ var total = 0.0").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved vector loop").unwrap();
        assert!(matches!(body.as_slice(), [_, _, _, Statement::For(_, _, inner, ..), Statement::Return(..)]
            if matches!(inner.first(), Some(Statement::Declare(_, Expr::VecIndex(87, _, _, _, true, Type::F64))))));
        parser.return_type = Type::Int;
        parser.names.remove("vector_arg");
        parser.binding_ranges.remove(&87);
        parser.seek_scalar_offset(source.find("{ let empty_vec =").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved owning vector loop").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Declare(vector, Expr::VecLit(values, Type::Vec, ..)), _, _, Statement::For(_, _, inner, ..), Statement::Return(_, owned), Statement::Frees(_)]
            if values.is_empty() && owned.contains(&(*vector, Type::Vec))
                && matches!(inner.first(), Some(Statement::Declare(_, Expr::VecIndex(source, _, _, _, false, Type::Int))) if source == vector)));
        parser.seek_scalar_offset(source.find("{ spawn(").unwrap());
        // A spawn statement still keeps the body on the typed route; the
        // A bare `spawn(..)` statement is evaluated (the spawn machinery) and
        // then rejected with the typed route's `?` rule; the path leaves no
        // move state behind.
        let error = parser.lower_resolved_scalar_body().expect("resolved spawn statement").err().unwrap();
        assert!(error.message.contains("must end with '?'"), "{}", error.message);
        let start = source.find("words: &vec[string]").unwrap();
        parser.names.insert("words".into(), (88, false, Type::StrVecRef));
        parser.binding_ranges.insert(88, start..start + "words".len());
        parser.seek_scalar_offset(source.find("{ for word in words").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved string vector loop").unwrap();
        assert!(matches!(body.as_slice(), [_, _, Statement::For(_, _, inner, ..), Statement::Return(..)]
            if matches!(inner.as_slice(), [Statement::Declare(slot, Expr::StrVecIndex(88, _, _, _, true)),
                Statement::Print(Printed::Expr(Expr::Variable(read, Type::StrView, ..)))] if slot == read)));
        parser.names.remove("words");
        parser.binding_ranges.remove(&88);
        parser.seek_scalar_offset(source.find("{ let text_base").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved token loop").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Declare(base, Expr::StrLit(..)),
            Statement::Declare(cursor, Expr::Tokens(source, None, ..)), Statement::ForTokens(read, ..), ..]
            if base == source && cursor == read));
        assert!(parser.borrows.is_empty());
        parser.seek_scalar_offset(source.find("{ let pieces = split").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved local string vector loop").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Declare(vector, Expr::Split(..)), _, _, Statement::For(_, _, inner, ..), Statement::Return(_, owned), Statement::Frees(_)]
            if owned.contains(&(*vector, Type::StrVec))
                && matches!(inner.first(), Some(Statement::Declare(_, Expr::StrVecIndex(source, _, _, _, false))) if source == vector)));
        parser.next_var = next_var;
        parser.next_label = 0;
        parser.generics.names.insert("CachedBox".into(), 0);
        parser.generics.defs.push(GenericEnum {
            name: "CachedBox".into(), params: vec!["T".into()],
            variants: vec![("Item".into(), vec![Payload::Param(0)])],
            file: "case.ae".into(), public: false,
        });
        let payload = source.find("specialized_text:").unwrap();
        parser.names.insert("specialized_text".into(), (82, false, Type::String));
        parser.binding_ranges.insert(82, payload..payload + "specialized_text".len());
        let variant = source.find("CachedBox[string]::Item").unwrap();
        parser.seek_scalar_offset(variant);
        let start = parser.pos;
        assert!(parser.lower_shared_value(Some(Kind::Semi), true).is_none());
        assert_eq!(parser.pos, start);
        assert!(!parser.moved.contains(&82));
        let eid = parser.instantiate_enum(0, vec![Type::String], variant).unwrap();
        parser.seek_scalar_offset(variant + "CachedBox[string]::Item".len());
        let open = parser.pos;
        assert!(parser.lower_committed_enum_payload(&[Type::Int]).is_none());
        assert_eq!(parser.pos, open);
        assert!(!parser.moved.contains(&82));
        assert!(matches!(
            parser.lower_committed_enum_payload(&[Type::String]).as_deref(),
            Some([Expr::Variable(82, Type::String, ..)])
        ));
        assert_eq!(*parser.current(), Kind::Semi);
        assert!(parser.moved.remove(&82));
        parser.seek_scalar_offset(variant);
        assert!(matches!(
            parser.lower_shared_value(Some(Kind::Semi), true),
            Some((Expr::EnumLit(_, actual, 0, payload), Type::Enum(_, ty)))
                if actual == eid && ty == eid && matches!(payload.as_slice(), [Expr::Variable(82, Type::String, ..)])
        ));
        assert!(parser.moved.remove(&82));
        parser.names.remove("specialized_text");
        parser.binding_ranges.remove(&82);
        parser.generics.struct_names.insert("CachedRecord".into(), 0);
        parser.generics.struct_defs.push(GenericStruct {
            name: "CachedRecord".into(), params: vec!["T".into()],
            fields: vec![("value".into(), Payload::Param(0)), ("count".into(), Payload::Concrete(Type::Int))],
            file: "case.ae".into(), public: false,
        });
        let payload = source.find("record_text:").unwrap();
        parser.names.insert("record_text".into(), (83, false, Type::String));
        parser.binding_ranges.insert(83, payload..payload + "record_text".len());
        let constructor = source.find("CachedRecord[string] { count").unwrap();
        parser.seek_scalar_offset(constructor);
        let start = parser.pos;
        assert!(parser.lower_shared_value(Some(Kind::Semi), true).is_none());
        assert_eq!(parser.pos, start);
        assert!(!parser.moved.contains(&83));
        let sid = parser.instantiate_struct(0, vec![Type::String], constructor).unwrap();
        let mismatched = parser.instantiate_struct(0, vec![Type::Int], constructor).unwrap();
        parser.seek_scalar_offset(constructor + "CachedRecord[string] ".len());
        let brace = parser.pos;
        assert!(parser.lower_committed_specialized_struct(mismatched).is_none());
        assert_eq!(parser.pos, brace);
        assert!(!parser.moved.contains(&83));
        assert!(matches!(
            parser.lower_committed_specialized_struct(sid),
            Some((Expr::StructLit(actual, fields), Type::Struct(ty)))
                if actual == sid && ty == sid
                    && matches!(fields.as_slice(), [Expr::Variable(83, Type::String, ..), Expr::Number(2)])
        ));
        assert_eq!(*parser.current(), Kind::Semi);
        assert!(parser.moved.remove(&83));
        parser.seek_scalar_offset(constructor);
        assert!(matches!(
            parser.lower_shared_value(Some(Kind::Semi), true),
            Some((Expr::StructLit(actual, fields), Type::Struct(ty)))
                if actual == sid && ty == sid
                    && matches!(fields.as_slice(), [Expr::Variable(83, Type::String, ..), Expr::Number(2)])
        ));
        assert!(parser.moved.remove(&83));
        parser.names.remove("record_text");
        parser.binding_ranges.remove(&83);
        let definition = source.find("(generic_value: T)").unwrap();
        parser.generics.fn_names.insert("generic_scalar".into(), 0);
        parser.generics.fn_defs.push(GenericFn {
            name: "generic_scalar".into(), bindings: None, params: vec!["T".into()],
            bounds: vec![vec![]], trait_bounds: vec![vec![]],
            start: parser.tokens.partition_point(|token| token.offset < definition),
            tokens: Rc::clone(&parser.tokens), source: Rc::clone(&parser.shared_source),
            file: "case.ae".into(), public: false,
        });
        let payload = source.find("call_text:").unwrap();
        parser.names.insert("call_text".into(), (84, false, Type::String));
        parser.binding_ranges.insert(84, payload..payload + "call_text".len());
        let call = source.find("generic_scalar[string](call_text)").unwrap();
        parser.seek_scalar_offset(call);
        let start = parser.pos;
        assert!(parser.lower_shared_value(Some(Kind::Semi), true).is_none());
        assert_eq!(parser.pos, start);
        assert!(!parser.moved.contains(&84));
        let (id, params, _) = parser.instantiate_fn(0, vec![Type::String], call).unwrap();
        parser.seek_scalar_offset(call + "generic_scalar[string]".len());
        let open = parser.pos;
        assert!(parser.lower_committed_specialized_arguments(&[Type::Int]).is_none());
        assert_eq!(parser.pos, open);
        assert!(!parser.moved.contains(&84));
        assert!(matches!(
            parser.lower_committed_specialized_arguments(&params).as_deref(),
            Some([Argument::Value(Expr::Variable(84, Type::String, ..))])
        ));
        assert_eq!(*parser.current(), Kind::Semi);
        assert!(parser.moved.remove(&84));
        parser.seek_scalar_offset(call);
        assert!(matches!(
            parser.lower_shared_value(Some(Kind::Semi), true),
            Some((Expr::Call(actual, arguments, Type::String), Type::String))
                if actual == id && matches!(arguments.as_slice(), [Argument::Value(Expr::Variable(84, Type::String, ..))])
        ));
        assert!(parser.moved.remove(&84));
        parser.names.remove("call_text");
        parser.binding_ranges.remove(&84);
        let mut imported = parser.generics.fn_defs[0].clone();
        imported.name = "imported_generic".into();
        imported.file = "imported.ae".into();
        imported.public = true;
        parser.generics.fn_names.insert(imported.name.clone(), 1);
        parser.generics.fn_defs.push(imported);
        let offset = source.find("imported_generic[i64](7)").unwrap();
        let (id, _, _) = parser.instantiate_fn(1, vec![Type::Int], offset).unwrap();
        parser.seek_scalar_offset(offset);
        assert!(matches!(
            parser.lower_shared_value(Some(Kind::Semi), false),
            Some((Expr::Call(actual, _, Type::Int), Type::Int)) if actual == id
        ));
        parser.generics.fn_defs[1].public = false;
        parser.seek_scalar_offset(offset);
        let start = parser.pos;
        assert!(parser.lower_shared_value(Some(Kind::Semi), false).is_none());
        assert_eq!(parser.pos, start);
        let capture_start = source.find("captured_scalar:").unwrap();
        parser.names.insert("captured_scalar".into(), (80, true, Type::Int));
        parser.seek_scalar_offset(source.find("{ return captured_scalar;").unwrap());
        let closure = parser.lower_resolved_body_with_captures(&[(80, capture_start)])
            .expect("resolved emitted closure body").unwrap();
        assert!(matches!(closure.as_slice(), [Statement::Return(Expr::Variable(80, Type::Int, ..), _)]));
        parser.names.remove("captured_scalar");
        // A captured owned string flows through the capture slot: the
        // resolver records the capture and the route re-admits its read.
        let keep_start = source.find("let keep =").unwrap() + 4;
        parser.names.insert("keep".into(), (82, false, Type::StrView));
        parser.seek_scalar_offset(source.find("|| -> i64 { print(keep)").unwrap() + 10);
        let closure = parser.lower_resolved_body_with_captures(&[(82, keep_start)])
            .expect("resolved string-capture closure body").unwrap();
        assert!(matches!(closure.as_slice(), [Statement::Print(Printed::Expr(Expr::Variable(82, Type::StrView, ..))),
            Statement::Return(Expr::Number(0), _)]));
        parser.names.insert("keep".into(), (82, false, Type::String));
        parser.seek_scalar_offset(source.find("{ print(keep)").unwrap());
        assert!(parser.lower_resolved_body_with_captures(&[(82, keep_start)]).is_none());
        parser.names.remove("keep");
        let numbers_start = source.find("let numbers = vec[10, 20]").unwrap() + 4;
        let vector_body = source.find("{ print(numbers[1])").unwrap();
        for (capture_type, element) in [(Type::VecRef, Type::Int), (Type::VecFRef, Type::F64)] {
            parser.names.insert("numbers".into(), (82, false, capture_type));
            parser.seek_scalar_offset(vector_body);
            let closure = parser.lower_resolved_body_with_captures(&[(82, numbers_start)])
                .expect("resolved numeric vector-capture closure body").unwrap();
            assert!(matches!(closure.as_slice(),
                [Statement::Print(Printed::Expr(Expr::VecIndex(82, _, _, _, true, actual))),
                 Statement::Return(Expr::Number(0), _)] if *actual == element));
        }
        // An owning value capture moves the vector into the closure's slot;
        // the element read is a borrowed index into that slot, and the
        // body's own Frees releases the slot after the call (FnOnce).
        parser.names.insert("numbers".into(), (82, false, Type::Vec));
        parser.seek_scalar_offset(vector_body);
        let closure = parser.lower_resolved_body_with_captures(&[(82, numbers_start)])
            .expect("resolved owning numeric vector-capture closure body").unwrap();
        assert!(matches!(closure.as_slice(),
            [Statement::Print(Printed::Expr(Expr::VecIndex(82, _, _, _, false, Type::Int))),
             Statement::Return(Expr::Number(0), _)]), "{closure:?}");
        parser.names.remove("numbers");
        // A borrowed string-vector capture: its element read is a view.
        let labels_start = source.find("let labels = vec[").unwrap() + 4;
        parser.names.insert("labels".into(), (83, false, Type::StrVecRef));
        parser.seek_scalar_offset(source.find("{ print(labels[0])").unwrap());
        let closure = parser.lower_resolved_body_with_captures(&[(83, labels_start)])
            .expect("resolved borrowed string-vector capture").unwrap();
        assert!(matches!(closure.as_slice(),
            [Statement::Print(Printed::Expr(Expr::StrVecIndex(83, _, _, _, true))),
             Statement::Return(Expr::Number(0), _)]), "{closure:?}");
        parser.names.remove("labels");
        // An owning string-vector capture: the element read comes from the
        // closure's own slot (by_ref=false). The caller (closure_body)
        // appends the scope's Frees, which releases the vector and its
        // elements after the call.
        parser.names.insert("labels".into(), (84, false, Type::StrVec));
        parser.seek_scalar_offset(source.find("{ print(labels[0])").unwrap());
        let closure = parser.lower_resolved_body_with_captures(&[(84, labels_start)])
            .expect("resolved owning string-vector capture").unwrap();
        assert!(matches!(closure.as_slice(),
            [Statement::Print(Printed::Expr(Expr::StrVecIndex(84, _, _, _, false))),
             Statement::Return(Expr::Number(0), _)]), "{closure:?}");
        parser.names.remove("labels");
        let generic_binding = source.find("generic_value:").unwrap();
        let body = source.find("{ return generic_value;").unwrap();
        for ty in [Type::Int, Type::Bool] {
            parser.subst.insert("T".into(), ty);
            parser.return_type = ty;
            parser.names.insert("generic_value".into(), (80, false, ty));
            parser.binding_ranges.insert(80, generic_binding..generic_binding + "generic_value".len());
            parser.seek_scalar_offset(body);
            let lowered = parser.lower_resolved_scalar_body().expect("resolved generic instance").unwrap();
            assert!(matches!(lowered.as_slice(), [Statement::Return(Expr::Variable(80, actual, ..), _)] if *actual == ty));
        }
        parser.names.remove("generic_value");
        let method_binding = source.find("method_value:").unwrap();
        parser.return_type = Type::Int;
        parser.subst.insert("T".into(), Type::Int);
        parser.impl_self = Some(Type::Struct(0));
        parser.names.insert("method_value".into(), (80, false, Type::Int));
        parser.binding_ranges.insert(80, method_binding..method_binding + "method_value".len());
        parser.seek_scalar_offset(source.find("{ return method_value;").unwrap());
        let lowered = parser.lower_resolved_scalar_body().expect("resolved generic associated body").unwrap();
        assert!(matches!(lowered.as_slice(), [Statement::Return(Expr::Variable(80, Type::Int, ..), _)]));
        parser.names.remove("method_value");
        let receiver_binding = source.find("get(&self)").unwrap() + "get(&".len();
        let instance = parser.structs.len();
        parser.structs.push(StructDef {
            name: "ScalarBox[i64]".into(), fields: vec![("item".into(), Type::Int)],
            file: "case.ae".into(), public: false, repr_c: false, drop_fn: None,
        });
        let receiver = Type::StructRef(instance);
        parser.receiver = Some(receiver);
        parser.names.insert("self".into(), (80, false, receiver));
        parser.binding_ranges.insert(80, receiver_binding..receiver_binding + "self".len());
        let receiver_body = source.find("{ return self.item;").unwrap();
        parser.seek_scalar_offset(receiver_body);
        let lowered = parser.lower_resolved_scalar_body().expect("resolved generic receiver body").unwrap();
        assert!(matches!(lowered.as_slice(), [Statement::Return(Expr::Field(base, name, Type::Int), _)]
            if name == "item" && matches!(**base, Expr::Variable(80, actual, ..) if actual == receiver)));
        let setter_binding = source.find("set(&mut self").unwrap() + "set(&mut ".len();
        let value_binding = source.find("new_value:").unwrap();
        let setter_body = source.find("{ self.item = new_value;").unwrap();
        let mutable_receiver = Type::StructMut(instance);
        parser.receiver = Some(mutable_receiver);
        parser.names.insert("self".into(), (80, true, mutable_receiver));
        parser.names.insert("new_value".into(), (81, false, Type::Int));
        parser.binding_ranges.insert(80, setter_binding..setter_binding + "self".len());
        parser.binding_ranges.insert(81, value_binding..value_binding + "new_value".len());
        parser.seek_scalar_offset(setter_body);
        let lowered = parser.lower_resolved_scalar_body().expect("resolved generic receiver store").unwrap();
        assert!(matches!(lowered.as_slice(), [Statement::FieldStore(80, name, Type::Int, Expr::Variable(81, Type::Int, ..), actual), Statement::Return(Expr::Field(..), _)]
            if name == "item" && *actual == mutable_receiver));
        parser.names.insert("new_value".into(), (81, false, Type::Bool));
        parser.seek_scalar_offset(setter_body);
        let error = parser.lower_resolved_scalar_body().expect("resolved setter error").err().unwrap();
        assert_eq!(error.message, "field 'item' must be i64");
        assert_eq!(*parser.current(), Kind::Return);
        parser.names.remove("new_value");
        parser.binding_ranges.remove(&81);
        let owned_binding = source.find("consume_box(self)").unwrap() + "consume_box(".len();
        let owned_body = source.find("consume_box(self) -> T {").unwrap() + "consume_box(self) -> T ".len();
        let owned_receiver = Type::Struct(instance);
        parser.receiver = Some(owned_receiver);
        parser.names.insert("self".into(), (80, false, owned_receiver));
        parser.binding_ranges.insert(80, owned_binding..owned_binding + "self".len());
        parser.scopes.push(vec![(80, owned_receiver)]);
        parser.seek_scalar_offset(owned_body);
        let lowered = parser.lower_resolved_scalar_body().expect("resolved owned receiver body").unwrap();
        assert!(matches!(lowered.as_slice(), [Statement::Return(Expr::Field(..), owned)]
            if owned == &[(80, owned_receiver)]));
        parser.scopes.pop();
        let transfer_binding = source.find("round_trip(self,").unwrap() + "round_trip(".len();
        let route_binding = source.find("route:").unwrap();
        parser.return_type = owned_receiver;
        parser.binding_ranges.insert(80, transfer_binding..transfer_binding + "self".len());
        parser.names.insert("route".into(), (81, false, Type::Bool));
        parser.binding_ranges.insert(81, route_binding..route_binding + "route".len());
        parser.scopes.push(vec![(80, owned_receiver)]);
        parser.seek_scalar_offset(source.find("{ if route").unwrap());
        let lowered = parser.lower_resolved_scalar_body().expect("resolved owning transfer").unwrap();
        let [Statement::If(_, yes, no)] = lowered.as_slice() else { panic!("expected both return branches") };
        for branch in [yes, no] {
            assert!(matches!(branch.as_slice(), [Statement::Return(Expr::Variable(80, ty, ..), owned)]
                if *ty == owned_receiver && owned == &[(80, owned_receiver)]));
        }
        assert!(parser.moved.remove(&80));
        parser.scopes.pop();
        parser.names.remove("route");
        parser.binding_ranges.remove(&81);
        parser.return_type = Type::Int;
        parser.functions.insert("take_box".into(), Signature {
            id: 13, params: vec![owned_receiver], ret: Type::Int, file: "case.ae".into(),
            public: false, extern_c: false,
        });
        let send_binding = source.find("send(self)").unwrap() + "send(".len();
        parser.binding_ranges.insert(80, send_binding..send_binding + "self".len());
        parser.seek_scalar_offset(source.find("{ return take_box((self));").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved receiver argument").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Return(Expr::Call(13, args, Type::Int), _)]
            if matches!(args.as_slice(), [Argument::Value(Expr::Variable(80, ty, ..))] if *ty == owned_receiver)));
        assert!(parser.moved.remove(&80));
        let twice_binding = source.find("send_twice(self)").unwrap() + "send_twice(".len();
        let next_slot = parser.next_var;
        parser.binding_ranges.insert(80, twice_binding..twice_binding + "self".len());
        parser.seek_scalar_offset(source.find("{ let sent = take_box(self)").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved moved receiver").err().unwrap();
        assert_eq!(error.code.as_deref(), Some("E0102"));
        parser.moved.remove(&80);
        parser.names.remove("sent");
        parser.binding_ranges.remove(&next_slot);
        parser.next_var = next_slot;
        parser.receiver = Some(receiver);
        parser.names.insert("self".into(), (80, false, receiver));
        parser.binding_ranges.insert(80, receiver_binding..receiver_binding + "self".len());
        parser.structs[instance].fields[0].1 = Type::String;
        parser.seek_scalar_offset(receiver_body);
        assert!(parser.lower_resolved_scalar_body().is_none(), "non-scalar receiver field stays typed");
        parser.structs.pop();
        parser.receiver = None;
        parser.names.remove("self");
        parser.subst.clear();
        parser.impl_self = None;
        parser.binding_ranges.remove(&80);
        parser.return_type = Type::Bool;
        let comparison = "local == 20";
        let start = source.find(comparison).unwrap();
        assert_eq!(
            parser.resolved_expressions().scalar_expression_types.get(&(start..start + comparison.len())),
            Some(&ae_resolve::resolve::ExpressionType::Checked(ae_resolve::typecheck::Scalar::Bool)),
        );
        parser.seek_scalar_offset(source.find("{ let local").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved scalar route").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Declare(0, _), Statement::Return(Expr::Binary(_, Kind::EqEq, _, _, _, Type::Bool), _)]));
        parser.return_type = Type::Int;
        parser.seek_scalar_offset(source.find("{ return true").unwrap());
        let mismatch = parser.lower_resolved_scalar_body().expect("resolved error route").err().unwrap();
        assert_eq!(mismatch.message, "return type must be i64");
        assert_eq!(mismatch.code.as_deref(), Some("E0204"));

        parser.seek_scalar_offset(source.find("{ if false").unwrap());
        let branches = parser.lower_resolved_scalar_body().expect("resolved branches").unwrap();
        let [Statement::If(_, yes, otherwise)] = branches.as_slice() else { panic!("expected if") };
        let [Statement::If(_, other_yes, other_no)] = otherwise.as_slice() else { panic!("expected else if") };
        for (index, body) in [yes, other_yes, other_no].into_iter().enumerate() {
            assert!(matches!(body.as_slice(), [Statement::Declare(slot, _), Statement::Return(Expr::Variable(read, Type::Int, ..), _)] if *slot == index + 1 && slot == read));
        }

        parser.seek_scalar_offset(source.find("{ while true").unwrap());
        let loops = parser.lower_resolved_scalar_body().expect("resolved loops").unwrap();
        let [Statement::While(_, outer), Statement::Return(Expr::Number(7), _)] = loops.as_slice() else {
            panic!("expected loop and return");
        };
        let [Statement::While(_, inner), Statement::Break] = outer.as_slice() else {
            panic!("expected nested loop and break");
        };
        assert!(matches!(inner.as_slice(), [Statement::Continue(None)]));
        parser.seek_scalar_offset(source.find("{ continue; return").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved loop error").err().unwrap();
        assert_eq!(error.message, "'continue' outside of a loop");
        assert_eq!(parser.loop_depth, 0);

        parser.seek_scalar_offset(source.find("{ for i in 2").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved range").unwrap();
        let [Statement::Declare(start, Expr::Number(2)), Statement::Declare(end, Expr::Number(5)), Statement::For(counter, limit, loop_body, label, ..), Statement::Declare(reused, Expr::Number(7)), Statement::Return(Expr::Variable(read, Type::Int, ..), _)] = body.as_slice() else {
            panic!("expected range bounds and loop");
        };
        assert_eq!((start, end), (counter, limit));
        assert_eq!(*start, *end + 1);
        assert_ne!(counter, reused);
        assert_eq!(reused, read);
        let [Statement::While(_, inner), Statement::If(_, branch, _)] = loop_body.as_slice() else {
            panic!("expected nested while and conditional");
        };
        assert!(matches!(inner.as_slice(), [Statement::Continue(None)]));
        assert!(matches!(branch.as_slice(), [Statement::Continue(Some(jump))] if jump == label));

        parser.seek_scalar_offset(source.find("{ for j in false").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved range error").err().unwrap();
        assert_eq!(error.message, "range bounds must be i64");

        parser.seek_scalar_offset(source.find("{ var n = 4").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved assignments").unwrap();
        let [Statement::Declare(slot, _), Statement::Assign(plain, Expr::Number(2), false), Statement::While(_, loop_body), Statement::Return(Expr::Variable(read, Type::Int, ..), _)] = body.as_slice() else {
            panic!("expected scalar assignments");
        };
        assert_eq!(slot, plain);
        assert_eq!(slot, read);
        assert!(matches!(loop_body.as_slice(), [Statement::Assign(compound, Expr::Binary(_, Kind::Minus, _, _, _, Type::Int), false)] if compound == slot));

        parser.seek_scalar_offset(source.find("{ let n = 1").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved immutable assignment").err().unwrap();
        assert_eq!(error.message, "cannot assign to immutable variable 'n'");
        assert_eq!(error.secondary.len(), 1);
        parser.names.remove("n");

        parser.seek_scalar_offset(source.find("{ print(2);").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved output").unwrap();
        let [Statement::Print(Printed::Expr(Expr::Number(2))), Statement::Assert(Expr::Binary(_, Kind::EqEq, _, _, _, Type::Bool), 1, column), Statement::Return(Expr::Number(0), _)] = body.as_slice() else {
            panic!("expected print, assert, and return");
        };
        assert_eq!(*column, source.find("assert(2 == 2)").unwrap() + 1);
        parser.seek_scalar_offset(source.find("{ assert(5)").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved assert error").err().unwrap();
        assert_eq!(error.message, "assert requires a boolean condition");
        assert_eq!(*parser.current(), Kind::RParen);

        parser.in_main = true;
        parser.seek_scalar_offset(source.find("{ print(23)").unwrap());
        let entry = parser.lower_resolved_scalar_body().expect("resolved entry body").unwrap();
        assert!(matches!(entry.as_slice(), [Statement::Print(Printed::Expr(Expr::Number(23)))]));
        parser.seek_scalar_offset(source.find("{ return true").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved entry return error").err().unwrap();
        assert_eq!(error.message, "return is not supported in main");
        assert_eq!(*parser.current(), Kind::Return);
        parser.in_main = false;

        parser.seek_scalar_offset(source.find("{ return compute(compute").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved direct calls").unwrap();
        let [Statement::Return(Expr::Call(9, args, Type::Int), _)] = body.as_slice() else {
            panic!("expected direct call");
        };
        assert!(matches!(args.as_slice(), [Argument::Value(Expr::Call(9, _, Type::Int))]));
        parser.seek_scalar_offset(source.find("{ return compute(false)").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved call error").err().unwrap();
        assert_eq!(error.message, "argument 1 to 'compute' must be i64");
        assert_eq!(*parser.current(), Kind::RParen);

        parser.seek_scalar_offset(source.find("{ return imported_scalar").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved cross-module call").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Return(Expr::Call(10, _, Type::Int), _)]));
        parser.seek_scalar_offset(source.find("{ return imported_private").unwrap());
        assert!(parser.lower_resolved_scalar_body().is_none());

        parser.seek_scalar_offset(source.find("{ let a = [4, 5]; return a[1]").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved local array").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Array(slot, values), Statement::Return(Expr::Index(read, 2, index, ..), _)]
            if slot == read && values.len() == 2 && matches!(**index, Expr::Number(1))));
        parser.seek_scalar_offset(source.find("{ let a = [4, 5]; return a[false]").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved array error").err().unwrap();
        assert_eq!(error.code.as_deref(), Some("E0201"));
        assert_eq!(*parser.current(), Kind::RBracket);
        parser.names.remove("a");

        parser.seek_scalar_offset(source.find("{ var a = [4, 5]; a[1] = 9").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved array store").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Array(slot, values), Statement::Store(read, 2, store_index, element, ..), Statement::Return(Expr::Index(read_back, 2, back_index, ..), _)]
            if slot == read && read == read_back && values.len() == 2
                && matches!(store_index, Expr::Number(1)) && matches!(element, Expr::Number(9))
                && matches!(**back_index, Expr::Number(1))));
        parser.names.remove("a");

        parser.seek_scalar_offset(source.find("{ var a = [4, 5]; a[false]").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved store index error").err().unwrap();
        assert_eq!(error.code.as_deref(), Some("E0201"));
        assert_eq!(*parser.current(), Kind::RBracket);
        parser.names.remove("a");

        parser.seek_scalar_offset(source.find("{ let a = [4, 5]; a[0] = 9").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved immutable store").err().unwrap();
        assert_eq!(error.message, "cannot assign to immutable variable 'a'");
        assert_eq!(error.secondary.len(), 1);
        parser.names.remove("a");

        parser.seek_scalar_offset(source.find("{ var a = [4, 5]; a[0] = false").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved store value error").err().unwrap();
        assert_eq!(error.message, "array elements must be i64");
        parser.names.remove("a");

        parser.seek_scalar_offset(source.find("{ var a = [4, 5]; a[0] += 1").unwrap());
        assert!(parser.lower_resolved_scalar_body().is_none(), "compound store stays typed");

        let param_offset = source.find("(a: &[i64; 2]").unwrap() + 1;
        parser.names.insert("a".into(), (0, false, Type::Array(2)));
        parser.binding_ranges.insert(0, param_offset..param_offset + 1);
        parser.seek_scalar_offset(source.find("{ return a[1]").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved array param").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Return(Expr::Index(slot, 2, index, ..), _)]
            if *slot == 0 && matches!(**index, Expr::Number(1))));
        let store_param_offset = source[param_offset + 1..].find("(a: &[i64; 2]").unwrap() + param_offset + 2;
        parser.binding_ranges.insert(0, store_param_offset..store_param_offset + 1);
        parser.seek_scalar_offset(source.find("{ a[0] = 3").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved param store").err().unwrap();
        assert_eq!(error.message, "cannot assign to immutable variable 'a'");
        assert_eq!(error.secondary.len(), 1);
        parser.names.remove("a");

        let p_offset = source.find("(p: &Point)").unwrap() + 1;
        parser.names.insert("p".into(), (0, false, Type::StructRef(0)));
        parser.binding_ranges.insert(0, p_offset..p_offset + 1);
        parser.seek_scalar_offset(source.find("{ return p.x").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved field reads").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Return(Expr::Binary(lhs, Kind::Plus, rhs, ..), _)]
            if matches!(&**lhs, Expr::Field(base, fname, fty)
                if matches!(&**base, Expr::Variable(0, Type::StructRef(0), ..)) && fname == "x" && *fty == Type::Int)
                && matches!(&**rhs, Expr::Field(_, fname, fty) if fname == "y" && *fty == Type::Int)));
        parser.names.remove("p");

        let h_offset = source.find("(h: &Holder)").unwrap() + 1;
        parser.names.insert("h".into(), (0, false, Type::StructRef(3)));
        parser.binding_ranges.insert(0, h_offset..h_offset + 1);
        parser.seek_scalar_offset(source.find("{ return h.text").unwrap());
        assert!(parser.lower_resolved_scalar_body().is_none(), "non-scalar field reads stay typed");
        parser.names.remove("h");

        let o_offset = source.find("(o: &Outer)").unwrap() + 1;
        parser.names.insert("o".into(), (0, false, Type::StructRef(2)));
        parser.binding_ranges.insert(0, o_offset..o_offset + 1);
        parser.seek_scalar_offset(source.find("{ return o.inner.x").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved nested field read").unwrap();
        assert!(matches!(body.as_slice(), [Statement::Return(Expr::Field(outer, fname, fty), _)]
            if matches!(&**outer, Expr::Field(inner, iname, ifty)
                if matches!(&**inner, Expr::Variable(0, Type::StructRef(2), ..)) && iname == "inner" && *ifty == Type::Struct(1))
                && fname == "x" && *fty == Type::Int));
        parser.names.remove("o");

        parser.seek_scalar_offset(source.find("{ let p = Point { x: 1").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved construction").unwrap();
        assert!(matches!(body.as_slice(),
            [Statement::Declare(slot, Expr::StructLit(sid, values)),
             Statement::Return(Expr::Binary(l, Kind::Plus, r, ..), frees),
             Statement::Frees(scope_frees)]
            if *sid == 0 && values.len() == 2
                && matches!(&**l, Expr::Field(b, f, t)
                    if f == "x" && *t == Type::Int
                        && matches!(&**b, Expr::Variable(ps, Type::Struct(0), ..) if *ps == *slot))
                && matches!(&**r, Expr::Field(_, f2, t2) if f2 == "y" && *t2 == Type::Int)
                && *frees == vec![(*slot, Type::Struct(0))]
                && *scope_frees == vec![(*slot, Type::Struct(0))]));
        parser.names.remove("p");

        parser.seek_scalar_offset(source.find("{ let g = Guard { id: 1").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved drop construction").unwrap();
        assert!(matches!(body.as_slice(),
            [Statement::Declare(slot, Expr::StructLit(sid, _)),
             Statement::Return(Expr::Field(_, fname, fty), frees),
             Statement::Frees(scope_frees)]
            if *sid == 4 && fname == "id" && *fty == Type::Int
                && *frees == vec![(*slot, Type::Struct(4))]
                && *scope_frees == vec![(*slot, Type::Struct(4))]),
            "a drop-carrying construction is owned and released on every exit path");

        parser.seek_scalar_offset(source.find("{ let wrong = Point").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved constructor field error").err().unwrap();
        assert_eq!(error.message, "field 'x' must be i64");
        assert_eq!(*parser.current(), Kind::Comma);
        for (binding, message) in [
            ("{ let extra", "struct 'Point' has no field 'z'"),
            ("{ let dup", "field 'x' specified twice"),
            ("{ let missing", "missing field 'y' in 'Point' construction"),
        ] {
            parser.seek_scalar_offset(source.find(binding).unwrap());
            let error = parser.lower_resolved_scalar_body().expect("resolved constructor shape error").err().unwrap();
            assert_eq!(error.message, message);
        }
        parser.names.remove("g");

        parser.seek_scalar_offset(source.find("{ let q = Point { x: 3").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved owned-aggregate call").unwrap();
        assert!(matches!(body.as_slice(),
            [Statement::Declare(q, Expr::StructLit(0, values)),
             Statement::Declare(r, Expr::Call(12, args, Type::Int)),
             Statement::Return(.., frees),
             Statement::Frees(scope_frees)]
            if values.len() == 2
                && matches!(args.as_slice(), [Argument::Value(Expr::Variable(qs, Type::Struct(0), ..))] if *qs == *q)
                && *frees == vec![(*q, Type::Struct(0))]
                && *scope_frees == vec![(*q, Type::Struct(0))]));
        parser.names.remove("q");
        parser.names.remove("r");

        parser.seek_scalar_offset(source.find("{ let q = Point { x: 5").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved use after move").err().unwrap();
        assert_eq!(error.message, "cannot use 'q' because it was moved");
        parser.names.remove("q");
        parser.names.remove("r");
        parser.names.remove("s");

        let b_offset = source.find("(b: bool)").unwrap() + 1;
        parser.names.insert("b".into(), (5, false, Type::Bool));
        parser.binding_ranges.insert(5, b_offset..b_offset + 1);
        parser.seek_scalar_offset(source.find("{ unsafe { let a = 1").unwrap());
        let body = parser.lower_resolved_scalar_body().expect("resolved unsafe block").unwrap();
        assert!(matches!(body.as_slice(),
            [Statement::Unsafe(inner), Statement::Return(Expr::Number(2), frees)]
            if matches!(inner.as_slice(), [Statement::Declare(_slot, Expr::Number(1))])
                && frees.is_empty()));
        parser.seek_scalar_offset(source.find("{ let q = Point { x: 7").unwrap());
        let error = parser.lower_resolved_scalar_body().expect("resolved loop move").err().unwrap();
        assert!(error.message.contains("cannot move 'q' inside a loop body"));
        parser.names.remove("q");
        parser.names.remove("r");
        parser.names.remove("b");

        for prefix in [
            "choose[",
            "Pair[i64, vec[f64]] {",
            "Pair[i64, vec[f64]]::",
            "Choice[i64, vec[f64]]::Some",
            "Choice[i64, vec[f64]]::empty",
        ] {
            let target = source.find(prefix).unwrap();
            let ranges = parser.shared_specialization_arguments(target).unwrap();
            let spellings: Vec<_> = ranges.iter().map(|range| &source[range.clone()]).collect();
            assert_eq!(spellings, ["i64", "vec[f64]"]);
        }

        let target = source.find("*const shared").unwrap();
        parser.pos = parser.tokens.partition_point(|token| token.offset < target);
        assert_eq!(parser.shared_pointer_cast_qualifier(), Some(false));
        assert_eq!(parser.pointer_cast_pointee(None).unwrap(), (false, Type::Shared(parser.ctx.intern_shared(Type::Int))));
        assert_eq!(*parser.current(), Kind::Semi);

        for (prefix, expected) in [("|| -> i64", "i64"), ("|x: i64|", "bool")] {
            let offset = source.find(prefix).unwrap();
            parser.pos = parser.tokens.partition_point(|token| token.offset < offset);
            let header = parser.shared_closure_header().unwrap();
            let ae_syntax::expression::ExpressionKind::Closure { params, return_type, .. } = &header.kind else {
                unreachable!("closure header index");
            };
            assert_eq!(&source[return_type.clone()], expected);
            if expected == "bool" {
                assert_eq!(params[0].name, "x");
                assert_eq!(&source[params[0].ty.clone()], "i64");
            } else {
                assert!(params.is_empty());
            }
        }
        let inline = source.find("task_spawn(ex, ||").unwrap() + "task_spawn(ex, ".len();
        parser.seek_scalar_offset(inline);
        assert!(matches!(parser.spawn_callable(), Ok((Expr::ClosureLit(id), Type::Closure(ty))) if id == ty));
        assert_eq!(*parser.current(), Kind::RParen);

        parser.names.insert("flag".into(), (7, false, Type::Bool));
        parser.binding_ranges.insert(7, binding..binding + 4);
        parser.pos = initializer_token;
        assert!(matches!(
            parser.expression(0, true).unwrap(),
            (Expr::Variable(7, Type::Bool, _, _), Type::Bool),
        ));
        assert_eq!(*parser.current(), Kind::Semi);

        parser.pos = print_token + 2;
        assert!(parser.lower_shared_value(Some(Kind::Semi), false).is_none());
        assert_eq!(parser.pos, print_token + 2);

        parser.pos = condition_token;
        assert!(matches!(
            parser.lower_shared_value(Some(Kind::LBrace), false),
            Some((Expr::Variable(7, Type::Bool, _, _), Type::Bool)),
        ));
        assert_eq!(*parser.current(), Kind::LBrace);

        parser.pos = print_token;
        assert!(matches!(
            parser.lower_output_from_syntax(),
            Some(Statement::Print(Printed::Expr(Expr::Variable(7, Type::Bool, _, _)))),
        ));

        parser.pos = print_token;
        parser.binding_ranges.insert(7, 0..4);
        assert!(parser.lower_output_from_syntax().is_none());
        assert_eq!(parser.pos, print_token);

        parser.binding_ranges.insert(7, binding..binding + 4);
        parser.pos = initializer_token;
        parser.expression_depth = 1;
        parser.expression_nodes = 127;
        assert!(matches!(
            parser.expression(0, false).unwrap(),
            (Expr::Variable(7, Type::Bool, _, _), Type::Bool),
        ));
        assert_eq!(parser.expression_nodes, 128);
        parser.pos = initializer_token;
        let error = match parser.expression(0, false) {
            Err(error) => error,
            Ok(_) => panic!("nested scalar exceeded the expression budget"),
        };
        assert_eq!(error.message, "expression complexity limit exceeded (128)");

        parser.pos = call_token;
        assert!(matches!(
            parser.lower_output_from_syntax(),
            Some(Statement::Print(Printed::Expr(Expr::Call(9, _, Type::Int)))),
        ));
        parser.pos = call_token;
        parser.functions.get_mut("compute").unwrap().extern_c = true;
        assert!(parser.lower_output_from_syntax().is_none());
        assert_eq!(parser.pos, call_token);

        parser.functions.insert("consume".into(), Signature {
            id: 10, params: vec![Type::String], ret: Type::Int, file: "case.ae".into(),
            public: false, extern_c: false,
        });
        let binding = source.find("let text").unwrap() + 4;
        parser.names.insert("text".into(), (8, false, Type::String));
        parser.binding_ranges.insert(8, binding..binding + 4);
        parser.borrows.push((8, 0, BorrowKind::View));
        parser.pos = parser.tokens.iter().position(|token| token.offset == source.find("assert(consume").unwrap()).unwrap();
        let assertion = parser.pos;
        assert!(parser.lower_output_from_syntax().is_none());
        assert_eq!(parser.pos, assertion);
        assert!(!parser.moved.contains(&8));
        assert!(parser.deferred_write_conflicts.is_empty());

        parser.pos = parser.tokens.iter().position(|token| token.offset == source.find("print(consume").unwrap()).unwrap();
        assert!(matches!(
            parser.lower_output_from_syntax(),
            Some(Statement::Print(Printed::Expr(Expr::Call(10, _, Type::Int)))),
        ));
        assert!(parser.moved.contains(&8));
        let offset = source.find("print(consume(text").unwrap() + "print(consume(".len();
        assert_eq!(parser.deferred_write_conflicts, vec![(8, offset, "text".into(), "move", None)]);

        let vector_offset = source.find("vec[1.0").unwrap();
        parser.pos = parser.tokens.partition_point(|token| token.offset < vector_offset);
        assert!(matches!(
            parser.lower_shared_value(Some(Kind::Semi), true),
            Some((Expr::VecLit(values, Type::VecF, _, _), Type::VecF)) if values.len() == 2
        ));
        let print_offset = source.find("print(vec[1])").unwrap();
        parser.pos = parser.tokens.partition_point(|token| token.offset < print_offset);
        let print = parser.pos;
        assert!(parser.lower_output_from_syntax().is_none());
        assert_eq!(parser.pos, print);

        let binding = source.find("let v =").unwrap() + 4;
        parser.names.insert("v".into(), (11, false, Type::VecF));
        parser.binding_ranges.insert(11, binding..binding + 1);
        let index_print = source.find("print(v[0])").unwrap();
        parser.pos = parser.tokens.partition_point(|token| token.offset < index_print);
        assert!(matches!(
            parser.lower_output_from_syntax(),
            Some(Statement::Print(Printed::Expr(Expr::VecIndex(11, _, 1, _, false, Type::F64))))
        ));

        let binding = source.find("var value").unwrap() + 4;
        parser.names.insert("value".into(), (12, true, Type::Bool));
        parser.binding_ranges.insert(12, binding..binding + 5);
        for (prefix, mutable) in [("&value", false), ("&mut value", true)] {
            let cast = source.find(prefix).unwrap();
            parser.pos = parser.tokens.partition_point(|token| token.offset < cast);
            assert!(matches!(
                parser.lower_shared_value(Some(Kind::Semi), false),
                Some((Expr::BorrowAs(operand, actual, 1, column), ty))
                    if matches!(*operand, Expr::Variable(12, Type::Bool, _, _))
                        && actual == mutable && matches!(ty, Type::PtrMut(_)) == mutable
                        && column == cast + 1
            ));
            assert_eq!(*parser.current(), Kind::Semi);
        }

        parser.names.get_mut("value").unwrap().1 = false;
        let cast = source.find("&mut value").unwrap();
        let start = parser.tokens.partition_point(|token| token.offset < cast);
        parser.pos = start;
        assert!(parser.lower_shared_value(Some(Kind::Semi), false).is_none());
        assert_eq!(parser.pos, start);

        let binding = source.find("let raw").unwrap() + 4;
        let raw = Type::Ptr(parser.ctx.intern_pointee(Type::Bool));
        parser.names.insert("raw".into(), (13, false, raw));
        parser.binding_ranges.insert(13, binding..binding + 3);
        let cast = source.find("raw as").unwrap();
        parser.pos = parser.tokens.partition_point(|token| token.offset < cast);
        assert!(matches!(
            parser.lower_shared_value(Some(Kind::Semi), false),
            Some((Expr::PtrCast(operand, true, id, _, column), Type::PtrMut(pointee)))
                if matches!(*operand, Expr::Variable(13, ty, _, _) if ty == raw)
                    && id == pointee && parser.ctx.ptr_pointee(id) == Type::F64
                    && column == cast + "raw as *mut f64;".len()
        ));
        assert_eq!(*parser.current(), Kind::Semi);
    }
}
