//! Route marks: shape, field and ownership predicates the resolved-route gates ask.
use super::*;

pub(super) fn scalar_type(ty: Type) -> bool {
    matches!(ty, Type::Int(IntTy::I64) | Type::Float(FloatTy::F64) | Type::Bool)
}

/// A builtin call that builds a fresh `string`: assigning its result to an
/// owned string releases the old value and takes the new one, exactly like a
/// text literal or another owned string on the right-hand side.
pub(super) fn string_result_builtin(name: &str) -> bool {
    matches!(name, "slice" | "trim" | "to_string" | "join" | "read_file" | "path_join" | "path_name" | "path_parent" | "utf8_prefix")
}

/// The pure value builtins the resolved body route lowers itself: each reads
/// its arguments (never moves except a string `+`, which the Binary arm
/// handles), so the body needs no borrow-argument machinery for them. Every
/// other reserved name keeps its typed body until its surface is reviewed.
pub(super) const RESOLVED_BUILTINS: &[&str] = &[
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
pub(super) const ENUM_RESULT_BUILTINS: &[(&str, &str)] = &[
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

pub(super) fn resolved_builtin(name: &str) -> bool {
    RESOLVED_BUILTINS.contains(&name) || MATH_FNS.iter().any(|(math, _, _)| *math == name)
}

/// Intrinsics the typed expression parser dispatches by name and the resolved
/// body route delegates whole to that layer: the pointer pair and `bytes`.
/// The scan admits their calls, and the lowering parses them with the typed
/// postfix code (whose operational checks and shapes are authoritative).
pub(super) fn typed_intrinsic(name: &str) -> bool {
    matches!(name, "ptr_write" | "ptr_drop_in_place" | "bytes" | "tokenize")
}

/// A struct the body route can own outright: no `Drop` impl and only scalar
/// fields. Constructing one allocates nothing a release routine would walk,
/// so a construction, a by-value argument, and a by-value return all move the
/// value without an ownership hook — the same shape the drop-free
/// construction collection marks. Structs with owned fields (`string`,
/// vectors) stay on the typed route until construction moves are tracked.
pub(super) fn owned_plain_struct(ty: Type, structs: &[StructDef]) -> bool {
    let Type::Struct(sid) = ty else { return false };
    structs.get(sid).is_some_and(|defn| defn.drop_fn.is_none()
        && defn.fields.iter().all(|(_, ty)| scalar_type(*ty)
            // A raw pointer is an address, not an owned value: it carries no
            // release surface, so a plain struct may hold one.
            || matches!(ty, Type::Ptr(_) | Type::PtrMut(_))
            || owned_plain_struct(*ty, structs)))
}

/// A struct whose fields are scalar or `string`: it carries one owned value
/// per string field, and the generated `ae_tN_release` frees them. The body
/// route may build one (each string value moves in) and own it.
/// A struct that declares `fn drop(&mut self)` and whose fields are plain:
/// the body route owns it by value exactly like a drop-free struct — the
/// generated release calls the user's drop and clears the handle, so a
/// parameter or local of this type releases correctly.
pub(super) fn plain_drop_struct(ty: Type, structs: &[StructDef]) -> bool {
    let Type::Struct(sid) = ty else { return false };
    structs.get(sid).is_some_and(|defn| defn.drop_fn.is_some()
        && defn.fields.iter().all(|(_, ty)| scalar_type(*ty)
            // A raw pointer carries no release surface (it is an address, not
            // an owned value): a guard that points at a slot releases cleanly.
            || matches!(ty, Type::Ptr(_) | Type::PtrMut(_))
            || owned_plain_struct(*ty, structs)))
}

pub(super) fn owned_field_struct(ty: Type, structs: &[StructDef]) -> bool {
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
pub(super) fn field_owner_struct(
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

pub(super) fn field_ty_in(sid: usize, field: &str, structs: &[StructDef]) -> Option<Type> {
    let defn = structs.get(sid)?;
    defn.fields.iter().find(|(name, _)| name == field).map(|(_, ty)| *ty)
}

/// The declared shape of one field read in a resolved body, including a
/// generic instance spelled in source (`let p = Pair[i64, Label] { .. };`
/// → `p.left` is `i64`, `p.right.name` walks through `Label`). Nested
/// generic instances (`Pair[string, Pair[string, string]].right.left`) stay
/// shapes until the final field resolves to a value. No layout is
/// instantiated, so the answer is stateless.
pub(super) fn resolved_field_shape(
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

pub(super) fn shape_field(shape: &ShapeArg, field: &str, structs: &[StructDef], generics: &Generics) -> Option<ShapeArg> {
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

pub(super) fn payload_shape(payload: &Payload, args: &[ShapeArg], generics: &Generics) -> Option<ShapeArg> {
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

pub(super) fn receiver_field_root(expression: &ae_syntax::expression::Expression) -> Option<usize> {
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
pub(super) fn field_chain_root(expression: &ae_syntax::expression::Expression) -> Option<&ae_syntax::expression::Expression> {
    use ae_syntax::expression::ExpressionKind;
    match &expression.kind {
        ExpressionKind::Field { base, .. } => field_chain_root(base),
        ExpressionKind::Atom(Kind::Ident(_)) => Some(expression),
        _ => None,
    }
}

/// Whether the expression IS the chain root (no field access on it).
pub(super) fn field_chain_is_bare(expression: &ae_syntax::expression::Expression) -> bool {
    matches!(expression.kind, ae_syntax::expression::ExpressionKind::Atom(Kind::Ident(_)))
}

pub(super) fn receiver_value(expression: &ae_syntax::expression::Expression) -> Option<&ae_syntax::expression::Expression> {
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
pub(super) fn receiver_field_type(
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
pub(super) fn visit_expression_tree(expression: &ae_syntax::expression::Expression, visit: &mut impl FnMut(&ae_syntax::expression::Expression)) {
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

// The unchecked-operation walkers: route admissibility predicates over the
// compiler's typed statement/expression tree.
impl<'a> Parser<'a> {
    // (spec/unsafe.md U4): a block with none is silent decoration.
    pub(super) fn block_has_unchecked_operation(&self, statements: &[Statement]) -> bool {
        for statement in statements {
            if self.statement_has_unchecked_operation(statement) { return true; }
        }
        false
    }
    pub(super) fn statement_has_unchecked_operation(&self, statement: &Statement) -> bool {
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
    pub(super) fn expr_has_unchecked_operation(&self, expr: &Expr) -> bool {
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
    }}
