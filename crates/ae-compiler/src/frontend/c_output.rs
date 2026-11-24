//! C type mapping, runtime pruning and whole-program C assembly.
use std::fmt::Write as _;
use super::*;
pub(super) fn c_type_field(ty: Type, ctx: &CompileContext) -> String {
    match ty {
        Type::Int(width) => width.c_name().to_owned(),
        Type::Bool => "bool".to_owned(),
        Type::Float(width) => width.c_name().to_owned(),
        Type::String => "ae_string".to_owned(),
        Type::StrView => "ae_strview".to_owned(),
        Type::Vec => "ae_vec".to_owned(),
        Type::StrVec => "ae_strvec".to_owned(),
        Type::VecF => "ae_f64vec".to_owned(),
        Type::Map => "ae_map".to_owned(),
        Type::MapF => "ae_mapf".to_owned(),
        Type::Struct(k) => format!("ae_t{k}"),
        // Unreachable: there is no syntax for a borrow-typed field; the value
        // keeps the pointer shape so a future slip prints valid C.
        Type::StructRef(k) => format!("const ae_t{k} *"),
        Type::StructMut(k) => format!("ae_t{k} *"),
        Type::Enum(_, k) => format!("ae_e{k}"),
        Type::VecRef => "const ae_vec *".to_owned(),
        Type::StrVecRef => "const ae_strvec *".to_owned(),
        Type::VecFRef => "const ae_f64vec *".to_owned(),
        Type::Array(n) => format!("int64_t [{n}]"),
        // Unreachable: there is no syntax for a cursor-typed field.
        Type::Tokens => "ae_tokens".to_owned(),
        // Unreachable: no syntax for a closure-typed field.
        Type::Closure(_) => "void".to_owned(),
        // Unreachable: no syntax for a function-typed field.
        Type::Fn(_) => "void".to_owned(),
        // Unreachable: thread handles bind as locals only (spec/concurrency.md
        // §2, like pointers); the pointer shape keeps a future slip valid C.
        Type::Thread(_) => "void *".to_owned(),
        // Unreachable: an atomic-typed field has no syntax yet; the struct
        // shape keeps a future slip valid C.
        Type::Atomic => "ae_atomic".to_owned(),
        // Unreachable: a shared-typed field has no syntax yet; the struct
        // shape keeps a future slip valid C.
        Type::Shared(_) => "ae_shared".to_owned(),
        Type::Mutex => "ae_mutex".to_owned(),
        Type::Guard => "ae_guard".to_owned(),
        Type::Chan => "ae_chan".to_owned(),
        Type::Exec => "ae_exec".to_owned(),
        Type::TaskScope => "ae_task_scope".to_owned(),
        Type::Pool => "ae_pool".to_owned(),
        // Unreachable: no syntax for a pointer-typed field (M2.12 pointers
        // bind as locals only); the value keeps the pointer shape so a future
        // slip prints valid C.
        Type::Ptr(id) => format!("const {} *", c_type_field(ctx.ptr_pointee(id), ctx)),
        Type::PtrMut(id) => format!("{} *", c_type_field(ctx.ptr_pointee(id), ctx)),
    }
}
pub(super) fn c_type(ty: Type, id: usize, declaration: bool, ctx: &CompileContext) -> String {
    match ty {
        Type::Int(width) => format!("{} v{id}", width.c_name()),
        Type::Bool => format!("bool v{id}"),
        Type::Float(width) => format!("{} v{id}", width.c_name()),
        Type::String => format!("ae_string v{id}"),
        Type::StrView => format!("ae_strview v{id}"),
        Type::Vec => format!("ae_vec v{id}"),
        Type::StrVec => format!("ae_strvec v{id}"),
        Type::VecF => format!("ae_f64vec v{id}"),
        Type::Map => format!("ae_map v{id}"),
        Type::MapF => format!("ae_mapf v{id}"),
        Type::Struct(k) => format!("ae_t{k} v{id}"),
        Type::StructRef(k) if declaration => format!("const ae_t{k} *v{id}"),
        Type::StructRef(k) => format!("const ae_t{k} *"),
        Type::StructMut(k) if declaration => format!("ae_t{k} *v{id}"),
        Type::StructMut(k) => format!("ae_t{k} *"),
        Type::Enum(_, k) => format!("ae_e{k} v{id}"),
        Type::VecRef if declaration => format!("const ae_vec *v{id}"),
        Type::VecRef => "const ae_vec *".to_owned(),
        Type::StrVecRef if declaration => format!("const ae_strvec *v{id}"),
        Type::StrVecRef => "const ae_strvec *".to_owned(),
        Type::VecFRef if declaration => format!("const ae_f64vec *v{id}"),
        Type::VecFRef => "const ae_f64vec *".to_owned(),
        Type::Tokens if declaration => format!("ae_tokens v{id}"),
        Type::Tokens => "ae_tokens".to_owned(),
        // Never declared: a closure has no storage.
        Type::Closure(_) => "void".to_owned(),
        // The handle is one heap slot: spawn allocates it, join consumes it.
        Type::Thread(_) if declaration => format!("void *v{id}"),
        Type::Thread(_) => "void *".to_owned(),
        Type::Atomic if declaration => format!("ae_atomic v{id}"),
        Type::Atomic => "ae_atomic".to_owned(),
        Type::Shared(_) if declaration => format!("ae_shared v{id}"),
        Type::Shared(_) => "ae_shared".to_owned(),
        Type::Mutex if declaration => format!("ae_mutex v{id}"),
        Type::Mutex => "ae_mutex".to_owned(),
        Type::Guard if declaration => format!("ae_guard v{id}"),
        Type::Guard => "ae_guard".to_owned(),
        Type::Chan if declaration => format!("ae_chan v{id}"),
        Type::Chan => "ae_chan".to_owned(),
        Type::Exec if declaration => format!("ae_exec v{id}"),
        Type::Exec => "ae_exec".to_owned(),
        Type::TaskScope if declaration => format!("ae_task_scope v{id}"),
        Type::TaskScope => "ae_task_scope".to_owned(),
        Type::Pool if declaration => format!("ae_pool v{id}"),
        Type::Pool => "ae_pool".to_owned(),
        // A function pointer parameter declares its name inside the pointer
        // parens, the way C requires.
        Type::Fn(sig) => {
            let (params, ret) = ctx.fn_sig(sig);
            let c_params = if params.is_empty() { "void".to_owned() } else { params.iter().map(|t| match t {
                Type::VecRef | Type::StrVecRef | Type::VecFRef | Type::StructRef(_) | Type::StructMut(_) | Type::Array(_) => c_type(*t, 0, false, ctx),
                _ => t.c_temp(ctx).trim_end().to_owned(),
            }).collect::<Vec<_>>().join(", ") };
            if declaration {
                format!("{} (*v{id})({})", ret.c_temp(ctx).trim_end(), c_params)
            } else {
                format!("{} (*)({})", ret.c_temp(ctx).trim_end(), c_params)
            }
        }
        Type::Array(_) if declaration => format!("const int64_t *v{id}"),
        Type::Array(_) => "const int64_t *".to_owned(),
        // M2.12 slice 2: a pointer binds as a plain local (no release, no
        // move tracking), so the declaration is the plain C pointer form.
        Type::Ptr(pid) if declaration => format!("const {} *v{id}", c_type_field(ctx.ptr_pointee(pid), ctx)),
        Type::Ptr(pid) => format!("const {} *", c_type_field(ctx.ptr_pointee(pid), ctx)),
        Type::PtrMut(pid) if declaration => format!("{} *v{id}", c_type_field(ctx.ptr_pointee(pid), ctx)),
        Type::PtrMut(pid) => format!("{} *", c_type_field(ctx.ptr_pointee(pid), ctx)),
    }
}
// Return type of a generated C function: the same spelling the expression
// temporaries use, without the variable name.
pub(super) fn c_return(ty: Type, ctx: &CompileContext) -> String {
    match ty {
        // Never returned: a function value cannot be a return type yet.
        Type::Fn(_) => "void".to_owned(),
        Type::Bool => "bool".to_owned(),
        Type::Float(FloatTy::F64) => "double".to_owned(),
        Type::String => "ae_string".to_owned(),
        Type::StrView => "ae_strview".to_owned(),
        Type::Vec | Type::StrVec | Type::VecF => c_type_field(ty, ctx),
        Type::Map => "ae_map".to_owned(),
        Type::MapF => "ae_mapf".to_owned(),
        Type::Struct(k) => format!("ae_t{k}"),
        Type::Enum(_, k) => format!("ae_e{k}"),
        // A fake-clock value (std/std/test/fake_clock.ae) is a copy-semantics
        // atomic handle; returning one must keep the handle type, not fall
        // into the scalar int64_t default (ADR-0224 R2.2).
        Type::Atomic => "ae_atomic".to_owned(),
        // A raw pointer return keeps its star: the extern-prototype contract
        // must match the runtime definition exactly (ADR-0208's allocator
        // bridge returns int64_t* from a void* wrapper).
        Type::Ptr(id) => format!("const {} *", c_type_field(ctx.ptr_pointee(id), ctx)),
        Type::PtrMut(id) => format!("{} *", c_type_field(ctx.ptr_pointee(id), ctx)),
        _ => "int64_t".to_owned(),
    }
}
/// `full` is the self-contained C translation unit that `emit-c` prints.
/// `runtime_len` marks where the program-specific suffix begins: the runtime
/// part is the reachable subset of the fixed runtime prefix, the suffix holds
/// user layouts and functions. Both views come from one string so they cannot
/// disagree about what the program means.
pub(super) struct GeneratedC { pub(super) full: String, pub(super) runtime_len: usize }

#[derive(Clone, Copy, PartialEq)]
pub(super) enum CScan { Code, Str, Char, Block }

/// Count braces outside strings, character literals and comments. Runtime code
/// contains JSON examples and format strings, so a literal '{' must never be
/// mistaken for the start of a function body.
pub(super) fn c_brace_delta(line: &str, state: &mut CScan) -> i32 {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    let mut delta = 0i32;
    while i < bytes.len() {
        let b = bytes[i];
        let next = bytes.get(i + 1).copied().unwrap_or(0);
        match *state {
            CScan::Block => { if b == b'*' && next == b'/' { *state = CScan::Code; i += 2; } else { i += 1; } }
            CScan::Str => { if b == b'\\' { i += 2; } else { if b == b'"' { *state = CScan::Code; } i += 1; } }
            CScan::Char => { if b == b'\\' { i += 2; } else { if b == b'\'' { *state = CScan::Code; } i += 1; } }
            CScan::Code => {
                if b == b'/' && next == b'*' { *state = CScan::Block; i += 2; }
                else if b == b'/' && next == b'/' { break; }
                else if b == b'"' { *state = CScan::Str; i += 1; }
                else if b == b'\'' { *state = CScan::Char; i += 1; }
                else { if b == b'{' { delta += 1; } else if b == b'}' { delta -= 1; } i += 1; }
            }
        }
    }
    delta
}

/// Recognise the single-line `static <ret> ae_xxx(<args>) {` form used by
/// every runtime function. Returns (specifier prefix, signature, name, tail).
pub(super) fn runtime_function_start(line: &str) -> Option<(&str, &str, &str, &str)> {
    let at = line.find("static ")?;
    let head = &line[..at];
    if !head.trim().is_empty() && head.trim() != "_Noreturn" { return None; }
    let rest = &line[at + "static ".len()..];
    let open = rest.find('(')?;
    let close = open + rest[open..].find(')')?;
    let brace = close + rest[close..].find('{')?;
    if !rest[close + 1..brace].trim().is_empty() { return None; }
    let name = &rest[..open];
    let name = &name[name.rfind("ae_")?..];
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') { return None; }
    Some((head, rest[..brace].trim_end(), name, &rest[brace + 1..]))
}

/// Split the runtime prefix into top-level units. `Some(name)` is a complete
/// function definition; `None` is any other text (includes, macros, typedefs,
/// static data, comments) that must be kept verbatim. Anything not recognised
/// as a function lands in a `None` unit, which is the conservative outcome.
pub(super) fn runtime_units(prefix: &str) -> Vec<(Option<String>, String)> {
    let mut units: Vec<(Option<String>, String)> = Vec::new();
    let mut pending = String::new();
    // A `// unit: <name>` line names the non-function region that follows
    // (typedefs, globals): the region becomes one prune unit kept only when
    // something reachable names it, instead of riding along with the
    // always-emitted header text. The name spans the region's functions too
    // (every pending chunk inside carries it), because globals are often
    // interrupted by the functions that serve them. `// unit: -` ends the
    // named region. The marker lines themselves are not emitted.
    let mut pending_name: Option<String> = None;
    // `//` comments and blank lines directly above a function belong to it: a
    // pruned function must not leave its explanation behind in the generated C
    // (ADR-50). A comment above code that is not a function is flushed back into
    // `pending`, and multi-line `/* */` blocks always stay there, so the file
    // header and section banners survive.
    let mut lead = String::new();
    let mut depth = 0i32;
    let mut state = CScan::Code;
    let mut in_function = false;
    for line in prefix.split_inclusive('\n') {
        let raw = line.strip_suffix('\n').unwrap_or(line);
        if !in_function {
            if let Some(name) = raw.trim_start().strip_prefix("// unit: ") {
                if !pending.is_empty() {
                    units.push((pending_name.take(), std::mem::take(&mut pending)));
                }
                let name = name.trim();
                pending_name = (name != "-").then(|| name.to_owned());
                continue;
            }
            if let Some((head, signature, name, tail)) = runtime_function_start(raw) {
                // A marker names a region that may span several physical
                // units (globals interrupted by functions): every pending
                // chunk inside the region carries the same name, so keeping
                // the name keeps the whole region.
                if !pending.is_empty() {
                    units.push((pending_name.clone(), std::mem::take(&mut pending)));
                }
                let mut text = std::mem::take(&mut lead);
                text.push_str(head); text.push_str(signature); text.push('{'); text.push_str(tail); text.push('\n');
                units.push((Some(name.to_owned()), text));
                let mut first = String::from("{");
                first.push_str(tail);
                state = CScan::Code;
                depth = c_brace_delta(&first, &mut state);
                in_function = depth > 0;
                continue;
            }
            let trimmed = raw.trim_start();
            if trimmed.is_empty() || trimmed.starts_with("//") {
                lead.push_str(line);
            } else {
                if !lead.is_empty() { pending.push_str(&std::mem::take(&mut lead)); }
                pending.push_str(line);
            }
            continue;
        }
        units.last_mut().unwrap().1.push_str(line);
        depth += c_brace_delta(raw, &mut state);
        if depth <= 0 { depth = 0; in_function = false; state = CScan::Code; }
    }
    if !lead.is_empty() { pending.push_str(&lead); }
    if !pending.is_empty() { units.push((pending_name, pending)); }
    units
}

/// Every `ae_...` identifier in a piece of C text. Used both for roots and
/// for the transitive dependency walk; a symbol that names no function is
/// harmless (it is a type, macro or a name in a comment).
pub(super) fn ae_symbols(text: &str) -> Vec<String> {
    // `ae_` is three bytes: the prefix length and the slice width must agree
    // (the 2026-11-20 rename shortened the prefix, so both were retuned).
    const PREFIX: &str = "ae_";
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut i = 0usize;
    while i + PREFIX.len() <= bytes.len() {
        if &bytes[i..i + PREFIX.len()] == PREFIX.as_bytes() && (i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_')) {
            let mut end = i + PREFIX.len();
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') { end += 1; }
            found.push(text[i..end].to_owned());
            i = end;
            continue;
        }
        i += 1;
    }
    found
}

/// Marker on every runtime function whose Windows branch needs a declaration
/// from `<windows.h>`. The marker rides along with the function unit, so
/// pruning removes it exactly when the last such function is removed.
pub(super) const WIN32_HEADER_MARKER: &str = "/* win32-header */";
/// A function that calls Win32 entry points this runtime declares itself; it
/// needs no header, so it must not force <windows.h> into every program (ADR-55:
/// `print` is used by nearly every program, and the header costs about half a
/// second of build time).
#[cfg(test)]
pub(super) const WIN32_DECLARED_MARKER: &str = "/* win32-declared */";
pub(super) const WIN32_HEADER_INCLUDE: &str = "#include <windows.h>";
pub(super) const WIN32_HEADER_OMITTED: &str = "/* windows.h omitted */";

/// Keep only runtime functions reachable from the program. Roots are the
/// symbols in the program suffix plus every symbol in the always-retained
/// non-function text (macro bodies and static initialisers can call functions
/// that the generated code never names directly). The walk then follows calls
/// between retained functions. Dropping an unreachable function cannot change
/// behaviour: it has no caller in this translation unit.
pub(super) fn strip_define_lines(text: &str) -> String {
    // `#define` bodies in this runtime are numbers and type names, never
    // function calls, so they are not roots. The prelude result macros name
    // `ae_eN` types; a macro's *use* in a kept function forces that
    // layout through the AE_<NAME>_ scan below, not its definition line.
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if line.trim_start().starts_with("#define") { continue; }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Which prelude enum an `AE_<NAME>_...` macro belongs to. The layout of
/// that enum must exist whenever a kept function uses any of its macros.
pub(super) const PRELUDE_MACRO_PREFIXES: &[(&str, usize)] = &[
    ("AE_READ_", 0), ("AE_WRITE_", 1), ("AE_INT_", 2), ("AE_DIR_", 3),
    ("AE_FILE_", 4), ("AE_STATUS_", 5), ("AE_BOOL_", 6), ("AE_COMMAND_", 7),
    ("AE_FLOAT_", 8),
];

/// Keep only runtime functions reachable from the program. Roots are the
/// symbols in the program suffix plus every symbol in the always-retained
/// non-function text (static initialisers can call functions that the
/// generated code never names directly). The walk then follows calls between
/// retained functions. Prelude enum layouts are emitted on demand (ADR-66):
/// a layout exists when user code names the type, or when a kept function
/// uses one of the enum's `AE_<NAME>_` macros.
pub(super) fn prune_runtime(core: &str, ex: &str, prelude: &[(String, String, String)], suffix: &str) -> (String, usize) {
    let core_units = runtime_units(core);
    let ex_units = runtime_units(ex);
    // The layout release/forfeit functions are ordinary named units: they join
    // the reachability walk and are emitted (right after their typedef) only
    // when something calls them.
    let mut units: Vec<(Option<String>, &str)> = Vec::with_capacity(core_units.len() + prelude.len() * 2 + ex_units.len());
    for (name, body) in &core_units {
        units.push((name.clone(), body.as_str()));
    }
    for (eid, (_, release, forfeit)) in prelude.iter().enumerate() {
        units.push((Some(format!("ae_e{eid}_release")), release.as_str()));
        units.push((Some(format!("ae_e{eid}_forfeit")), forfeit.as_str()));
    }
    for (name, body) in &ex_units {
        units.push((name.clone(), body.as_str()));
    }

    let mut definitions: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, (name, _)) in units.iter().enumerate() {
        if let Some(name) = name { definitions.entry(name.clone()).or_default().push(index); }
    }
    let mut keep: HashSet<String> = HashSet::new();
    let mut queue: Vec<String> = Vec::new();
    queue.extend(ae_symbols(suffix));
    for (index, (name, body)) in units.iter().enumerate() {
        if name.is_none() {
            queue.extend(ae_symbols(&strip_define_lines(body)));
        }
        let _ = index;
    }
    // Hosts embedding the generated C may substitute the allocator without
    // the generated code naming it — but only programs that allocate can
    // meaningfully substitute, so the documented entry point joins the walk
    // only once an allocation user has survived it (charter rule 8, M3.1.01:
    // a program that never allocates carries none of the allocator).
    let drain = |queue: &mut Vec<String>, keep: &mut HashSet<String>| {
        while let Some(symbol) = queue.pop() {
            if !keep.insert(symbol.clone()) { continue; }
            if let Some(indices) = definitions.get(&symbol) {
                for &index in indices {
                    // A kept function that uses a prelude result macro needs that
                    // enum's typedef, even though the body only names the macro.
                    let body = units[index].1;
                    for (macro_prefix, eid) in PRELUDE_MACRO_PREFIXES {
                        if body.contains(macro_prefix) {
                            let typedef = format!("ae_e{eid}");
                            if !keep.contains(&typedef) { queue.push(typedef); }
                        }
                    }
                    for next in ae_symbols(body) {
                        if !keep.contains(&next) { queue.push(next); }
                    }
                }
            }
        }
    };
    drain(&mut queue, &mut keep);
    const ALLOCATOR_USERS: [&str; 5] = [
        "ae_alloc", "ae_alloc_resize", "ae_alloc_try", "ae_free", "ae_args",
    ];
    if ALLOCATOR_USERS.iter().any(|symbol| keep.contains(*symbol)) {
        queue.push("ae_set_allocator".to_owned());
        drain(&mut queue, &mut keep);
    }
    let mut out = String::new();
    for (name, body) in &core_units {
        if name.is_none() || name.as_ref().is_some_and(|n| keep.contains(n)) { out.push_str(body); }
    }
    for (eid, (typedef, release, forfeit)) in prelude.iter().enumerate() {
        if !keep.contains(&format!("ae_e{eid}")) { continue; }
        out.push_str(typedef);
        if keep.contains(&format!("ae_e{eid}_release")) { out.push_str(release); }
        if keep.contains(&format!("ae_e{eid}_forfeit")) { out.push_str(forfeit); }
    }
    for (name, body) in &ex_units {
        if name.is_none() || name.as_ref().is_some_and(|n| keep.contains(n)) { out.push_str(body); }
    }
    // The <windows.h> include lives in a non-function unit, so pruning alone
    // keeps it even when every Win32-dependent function was removed. Parsing
    // that header costs ~545 ms on Windows (measured), pure overhead for a
    // program that never calls the Win32 layer. Check the runtime part only:
    // user code is appended afterwards and must never influence this decision.
    // Failure mode is conservative: a missing marker only costs an unnecessary
    // header parse; a wrongly dropped header is a compile error, never a
    // silent behaviour change.
    if !out.contains(WIN32_HEADER_MARKER) {
        out = out.replacen(WIN32_HEADER_INCLUDE, WIN32_HEADER_OMITTED, 1);
    }
    let runtime_len = out.len();
    out.push_str(suffix);
    (out, runtime_len)
}

/// One prelude enum's C layout, in three independently-emittable pieces
/// (ADR-66): the typedef/tags, the release function, the forfeit function.
pub(super) fn prelude_enum_pieces(enums: &[EnumDef], eid: usize, ctx: &CompileContext) -> (String, String, String) {
    let def = &enums[eid];
    let mut typedef = String::new();
    let _ = writeln!(typedef, "\ntypedef enum {{");
    for v in 0..def.variants.len() {
        if v > 0 { typedef.push_str(",\n"); }
        let _ = write!(typedef, "    AE_E{eid}_{v} = {v}");
    }
    let _ = writeln!(typedef, "\n}} ae_e{eid}_tag;");
    let mut members = String::new();
    for (vidx, (_, fields)) in def.variants.iter().enumerate() {
        if fields.is_empty() { continue; }
        let _ = writeln!(typedef, "typedef struct {{");
        for (index, ty) in fields.iter().enumerate() {
            let _ = writeln!(typedef, "    {} p{index};", c_type_field(*ty, ctx));
        }
        let _ = writeln!(typedef, "}} ae_e{eid}_v{vidx};");
        let _ = writeln!(members, "        ae_e{eid}_v{vidx} v{vidx};");
    }
    // A union needs at least one member; enums without payloads get a
    // placeholder so the initializer form stays uniform.
    if members.is_empty() { members.push_str("        char ae_empty;\n"); }
    let _ = writeln!(typedef, "typedef struct {{\n    ae_e{eid}_tag tag;\n    union {{\n{members}    }} u;\n}} ae_e{eid};");
    let mut release = String::new();
    let mut forfeit = String::new();
    let _ = writeln!(release, "static void ae_e{eid}_release(ae_e{eid} *v) {{");
    let _ = writeln!(forfeit, "static void ae_e{eid}_forfeit(ae_e{eid} *v) {{");
    for (vidx, (_, fields)) in def.variants.iter().enumerate() {
        if fields.is_empty() { continue; }
        let _ = writeln!(release, "    if (v->tag == AE_E{eid}_{vidx}) {{");
        let _ = writeln!(forfeit, "    if (v->tag == AE_E{eid}_{vidx}) {{");
        for (index, ty) in fields.iter().enumerate() {
            match ty {
                Type::String => {
                    let _ = writeln!(release, "        ae_str_release(&v->u.v{vidx}.p{index});");
                    let _ = writeln!(forfeit, "        ae_str_forfeit(&v->u.v{vidx}.p{index});");
                }
                Type::Vec => {
                    let _ = writeln!(release, "        ae_vec_release(&v->u.v{vidx}.p{index});");
                    let _ = writeln!(forfeit, "        ae_vec_forfeit(&v->u.v{vidx}.p{index});");
                }
                Type::StrVec => {
                    let _ = writeln!(release, "        ae_strvec_release(&v->u.v{vidx}.p{index});");
                    let _ = writeln!(forfeit, "        ae_strvec_forfeit(&v->u.v{vidx}.p{index});");
                }
                Type::VecF => {
                    let _ = writeln!(release, "        ae_f64vec_release(&v->u.v{vidx}.p{index});");
                    let _ = writeln!(forfeit, "        ae_f64vec_forfeit(&v->u.v{vidx}.p{index});");
                }
                Type::Map => {
                    let _ = writeln!(release, "        ae_map_release(&v->u.v{vidx}.p{index});");
                    let _ = writeln!(forfeit, "        ae_map_forfeit(&v->u.v{vidx}.p{index});");
                }
                Type::MapF => {
                    let _ = writeln!(release, "        ae_mapf_release(&v->u.v{vidx}.p{index});");
                    let _ = writeln!(forfeit, "        ae_mapf_forfeit(&v->u.v{vidx}.p{index});");
                }
                Type::Bool => {
                    let _ = writeln!(release, "        v->u.v{vidx}.p{index} = false;");
                    let _ = writeln!(forfeit, "        v->u.v{vidx}.p{index} = false;");
                }
                _ => {
                    let _ = writeln!(release, "        v->u.v{vidx}.p{index} = 0;");
                    let _ = writeln!(forfeit, "        v->u.v{vidx}.p{index} = 0;");
                }
            }
        }
        release.push_str("    }\n");
        forfeit.push_str("    }\n");
    }
    release.push_str("}\n");
    forfeit.push_str("}\n");
    (typedef, release, forfeit)
}

pub(super) fn emit_enum_layouts(enums: &[EnumDef], from: usize, to: usize, out: &mut String, ctx: &CompileContext) {
    // User enums (and any caller that wants a contiguous block) emit all three
    // pieces; the prelude path hands them to the pruner instead (ADR-66).
    for eid in from..to {
        let (typedef, release, forfeit) = prelude_enum_pieces(enums, eid, ctx);
        out.push_str(&typedef);
        out.push_str(&release);
        out.push_str(&forfeit);
    }
}

fn emit_body(function: &Function, out: &mut String, ctx: &CompileContext) -> Result<(), String> {
    if codegen_mir::enabled() {
        codegen_mir::emit_function_body(function, out, ctx)?;
    } else {
        emit_statements(&function.body, out, &mut 0, ctx);
    }
    Ok(())
}

pub(super) fn emit_program(program: &mut Program) -> Result<GeneratedC, String> {
    let files = &program.files;
    let structs = &program.structs;
    let enums = &program.enums;
    let prelude_count = prelude_enums().0.len();
    // Fallible built-ins and platform helpers name prelude enum types, so the
    // prelude layouts come before runtime_ex.c. Their order is checked here
    // rather than trusted; a reordered prelude would silently change payload
    // ids in generated code.
    let names: Vec<&str> = enums.iter().take(6).map(|def| def.name).collect();
    if names != ["ReadResult", "WriteResult", "IntResult", "DirResult", "FileResult", "StatusResult"] {
        return Err(format!("internal: prelude enums must be registered first and in order, found {names:?}"));
    }
    // The runtime reaches the pruner in three parts (ADR-66): the core, the
    // platform extensions, and one layout piece-triple per prelude enum. The
    // pruner emits a prelude layout only when the program references it, so a
    // program that uses no fallible built-in carries none of their layouts.
    let core = include_str!("../runtime.c");
    let ex = include_str!("../runtime_ex.c");
    let prelude: Vec<(String, String, String)> = (0..prelude_count)
        .map(|eid| prelude_enum_pieces(enums, eid, &program.ctx))
        .collect();
    let mut out = String::new();
    // User enums are emitted after the runtime prefix. runtime_ex.c only names
    // prelude ids, so this does not change any type it can reference.
    emit_enum_layouts(enums, prelude_count, enums.len(), &mut out, &program.ctx);
    // Struct layouts and their ownership helpers. Release zeroes every field
    // and frees owned ones; forfeit only zeroes (the value moved elsewhere).
    for (sid, def) in structs.iter().enumerate() {
        let mut layout = String::new();
        for (n, ty) in &def.fields {
            let _ = writeln!(layout, "    {} {n};", c_type_field(*ty, &program.ctx));
        }
        let mut release = String::new();
        let mut forfeit = String::new();
        let mut activate = "((void)0)".to_owned();
        if let Some(drop_id) = def.drop_fn {
            if def.repr_c {
                let _ = writeln!(release, "    ae_fn_{drop_id}(v);");
            } else {
                let mut live = "ae_live".to_owned();
                while def.fields.iter().any(|(name, _)| name == &live) {
                    live.push('_');
                }
                let _ = writeln!(layout, "    bool {live};");
                activate = format!("((value).{live} = true)");
                let _ = writeln!(release, "    if (!v->{live}) return;\n    v->{live} = false;\n    ae_fn_{drop_id}(v);");
                let _ = writeln!(forfeit, "    v->{live} = false;");
            }
        }
        let _ = writeln!(out, "\n#define AE_T{sid}_ACTIVATE(value) {activate}");
        for (n, ty) in &def.fields {
            match ty {
                Type::String => {
                    let _ = writeln!(release, "    ae_str_release(&v->{n});");
                    let _ = writeln!(forfeit, "    ae_str_forfeit(&v->{n});");
                }
                Type::Vec => {
                    let _ = writeln!(release, "    ae_vec_release(&v->{n});");
                    let _ = writeln!(forfeit, "    ae_vec_forfeit(&v->{n});");
                }
                Type::Struct(k) => {
                    let _ = writeln!(release, "    ae_t{k}_release(&v->{n});");
                    let _ = writeln!(forfeit, "    ae_t{k}_forfeit(&v->{n});");
                }
                Type::StrVec => {
                    let _ = writeln!(release, "    ae_strvec_release(&v->{n});");
                    let _ = writeln!(forfeit, "    ae_strvec_forfeit(&v->{n});");
                }
                Type::VecF => {
                    let _ = writeln!(release, "    ae_f64vec_release(&v->{n});");
                    let _ = writeln!(forfeit, "    ae_f64vec_forfeit(&v->{n});");
                }
                Type::Map => {
                    let _ = writeln!(release, "    ae_map_release(&v->{n});");
                    let _ = writeln!(forfeit, "    ae_map_forfeit(&v->{n});");
                }
                Type::MapF => {
                    let _ = writeln!(release, "    ae_mapf_release(&v->{n});");
                    let _ = writeln!(forfeit, "    ae_mapf_forfeit(&v->{n});");
                }
                Type::Enum(_, k) => {
                    let _ = writeln!(release, "    ae_e{k}_release(&v->{n});");
                    let _ = writeln!(forfeit, "    ae_e{k}_forfeit(&v->{n});");
                }
                other => {
                    // Owned fields got a release/forfeit call above. Scalars
                    // zero so a released slot reads as visibly empty; copy
                    // handles (cell/channel/exec/pool/scope) and views own
                    // nothing, so releasing them is a no-op — and their C
                    // types are struct values no literal zero can assign.
                    match other {
                        Type::Bool => {
                            let _ = writeln!(release, "    v->{n} = false;");
                            let _ = writeln!(forfeit, "    v->{n} = false;");
                        }
                        Type::Int(IntTy::I64) | Type::Float(FloatTy::F64) => {
                            let _ = writeln!(release, "    v->{n} = 0;");
                            let _ = writeln!(forfeit, "    v->{n} = 0;");
                        }
                        _ => {}
                    }
                }
            }
        }
        let _ = writeln!(out, "\ntypedef struct {{\n{layout}}} ae_t{sid};");
        if let Some(drop_id) = def.drop_fn {
            let _ = writeln!(out, "static int64_t ae_fn_{drop_id}(ae_t{sid} *v);");
        }
        let _ = writeln!(out, "static void ae_t{sid}_release(ae_t{sid} *v) {{\n{release}}}");
        let _ = writeln!(out, "static void ae_t{sid}_forfeit(ae_t{sid} *v) {{\n{forfeit}}}");
    }
    // The `extern "C"` declarations' prototypes (M2.12 slice 4): the call
    // site emits the C symbol bare, so without a prototype every foreign
    // call is an implicit declaration — an error at C99 and UB at C89 (the
    // implicit `int` return truncates i64). The prototypes sit after the
    // struct typedefs, so a `#[repr(C)]` parameter names a complete type.
    for (c_name, params, ret) in program.ctx.extern_declarations() {
        let params_c = if params.is_empty() {
            "void".to_owned()
        } else {
            params.iter().enumerate().map(|(index, ty)| c_type(*ty, index, true, &program.ctx)).collect::<Vec<_>>().join(", ")
        };
        out.push_str(&format!("{} {}({});
", c_return(ret, &program.ctx), c_name, params_c));
    }

    let all: Vec<&Function> = files.iter().flatten().collect();
    for function in &all {
        if function.is_main { continue; }
        let params = if function.params.is_empty() { "void".to_owned() } else { function.params.iter().map(|&(id, ty)| c_type(ty, id, false, &program.ctx)).collect::<Vec<_>>().join(", ") };
        let ret = c_return(function.returns, &program.ctx);
        out.push_str(&format!("\nstatic {ret} ae_fn_{}({params});\n", function.id));
    }
    // Thread support (ADR-0214): the context layouts and trampolines the
    // program's spawn sites reference, then the one slot type. They sit after
    // the functions they call and before the bodies that call them. The
    // return value is the context's first member, so wait reads it without
    // knowing the capture layout.
    let spawned: Vec<crate::frontend::closures::SpawnedThread> = program.ctx.take_spawned();
    if !spawned.is_empty() {
        out.push_str("
typedef struct { void *native; void *ctx; } ae_thread_slot;
");
    }
    for entry in &spawned {
        match entry {
            crate::frontend::closures::SpawnedThread::Closure { fn_id, captures, borrows, ret } => {
                let mut all = captures.clone();
                all.extend(borrows.iter().map(|(_, ty)| *ty));
                let fields = all.iter().enumerate()
                    .map(|(index, ty)| format!("    {} f{index};
", c_type_field(*ty, &program.ctx)))
                    .collect::<Vec<_>>().join("");
                out.push_str(&format!("
typedef struct {{
    {} ret;
{fields}}} ae_thread_ctx_{fn_id};
", c_type_field(*ret, &program.ctx)));
                let arguments = if all.is_empty() {
                    String::new()
                } else {
                    (0..all.len()).map(|index| format!("c->f{index}")).collect::<Vec<_>>().join(", ")
                };
                out.push_str(&format!("static void ae_thread_entry_{fn_id}(void *raw) {{
    ae_thread_ctx_{fn_id} *c = raw;
    c->ret = ae_fn_{fn_id}({arguments});
}}
"));
            }
            crate::frontend::closures::SpawnedThread::FnValue { sig, ret } => {
                let ret_c = c_type_field(*ret, &program.ctx);
                out.push_str(&format!("
typedef struct {{
    {ret_c} ret;
    {ret_c} (*fn)(void);
}} ae_thread_fncall_{sig};
"));
                out.push_str(&format!("static void ae_thread_entry_f{sig}(void *raw) {{
    ae_thread_fncall_{sig} *c = raw;
    c->ret = c->fn();
}}
"));
            }
        }
    }
    for function in &all {
        if function.is_main { continue; }
        let params = if function.params.is_empty() { "void".to_owned() } else { function.params.iter().map(|&(id, ty)| c_type(ty, id, true, &program.ctx)).collect::<Vec<_>>().join(", ") };
        let ret = c_return(function.returns, &program.ctx);
        let signature = match program.ctx.exposure_name(function.id) {
            // A `#[no_mangle]` exposure keeps its own name with external
            // linkage (M2.12 U7); the rest stay internal to the unit.
            Some(name) => format!("{ret} {name}({params})"),
            None => format!("static {ret} ae_fn_{}({params})", function.id),
        };
        out.push_str(&format!("\n{signature} {{\n"));
        emit_body(function, &mut out, &program.ctx)?;
        // Defensive backstop: even a future control-flow analysis bug must not
        // produce a C function that falls through without returning a value.
        out.push_str("    ae_fail(\"function exited without return\", 0, 0);\n}\n");
    }
    let main = all.iter().find(|f| f.is_main).expect("load_program checks exactly one main");
    out.push_str("\nint main(int argc, char **argv) {\n    ae_args_init(argc, argv);\n    ae_argc_g = (int64_t)argc;\n    ae_argv_g = argv;\n");
    emit_body(main, &mut out, &program.ctx)?;
    out.push_str("    if (fflush(stdout) == EOF) return 1;\n    return 0;\n}\n");
    // The fixed prefix is program-independent; emit only its reachable part.
    // `split_off` avoids copying the 100 KB runtime text.
    let user = out;
    let (full, pruned_len) = prune_runtime(core, ex, &prelude, &user);
    Ok(GeneratedC { full, runtime_len: pruned_len })
}
