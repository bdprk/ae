//! M2.11: In-process execution interpreter with UB / checked-arithmetic /
//! use-after-move / bounds detection (`ae test --interp`, `ae run --interp`).
//!
//! Executes the lowered program directly in the compiler process without
//! invoking a C compiler or linker. Enforces the same runtime invariants as
//! the generated C runtime (`integer overflow`, `division by zero`,
//! `assertion failed`, `array index out of bounds`, `vector index out of bounds`,
//! `use of moved ...`), reporting failures in the exact `ae:line:col: msg`
//! diagnostic format.
use std::collections::HashMap;

use super::*;

const LF_CHAR: char = '\n';

#[derive(Debug, Clone)]
enum Val {
    Int(i64),
    I8(i8),
    I16(i16),
    I32(i32),
    Isize(isize),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    Usize(usize),
    F32(f32),
    Float(f64),
    Bool(bool),
    Char(char),
    Unit,
    Str(String),
    Array(Vec<i64>),
    VecI(Vec<i64>),
    VecS(Vec<String>),
    VecF(Vec<f64>),
    Map(HashMap<String, i64>),
    MapF(HashMap<String, f64>),
    Tokens { base: Rc<str>, sep: Rc<str> },
    Struct(#[allow(dead_code)] usize, HashMap<String, Val>),
    Enum(#[allow(dead_code)] usize, usize, Vec<Val>),
    Closure(usize),
    FnPtr(usize),
    // A raw pointer (M2.12 slice 2): the slot it points at. Pointers are
    // locals-only — no pointer type annotation exists on parameters or
    // returns, so a pointer cannot cross a frame.
    Ptr(usize),
    Ref(usize),
    // A thread handle (ADR-0214): the sequence number of its stashed result.
    // The interpreter is single-threaded — the spawned callable runs to
    // completion at the spawn site and wait hands back the value, the same
    // observable behavior the compiled backends give a deterministic program.
    Thread(usize),
    // A copy-semantics atomic handle (spec/concurrency.md §8): the index of
    // the shared cell. Cloning the value shares the cell, matching the C
    // struct copy the compiled backends perform.
    Atomic(usize),
    // Shared boxes (ADR-0216, spec/concurrency.md §9), indexed by handle.
    Shared(usize),
    // Mutex and lock guard (ADR-0216, single-threaded interpreter).
    Mutex,
    Guard,
    // Channel handle (M4.3, single-threaded interpreter).
    Chan(usize),
    // Task executor handle (M4.2, ADR-0218).
    Exec(usize),
    // Structured task scope and work-stealing pool (ADR-0220); cancel
    // tokens are ae source since ADR-0224 R2.4.
    TaskScope(usize),
    Pool(usize),
    Moved,
}

enum TaskFrame {
    Block(Vec<Statement>, usize),
    While(Expr, Vec<Statement>),
}

struct TaskState {
    locals: HashMap<usize, Val>,
    stack: Vec<TaskFrame>,
}

enum Flow {
    Next,
    Return(Val),
    Break,
    Continue(usize),
    Exit(u8),
    Trap(String),
}

struct Env<'a> {
    program: &'a Program,
    fns: HashMap<usize, &'a Function>,
    out: String,
    steps: usize,
    // Spawned results, indexed by handle (ADR-0214). `None` marks a handle
    // whose value wait already took.
    // A spawned body's outcome, replayed at wait: a recorded trap surfaces at
    // the same program point as the compiled backend (ADR-0217).
    threads: Vec<Option<Result<Val, Flow>>>,
    // Atomic cells (spec/concurrency.md §8), indexed by handle: the
    // single-threaded interpreter orders operations naturally, so the cell
    // is a plain i64 and the handle shares it by copy.
    atomics: Vec<i64>,
    // Shared boxes (ADR-0216, spec/concurrency.md §9), indexed by handle.
    shared: Vec<Val>,
    // Channels (M4.3, spec/concurrency.md §10): (buffer, capacity, closed).
    chans: Vec<(std::collections::VecDeque<i64>, i64, bool)>,
    // Task executors (M4.2, ADR-0218): FIFO queues of cooperative tasks.
    execs: Vec<std::collections::VecDeque<TaskState>>,
}

const MAX_STEPS: usize = 5_000_000;

pub fn interpret_path(path: &Path) -> Result<(String, u8), (String, String)> {
    interpret_path_with_imports(path, &PackageImports::new())
}

pub fn interpret_path_with_imports(path: &Path, imports: &PackageImports) -> Result<(String, u8), (String, String)> {
    let program = loader::load_program_with_imports(path, imports).map_err(|e| (String::new(), e))?;
    let mut fns = HashMap::new();
    let mut main_fn = None;
    for file in &program.files {
        for function in file {
            fns.insert(function.id, function);
            if function.is_main {
                main_fn = Some(function);
            }
        }
    }
    let main_fn = main_fn.ok_or_else(|| (String::new(), "program must define a main function".to_owned()))?;
    let mut env = Env { program: &program, fns, out: String::new(), steps: 0, threads: Vec::new(), atomics: Vec::new(), shared: Vec::new(), chans: Vec::new(), execs: Vec::new() };
    let mut locals: HashMap<usize, Val> = HashMap::new();
    match eval_stmts(&mut env, &main_fn.body, &mut locals, &mut None) {
        Flow::Next | Flow::Return(_) => Ok((env.out, 0)),
        Flow::Exit(code) => Ok((env.out, code)),
        Flow::Trap(msg) => Err((env.out, msg)),
        Flow::Break | Flow::Continue(_) => Err((env.out, "internal: unwound loop control in main".to_owned())),
    }
}

// A prelude result enum (IntResult, FloatResult, ...), resolved by name:
// its enum id and the Ok/Err variant indices in declaration order.
fn result_enum_of(env: &Env, name: &str) -> Option<(usize, usize, usize)> {
    // The enum id is the declaration's position in the program's enum list.
    let (eid, def) = env.program.enums.iter().enumerate().find(|(_, def)| def.name == name)?;
    let ok = def.variants.iter().position(|(v, _)| v == "Ok")?;
    let err = def.variants.iter().position(|(v, _)| v == "Err")?;
    Some((eid, ok, err))
}

// The map a place names, resolved through the locals (and the caller's, for
// a closure reading a captured map). Kind and storage must agree.
enum MapVal<'a> {
    MapI(&'a mut HashMap<String, i64>),
    MapF(&'a mut HashMap<String, f64>),
    Missing,
}

fn map_val_ref<'a>(
    place: &MapPlace,
    locals: &'a mut HashMap<usize, Val>,
    caller_locals: &'a mut Option<&mut HashMap<usize, Val>>,
) -> Result<MapVal<'a>, Flow> {
    let slot: Option<&mut Val> = match place {
        MapPlace::Var(id, _) => locals.get_mut(id),
        MapPlace::Field(id, fname, _) => match locals.get_mut(id) {
            Some(Val::Struct(_, fields)) => fields.get_mut(fname),
            _ => None,
        },
    };
    let slot = slot.or_else(|| match (place, caller_locals.as_deref_mut()) {
        (MapPlace::Var(id, _), Some(caller)) => caller.get_mut(id),
        (MapPlace::Field(id, fname, _), Some(caller)) => match caller.get_mut(id) {
            Some(Val::Struct(_, fields)) => fields.get_mut(fname),
            _ => None,
        },
        _ => None,
    });
    match (place, slot) {
        (_, Some(Val::Map(m))) => Ok(MapVal::MapI(m)),
        (_, Some(Val::MapF(m))) => Ok(MapVal::MapF(m)),
        (_, Some(Val::Moved)) => Err(trap("use of moved map", 0, 0)),
        _ => Ok(MapVal::Missing),
    }
}

// A prelude result value: the enum type carries the name, the Ok/Err
// variant indices come from the declaration, and the payloads match the
// variant's fields. The error messages the interp produces differ from the
// C runtime's strerror text — programs cannot rely on it (ADR-55's rule).
fn result_value(env: &Env, ty: &Type, ok: bool, payloads: Vec<Val>) -> Result<Val, Flow> {
    let Some(Type::Enum(name, _)) = Some(ty) else {
        return Err(Flow::Trap("internal: result type not an enum".to_owned()));
    };
    let Some((eid, ok_idx, err_idx)) = result_enum_of(env, name) else {
        return Err(Flow::Trap("internal: result enum missing".to_owned()));
    };
    Ok(Val::Enum(eid, if ok { ok_idx } else { err_idx }, payloads))
}

fn path_string(val: Val) -> Option<String> {
    match val { Val::Str(s) => Some(s), _ => None }
}

// A path operand: evaluated, then narrowed to a string with a positioned
// internal trap when it is not one.
fn path_operand(result: Result<Val, Flow>) -> Result<String, Flow> {
    match result? {
        Val::Str(s) => Ok(s),
        _ => Err(Flow::Trap("internal: path operand".to_owned())),
    }
}

fn is_separator(byte: char) -> bool { byte == '/' || byte == chr_backslash() }
fn chr_backslash() -> char { chr(92) }
fn chr(n: u8) -> char { n as char }

fn path_is_absolute(path: &str) -> bool {
    if path.is_empty() { return false; }
    let mut chars = path.chars();
    let first = chars.next().unwrap();
    if is_separator(first) { return true; }
    // A drive letter (`x:`) — the second character decides.
    path.len() >= 2 && path.as_bytes()[1] == b':'
}

// `path_name`: everything after the final separator; `path_parent`: every-
// thing before it (the separator itself dropped, matching the runtime).
fn path_name(path: &str) -> String {
    match path.rfind(is_separator) {
        Some(idx) => path[idx + 1..].to_owned(),
        None => path.to_owned(),
    }
}

fn path_parent(path: &str) -> String {
    match path.rfind(is_separator) {
        Some(idx) => path[..idx].to_owned(),
        None => String::new(),
    }
}

// The map a place names, resolved through the locals: whether the key is
// present, looked up through the same place resolution as the value above.
fn m_contains_ref(place: &MapPlace, locals: &HashMap<usize, Val>, caller_locals: &Option<&mut HashMap<usize, Val>>, key: &str) -> bool {
    let slot: Option<&Val> = match place {
        MapPlace::Var(id, _) => locals.get(id),
        MapPlace::Field(id, fname, _) => match locals.get(id) {
            Some(Val::Struct(_, fields)) => fields.get(fname),
            _ => None,
        },
    };
    let slot = slot.or_else(|| match (place, caller_locals.as_deref()) {
        (MapPlace::Var(id, _), Some(caller)) => caller.get(id),
        (MapPlace::Field(id, fname, _), Some(caller)) => match caller.get(id) {
            Some(Val::Struct(_, fields)) => fields.get(fname),
            _ => None,
        },
        _ => None,
    });
    match slot {
        Some(Val::Map(m)) => m.contains_key(key),
        Some(Val::MapF(m)) => m.contains_key(key),
        _ => false,
    }
}

// ---- JSON navigation (json_get / json_int) ---------------------------------
//
// The interp scans the raw document text: a path walks keys and indices, and
// a value is returned as its raw slice (strings decoded). The runtime scans
// the same way, so the semantics match without a materialised tree.

fn json_skip_ws(bytes: &[u8], mut at: usize) -> usize {
    while at < bytes.len() && matches!(bytes[at], b' ' | b'\t' | b'\n' | b'\r') { at += 1; }
    at
}

// The exclusive end of the value whose first byte sits at `at`: a string's
// closing quote, a container's matching brace/bracket, or a literal's end.
fn json_value_end(bytes: &[u8], at: usize) -> usize {
    json_value_end_inner(bytes, at)
}

fn json_value_end_inner(bytes: &[u8], mut at: usize) -> usize {
    match bytes.get(at) {
        Some(b'"') => {
            at += 1;
            while at < bytes.len() {
                match bytes[at] {
                    b'\\' => { at += 2; }
                    b'"' => return at + 1,
                    _ => at += 1,
                }
            }
            at
        }
        Some(open @ (b'{' | b'[')) => {
            let close = if *open == b'{' { b'}' } else { b']' };
            let mut depth = 0usize;
            while at < bytes.len() {
                match bytes[at] {
                    b'"' => {
                        at += 1;
                        while at < bytes.len() {
                            match bytes[at] {
                                b'\\' => at += 2,
                                b'"' => { at += 1; break; }
                                _ => at += 1,
                            }
                        }
                    }
                    c if c == *open => { depth += 1; at += 1; }
                    c if c == close => { depth -= 1; if depth == 0 { return at + 1; } at += 1; }
                    _ => at += 1,
                }
            }
            at
        }
        _ => {
            while at < bytes.len() && !matches!(bytes[at], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') { at += 1; }
            at
        }
    }
}

// Decode a JSON string's raw interior (escapes included, surrogate pairs
// combined). `raw` excludes the surrounding quotes.
fn json_decode_string(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' { out.push(c); continue; }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('b') => out.push('\u{0008}'),
            Some('f') => out.push('\u{000C}'),
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                let code = u32::from_str_radix(&hex, 16).unwrap_or(0xFFFD);
                let decoded = if (0xD800..0xDC00).contains(&code) {
                    // A high surrogate: the pair's low half follows.
                    if chars.next() == Some('\\') && chars.next() == Some('u') {
                        let low: String = chars.by_ref().take(4).collect();
                        let low = u32::from_str_radix(&low, 16).unwrap_or(0xFFFD);
                        let combined = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                        char::from_u32(combined).unwrap_or('\u{FFFD}')
                    } else {
                        '\u{FFFD}'
                    }
                } else {
                    char::from_u32(code).unwrap_or('\u{FFFD}')
                };
                out.push(decoded);
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

// The members of the object whose opening brace sits at `at`: each yields
// (decoded key, value start, value end).
fn json_object_members(bytes: &[u8], at: usize) -> Vec<(String, usize, usize)> {
    let mut members = Vec::new();
    let mut cursor = json_skip_ws(bytes, at + 1);
    loop {
        if cursor >= bytes.len() || bytes[cursor] == b'}' { break; }
        cursor = json_skip_ws(bytes, cursor);
        if cursor >= bytes.len() || bytes[cursor] != b'"' { break; }
        // The member's key, raw interior then decoded.
        let key_start = cursor + 1;
        let mut key_end = key_start;
        while key_end < bytes.len() {
            match bytes[key_end] {
                b'\\' => key_end += 2,
                b'"' => break,
                _ => key_end += 1,
            }
        }
        let raw_key = String::from_utf8_lossy(&bytes[key_start..key_end]).to_string();
        let key = json_decode_string(&raw_key);
        let after_key = json_skip_ws(bytes, key_end + 1);
        if after_key >= bytes.len() || bytes[after_key] != b':' { break; }
        let value_start = json_skip_ws(bytes, after_key + 1);
        let value_end = json_value_end(bytes, value_start);
        members.push((key, value_start, value_end));
        cursor = json_skip_ws(bytes, value_end);
        if cursor < bytes.len() && bytes[cursor] == b',' { cursor += 1; } else { break; }
    }
    members
}

// The elements of the array whose opening bracket sits at `at`.
fn json_array_elements(bytes: &[u8], at: usize) -> Vec<(usize, usize)> {
    let mut elements = Vec::new();
    let mut cursor = json_skip_ws(bytes, at + 1);
    loop {
        if cursor >= bytes.len() || bytes[cursor] == b']' { break; }
        let value_start = cursor;
        let value_end = json_value_end(bytes, value_start);
        elements.push((value_start, value_end));
        cursor = json_skip_ws(bytes, value_end);
        if cursor < bytes.len() && bytes[cursor] == b',' { cursor = json_skip_ws(bytes, cursor + 1); } else { break; }
    }
    elements
}

// The raw slice the path selects, or the runtime's exact "why": the
// segment walk mirrors ae_json_locate, whose messages programs can
// observe through the result enums.
fn json_locate(text: &str, path: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let kind_of = |raw: &str| -> &'static str {
        match raw.trim().as_bytes().first() {
            Some(b'"') => "a string",
            Some(b'{') => "an object",
            Some(b'[') => "an array",
            Some(b't') | Some(b'f') => "a boolean",
            Some(b'n') => "null",
            Some(_) => "a number",
            None => "nothing",
        }
    };
    let mut start = json_skip_ws(bytes, 0);
    let mut end = json_value_end(bytes, start);
    let mut segments: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_index = false;
    for c in path.chars() {
        match c {
            '.' if !in_index => { segments.push(current.clone()); current = String::new(); }
            '[' => { if !current.is_empty() { segments.push(current.clone()); current = String::new(); } in_index = true; }
            ']' => { segments.push(current.clone()); current = String::new(); in_index = false; }
            _ => current.push(c),
        }
    }
    if !current.is_empty() { segments.push(current); }
    for segment in &segments {
        let index: Option<usize> = segment.parse().ok();
        match index {
            Some(i) => {
                let raw = text[start..end].to_owned();
                let kind = kind_of(&raw);
                if kind != "an array" {
                    return Err(format!("path segment [{}] needs an array, found {}", segment, kind));
                }
                let elements = json_array_elements(bytes, start);
                match elements.get(i) {
                    Some((value_start, value_end)) => { start = *value_start; end = *value_end; }
                    None => return Err(format!("path segment [{}] is past the end of the array", i)),
                }
            }
            None => {
                let raw = text[start..end].to_owned();
                let kind = kind_of(&raw);
                if kind != "an object" {
                    return Err(format!("path segment '{}' needs an object, found {}", segment, kind));
                }
                let members = json_object_members(bytes, start);
                match members.iter().find(|(key, _, _)| key == segment) {
                    Some((_, value_start, value_end)) => { start = *value_start; end = *value_end; }
                    None => return Err(format!("no member named '{}'", segment)),
                }
            }
        }
    }
    Ok(text[start..end].to_owned())
}
fn trap(msg: &str, line: usize, col: usize) -> Flow {
    // The exact format the compiled binary's runtime prints
    // (`ae:%zu:%zu: %s`, runtime.c): the interp's trap output must be
    // indistinguishable from it, prefix included (differential contract).
    Flow::Trap(format!("ae:{line}:{col}: {msg}"))
}

fn eval_stmts(
    env: &mut Env<'_>,
    stmts: &[Statement],
    locals: &mut HashMap<usize, Val>,
    caller_locals: &mut Option<&mut HashMap<usize, Val>>,
) -> Flow {
    for stmt in stmts {
        env.steps += 1;
        if env.steps > MAX_STEPS {
            return Flow::Trap("interpreter step limit exceeded".to_owned());
        }
        match stmt {
            Statement::Declare(id, expr) => {
                match eval_expr(env, expr, locals, caller_locals, true) {
                    Ok(v) => { locals.insert(*id, v); }
                    Err(f) => return f,
                }
            }
            Statement::Assign(id, expr, _) => {
                match eval_expr(env, expr, locals, caller_locals, true) {
                    Ok(v) => { locals.insert(*id, v); }
                    Err(f) => return f,
                }
            }
            Statement::Array(id, exprs) => {
                let mut items = Vec::with_capacity(exprs.len());
                for e in exprs {
                    match eval_expr(env, e, locals, caller_locals, false) {
                        Ok(Val::Int(n)) => items.push(n),
                        Ok(_) => return Flow::Trap("internal: array element not i64".to_owned()),
                        Err(f) => return f,
                    }
                }
                locals.insert(*id, Val::Array(items));
            }
            Statement::Store(id, len, idx_expr, val_expr, line, col) => {
                let idx = match eval_expr(env, idx_expr, locals, caller_locals, false) {
                    Ok(Val::Int(n)) => n,
                    Ok(_) => return Flow::Trap("internal: index not i64".to_owned()),
                    Err(f) => return f,
                };
                let val = match eval_expr(env, val_expr, locals, caller_locals, false) {
                    Ok(Val::Int(n)) => n,
                    Ok(_) => return Flow::Trap("internal: array store value not i64".to_owned()),
                    Err(f) => return f,
                };
                if idx < 0 || (idx as usize) >= *len {
                    return trap("array index out of bounds", *line, *col);
                }
                if let Some(Val::Array(arr)) = locals.get_mut(id) {
                    arr[idx as usize] = val;
                }
            }
            Statement::FieldStore(id, fname, _, val_expr, _through_ptr) => {
                let val = match eval_expr(env, val_expr, locals, caller_locals, true) {
                    Ok(v) => v,
                    Err(f) => return f,
                };
                // The write lands where the storage is. A slot holding a
                // reference is written through to the lender's frame —
                // whatever the statement's flag says, because the flag's
                // provenance varies with the parse — and the direct case
                // writes the owning slot. The ref-through branch consumes
                // the value; the direct branch clones it, because only one
                // of the two runs.
                let ref_target = match locals.get(id).cloned() {
                    Some(Val::Ref(target_id)) => Some(target_id),
                    _ => None,
                };
                let mut wrote = false;
                if let Some(target_id) = ref_target
                    && let Some(caller) = caller_locals.as_mut()
                        && let Some(Val::Struct(_, fields)) = caller.get_mut(&target_id) {
                            fields.insert(fname.clone(), val.clone());
                            wrote = true;
                        }
                if !wrote
                    && let Some(Val::Struct(_, fields)) = locals.get_mut(id) {
                        fields.insert(fname.clone(), val);
                    }
            }
            Statement::VecPush(id, val_expr, line, col) => {
                let val = match eval_expr(env, val_expr, locals, caller_locals, false) {
                    Ok(Val::Int(n)) => n,
                    Ok(_) => return Flow::Trap("internal: vec push type".to_owned()),
                    Err(f) => return f,
                };
                match locals.get_mut(id) {
                    Some(Val::VecI(v)) => v.push(val),
                    Some(Val::Moved) => return trap("use of moved vector", *line, *col),
                    _ => return Flow::Trap("internal: vec push on non-vec".to_owned()),
                }
            }
            Statement::StrVecPush(id, val_expr, line, col, _) => {
                let val = match eval_expr(env, val_expr, locals, caller_locals, false) {
                    Ok(Val::Str(s)) => s,
                    Ok(_) => return Flow::Trap("internal: strvec push type".to_owned()),
                    Err(f) => return f,
                };
                match locals.get_mut(id) {
                    Some(Val::VecS(v)) => v.push(val),
                    Some(Val::Moved) => return trap("use of moved vector", *line, *col),
                    _ => return Flow::Trap("internal: strvec push on non-vec".to_owned()),
                }
            }
            Statement::VecFPush(id, val_expr, line, col) => {
                let val = match eval_expr(env, val_expr, locals, caller_locals, false) {
                    Ok(Val::Float(x)) => x,
                    Ok(_) => return Flow::Trap("internal: f64vec push type".to_owned()),
                    Err(f) => return f,
                };
                match locals.get_mut(id) {
                    Some(Val::VecF(v)) => v.push(val),
                    Some(Val::Moved) => return trap("use of moved vector", *line, *col),
                    _ => return Flow::Trap("internal: f64vec push on non-vec".to_owned()),
                }
            }
            Statement::VecPop(id, line, col) | Statement::StrVecPop(id, line, col) | Statement::VecFPop(id, line, col) => {
                match locals.get_mut(id) {
                    Some(Val::VecI(v)) => {
                        if v.pop().is_none() { return trap("pop on empty vector", *line, *col); }
                    }
                    Some(Val::VecS(v)) => {
                        if v.pop().is_none() { return trap("pop on empty vector", *line, *col); }
                    }
                    Some(Val::VecF(v)) => {
                        if v.pop().is_none() { return trap("pop on empty vector", *line, *col); }
                    }
                    Some(Val::Moved) => return trap("use of moved vector", *line, *col),
                    _ => return Flow::Trap("internal: pop on non-vec".to_owned()),
                }
            }
            // `m[key] = value;` (and the field form): the map owns the
            // bytes, the key copies, and the value must match the map's
            // value type — the same contract the runtime's put enforces.
            Statement::ForTokens(cursor, elem, body, ..) => {
                let (base, sep) = match locals.get(cursor) {
                    Some(Val::Tokens { base, sep }) => (Rc::clone(base), Rc::clone(sep)),
                    _ => return Flow::Trap("internal: for-tokens cursor".to_owned()),
                };
                let mut offset = 0;
                while offset < base.len() {
                    let rest = &base[offset..];
                    let token = match rest.find(sep.as_ref()) {
                        Some(relative) => {
                            offset += relative + sep.len();
                            &rest[..relative]
                        }
                        None => {
                            offset = base.len();
                            rest
                        }
                    };
                    locals.insert(*elem, Val::Str(token.to_owned()));
                    match eval_stmts(env, body, locals, caller_locals) {
                        Flow::Next | Flow::Continue(0) => {}
                        Flow::Continue(d) => return Flow::Continue(d - 1),
                        Flow::Break => break,
                        other => return other,
                    }
                }
            }
            Statement::MapPut(place, key_expr, value_expr, ..) => {
                let key = match eval_expr(env, key_expr, locals, caller_locals, false) {
                    Ok(Val::Str(k)) => k,
                    Ok(_) => return Flow::Trap("internal: map key type".to_owned()),
                    Err(f) => return f,
                };
                let int_value = match eval_expr(env, value_expr, locals, caller_locals, false) {
                    Ok(Val::Int(n)) => Some(n),
                    Ok(Val::Float(_)) => None,
                    Err(f) => return f,
                    _ => return Flow::Trap("internal: map value type".to_owned()),
                };
                let float_value = match eval_expr(env, value_expr, locals, caller_locals, false) {
                    Ok(Val::Float(x)) => Some(x),
                    _ => None,
                };
                match place {
                    MapPlace::Var(id, MapKind::Int) => match locals.get_mut(id) {
                        Some(Val::Map(m)) => { if let Some(n) = int_value { m.insert(key, n); } }
                        _ => return Flow::Trap("internal: map put on non-map".to_owned()),
                    },
                    MapPlace::Var(id, MapKind::Float) => match locals.get_mut(id) {
                        Some(Val::MapF(m)) => { if let Some(x) = float_value { m.insert(key, x); } }
                        _ => return Flow::Trap("internal: map put on non-map".to_owned()),
                    },
                    MapPlace::Field(id, fname, MapKind::Int) => match locals.get_mut(id) {
                        Some(Val::Struct(_, fields)) => match fields.get_mut(fname) {
                            Some(Val::Map(m)) => { if let Some(n) = int_value { m.insert(key, n); } }
                            _ => return Flow::Trap("internal: map put on non-map field".to_owned()),
                        },
                        _ => return Flow::Trap("internal: map put on non-struct".to_owned()),
                    },
                    MapPlace::Field(id, fname, MapKind::Float) => match locals.get_mut(id) {
                        Some(Val::Struct(_, fields)) => match fields.get_mut(fname) {
                            Some(Val::MapF(m)) => { if let Some(x) = float_value { m.insert(key, x); } }
                            _ => return Flow::Trap("internal: map put on non-map field".to_owned()),
                        },
                        _ => return Flow::Trap("internal: map put on non-struct".to_owned()),
                    },
                }
            }
            Statement::VecStore(id, idx_expr, val_expr, line, col) => {
                let idx = match eval_expr(env, idx_expr, locals, caller_locals, false) {
                    Ok(Val::Int(n)) => n,
                    Ok(_) => return Flow::Trap("internal: index not i64".to_owned()),
                    Err(f) => return f,
                };
                let val = match eval_expr(env, val_expr, locals, caller_locals, false) {
                    Ok(Val::Int(n)) => n,
                    Ok(_) => return Flow::Trap("internal: vec store type".to_owned()),
                    Err(f) => return f,
                };
                match locals.get_mut(id) {
                    Some(Val::VecI(v)) => {
                        if idx < 0 || (idx as usize) >= v.len() { return trap("vector index out of bounds", *line, *col); }
                        v[idx as usize] = val;
                    }
                    Some(Val::Moved) => return trap("use of moved vector", *line, *col),
                    _ => return Flow::Trap("internal: vec store on non-vec".to_owned()),
                }
            }
            Statement::StrVecStore(id, idx_expr, val_expr, line, col, _) => {
                let idx = match eval_expr(env, idx_expr, locals, caller_locals, false) {
                    Ok(Val::Int(n)) => n,
                    Ok(_) => return Flow::Trap("internal: index not i64".to_owned()),
                    Err(f) => return f,
                };
                let val = match eval_expr(env, val_expr, locals, caller_locals, false) {
                    Ok(Val::Str(s)) => s,
                    Ok(_) => return Flow::Trap("internal: strvec store type".to_owned()),
                    Err(f) => return f,
                };
                match locals.get_mut(id) {
                    Some(Val::VecS(v)) => {
                        if idx < 0 || (idx as usize) >= v.len() { return trap("vector index out of bounds", *line, *col); }
                        v[idx as usize] = val;
                    }
                    Some(Val::Moved) => return trap("use of moved vector", *line, *col),
                    _ => return Flow::Trap("internal: strvec store on non-vec".to_owned()),
                }
            }
            Statement::VecFStore(id, idx_expr, val_expr, line, col) => {
                let idx = match eval_expr(env, idx_expr, locals, caller_locals, false) {
                    Ok(Val::Int(n)) => n,
                    Ok(_) => return Flow::Trap("internal: index not i64".to_owned()),
                    Err(f) => return f,
                };
                let val = match eval_expr(env, val_expr, locals, caller_locals, false) {
                    Ok(Val::Float(x)) => x,
                    Ok(_) => return Flow::Trap("internal: f64vec store type".to_owned()),
                    Err(f) => return f,
                };
                match locals.get_mut(id) {
                    Some(Val::VecF(v)) => {
                        if idx < 0 || (idx as usize) >= v.len() { return trap("vector index out of bounds", *line, *col); }
                        v[idx as usize] = val;
                    }
                    Some(Val::Moved) => return trap("use of moved vector", *line, *col),
                    _ => return Flow::Trap("internal: f64vec store on non-vec".to_owned()),
                }
            }
            Statement::Sort(id, is_str, line, col) => {
                match locals.get_mut(id) {
                    Some(Val::VecI(v)) if !*is_str => v.sort(),
                    Some(Val::VecS(v)) if *is_str => v.sort(),
                    Some(Val::Moved) => return trap("use of moved vector", *line, *col),
                    _ => return Flow::Trap("internal: sort target".to_owned()),
                }
            }
            Statement::Reverse(id, is_str, line, col) => {
                match locals.get_mut(id) {
                    Some(Val::VecI(v)) if !*is_str => v.reverse(),
                    Some(Val::VecS(v)) if *is_str => v.reverse(),
                    Some(Val::Moved) => return trap("use of moved vector", *line, *col),
                    _ => return Flow::Trap("internal: reverse target".to_owned()),
                }
            }
            Statement::Print(printed) => {
                match printed {
                    Printed::Text(t) => {
                        env.out.push_str(&unescape_str(t));
                        env.out.push('\n');
                    }
                    Printed::Expr(e) => {
                        match eval_expr(env, e, locals, caller_locals, false) {
                            Ok(v) => {
                                env.out.push_str(&format_val(&v));
                                env.out.push('\n');
                            }
                            Err(f) => return f,
                        }
                    }
                }
            }
            Statement::Assert(cond, line, col) => {
                match eval_expr(env, cond, locals, caller_locals, false) {
                    Ok(Val::Bool(true)) => {}
                    Ok(Val::Bool(false)) => return trap("assertion failed", *line, *col),
                    Ok(_) => return Flow::Trap("internal: assert non-bool".to_owned()),
                    Err(f) => return f,
                }
            }
            Statement::If(cond, yes, no) => {
                let b = match eval_expr(env, cond, locals, caller_locals, false) {
                    Ok(Val::Bool(b)) => b,
                    Ok(_) => return Flow::Trap("internal: if condition non-bool".to_owned()),
                    Err(f) => return f,
                };
                let flow = if b {
                    eval_stmts(env, yes, locals, caller_locals)
                } else {
                    eval_stmts(env, no, locals, caller_locals)
                };
                if !matches!(flow, Flow::Next) {
                    return flow;
                }
            }
            Statement::While(cond, body) => {
                loop {
                    let b = match eval_expr(env, cond, locals, caller_locals, false) {
                        Ok(Val::Bool(b)) => b,
                        Ok(_) => return Flow::Trap("internal: while condition non-bool".to_owned()),
                        Err(f) => return f,
                    };
                    if !b { break; }
                    match eval_stmts(env, body, locals, caller_locals) {
                        Flow::Next | Flow::Continue(0) => continue,
                        Flow::Continue(d) => return Flow::Continue(d - 1),
                        Flow::Break => break,
                        other => return other,
                    }
                }
            }
            Statement::For(var, end_var, body, _label, line, col) => {
                // The tree's counted loop: `counter` and `limit` are i64 slots
                // the parser declared before the loop, and a vec loop's
                // element binding sits at the top of the body, reading the
                // counter — so re-reading both slots each iteration is the
                // whole semantics. The hidden increment is the checked add at
                // the loop's position, exactly what the C backend emits.
                loop {
                    let counter = match read_var(locals, caller_locals, *var, *line, *col) {
                        Ok(Val::Int(n)) => n,
                        Ok(_) => return Flow::Trap("internal: for counter non-int".to_owned()),
                        Err(f) => return f,
                    };
                    let limit = match read_var(locals, caller_locals, *end_var, *line, *col) {
                        Ok(Val::Int(n)) => n,
                        Ok(_) => return Flow::Trap("internal: for limit non-int".to_owned()),
                        Err(f) => return f,
                    };
                    if counter >= limit { break; }
                    match eval_stmts(env, body, locals, caller_locals) {
                        Flow::Next | Flow::Continue(0) => {}
                        Flow::Continue(d) => return Flow::Continue(d - 1),
                        Flow::Break => break,
                        other => return other,
                    }
                    match counter.checked_add(1) {
                        Some(n) => { locals.insert(*var, Val::Int(n)); }
                        None => return trap("integer overflow", *line, *col),
                    }
                }
            }
            Statement::Match(subject, _, arms) => {
                let (vidx, payloads) = match read_var(locals, caller_locals, *subject, 0, 0) {
                    Ok(Val::Enum(_, vidx, payloads)) => (vidx, payloads),
                    Ok(_) => return Flow::Trap("internal: match subject not enum".to_owned()),
                    Err(f) => return f,
                };
                for arm in arms {
                    if arm.variant == vidx {
                        for (bind_id, _, field_idx) in &arm.bindings {
                            if let Some(p) = payloads.get(*field_idx) {
                                locals.insert(*bind_id, p.clone());
                            }
                        }
                        let flow = eval_stmts(env, &arm.body, locals, caller_locals);
                        if !matches!(flow, Flow::Next) {
                            return flow;
                        }
                        break;
                    }
                }
            }
            Statement::Return(expr, frees) => {
                let res = match eval_expr(env, expr, locals, caller_locals, true) {
                    Ok(v) => v,
                    Err(f) => return f,
                };
                for (id, ty) in frees {
                    if *ty == Type::TaskScope
                        && let Some(Val::TaskScope(eid)) = locals.get(id).cloned()
                            && let Err(f) = drain_exec_queue(env, eid) { return f; }
                }
                return Flow::Return(res);
            }
            Statement::Exit(expr, _) => {
                return match eval_expr(env, expr, locals, caller_locals, false) {
                    Ok(Val::Int(code)) => Flow::Exit((code & 0xFF) as u8),
                    Ok(_) => Flow::Exit(1),
                    Err(f) => f,
                };
            }
            // `?`: the success variant binds its payload and continues; the
            // failure variant propagates out of the function unchanged.
            Statement::Propagate(expr, _, _, binding, _) => {
                let value = match eval_expr(env, expr, locals, caller_locals, false) {
                Ok(v) => v,
                Err(f) => return f,
            };
                match value {
                    Val::Enum(eid, vidx, payloads) if vidx != 0 => {
                        return Flow::Return(Val::Enum(eid, vidx, payloads));
                    }
                    Val::Enum(_, _, payloads) => {
                        if let (Some((id, _)), Some(payload)) = (binding, payloads.first()) {
                            locals.insert(*id, payload.clone());
                        }
                    }
                    _ => return Flow::Trap("internal: propagate subject".to_owned()),
                }
            }
            Statement::Break => return Flow::Break,
            // A `continue` targets the innermost enclosing loop — the
            // statement's label is the C backend's naming bookkeeping, not a
            // targeting depth. The Flow::Continue(0) is what every loop arm
            // (for, while, for-tokens) catches as its own step.
            Statement::Continue(_) => return Flow::Continue(0),
            Statement::Frees(frees) => {
                for (id, ty) in frees {
                    if *ty == Type::TaskScope
                        && let Some(Val::TaskScope(eid)) = locals.get(id).cloned()
                            && let Err(f) = drain_exec_queue(env, eid) { return f; }
                }
            }
            // An `unsafe` block is a scoping container (M2.12 slice 1): the
            // inner statements evaluate inline and any control flow — a
            // return, a break, a continue — unwinds through it unchanged.
            Statement::Unsafe(inner) => {
                let flow = eval_stmts(env, inner, locals, caller_locals);
                if !matches!(flow, Flow::Next) { return flow; }
            }
            // `*p = value;` (M2.12 slice 3): the pointer variable evaluates to
            // the slot it names; the write replaces that slot's current value,
            // exactly what the compiled backends' `*ptr = value` does.
            // spec/concurrency.md §8: the store writes the shared cell.
            Statement::AtomicStore(handle, value) => {
                let cell = match eval_expr(env, handle, locals, caller_locals, false) {
                    Ok(Val::Atomic(cell)) => cell,
                    Ok(_) => return Flow::Trap("internal: cell_store on a non-handle".to_owned()),
                    Err(flow) => return flow,
                };
                let stored = match eval_expr(env, value, locals, caller_locals, false) {
                    Ok(Val::Int(n)) => n,
                    Ok(_) => return Flow::Trap("internal: cell_store value".to_owned()),
                    Err(flow) => return flow,
                };
                env.atomics[cell] = stored;
            }
            Statement::SharedSet(handle, value, _) => {
                let id = match eval_expr(env, handle, locals, caller_locals, false) {
                    Ok(Val::Shared(id)) => id,
                    Ok(_) => return Flow::Trap("internal: shared_set on a non-handle".to_owned()),
                    Err(flow) => return flow,
                };
                let stored = match eval_expr(env, value, locals, caller_locals, false) {
                    Ok(v) => v,
                    Err(flow) => return flow,
                };
                env.shared[id] = stored;
            }
            Statement::ChanClose(handle) => {
                let id = match eval_expr(env, handle, locals, caller_locals, false) {
                    Ok(Val::Chan(id)) => id,
                    Ok(_) => return Flow::Trap("internal: chan_close on a non-handle".to_owned()),
                    Err(flow) => return flow,
                };
                env.chans[id].2 = true;
            }
            Statement::TaskSpawn(ex, slots, _callable, cid, ..) | Statement::ScopeSpawn(ex, slots, _callable, cid, ..) | Statement::PoolSpawn(ex, slots, _callable, cid, ..) => {
                let eid = match eval_expr(env, ex, locals, caller_locals, false) {
                    Ok(Val::Exec(id)) | Ok(Val::TaskScope(id)) | Ok(Val::Pool(id)) => id,
                    Ok(_) => return Flow::Trap("internal: task/scope/pool spawn on a non-handle".to_owned()),
                    Err(flow) => return flow,
                };
                let mut cap_vals = Vec::with_capacity(slots.len());
                for slot in slots {
                    match eval_expr(env, slot, locals, caller_locals, true) {
                        Ok(v) => cap_vals.push(v),
                        Err(flow) => return flow,
                    }
                }
                let fn_id = env.program.ctx.closure_fn_id(*cid);
                let Some(func) = env.fns.get(&fn_id).copied() else {
                    return Flow::Trap(format!("internal: missing task closure fn #{fn_id}"));
                };
                let mut task_locals = HashMap::new();
                for ((pid, _), val) in func.params.iter().zip(cap_vals) {
                    task_locals.insert(*pid, val);
                }
                let state = TaskState {
                    locals: task_locals,
                    stack: vec![TaskFrame::Block(func.body.clone(), 0)],
                };
                env.execs[eid].push_back(state);
            }
            Statement::TaskYield => {}
            Statement::PtrStore(id, value, ..) => {
                let pointer = match eval_expr(env, &Expr::Variable(*id, Type::Int(IntTy::I64), 0, 0), locals, caller_locals, false) {
                    Ok(val) => val,
                    Err(flow) => return flow,
                };
                let target = match pointer {
                    Val::Ptr(slot) => slot,
                    _ => return Flow::Trap("internal: pointer write through a non-pointer".to_owned()),
                };
                let written = match eval_expr(env, value, locals, caller_locals, false) {
                    Ok(val) => val,
                    Err(flow) => return flow,
                };
                locals.insert(target, written);
            }
            _ => return Flow::Trap("unsupported statement in MIR interpreter".to_owned()),
        }
    }
    Flow::Next
}

fn read_var(
    locals: &HashMap<usize, Val>,
    caller_locals: &Option<&mut HashMap<usize, Val>>,
    id: usize,
    line: usize,
    col: usize,
) -> Result<Val, Flow> {
    match locals.get(&id) {
        Some(Val::Moved) => Err(trap("use of moved value", line, col)),
        Some(Val::Ref(target_id)) => {
            if let Some(caller) = caller_locals.as_ref()
                && let Some(v) = caller.get(target_id) {
                    return Ok(v.clone());
                }
            Err(Flow::Trap("internal: dangling reference in interpreter".to_owned()))
        }
        Some(v) => Ok(v.clone()),
        None => Err(Flow::Trap(format!("internal: uninitialized local v{id}"))),
    }
}

fn eval_expr(
    env: &mut Env<'_>,
    expr: &Expr,
    locals: &mut HashMap<usize, Val>,
    caller_locals: &mut Option<&mut HashMap<usize, Val>>,
    moving: bool,
) -> Result<Val, Flow> {
    match expr {
        Expr::Number(n) => Ok(Val::Int(*n)),
        Expr::FloatLit(x) => Ok(Val::Float(*x)),
        Expr::BoolLit(b) => Ok(Val::Bool(*b)),
        Expr::CharLit(c) => Ok(Val::Char(*c)),
        Expr::Unit => Ok(Val::Unit),
        Expr::StrLit(s, _, _) => Ok(Val::Str(unescape_str(s))),
        Expr::Variable(id, ty, line, col) => {
            let val = read_var(locals, caller_locals, *id, *line, *col)?;
            if moving && ty.owned() && !matches!(locals.get(id), Some(Val::Ref(_))) {
                locals.insert(*id, Val::Moved);
            }
            Ok(val)
        }
        Expr::Field(base, fname, _) => {
            let base_val = eval_expr(env, base, locals, caller_locals, false)?;
            match base_val {
                // A read only observes the bytes; a clone is the same value.
                Val::Struct(_, fields) => fields.get(fname).cloned().ok_or_else(|| Flow::Trap(format!("internal: missing field {fname}"))),
                // A &mut receiver is a reference: the field read goes
                // through to the lender's storage (the write path already
                // does the same through Statement::FieldStore).
                Val::Ref(target_id) => {
                    let Some(caller) = caller_locals.as_ref() else {
                        return Err(Flow::Trap("internal: dangling reference in interpreter".to_owned()));
                    };
                    match caller.get(&target_id) {
                        Some(Val::Struct(_, fields)) => fields.get(fname).cloned().ok_or_else(|| Flow::Trap(format!("internal: missing field {fname}"))),
                        _ => Err(Flow::Trap("internal: field access on non-struct".to_owned())),
                    }
                }
                _ => Err(Flow::Trap("internal: field access on non-struct".to_owned())),
            }
        }
        Expr::StructLit(sid, values) => {
            let def = env.program.structs.get(*sid).ok_or_else(|| Flow::Trap("internal: missing struct def".to_owned()))?.clone();
            let mut fields = HashMap::new();
            for ((fname, _), vexpr) in def.fields.iter().zip(values.iter()) {
                let v = eval_expr(env, vexpr, locals, caller_locals, true)?;
                fields.insert(fname.clone(), v);
            }
            Ok(Val::Struct(*sid, fields))
        }
        Expr::EnumLit(_, eid, vidx, values) => {
            let mut payloads = Vec::with_capacity(values.len());
            for vexpr in values {
                payloads.push(eval_expr(env, vexpr, locals, caller_locals, true)?);
            }
            Ok(Val::Enum(*eid, *vidx, payloads))
        }
        Expr::VecLit(values, ty, _, _) => {
            match ty {
                Type::Vec => {
                    let mut out = Vec::with_capacity(values.len());
                    for v in values {
                        if let Val::Int(n) = eval_expr(env, v, locals, caller_locals, false)? { out.push(n); }
                    }
                    Ok(Val::VecI(out))
                }
                Type::VecF => {
                    let mut out = Vec::with_capacity(values.len());
                    for v in values {
                        if let Val::Float(x) = eval_expr(env, v, locals, caller_locals, false)? { out.push(x); }
                    }
                    Ok(Val::VecF(out))
                }
                _ => Err(Flow::Trap("internal: unsupported vec literal type".to_owned())),
            }
        }
        Expr::StrVecNew(_, _) => Ok(Val::VecS(Vec::new())),
        Expr::F64VecNew(_, _) => Ok(Val::VecF(Vec::new())),
        Expr::Index(id, len, idx_expr, line, col) => {
            let idx = match eval_expr(env, idx_expr, locals, caller_locals, false)? {
                Val::Int(n) => n,
                _ => return Err(Flow::Trap("internal: index type".to_owned())),
            };
            if idx < 0 || (idx as usize) >= *len {
                return Err(trap("array index out of bounds", *line, *col));
            }
            match read_var(locals, caller_locals, *id, *line, *col)? {
                Val::Array(arr) => Ok(Val::Int(arr[idx as usize])),
                _ => Err(Flow::Trap("internal: array index on non-array".to_owned())),
            }
        }
        Expr::VecIndex(id, idx_expr, line, col, _, _) => {
            let idx = match eval_expr(env, idx_expr, locals, caller_locals, false)? {
                Val::Int(n) => n,
                _ => return Err(Flow::Trap("internal: vec index type".to_owned())),
            };
            match read_var(locals, caller_locals, *id, *line, *col)? {
                Val::VecI(v) => {
                    if idx < 0 || (idx as usize) >= v.len() { return Err(trap("vector index out of bounds", *line, *col)); }
                    Ok(Val::Int(v[idx as usize]))
                }
                Val::VecF(v) => {
                    if idx < 0 || (idx as usize) >= v.len() { return Err(trap("vector index out of bounds", *line, *col)); }
                    Ok(Val::Float(v[idx as usize]))
                }
                _ => Err(Flow::Trap("internal: vec index on non-vec".to_owned())),
            }
        }
        Expr::StrVecIndex(id, idx_expr, line, col, _) => {
            let idx = match eval_expr(env, idx_expr, locals, caller_locals, false)? {
                Val::Int(n) => n,
                _ => return Err(Flow::Trap("internal: strvec index type".to_owned())),
            };
            match read_var(locals, caller_locals, *id, *line, *col)? {
                Val::VecS(v) => {
                    if idx < 0 || (idx as usize) >= v.len() { return Err(trap("vector index out of bounds", *line, *col)); }
                    Ok(Val::Str(v[idx as usize].clone()))
                }
                _ => Err(Flow::Trap("internal: strvec index on non-vec".to_owned())),
            }
        }
        // `join` totals the length in the runtime and traps on overflow;
        // the interpreter's strings are heap-backed Rust strings, so the
        // overflow case cannot arise here (the fuzzer's seeds never
        // approach it) and the join is exact.
        Expr::Join(id, sep_expr, line, col) => {
            let sep = match eval_expr(env, sep_expr, locals, caller_locals, false)? {
                Val::Str(s) => s,
                _ => return Err(Flow::Trap("internal: join separator".to_owned())),
            };
            match read_var(locals, caller_locals, *id, *line, *col)? {
                Val::VecS(v) => Ok(Val::Str(v.join(&sep))),
                _ => Err(Flow::Trap("internal: join on non-strvec".to_owned())),
            }
        }
        Expr::Contains(haystack, needle, ..) => {
            let hay = match eval_expr(env, haystack, locals, caller_locals, false)? { Val::Str(s) => s, _ => return Err(Flow::Trap("internal: contains haystack".to_owned())) };
            let needle = match eval_expr(env, needle, locals, caller_locals, false)? { Val::Str(s) => s, _ => return Err(Flow::Trap("internal: contains needle".to_owned())) };
            Ok(Val::Bool(hay.contains(&needle)))
        }
        // to_i64 is an EXACT conversion: non-finite, out-of-range, and
        // fractional values are IntResult::Err with the runtime's exact
        // texts — no truncation.
        Expr::ToI64(inner, ty, ..) => {
            let value = eval_expr(env, inner, locals, caller_locals, false)?;
            match value {
                Val::Int(n) => {
                    let (eid, ok, _) = result_enum_of(env, "IntResult").ok_or_else(|| Flow::Trap("internal: result enum missing".to_owned()))?;
                    Ok(Val::Enum(eid, ok, vec![Val::Int(n)]))
                }
                Val::Float(x) => {
                    let (eid, ok, err) = result_enum_of(env, "IntResult").ok_or_else(|| Flow::Trap("internal: result enum missing".to_owned()))?;
                    if !x.is_finite() {
                        return Ok(Val::Enum(eid, err, vec![Val::Str("f64 value is not finite".to_owned())]));
                    }
                    if x < -9223372036854775808.0 || x >= 9223372036854775808.0 {
                        return Ok(Val::Enum(eid, err, vec![Val::Str("f64 value is outside the i64 range".to_owned())]));
                    }
                    let converted = x as i64;
                    if converted as f64 != x {
                        return Ok(Val::Enum(eid, err, vec![Val::Str("f64 value is not an exact integer".to_owned())]));
                    }
                    let _ = ty;
                    Ok(Val::Enum(eid, ok, vec![Val::Int(converted)]))
                }
                _ => Err(Flow::Trap("internal: to_i64 operand".to_owned())),
            }
        }
        Expr::Trim(inner, _, _) => {
            match eval_expr(env, inner, locals, caller_locals, false)? {
                Val::Str(s) => Ok(Val::Str(s.trim().to_owned())),
                _ => Err(Flow::Trap("internal: trim operand".to_owned())),
            }
        }
        Expr::StartsWith(a, b) | Expr::EndsWith(a, b) => {
            let is_start = matches!(expr, Expr::StartsWith(..));
            let hay = match eval_expr(env, a, locals, caller_locals, false)? { Val::Str(s) => s, _ => return Err(Flow::Trap("internal: starts/ends operand".to_owned())) };
            let prefix = match eval_expr(env, b, locals, caller_locals, false)? { Val::Str(s) => s, _ => return Err(Flow::Trap("internal: starts/ends operand".to_owned())) };
            Ok(Val::Bool(if is_start { hay.starts_with(&prefix) } else { hay.ends_with(&prefix) }))
        }
        // find: the first byte offset (an empty needle matches at 0).
        Expr::Find(text, pattern, ..) => {
            let hay = match eval_expr(env, text, locals, caller_locals, false)? {
                Val::Str(s) => s,
                _ => return Err(Flow::Trap("internal: find text".to_owned())),
            };
            let needle = match eval_expr(env, pattern, locals, caller_locals, false)? {
                Val::Str(s) => s,
                _ => return Err(Flow::Trap("internal: find pattern".to_owned())),
            };
            let (eid, ok, err) = result_enum_of(env, "IntResult").ok_or_else(|| Flow::Trap("internal: result enum missing".to_owned()))?;
            match hay.find(&needle) {
                Some(offset) => Ok(Val::Enum(eid, ok, vec![Val::Int(offset as i64)])),
                None => Ok(Val::Enum(eid, err, vec![Val::Str("substring not found".to_owned())])),
            }
        }
        Expr::Utf8Prefix(text, limit, ..) => {
            let s = match eval_expr(env, text, locals, caller_locals, false)? {
                Val::Str(v) => v,
                _ => return Err(Flow::Trap("internal: utf8_prefix text".to_owned())),
            };
            let n = match eval_expr(env, limit, locals, caller_locals, false)? {
                Val::Int(n) => n,
                _ => return Err(Flow::Trap("internal: utf8_prefix limit".to_owned())),
            };
            let out: String = s.chars().take(n.max(0) as usize).collect();
            Ok(Val::Str(out))
        }
        Expr::Utf8Len(inner, _, _) => {
            match eval_expr(env, inner, locals, caller_locals, false)? {
                Val::Str(s) => Ok(Val::Int(s.chars().count() as i64)),
                _ => Err(Flow::Trap("internal: utf8_len operand".to_owned())),
            }
        }
        Expr::Len(inner, line, col) => {
            match eval_expr(env, inner, locals, caller_locals, false)? {
                Val::Str(s) => Ok(Val::Int(s.len() as i64)),
                Val::VecI(v) => Ok(Val::Int(v.len() as i64)),
                Val::VecS(v) => Ok(Val::Int(v.len() as i64)),
                Val::VecF(v) => Ok(Val::Int(v.len() as i64)),
                Val::Array(v) => Ok(Val::Int(v.len() as i64)),
                Val::Map(m) => Ok(Val::Int(m.len() as i64)),
                Val::MapF(m) => Ok(Val::Int(m.len() as i64)),
                Val::Moved => Err(trap("use of moved value", *line, *col)),
                _ => Err(Flow::Trap("internal: len on unsupported value".to_owned())),
            }
        }
        Expr::Tokens(id, field, sep, ..) => {
            // The cursor copies the separator and reads through a view of
            // the base; the interp copies the base's current bytes, which
            // the borrow rules make equivalent (the base cannot move or be
            // reassigned while the cursor lives).
            let (base, sep) = {
                let sep = match eval_expr(env, sep, locals, caller_locals, false)? {
                    Val::Str(s) => s,
                    _ => return Err(Flow::Trap("internal: tokenize separator".to_owned())),
                };
                let base_val = read_var(locals, caller_locals, *id, 0, 0)?;
                let base = match (&base_val, field) {
                    (Val::Str(text), None) => text.clone(),
                    (Val::Struct(_, fields), Some(fname)) => match fields.get(fname) {
                        Some(Val::Str(text)) => text.clone(),
                        _ => return Err(Flow::Trap("internal: tokenize field".to_owned())),
                    },
                    _ => return Err(Flow::Trap("internal: tokenize base".to_owned())),
                };
                (base, sep)
            };
            Ok(Val::Tokens { base: Rc::from(base), sep: Rc::from(sep) })
        }
        // The string builtins the run corpus uses. The split/lines/slice
        // semantics mirror the runtime's exactly: split retains empty
        // segments and traps on an empty separator; lines keep internal
        // empties and drop the trailing newline's empty tail; slice checks
        // the UTF-8 boundaries before cutting.
        Expr::SliceView(base, field, start, end, line, col) => {
            // A view reads as the bytes it points at; the interp materialises
            // a copy, which the borrow rules make equivalent (the base
            // cannot move or be reassigned while the view lives).
            let bytes = match read_var(locals, caller_locals, *base, *line, *col)? {
                Val::Str(text) => text,
                Val::Struct(_, fields) => match field {
                    Some(fname) => match fields.get(fname) {
                        Some(Val::Str(text)) => text.clone(),
                        _ => return Err(Flow::Trap("internal: view field".to_owned())),
                    },
                    None => return Err(Flow::Trap("internal: view base".to_owned())),
                },
                _ => return Err(Flow::Trap("internal: view operand".to_owned())),
            };
            let start = match eval_expr(env, start, locals, caller_locals, false)? {
                Val::Int(n) => n,
                _ => return Err(Flow::Trap("internal: view start".to_owned())),
            };
            let end = match eval_expr(env, end, locals, caller_locals, false)? {
                Val::Int(n) => n,
                _ => return Err(Flow::Trap("internal: view end".to_owned())),
            };
            if start < 0 || end < start || (end as usize) > bytes.len() {
                return Err(trap("slice index out of bounds", *line, *col));
            }
            let (s, e) = (start as usize, end as usize);
            if !bytes.is_char_boundary(s) || !bytes.is_char_boundary(e) {
                return Err(trap("string index splits a UTF-8 character", *line, *col));
            }
            Ok(Val::Str(bytes[s..e].to_owned()))
        }
        // count_lines_with: the runtime's single-pass scan. The interp
        // counts lines containing the pattern with the same semantics
        // (empty pattern matches every line; empty text is zero).
        Expr::CountLinesWith(text, pattern, ..) => {
            let hay = match eval_expr(env, text, locals, caller_locals, false)? {
                Val::Str(s) => s,
                _ => return Err(Flow::Trap("internal: count_lines_with text".to_owned())),
            };
            let needle = match eval_expr(env, pattern, locals, caller_locals, false)? {
                Val::Str(s) => s,
                _ => return Err(Flow::Trap("internal: count_lines_with pattern".to_owned())),
            };
            if hay.is_empty() { return Ok(Val::Int(0)); }
            let mut count = 0i64;
            for line in hay.split(LF_CHAR) {
                if needle.is_empty() || line.contains(&needle) { count += 1; }
            }
            Ok(Val::Int(count))
        }
        Expr::Split(base, sep, line, col) => {
            let base = match eval_expr(env, base, locals, caller_locals, false)? {
                Val::Str(s) => s,
                _ => return Err(Flow::Trap("internal: split operand".to_owned())),
            };
            let sep = match eval_expr(env, sep, locals, caller_locals, false)? {
                Val::Str(s) => s,
                _ => return Err(Flow::Trap("internal: split separator".to_owned())),
            };
            if sep.is_empty() { return Err(trap("empty separator", *line, *col)); }
            Ok(Val::VecS(base.split(&sep).map(|part| part.to_owned()).collect()))
        }
        Expr::Lines(inner, _, _) => {
            match eval_expr(env, inner, locals, caller_locals, false)? {
                Val::Str(s) => {
                    if s.is_empty() { return Ok(Val::VecS(Vec::new())); }
                    let mut lines: Vec<String> = s.split(LF_CHAR).map(|part| part.to_owned()).collect();
                    if lines.last().map(|last| last.is_empty()).unwrap_or(false) { lines.pop(); }
                    Ok(Val::VecS(lines))
                }
                _ => Err(Flow::Trap("internal: lines operand".to_owned())),
            }
        }
        Expr::Slice(base, start, end, line, col) => {
            let base = match eval_expr(env, base, locals, caller_locals, false)? {
                Val::Str(s) => s,
                _ => return Err(Flow::Trap("internal: slice operand".to_owned())),
            };
            let start = match eval_expr(env, start, locals, caller_locals, false)? {
                Val::Int(n) => n,
                _ => return Err(Flow::Trap("internal: slice start".to_owned())),
            };
            let end = match eval_expr(env, end, locals, caller_locals, false)? {
                Val::Int(n) => n,
                _ => return Err(Flow::Trap("internal: slice end".to_owned())),
            };
            if start < 0 || end < start || (end as usize) > base.len() {
                return Err(trap("slice index out of bounds", *line, *col));
            }
            let (s, e) = (start as usize, end as usize);
            if !base.is_char_boundary(s) || !base.is_char_boundary(e) {
                return Err(trap("string index splits a UTF-8 character", *line, *col));
            }
            Ok(Val::Str(base[s..e].to_owned()))
        }
        // The file builtins. read_file traps (the runtime's ae_fail);
        // the try_* family wraps failures in their result enum, whose error
        // text differs from the C runtime's strerror wording — programs
        // cannot rely on it. Listings sort by bytes, file_info's mtime is
        // seconds since the Unix epoch, and write_file returns the byte
        // count.
        Expr::ReadFile(path, line, col) => {
            let Some(p) = path_string(eval_expr(env, path, locals, caller_locals, false)?) else {
                return Err(Flow::Trap("internal: path operand".to_owned()));
            };
            match std::fs::read_to_string(&p) {
                Ok(text) => Ok(Val::Str(text)),
                Err(_) => Err(trap("cannot read file", *line, *col)),
            }
        }
        Expr::TryReadFile(path, ty, ..) => {
            let p = path_operand(eval_expr(env, path, locals, caller_locals, false))?;
            match std::fs::read_to_string(&p) {
                Ok(text) => result_value(env, ty, true, vec![Val::Str(text)]),
                Err(error) => result_value(env, ty, false, vec![Val::Str(format!("cannot read file: {error}"))]),
            }
        }
        Expr::WriteFile(path, text, line, col) => {
            let p = path_operand(eval_expr(env, path, locals, caller_locals, false))?;
            let t = path_operand(eval_expr(env, text, locals, caller_locals, false))?;
            match std::fs::write(&p, &t) {
                Ok(()) => Ok(Val::Int(t.len() as i64)),
                Err(_) => Err(trap("cannot write file", *line, *col)),
            }
        }
        Expr::TryWriteFile(path, text, ty, ..) => {
            let p = path_operand(eval_expr(env, path, locals, caller_locals, false))?;
            let t = path_operand(eval_expr(env, text, locals, caller_locals, false))?;
            match std::fs::write(&p, &t) {
                Err(error) => result_value(env, ty, false, vec![Val::Str(format!("cannot write file: {error}"))]),
                Ok(()) => result_value(env, ty, true, vec![Val::Int(t.len() as i64)]),
            }
        }
        Expr::TryAppendFile(path, text, ty, ..) => {
            use std::io::Write as _;
            let p = path_operand(eval_expr(env, path, locals, caller_locals, false))?;
            let t = path_operand(eval_expr(env, text, locals, caller_locals, false))?;
            match std::fs::OpenOptions::new().create(true).append(true).open(&p) {
                Ok(mut file) => match file.write_all(t.as_bytes()) {
                    Ok(()) => result_value(env, ty, true, vec![Val::Int(t.len() as i64)]),
                    Err(error) => result_value(env, ty, false, vec![Val::Str(format!("cannot append file: {error}"))]),
                },
                Err(error) => result_value(env, ty, false, vec![Val::Str(format!("cannot append file: {error}"))]),
            }
        }
        Expr::TryListDir(path, ty, ..) => {
            let p = path_operand(eval_expr(env, path, locals, caller_locals, false))?;
            match std::fs::read_dir(&p) {
                Ok(entries) => {
                    let mut names: Vec<String> = entries.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect();
                    // Byte order, the same sort the runtime applies.
                    names.sort();
                    result_value(env, ty, true, vec![Val::VecS(names)])
                }
                Err(error) => result_value(env, ty, false, vec![Val::Str(format!("cannot list directory: {error}"))]),
            }
        }
        Expr::PathExists(path, ..) => {
            let p = path_operand(eval_expr(env, path, locals, caller_locals, false))?;
            Ok(Val::Bool(std::path::Path::new(&p).exists()))
        }
        Expr::PathIsDir(path, ..) => {
            let p = path_operand(eval_expr(env, path, locals, caller_locals, false))?;
            Ok(Val::Bool(std::path::Path::new(&p).is_dir()))
        }
        Expr::TryFileInfo(path, ty, ..) => {
            let p = path_operand(eval_expr(env, path, locals, caller_locals, false))?;
            match std::fs::metadata(&p) {
                Ok(meta) => {
                    let size = meta.len() as i64;
                    let modified = meta.modified().ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    let is_dir = meta.is_dir();
                    result_value(env, ty, true, vec![Val::Int(size), Val::Int(modified), Val::Bool(is_dir)])
                }
                Err(error) => result_value(env, ty, false, vec![Val::Str(format!("cannot read file metadata: {error}"))]),
            }
        }
        Expr::TryCopyFile(from, to, ty, ..) => {
            let from = path_operand(eval_expr(env, from, locals, caller_locals, false))?;
            let to = path_operand(eval_expr(env, to, locals, caller_locals, false))?;
            match std::fs::copy(&from, &to) {
                Ok(_) => result_value(env, ty, true, vec![]),
                Err(error) => result_value(env, ty, false, vec![Val::Str(format!("cannot copy file: {error}"))]),
            }
        }
        Expr::TryRenamePath(from, to, ty, ..) => {
            let from = path_operand(eval_expr(env, from, locals, caller_locals, false))?;
            let to = path_operand(eval_expr(env, to, locals, caller_locals, false))?;
            match std::fs::rename(&from, &to) {
                Ok(()) => result_value(env, ty, true, vec![]),
                Err(error) => result_value(env, ty, false, vec![Val::Str(format!("cannot rename path: {error}"))]),
            }
        }
        Expr::TryRemoveFile(path, ty, ..) => {
            let p = path_operand(eval_expr(env, path, locals, caller_locals, false))?;
            match std::fs::remove_file(&p) {
                Ok(()) => result_value(env, ty, true, vec![]),
                Err(error) => result_value(env, ty, false, vec![Val::Str(format!("cannot remove file: {error}"))]),
            }
        }
        Expr::TryRemoveDir(path, ty, ..) => {
            let p = path_operand(eval_expr(env, path, locals, caller_locals, false))?;
            match std::fs::remove_dir(&p) {
                Ok(()) => result_value(env, ty, true, vec![]),
                Err(error) => result_value(env, ty, false, vec![Val::Str(format!("cannot remove directory: {error}"))]),
            }
        }
        Expr::TryCreateDir(path, ty, ..) => {
            let p = path_operand(eval_expr(env, path, locals, caller_locals, false))?;
            // The runtime creates every missing level of the path.
            match std::fs::create_dir_all(&p) {
                Ok(()) => result_value(env, ty, true, vec![]),
                Err(error) => result_value(env, ty, false, vec![Val::Str(format!("cannot create directory: {error}"))]),
            }
        }
        Expr::SetCwd(path, ty, ..) => {
            let p = path_operand(eval_expr(env, path, locals, caller_locals, false))?;
            match std::env::set_current_dir(&p) {
                Ok(()) => result_value(env, ty, true, vec![]),
                Err(error) => result_value(env, ty, false, vec![Val::Str(format!("cannot set cwd: {error}"))]),
            }
        }
        Expr::PathJoin(dir, name, ..) => {
            let d = path_operand(eval_expr(env, dir, locals, caller_locals, false))?;
            let n = path_operand(eval_expr(env, name, locals, caller_locals, false))?;
            // The runtime's join: an empty side returns the other, an
            // absolute name wins, and a separator is inserted only when the
            // directory does not already end with one.
            if n.is_empty() { return Ok(Val::Str(d)); }
            if d.is_empty() { return Ok(Val::Str(n)); }
            if path_is_absolute(&n) { return Ok(Val::Str(n)); }
            let separated = d.chars().last().map(is_separator).unwrap_or(false);
            if separated { return Ok(Val::Str(format!("{d}{n}"))); }
            Ok(Val::Str(format!("{d}/{n}")))
        }
        Expr::PathName(path, ..) => Ok(Val::Str(path_name(&path_operand(eval_expr(env, path, locals, caller_locals, false))?))),
        Expr::PathParent(path, ..) => Ok(Val::Str(path_parent(&path_operand(eval_expr(env, path, locals, caller_locals, false))?))),
        Expr::Args(..) => Ok(Val::VecS(std::env::args().collect())),
        // now_s is now_ms in whole seconds.
        Expr::NowS => {
            let millis = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            Ok(Val::Int(millis / 1000))
        }
        Expr::NowMs => {
            let millis = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            Ok(Val::Int(millis))
        }
        // The CommandResult enum comes from the name table, so the carried
        // result type is redundant here.
        Expr::RunCommand(program, vid, ..) => {
            // The runtime spawns the program with the vector's elements as
            // argv and captures stdout plus the exit code into CommandResult
            // (Ok(code, output), Err(output-on-failure-to-spawn)). The
            // interp spawns through std::process with the same shape.
            let program = path_operand(eval_expr(env, program, locals, caller_locals, false))?;
            let args: Vec<String> = match read_var(locals, caller_locals, *vid, 0, 0)? {
                Val::VecS(v) => v,
                _ => return Err(Flow::Trap("internal: run_command argv".to_owned())),
            };
            let (eid, ok, err) = result_enum_of(env, "CommandResult").ok_or_else(|| Flow::Trap("internal: result enum missing".to_owned()))?;
            let output = std::process::Command::new(&program).args(&args).output();
            match output {
                Ok(out) => {
                    if out.status.success() {
                        Ok(Val::Enum(eid, ok, vec![Val::Int(out.status.code().unwrap_or(0) as i64), Val::Str(String::from_utf8_lossy(&out.stdout).to_string())]))
                    } else {
                        Ok(Val::Enum(eid, err, vec![Val::Str(String::from_utf8_lossy(&out.stdout).to_string())]))
                    }
                }
                Err(error) => Ok(Val::Enum(eid, err, vec![Val::Str(format!("cannot run program: {error}"))])),
            }
        }
        Expr::Env(variable, ty, ..) => {
            // ae_try_env: found -> Ok(value), missing -> Err(the same
            // read-only constant family the runtime hands back).
            let name = path_operand(eval_expr(env, variable, locals, caller_locals, false))?;
            match std::env::var(&name) {
                Ok(value) => result_value(env, ty, true, vec![Val::Str(value)]),
                Err(std::env::VarError::NotPresent) => result_value(env, ty, false, vec![Val::Str("environment variable not found".to_owned())]),
                Err(error) => result_value(env, ty, false, vec![Val::Str(format!("environment variable: {error}"))]),
            }
        }
        Expr::ReadStdin(ty, ..) => {
            let mut line = String::new();
            let read = std::io::stdin().read_line(&mut line);
            match read {
                Ok(0) => result_value(env, ty, false, vec![Val::Str("end of input".to_owned())]),
                Ok(_) => result_value(env, ty, true, vec![Val::Str(line.trim_end_matches(['\r', '\n']).to_owned())]),
                Err(error) => result_value(env, ty, false, vec![Val::Str(format!("cannot read stdin: {error}"))]),
            }
        }
        Expr::Cwd(ty, ..) => {
            match std::env::current_dir() {
                Ok(dir) => result_value(env, ty, true, vec![Val::Str(dir.to_string_lossy().to_string())]),
                Err(error) => result_value(env, ty, false, vec![Val::Str(format!("cannot read the working directory: {error}"))]),
            }
        }
        // json_get: strings decode escapes; numbers, true/false/null, and
        // containers come back as their raw slices. A missing segment is
        // Err("key not found") — the runtime's constant.
        Expr::JsonGet(text, path, ..) => {
            let doc = match eval_expr(env, text, locals, caller_locals, false)? {
                Val::Str(v) => v,
                _ => return Err(Flow::Trap("internal: json text".to_owned())),
            };
            let path = match eval_expr(env, path, locals, caller_locals, false)? {
                Val::Str(v) => v,
                _ => return Err(Flow::Trap("internal: json path".to_owned())),
            };
            let (eid, ok, err) = result_enum_of(env, "ReadResult").ok_or_else(|| Flow::Trap("internal: result enum missing".to_owned()))?;
            match json_locate(&doc, &path) {
                Ok(raw) => {
                    let decoded = if raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2 {
                        json_decode_string(&raw[1..raw.len() - 1])
                    } else {
                        raw
                    };
                    Ok(Val::Enum(eid, ok, vec![Val::Str(decoded)]))
                }
                Err(why) => Ok(Val::Enum(eid, err, vec![Val::Str(why)])),
            }
        }
        // json_bool: only true/false are accepted; anything else is Err.
        Expr::JsonBool(text, path, ..) => {
            let doc = match eval_expr(env, text, locals, caller_locals, false) {
                Ok(Val::Str(v)) => v,
                Ok(_) => return Err(Flow::Trap("internal: json text".to_owned())),
                Err(f) => return Err(f),
            };
            let path = match eval_expr(env, path, locals, caller_locals, false) {
                Ok(Val::Str(v)) => v,
                Ok(_) => return Err(Flow::Trap("internal: json path".to_owned())),
                Err(f) => return Err(f),
            };
            let (eid, ok, err) = result_enum_of(env, "BoolResult").ok_or_else(|| Flow::Trap("internal: result enum missing".to_owned()))?;
            match json_locate(&doc, &path) {
                Err(why) => Ok(Val::Enum(eid, err, vec![Val::Str(why)])),
                Ok(raw) => {
                    let kind = match raw.trim().as_bytes().first() {
                        Some(b'"') => "a string",
                        Some(b'{') => "an object",
                        Some(b'[') => "an array",
                        Some(b't') | Some(b'f') => "a boolean",
                        Some(b'n') => "null",
                        Some(_) => "a number",
                        None => "nothing",
                    };
                    if kind == "a boolean" {
                        Ok(Val::Enum(eid, ok, vec![Val::Bool(raw.trim() == "true")]))
                    } else {
                        Ok(Val::Enum(eid, err, vec![Val::Str(format!("'{}' is {}, not a boolean", path, kind))]))
                    }
                }
            }
        }
        // json_len: the element or member count of the selected container.
        // json_has: existence only — null counts as existent, and a failed
        // locate is simply false (a plain bool, no result enum).
        Expr::JsonHas(text, path, ..) => {
            let doc = match eval_expr(env, text, locals, caller_locals, false) {
                Ok(Val::Str(v)) => v,
                Ok(_) => return Err(Flow::Trap("internal: json text".to_owned())),
                Err(f) => return Err(f),
            };
            let path = match eval_expr(env, path, locals, caller_locals, false) {
                Ok(Val::Str(v)) => v,
                Ok(_) => return Err(Flow::Trap("internal: json path".to_owned())),
                Err(f) => return Err(f),
            };
            Ok(Val::Bool(json_locate(&doc, &path).is_ok()))
        }
        Expr::JsonLen(text, path, ..) => {
            let doc = match eval_expr(env, text, locals, caller_locals, false) {
                Ok(Val::Str(v)) => v,
                Ok(_) => return Err(Flow::Trap("internal: json text".to_owned())),
                Err(f) => return Err(f),
            };
            let path = match eval_expr(env, path, locals, caller_locals, false) {
                Ok(Val::Str(v)) => v,
                Ok(_) => return Err(Flow::Trap("internal: json path".to_owned())),
                Err(f) => return Err(f),
            };
            let (eid, ok, err) = result_enum_of(env, "IntResult").ok_or_else(|| Flow::Trap("internal: result enum missing".to_owned()))?;
            match json_locate(&doc, &path) {
                Err(why) => Ok(Val::Enum(eid, err, vec![Val::Str(why)])),
                Ok(raw) => {
                    let bytes = raw.as_bytes();
                    let start = json_skip_ws(bytes, 0);
                    let count = match bytes.get(start) {
                        Some(b'[') => json_array_elements(bytes, start).len() as i64,
                        Some(b'{') => json_object_members(bytes, start).len() as i64,
                        _ => -1,
                    };
                    if count < 0 {
                        return Ok(Val::Enum(eid, err, vec![Val::Str(format!("'{}' is {}; only arrays and objects have a member count", path, "a value"))]));
                    }
                    Ok(Val::Enum(eid, ok, vec![Val::Int(count)]))
                }
            }
        }
        // json_int: the selected value must be a strictly pure integer —
        // decimals or exponents are errors, as is i64 overflow.
        Expr::JsonInt(text, path, ..) => {
            let doc = match eval_expr(env, text, locals, caller_locals, false)? {
                Val::Str(v) => v,
                _ => return Err(Flow::Trap("internal: json text".to_owned())),
            };
            let path = match eval_expr(env, path, locals, caller_locals, false)? {
                Val::Str(v) => v,
                _ => return Err(Flow::Trap("internal: json path".to_owned())),
            };
            let (eid, ok, err) = result_enum_of(env, "IntResult").ok_or_else(|| Flow::Trap("internal: result enum missing".to_owned()))?;
            match json_locate(&doc, &path) {
                Err(why) => Ok(Val::Enum(eid, err, vec![Val::Str(why)])),
                Ok(raw) => {
                    let kind = match raw.trim().as_bytes().first() {
                        Some(b'"') => "a string",
                        Some(b'{') => "an object",
                        Some(b'[') => "an array",
                        Some(b't') | Some(b'f') => "a boolean",
                        Some(b'n') => "null",
                        Some(_) => "a number",
                        None => "nothing",
                    };
                    if kind != "a number" {
                        return Ok(Val::Enum(eid, err, vec![Val::Str(format!("'{}' is {}, not a number", path, kind))]));
                    }
                    let trimmed = raw.trim();
                    if trimmed.contains('.') || trimmed.contains('e') || trimmed.contains('E') {
                        return Ok(Val::Enum(eid, err, vec![Val::Str(format!("'{}' is not an integer", path))]));
                    }
                    match trimmed.parse::<i64>() {
                        Ok(n) => Ok(Val::Enum(eid, ok, vec![Val::Int(n)])),
                        Err(_) => Ok(Val::Enum(eid, err, vec![Val::Str(format!("'{}' does not fit in i64", path))])),
                    }
                }
            }
        }
        Expr::MapNew(_, _, kind) => match kind {
            MapKind::Int => Ok(Val::Map(HashMap::new())),
            MapKind::Float => Ok(Val::MapF(HashMap::new())),
        },
        // The map builtins the corpus uses: has/remove are predicates, inc
        // inserts-or-increments and returns the new value, keys returns the
        // key set in byte order — the same order the runtime sorts into.
        Expr::MapHas(place, key, ..) => {
            let key = match eval_expr(env, key, locals, caller_locals, false)? {
                Val::Str(k) => k,
                _ => return Err(Flow::Trap("internal: map key type".to_owned())),
            };
            match map_val_ref(place, locals, caller_locals)? {
                MapVal::MapI(m) => Ok(Val::Bool(m.contains_key(&key))),
                MapVal::MapF(m) => Ok(Val::Bool(m.contains_key(&key))),
                _ => Err(Flow::Trap("internal: map has on non-map".to_owned())),
            }
        }
        Expr::MapRemove(place, key, ..) => {
            let key = match eval_expr(env, key, locals, caller_locals, false)? {
                Val::Str(k) => k,
                _ => return Err(Flow::Trap("internal: map key type".to_owned())),
            };
            match map_val_ref(place, locals, caller_locals)? {
                MapVal::MapI(m) => Ok(Val::Bool(m.remove(&key).is_some())),
                MapVal::MapF(m) => Ok(Val::Bool(m.remove(&key).is_some())),
                _ => Err(Flow::Trap("internal: map remove on non-map".to_owned())),
            }
        }
        Expr::MapGet(place, key, ty, ..) => {
            // A hit is Ok(value); a miss is Err("key not found") — the same
            // read-only constant the runtime hands back, zero-allocation.
            let key = match eval_expr(env, key, locals, caller_locals, false)? {
                Val::Str(k) => k,
                _ => return Err(Flow::Trap("internal: map key type".to_owned())),
            };
            let float_result = matches!(ty, Type::Enum(name, _) if *name == "FloatResult");
            let (result_name, ok_value) = match map_val_ref(place, locals, caller_locals)? {
                MapVal::MapI(m) => ("IntResult", Val::Int(*m.get(&key).unwrap_or(&0))),
                MapVal::MapF(m) => ("FloatResult", Val::Float(*m.get(&key).unwrap_or(&0.0))),
                _ => return Err(Flow::Trap("internal: map get on non-map".to_owned())),
            };
            let _ = float_result;
            let (eid, ok, err) = result_enum_of(env, result_name).ok_or_else(|| Flow::Trap("internal: result enum missing".to_owned()))?;
            if m_contains_ref(place, locals, caller_locals, &key) {
                Ok(Val::Enum(eid, ok, vec![ok_value]))
            } else {
                let _ = err;
                Ok(Val::Enum(eid, err, vec![Val::Str("key not found".to_owned())]))
            }
        }
        Expr::ParseInt(inner, ty, line, col) => {
            // The runtime's parser: optional sign, base-10 digits, the i64
            // range, and the same three error strings.
            let text = match eval_expr(env, inner, locals, caller_locals, false)? {
                Val::Str(s) => s,
                _ => return Err(Flow::Trap("internal: parse_int operand".to_owned())),
            };
            let (eid, ok, err) = result_enum_of(env, "IntResult").ok_or_else(|| Flow::Trap("internal: result enum missing".to_owned()))?;
            let _ = ty;
            let mut at = 0usize;
            let bytes = text.as_bytes();
            let mut negative = false;
            if !bytes.is_empty() && (bytes[0] == b'-' || bytes[0] == b'+') {
                negative = bytes[0] == b'-';
                at = 1;
            }
            if at >= bytes.len() {
                let payload = "empty or sign-only integer";
                let (_, e, _) = (eid, err, ok);
                return Ok(Val::Enum(eid, e, vec![Val::Str(payload.to_owned())]));
            }
            let mut magnitude: u64 = 0;
            let limit: u64 = if negative { (u64::MAX - 1) / 2 + 1 } else { i64::MAX as u64 };
            for &byte in &bytes[at..] {
                if !byte.is_ascii_digit() {
                    return Ok(Val::Enum(eid, err, vec![Val::Str("not a base-10 integer".to_owned())]));
                }
                let digit = (byte - b'0') as u64;
                if magnitude > (limit - digit) / 10 {
                    return Ok(Val::Enum(eid, err, vec![Val::Str("integer out of i64 range".to_owned())]));
                }
                magnitude = magnitude * 10 + digit;
            }
            let _ = line;
            let _ = col;
            let value = if negative {
                if magnitude > (i64::MAX as u64) + 1 { return Ok(Val::Enum(eid, err, vec![Val::Str("integer out of i64 range".to_owned())])); }
                (0u64).wrapping_sub(magnitude) as i64
            } else {
                if magnitude > i64::MAX as u64 { return Ok(Val::Enum(eid, err, vec![Val::Str("integer out of i64 range".to_owned())])); }
                magnitude as i64
            };
            Ok(Val::Enum(eid, ok, vec![Val::Int(value)]))
        }
        Expr::MapInc(place, key, delta, ..) => {
            // Both operands evaluate before the map resolves: the map borrow
            // lasts to the end of the arm, and the operand evaluation needs
            // the same locals.
            let key = match eval_expr(env, key, locals, caller_locals, false)? {
                Val::Str(k) => k,
                _ => return Err(Flow::Trap("internal: map key type".to_owned())),
            };
            let int_delta = match eval_expr(env, delta, locals, caller_locals, false)? {
                Val::Int(n) => Some(n),
                Val::Float(_) => None,
                _ => return Err(Flow::Trap("internal: map inc type".to_owned())),
            };
            let float_delta = match eval_expr(env, delta, locals, caller_locals, false)? {
                Val::Float(x) => Some(x),
                _ => None,
            };
            match map_val_ref(place, locals, caller_locals)? {
                MapVal::MapI(m) => {
                    let Some(n) = int_delta else { return Err(Flow::Trap("internal: map inc type".to_owned())) };
                    let next = m.entry(key).or_insert(0).checked_add(n).ok_or_else(|| trap("integer overflow", 0, 0))?;
                    Ok(Val::Int(next))
                }
                MapVal::MapF(m) => {
                    let Some(x) = float_delta else { return Err(Flow::Trap("internal: map inc type".to_owned())) };
                    let entry = m.entry(key).or_insert(0.0);
                    *entry += x;
                    Ok(Val::Float(*entry))
                }
                _ => Err(Flow::Trap("internal: map inc on non-map".to_owned())),
            }
        }
        Expr::MapKeys(place, ..) => {
            match map_val_ref(place, locals, caller_locals)? {
                MapVal::MapI(m) => {
                    let mut keys: Vec<String> = m.keys().cloned().collect();
                    keys.sort();
                    Ok(Val::VecS(keys))
                }
                MapVal::MapF(m) => {
                    let mut keys: Vec<String> = m.keys().cloned().collect();
                    keys.sort();
                    Ok(Val::VecS(keys))
                }
                _ => Err(Flow::Trap("internal: keys on non-map".to_owned())),
            }
        }
        // The math builtins are libm calls in the C; the interp computes
        // the same f64 operations. min/max are the language's documented
        // NaN-aware total order (ae_f64_min/max), not f64::min/max.
        Expr::MathCall(index, args, ..) => {
            let name = super::MATH_FNS[*index as usize].0;
            let mut values: Vec<f64> = Vec::new();
            for arg in args {
                match eval_expr(env, arg, locals, caller_locals, false)? {
                    Val::Float(x) => values.push(x),
                    _ => return Err(Flow::Trap("internal: math operand".to_owned())),
                }
            }
            let x = values.first().copied().unwrap_or(f64::NAN);
            let y = values.get(1).copied().unwrap_or(f64::NAN);
            let out = match name {
                "sqrt" => x.sqrt(),
                "abs" => x.abs(),
                "floor" => x.floor(),
                "ceil" => x.ceil(),
                "round" => x.round(),
                "exp" => x.exp(),
                "log" => x.ln(),
                "sin" => x.sin(),
                "cos" => x.cos(),
                "pow" => x.powf(y),
                "atan2" => x.atan2(y),
                "min" => {
                    // NaN-aware: the non-NaN side wins; both NaN -> NaN.
                    if x.is_nan() { y } else if y.is_nan() || x < y { x } else { y }
                }
                "max" => {
                    if x.is_nan() { y } else if y.is_nan() || x > y { x } else { y }
                }
                _ => return Err(Flow::Trap("internal: math function".to_owned())),
            };
            Ok(Val::Float(out))
        }
        Expr::Neg(inner, line, col) => {
            match eval_expr(env, inner, locals, caller_locals, false)? {
                Val::Int(n) => n.checked_neg().map(Val::Int).ok_or_else(|| trap("integer overflow", *line, *col)),
                Val::I8(n) => n.checked_neg().map(Val::I8).ok_or_else(|| trap("integer overflow", *line, *col)),
                Val::I16(n) => n.checked_neg().map(Val::I16).ok_or_else(|| trap("integer overflow", *line, *col)),
                Val::I32(n) => n.checked_neg().map(Val::I32).ok_or_else(|| trap("integer overflow", *line, *col)),
                Val::Isize(n) => n.checked_neg().map(Val::Isize).ok_or_else(|| trap("integer overflow", *line, *col)),
                Val::F32(x) => Ok(Val::F32(-x)),
                Val::Float(x) => Ok(Val::Float(-x)),
                _ => Err(Flow::Trap("internal: neg operand".to_owned())),
            }
        }
        Expr::Not(inner) => {
            match eval_expr(env, inner, locals, caller_locals, false)? {
                Val::Bool(b) => Ok(Val::Bool(!b)),
                _ => Err(Flow::Trap("internal: not operand".to_owned())),
            }
        }
        Expr::StrEq(a, b, negated) => {
            let Val::Str(sa) = eval_expr(env, a, locals, caller_locals, false)? else { return Err(Flow::Trap("internal: streq".to_owned())); };
            let Val::Str(sb) = eval_expr(env, b, locals, caller_locals, false)? else { return Err(Flow::Trap("internal: streq".to_owned())); };
            Ok(Val::Bool(if *negated { sa != sb } else { sa == sb }))
        }
        Expr::ToString(inner, _, _) | Expr::FloatToString(inner, _, _) => {
            let v = eval_expr(env, inner, locals, caller_locals, false)?;
            Ok(Val::Str(format_val(&v)))
        }
        Expr::ScalarCast(inner, from, to, line, col) => {
            let value = eval_expr(env, inner, locals, caller_locals, false)?;
            eval_scalar_cast(from, to, value, *line, *col)
        }
        Expr::IntMethod(kind, receiver, arg, result_ty, _, _) => {
            let recv = eval_expr(env, receiver, locals, caller_locals, false)?;
            let rhs = match arg {
                Some(a) => Some(eval_expr(env, a, locals, caller_locals, false)?),
                None => None,
            };
            dispatch_int_method(*kind, recv, rhs, result_ty)
        }
        Expr::ToF64(inner, line, col) => {
            let Val::Int(n) = eval_expr(env, inner, locals, caller_locals, false)? else { return Err(Flow::Trap("internal: to_f64".to_owned())); };
            if !(-9007199254740992..=9007199254740992).contains(&n) {
                return Err(trap("i64 to f64 conversion would lose precision", *line, *col));
            }
            Ok(Val::Float(n as f64))
        }
        Expr::Binary(a, op, b, line, col, _) => {
            if *op == Kind::And {
                let Val::Bool(la) = eval_expr(env, a, locals, caller_locals, false)? else { return Err(Flow::Trap("internal: &&".to_owned())); };
                if !la { return Ok(Val::Bool(false)); }
                return eval_expr(env, b, locals, caller_locals, false);
            }
            if *op == Kind::Or {
                let Val::Bool(la) = eval_expr(env, a, locals, caller_locals, false)? else { return Err(Flow::Trap("internal: ||".to_owned())); };
                if la { return Ok(Val::Bool(true)); }
                return eval_expr(env, b, locals, caller_locals, false);
            }
            let va = eval_expr(env, a, locals, caller_locals, false)?;
            let vb = eval_expr(env, b, locals, caller_locals, false)?;
            match (va, vb) {
                (Val::Int(x), Val::Int(y)) => match op {
                    Kind::Plus => x.checked_add(y).map(Val::Int).ok_or_else(|| trap("integer overflow", *line, *col)),
                    Kind::Minus => x.checked_sub(y).map(Val::Int).ok_or_else(|| trap("integer overflow", *line, *col)),
                    Kind::Star => x.checked_mul(y).map(Val::Int).ok_or_else(|| trap("integer overflow", *line, *col)),
                    Kind::Slash => {
                        if y == 0 { return Err(trap("division by zero", *line, *col)); }
                        x.checked_div(y).map(Val::Int).ok_or_else(|| trap("integer overflow", *line, *col))
                    }
                    Kind::Pipe => Ok(Val::Int(x | y)),
                    Kind::Caret => Ok(Val::Int(x ^ y)),
                    Kind::Amp => Ok(Val::Int(x & y)),
                    Kind::Shl => Ok(Val::Int(x.wrapping_shl((y as u32) & 63))),
                    Kind::Shr => Ok(Val::Int(x.wrapping_shr((y as u32) & 63))),
                    Kind::Percent => {
                        if y == 0 { return Err(trap("division by zero", *line, *col)); }
                        x.checked_rem(y).map(Val::Int).ok_or_else(|| trap("integer overflow", *line, *col))
                    }
                    Kind::EqEq => Ok(Val::Bool(x == y)),
                    Kind::NotEq => Ok(Val::Bool(x != y)),
                    Kind::Less => Ok(Val::Bool(x < y)),
                    Kind::LessEq => Ok(Val::Bool(x <= y)),
                    Kind::Greater => Ok(Val::Bool(x > y)),
                    Kind::GreaterEq => Ok(Val::Bool(x >= y)),
                    _ => Err(Flow::Trap("internal: int op".to_owned())),
                },
                (Val::I8(x), Val::I8(y)) => eval_width_binop(x, y, op, *line, *col),
                (Val::I16(x), Val::I16(y)) => eval_width_binop(x, y, op, *line, *col),
                (Val::I32(x), Val::I32(y)) => eval_width_binop(x, y, op, *line, *col),
                (Val::Isize(x), Val::Isize(y)) => eval_width_binop(x, y, op, *line, *col),
                (Val::U8(x), Val::U8(y)) => eval_width_binop(x, y, op, *line, *col),
                (Val::U16(x), Val::U16(y)) => eval_width_binop(x, y, op, *line, *col),
                (Val::U32(x), Val::U32(y)) => eval_width_binop(x, y, op, *line, *col),
                (Val::U64(x), Val::U64(y)) => eval_width_binop(x, y, op, *line, *col),
                (Val::Usize(x), Val::Usize(y)) => eval_width_binop(x, y, op, *line, *col),
                (Val::F32(x), Val::F32(y)) => match op {
                    Kind::Plus => Ok(Val::F32(x + y)),
                    Kind::Minus => Ok(Val::F32(x - y)),
                    Kind::Star => Ok(Val::F32(x * y)),
                    Kind::Slash => {
                        if y == 0.0 { return Err(trap("f32 division by zero", *line, *col)); }
                        Ok(Val::F32(x / y))
                    }
                    Kind::EqEq => Ok(Val::Bool(x == y)),
                    Kind::NotEq => Ok(Val::Bool(x != y)),
                    Kind::Less => Ok(Val::Bool(x < y)),
                    Kind::LessEq => Ok(Val::Bool(x <= y)),
                    Kind::Greater => Ok(Val::Bool(x > y)),
                    Kind::GreaterEq => Ok(Val::Bool(x >= y)),
                    _ => Err(Flow::Trap("internal: float op".to_owned())),
                },
                (Val::Float(x), Val::Float(y)) => match op {
                    Kind::Plus => Ok(Val::Float(x + y)),
                    Kind::Minus => Ok(Val::Float(x - y)),
                    Kind::Star => Ok(Val::Float(x * y)),
                    Kind::Slash => {
                        if y == 0.0 { return Err(trap("f64 division by zero", *line, *col)); }
                        Ok(Val::Float(x / y))
                    }
                    Kind::EqEq => Ok(Val::Bool(x == y)),
                    Kind::NotEq => Ok(Val::Bool(x != y)),
                    Kind::Less => Ok(Val::Bool(x < y)),
                    Kind::LessEq => Ok(Val::Bool(x <= y)),
                    Kind::Greater => Ok(Val::Bool(x > y)),
                    Kind::GreaterEq => Ok(Val::Bool(x >= y)),
                    _ => Err(Flow::Trap("internal: float op".to_owned())),
                },
                (Val::Bool(x), Val::Bool(y)) => match op {
                    Kind::EqEq => Ok(Val::Bool(x == y)),
                    Kind::NotEq => Ok(Val::Bool(x != y)),
                    _ => Err(Flow::Trap("internal: bool op".to_owned())),
                },
                (Val::Char(x), Val::Char(y)) => match op {
                    Kind::EqEq => Ok(Val::Bool(x == y)),
                    Kind::NotEq => Ok(Val::Bool(x != y)),
                    Kind::Less => Ok(Val::Bool(x < y)),
                    Kind::LessEq => Ok(Val::Bool(x <= y)),
                    Kind::Greater => Ok(Val::Bool(x > y)),
                    Kind::GreaterEq => Ok(Val::Bool(x >= y)),
                    _ => Err(Flow::Trap("internal: char op".to_owned())),
                },
                (Val::Str(x), Val::Str(y)) if *op == Kind::Plus => Ok(Val::Str(x + &y)),
                _ => Err(Flow::Trap("internal: binary operand mismatch".to_owned())),
            }
        }
        Expr::ClosureLit(cid) => Ok(Val::Closure(*cid)),
        // `&v as *const T` (M2.12 slice 2): the operand is a bare variable,
        // so the pointer carries its slot; a deref reads the slot's current
        // value, exactly what the compiled backends read through the address.
        Expr::BorrowAs(operand, ..) => match &**operand {
            Expr::Variable(id, ..) => Ok(Val::Ptr(*id)),
            _ => Err(Flow::Trap("internal: pointer operand is not a variable".to_owned())),
        },
        // ADR-0209 step A: a retag keeps the address and only changes the
        // view — the slot model is type-erased, so the passthrough is exact
        // for same-typed store/load pairs. Cross-type punning (store as i64,
        // read as f64) has no interpreter model: the compiled backends
        // reinterpret the bits, the slot hands back the stored value —
        // corpus shapes avoid the punning, and the limitation is stated in
        // the ADR rather than papered over.
        Expr::PtrCast(operand, ..) => match eval_expr(env, operand, locals, caller_locals, moving)? {
            Val::Usize(addr) => Ok(Val::Ptr(addr.wrapping_sub(0x1000))),
            other => Ok(other),
        },
        // ADR-0209 step A: the interpreter's pointers are slots into the
        // frame, not addresses, so byte/element arithmetic has no model here
        // — the same story as extern calls (which trap by name). The
        // compiled backends run the arithmetic; the corpus pins their
        // behavior without asserting interp agreement.
        Expr::PtrOffset(..) => Err(Flow::Trap("pointer arithmetic cannot run in the interpreter; run the compiled program".to_owned())),
        Expr::PtrDropInPlace(..) => Err(Flow::Trap("ptr_drop_in_place cannot run in the interpreter; run the compiled program".to_owned())),
        Expr::PtrWrite(..) => Err(Flow::Trap("ptr_write cannot run in the interpreter; run the compiled program".to_owned())),
        Expr::DerefPtr(pointer, ..) => match eval_expr(env, pointer, locals, caller_locals, false)? {
            Val::Ptr(slot) => match locals.get(&slot).cloned() {
                Some(Val::Moved) => Err(trap("reading through a pointer to a moved value", 0, 0)),
                Some(value) => Ok(value),
                None => Err(Flow::Trap("internal: pointer target vanished".to_owned())),
            },
            _ => Err(Flow::Trap("internal: deref of a non-pointer".to_owned())),
        },
        Expr::Call(fn_id, args, _) => invoke_call(env, *fn_id, args, locals, caller_locals),
        Expr::FnCall(var_id, args, ..) => {
            let target_fn = match read_var(locals, caller_locals, *var_id, 0, 0)? {
                Val::FnPtr(fid) => fid,
                Val::Closure(cid) => env.program.ctx.closure_fn_id(cid),
                _ => return Err(Flow::Trap("internal: FnCall on non-function".to_owned())),
            };
            invoke_call(env, target_fn, args, locals, caller_locals)
        }
        // ADR-0214: the spawned callable runs to completion here (its capture
        // slots evaluate as by-value arguments, so the interpreter's move
        // markers match the compiled move), and wait reads the stashed value.
        Expr::Spawn(slots, callable, ..) => {
            let mut args = slots.iter().map(|slot| Argument::Value(slot.clone())).collect::<Vec<_>>();
            let outcome = match &**callable {
                Expr::ClosureLit(cid) => {
                    // The borrow captures read the caller's frame through the
                    // same read-only forms the compiled context packages.
                    for (outer_id, hidden) in env.program.ctx.closure_borrow_captures(*cid) {
                        match hidden {
                            Type::StrView => args.push(Argument::View(Expr::Variable(outer_id, Type::String, 0, 0))),
                            _ => args.push(Argument::Borrow(outer_id, matches!(hidden, Type::VecRef | Type::StrVecRef | Type::VecFRef))),
                        }
                    }
                    invoke_call(env, env.program.ctx.closure_fn_id(*cid), &args, locals, caller_locals)
                }
                Expr::Variable(_, Type::Closure(cid), ..) => {
                    for (outer_id, hidden) in env.program.ctx.closure_borrow_captures(*cid) {
                        match hidden {
                            Type::StrView => args.push(Argument::View(Expr::Variable(outer_id, Type::String, 0, 0))),
                            _ => args.push(Argument::Borrow(outer_id, matches!(hidden, Type::VecRef | Type::StrVecRef | Type::VecFRef))),
                        }
                    }
                    invoke_call(env, env.program.ctx.closure_fn_id(*cid), &args, locals, caller_locals)
                }
                Expr::Variable(id, Type::Fn(_), ..) => {
                    let target = match read_var(locals, caller_locals, *id, 0, 0)? {
                        Val::FnPtr(fid) => fid,
                        _ => return Err(Flow::Trap("internal: spawn of a non-function value".to_owned())),
                    };
                    invoke_call(env, target, &args, locals, caller_locals)
                }
                _ => return Err(Flow::Trap("internal: spawn callable shape".to_owned())),
            };
            // A body's failure is recorded and re-raised at wait, so the
            // failure surfaces at the same program point as the compiled
            // backend; process-wide outcomes (exit) still leave immediately.
            let stored = match outcome {
                Ok(value) => Ok(value),
                Err(Flow::Trap(message)) => Err(Flow::Trap(message)),
                Err(other) => return Err(other),
            };
            env.threads.push(Some(stored));
            Ok(Val::Thread(env.threads.len() - 1))
        }
        Expr::Wait(handle, ..) => match eval_expr(env, handle, locals, caller_locals, true)? {
            Val::Thread(seq) => match env.threads[seq].take() {
                Some(Ok(value)) => Ok(value),
                Some(Err(flow)) => Err(flow),
                None => Err(Flow::Trap("internal: thread value already taken".to_owned())),
            },
            _ => Err(Flow::Trap("internal: wait on a non-handle".to_owned())),
        },
        // spec/concurrency.md §8: the cell table is indexed by handle, so a
        // cloned handle reads and writes the same cell.
        Expr::AtomicNew(value) => {
            let seed = match eval_expr(env, value, locals, caller_locals, false)? {
                Val::Int(n) => n,
                _ => return Err(Flow::Trap("internal: cell_new seed".to_owned())),
            };
            env.atomics.push(seed);
            Ok(Val::Atomic(env.atomics.len() - 1))
        }
        Expr::AtomicLoad(handle) => match eval_expr(env, handle, locals, caller_locals, false)? {
            Val::Atomic(cell) => Ok(Val::Int(env.atomics[cell])),
            _ => Err(Flow::Trap("internal: cell_load on a non-handle".to_owned())),
        },
        Expr::AtomicAdd(handle, value) => {
            let cell = match eval_expr(env, handle, locals, caller_locals, false)? {
                Val::Atomic(cell) => cell,
                _ => return Err(Flow::Trap("internal: cell_add on a non-handle".to_owned())),
            };
            let delta = match eval_expr(env, value, locals, caller_locals, false)? {
                Val::Int(n) => n,
                _ => return Err(Flow::Trap("internal: cell_add delta".to_owned())),
            };
            let old_value = env.atomics[cell];
            env.atomics[cell] = env.atomics[cell].wrapping_add(delta);
            Ok(Val::Int(old_value))
        }
        Expr::AtomicCas(handle, expect, new) => {
            let cell = match eval_expr(env, handle, locals, caller_locals, false)? {
                Val::Atomic(cell) => cell,
                _ => return Err(Flow::Trap("internal: cell_cas on a non-handle".to_owned())),
            };
            let expected = match eval_expr(env, expect, locals, caller_locals, false)? {
                Val::Int(n) => n,
                _ => return Err(Flow::Trap("internal: cell_cas expected".to_owned())),
            };
            let replacement = match eval_expr(env, new, locals, caller_locals, false)? {
                Val::Int(n) => n,
                _ => return Err(Flow::Trap("internal: cell_cas replacement".to_owned())),
            };
            if env.atomics[cell] == expected {
                env.atomics[cell] = replacement;
                Ok(Val::Bool(true))
            } else {
                Ok(Val::Bool(false))
            }
        }
        Expr::SharedNew(value, _) => {
            let v = eval_expr(env, value, locals, caller_locals, false)?;
            env.shared.push(v);
            Ok(Val::Shared(env.shared.len() - 1))
        }
        Expr::SharedGet(handle, _) => match eval_expr(env, handle, locals, caller_locals, false)? {
            Val::Shared(id) => Ok(env.shared[id].clone()),
            _ => Err(Flow::Trap("internal: shared_get on a non-handle".to_owned())),
        },
        Expr::MutexNew => Ok(Val::Mutex),
        Expr::MutexLock(handle) => match eval_expr(env, handle, locals, caller_locals, false)? {
            Val::Mutex => Ok(Val::Guard),
            _ => Err(Flow::Trap("internal: mutex_lock on a non-handle".to_owned())),
        },
        Expr::ChanNew(cap) => {
            let c = match eval_expr(env, cap, locals, caller_locals, false)? {
                Val::Int(n) if n >= 0 => n,
                Val::Int(_) => return Err(Flow::Trap("chan_new capacity must be non-negative".to_owned())),
                _ => return Err(Flow::Trap("internal: chan_new capacity".to_owned())),
            };
            env.chans.push((std::collections::VecDeque::new(), c, false));
            Ok(Val::Chan(env.chans.len() - 1))
        }
        Expr::ExecNew => {
            env.execs.push(std::collections::VecDeque::new());
            Ok(Val::Exec(env.execs.len() - 1))
        }
        Expr::ExecRun(ex) => {
            let eid = match eval_expr(env, ex, locals, caller_locals, false)? {
                Val::Exec(id) => id,
                _ => return Err(Flow::Trap("internal: exec_run on a non-exec".to_owned())),
            };
            let completed = drain_exec_queue(env, eid)?;
            Ok(Val::Int(completed))
        }
        Expr::ScopeNew => {
            env.execs.push(std::collections::VecDeque::new());
            Ok(Val::TaskScope(env.execs.len() - 1))
        }
        Expr::PoolNew(n) => {
            let _ = eval_expr(env, n, locals, caller_locals, false)?;
            env.execs.push(std::collections::VecDeque::new());
            Ok(Val::Pool(env.execs.len() - 1))
        }
        Expr::PoolJoin(p) => {
            let pid = match eval_expr(env, p, locals, caller_locals, false)? {
                Val::Pool(id) => id,
                _ => return Err(Flow::Trap("internal: pool_join on non-pool".to_owned())),
            };
            let completed = drain_exec_queue(env, pid)?;
            Ok(Val::Int(completed))
        }
        Expr::ChanSend(ch, val) => {
            let id = match eval_expr(env, ch, locals, caller_locals, false)? {
                Val::Chan(id) => id,
                _ => return Err(Flow::Trap("internal: chan_send on a non-handle".to_owned())),
            };
            let v = match eval_expr(env, val, locals, caller_locals, false)? {
                Val::Int(n) => n,
                _ => return Err(Flow::Trap("internal: chan_send value".to_owned())),
            };
            let (buf, _cap, closed) = &mut env.chans[id];
            if *closed {
                Ok(Val::Bool(false))
            } else {
                buf.push_back(v);
                Ok(Val::Bool(true))
            }
        }
        Expr::ChanTrySend(ch, val) => {
            let id = match eval_expr(env, ch, locals, caller_locals, false)? {
                Val::Chan(id) => id,
                _ => return Err(Flow::Trap("internal: chan_try_send on a non-handle".to_owned())),
            };
            let v = match eval_expr(env, val, locals, caller_locals, false)? {
                Val::Int(n) => n,
                _ => return Err(Flow::Trap("internal: chan_try_send value".to_owned())),
            };
            let (buf, cap, closed) = &mut env.chans[id];
            if *closed || *cap <= 0 || (buf.len() as i64) >= *cap {
                Ok(Val::Bool(false))
            } else {
                buf.push_back(v);
                Ok(Val::Bool(true))
            }
        }
        Expr::ChanRecv(ch) => {
            let id = match eval_expr(env, ch, locals, caller_locals, false)? {
                Val::Chan(id) => id,
                _ => return Err(Flow::Trap("internal: chan_recv on a non-handle".to_owned())),
            };
            let (buf, _cap, _closed) = &mut env.chans[id];
            Ok(Val::Int(buf.pop_front().unwrap_or(0)))
        }
        Expr::ChanSelect(c1, c2) => {
            let id1 = match eval_expr(env, c1, locals, caller_locals, false)? {
                Val::Chan(id) => id,
                _ => return Err(Flow::Trap("internal: chan_select on a non-handle".to_owned())),
            };
            let id2 = match eval_expr(env, c2, locals, caller_locals, false)? {
                Val::Chan(id) => id,
                _ => return Err(Flow::Trap("internal: chan_select on a non-handle".to_owned())),
            };
            // M4.3.05: a closed arm wins over an arm that merely holds a
            // value, so a cancelled receiver returns without consuming.
            let closed1 = env.chans[id1].2;
            let closed2 = env.chans[id2].2;
            let ready1 = !env.chans[id1].0.is_empty();
            let ready2 = !env.chans[id2].0.is_empty();
            if closed1 { Ok(Val::Int(0)) } else if closed2 { Ok(Val::Int(1)) } else if ready1 { Ok(Val::Int(0)) } else if ready2 { Ok(Val::Int(1)) } else { Ok(Val::Int(0)) }
        }
        other => Err(Flow::Trap(format!("unsupported expression in MIR interpreter: {}", format!("{other:?}").chars().take(60).collect::<String>()))),
    }
}

fn invoke_call(
    env: &mut Env<'_>,
    fn_id: usize,
    args: &[Argument],
    locals: &mut HashMap<usize, Val>,
    caller_locals: &mut Option<&mut HashMap<usize, Val>>,
) -> Result<Val, Flow> {
    let callee = *env.fns.get(&fn_id).ok_or_else(|| {
        match env.program.ctx.extern_name(fn_id) {
            // A declared extern has no ae body to interpret (M2.12
            // slice 4): the compiled backends link the C symbol; the
            // interpreter cannot.
            Some(name) => Flow::Trap(format!("extern '{name}' cannot run in the interpreter; run the compiled program")),
            None => Flow::Trap(format!("internal: missing fn #{fn_id}")),
        }
    })?;
    let mut callee_locals: HashMap<usize, Val> = HashMap::new();
    // A &mut parameter whose slot already holds a reference (a &mut borrow
    // forwarded deeper) resolves to a copy-in: the callee mutates its own
    // copy and the value is written back to the original frame on return.
    // The direct case keeps the live reference, whose one-level target the
    // callee's caller frame reaches.
    let mut copy_backs: Vec<(usize, usize)> = Vec::new();
    for ((param_id, param_ty), arg) in callee.params.iter().zip(args.iter()) {
        let val = match arg {
            Argument::Value(e) => eval_expr(env, e, locals, caller_locals, true)?,
            Argument::View(e) => eval_expr(env, e, locals, caller_locals, false)?,
            Argument::Borrow(slot, _) => {
                if matches!(param_ty, Type::StructMut(_)) {
                    if let Some(Val::Ref(target)) = locals.get(slot).cloned() {
                        // The reference's target lives in the frame that
                        // handed the borrow down: the caller's own locals,
                        // or — for a chain — the caller's caller's.
                        // The reference's target names a slot in the frame
                        // that created it — from this call's perspective, the
                        // caller_locals.
                        let value = caller_locals.as_ref()
                            .and_then(|caller| caller.get(&target).cloned())
                            .ok_or_else(|| Flow::Trap("internal: dangling reference in interpreter".to_owned()))?;
                        copy_backs.push((*param_id, target));
                        value
                    } else {
                        Val::Ref(*slot)
                    }
                } else {
                    read_var(locals, caller_locals, *slot, 0, 0)?
                }
            }
            Argument::FnPtr(id) => Val::FnPtr(*id),
        };
        callee_locals.insert(*param_id, val);
    }
    let mut parent = Some(&mut *locals);
    let outcome = eval_stmts(env, &callee.body, &mut callee_locals, &mut parent);
    // The copy-ins write back to the frames their references named.
    for (param_id, target) in &copy_backs {
        if let Some(value) = callee_locals.get(param_id)
            && let Some(caller) = caller_locals.as_mut() {
                caller.insert(*target, value.clone());
            }
    }
    match outcome {
        Flow::Return(v) => Ok(v),
        Flow::Next => Ok(Val::Int(0)),
        other => Err(other),
    }
}

fn unescape_str(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('0') => out.push('\0'),
                Some(other) => { out.push('\\'); out.push(other); }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn format_float(x: f64) -> String {
    if x.is_nan() { return "nan".to_owned(); }
    if x.is_infinite() { return if x < 0.0 { "-inf".to_owned() } else { "inf".to_owned() }; }
    if x == 0.0 && x.is_sign_negative() { return "-0.0".to_owned(); }
    let s = format!("{x:?}");
    if s.contains(['.', 'e', 'E']) { s } else { format!("{s}.0") }
}

fn format_val(val: &Val) -> String {
    match val {
        Val::Int(n) => n.to_string(),
        Val::I8(n) => n.to_string(),
        Val::I16(n) => n.to_string(),
        Val::I32(n) => n.to_string(),
        Val::Isize(n) => n.to_string(),
        Val::U8(n) => n.to_string(),
        Val::U16(n) => n.to_string(),
        Val::U32(n) => n.to_string(),
        Val::U64(n) => n.to_string(),
        Val::Usize(n) => n.to_string(),
        Val::Bool(b) => if *b { "true".to_owned() } else { "false".to_owned() },
        Val::Char(c) => c.to_string(),
        Val::Unit => "()".to_owned(),
        Val::Str(s) => s.clone(),
        Val::F32(x) => format_float(*x as f64),
        Val::Float(x) => format_float(*x),
        _ => "<val>".to_owned(),
    }
}

fn eval_scalar_cast(from: &Type, to: &Type, value: Val, line: usize, col: usize) -> Result<Val, Flow> {
    match (from, to, &value) {
        (Type::Char, Type::Int(IntTy::U32), Val::Char(c)) => return Ok(Val::U32(*c as u32)),
        (Type::Int(IntTy::U32), Type::Char, Val::U32(v)) => {
            return char::from_u32(*v)
                .map(Val::Char)
                .ok_or_else(|| trap("invalid Unicode scalar value", line, col));
        }
        (Type::Ptr(_) | Type::PtrMut(_), Type::Int(IntTy::Usize), Val::Ptr(slot)) => {
            return Ok(Val::Usize(slot.wrapping_add(0x1000)));
        }
        (Type::Float(FloatTy::F32), Type::Float(FloatTy::F64), Val::F32(x)) => return Ok(Val::Float(*x as f64)),
        (Type::Float(FloatTy::F64), Type::Float(FloatTy::F32), Val::Float(x)) => return Ok(Val::F32(*x as f32)),
        (Type::Float(FloatTy::F32), Type::Float(FloatTy::F32), Val::F32(x)) => return Ok(Val::F32(*x)),
        (Type::Float(FloatTy::F64), Type::Float(FloatTy::F64), Val::Float(x)) => return Ok(Val::Float(*x)),
        _ => {}
    }
    if let Type::Float(ft) = to {
        let as_f64 = match (from, &value) {
            (Type::Int(IntTy::I64), Val::Int(n)) => Some((*n as f64, *n as f32)),
            (Type::Int(IntTy::I8), Val::I8(n)) => Some((*n as f64, *n as f32)),
            (Type::Int(IntTy::I16), Val::I16(n)) => Some((*n as f64, *n as f32)),
            (Type::Int(IntTy::I32), Val::I32(n)) => Some((*n as f64, *n as f32)),
            (Type::Int(IntTy::Isize), Val::Isize(n)) => Some((*n as f64, *n as f32)),
            (Type::Int(IntTy::U8), Val::U8(n)) => Some((*n as f64, *n as f32)),
            (Type::Int(IntTy::U16), Val::U16(n)) => Some((*n as f64, *n as f32)),
            (Type::Int(IntTy::U32), Val::U32(n)) => Some((*n as f64, *n as f32)),
            (Type::Int(IntTy::U64), Val::U64(n)) => Some((*n as f64, *n as f32)),
            (Type::Int(IntTy::Usize), Val::Usize(n)) => Some((*n as f64, *n as f32)),
            _ => None,
        };
        if let Some((v64, v32)) = as_f64 {
            return Ok(match ft {
                FloatTy::F64 => Val::Float(v64),
                FloatTy::F32 => Val::F32(v32),
            });
        }
    }
    let bits: Option<u64> = match (from, &value) {
        (Type::Int(IntTy::I64), Val::Int(n)) => Some(*n as u64),
        (Type::Int(IntTy::I8), Val::I8(n)) => Some(*n as i64 as u64),
        (Type::Int(IntTy::I16), Val::I16(n)) => Some(*n as i64 as u64),
        (Type::Int(IntTy::I32), Val::I32(n)) => Some(*n as i64 as u64),
        (Type::Int(IntTy::Isize), Val::Isize(n)) => Some(*n as i64 as u64),
        (Type::Int(IntTy::U8), Val::U8(n)) => Some(*n as u64),
        (Type::Int(IntTy::U16), Val::U16(n)) => Some(*n as u64),
        (Type::Int(IntTy::U32), Val::U32(n)) => Some(*n as u64),
        (Type::Int(IntTy::U64), Val::U64(n)) => Some(*n),
        (Type::Int(IntTy::Usize), Val::Usize(n)) => Some(*n as u64),
        _ => None,
    };
    if let Some(b) = bits {
        return match to {
            Type::Int(IntTy::I64) => Ok(Val::Int(b as i64)),
            Type::Int(IntTy::I8) => Ok(Val::I8(b as i8)),
            Type::Int(IntTy::I16) => Ok(Val::I16(b as i16)),
            Type::Int(IntTy::I32) => Ok(Val::I32(b as i32)),
            Type::Int(IntTy::Isize) => Ok(Val::Isize(b as i64 as isize)),
            Type::Int(IntTy::U8) => Ok(Val::U8(b as u8)),
            Type::Int(IntTy::U16) => Ok(Val::U16(b as u16)),
            Type::Int(IntTy::U32) => Ok(Val::U32(b as u32)),
            Type::Int(IntTy::U64) => Ok(Val::U64(b)),
            Type::Int(IntTy::Usize) => Ok(Val::Usize(b as usize)),
            _ => Err(Flow::Trap(format!("internal: scalar cast on {value:?}"))),
        };
    }
    if let (Type::Float(FloatTy::F64), Type::Int(ity), Val::Float(x)) = (from, to, &value) {
        let out_of_range = !x.is_finite()
            || match ity {
                IntTy::I8 => *x <= -129.0 || *x >= 128.0,
                IntTy::U8 => *x <= -1.0 || *x >= 256.0,
                IntTy::I16 => *x <= -32769.0 || *x >= 32768.0,
                IntTy::U16 => *x <= -1.0 || *x >= 65536.0,
                IntTy::I32 => *x <= -2147483649.0 || *x >= 2147483648.0,
                IntTy::U32 => *x <= -1.0 || *x >= 4294967296.0,
                IntTy::I64 | IntTy::Isize => *x < -9223372036854775808.0 || *x >= 9223372036854775808.0,
                IntTy::U64 | IntTy::Usize => *x <= -1.0 || *x >= 18446744073709551616.0,
            };
        if out_of_range {
            return Err(trap("float-to-integer cast out of range", line, col));
        }
        return Ok(match ity {
            IntTy::I8 => Val::I8(*x as i8),
            IntTy::U8 => Val::U8(*x as u8),
            IntTy::I16 => Val::I16(*x as i16),
            IntTy::U16 => Val::U16(*x as u16),
            IntTy::I32 => Val::I32(*x as i32),
            IntTy::U32 => Val::U32(*x as u32),
            IntTy::I64 => Val::Int(*x as i64),
            IntTy::Isize => Val::Isize(*x as isize),
            IntTy::U64 => Val::U64(*x as u64),
            IntTy::Usize => Val::Usize(*x as usize),
        });
    }
    if let (Type::Float(FloatTy::F32), Type::Int(ity), Val::F32(x)) = (from, to, &value) {
        let out_of_range = !x.is_finite()
            || match ity {
                IntTy::I8 => *x <= -129.0f32 || *x >= 128.0f32,
                IntTy::U8 => *x <= -1.0f32 || *x >= 256.0f32,
                IntTy::I16 => *x <= -32769.0f32 || *x >= 32768.0f32,
                IntTy::U16 => *x <= -1.0f32 || *x >= 65536.0f32,
                IntTy::I32 => *x < -2147483648.0f32 || *x >= 2147483648.0f32,
                IntTy::U32 => *x <= -1.0f32 || *x >= 4294967296.0f32,
                IntTy::I64 | IntTy::Isize => *x < -9223372036854775808.0f32 || *x >= 9223372036854775808.0f32,
                IntTy::U64 | IntTy::Usize => *x <= -1.0f32 || *x >= 18446744073709551616.0f32,
            };
        if out_of_range {
            return Err(trap("float-to-integer cast out of range", line, col));
        }
        return Ok(match ity {
            IntTy::I8 => Val::I8(*x as i8),
            IntTy::U8 => Val::U8(*x as u8),
            IntTy::I16 => Val::I16(*x as i16),
            IntTy::U16 => Val::U16(*x as u16),
            IntTy::I32 => Val::I32(*x as i32),
            IntTy::U32 => Val::U32(*x as u32),
            IntTy::I64 => Val::Int(*x as i64),
            IntTy::Isize => Val::Isize(*x as isize),
            IntTy::U64 => Val::U64(*x as u64),
            IntTy::Usize => Val::Usize(*x as usize),
        });
    }
    Err(Flow::Trap(format!("internal: scalar cast on {value:?}")))
}

trait WidthInt: Copy + Ord + Eq {
    const ZERO: Self;
    fn checked_add(self, rhs: Self) -> Option<Self>;
    fn checked_sub(self, rhs: Self) -> Option<Self>;
    fn checked_mul(self, rhs: Self) -> Option<Self>;
    fn checked_div(self, rhs: Self) -> Option<Self>;
    fn checked_rem(self, rhs: Self) -> Option<Self>;
    fn checked_neg(self) -> Option<Self>;
    fn wrapping_add(self, rhs: Self) -> Self;
    fn wrapping_sub(self, rhs: Self) -> Self;
    fn wrapping_mul(self, rhs: Self) -> Self;
    fn wrapping_neg(self) -> Self;
    fn saturating_add(self, rhs: Self) -> Self;
    fn saturating_sub(self, rhs: Self) -> Self;
    fn saturating_mul(self, rhs: Self) -> Self;
    fn bit_or(self, rhs: Self) -> Self;
    fn bit_xor(self, rhs: Self) -> Self;
    fn bit_and(self, rhs: Self) -> Self;
    fn wrapping_shl(self, rhs: u32) -> Self;
    fn wrapping_shr(self, rhs: u32) -> Self;
    fn valid_shift(self) -> Option<u32>;
    fn from_val(val: &Val) -> Option<Self>;
    fn into_val(self) -> Val;
}

macro_rules! impl_width_int {
    ($ty:ty, $variant:ident, signed) => {
        impl WidthInt for $ty {
            const ZERO: Self = 0;
            fn checked_add(self, rhs: Self) -> Option<Self> { <$ty>::checked_add(self, rhs) }
            fn checked_sub(self, rhs: Self) -> Option<Self> { <$ty>::checked_sub(self, rhs) }
            fn checked_mul(self, rhs: Self) -> Option<Self> { <$ty>::checked_mul(self, rhs) }
            fn checked_div(self, rhs: Self) -> Option<Self> { <$ty>::checked_div(self, rhs) }
            fn checked_rem(self, rhs: Self) -> Option<Self> { <$ty>::checked_rem(self, rhs) }
            fn checked_neg(self) -> Option<Self> { <$ty>::checked_neg(self) }
            fn wrapping_add(self, rhs: Self) -> Self { <$ty>::wrapping_add(self, rhs) }
            fn wrapping_sub(self, rhs: Self) -> Self { <$ty>::wrapping_sub(self, rhs) }
            fn wrapping_mul(self, rhs: Self) -> Self { <$ty>::wrapping_mul(self, rhs) }
            fn wrapping_neg(self) -> Self { <$ty>::wrapping_neg(self) }
            fn saturating_add(self, rhs: Self) -> Self { <$ty>::saturating_add(self, rhs) }
            fn saturating_sub(self, rhs: Self) -> Self { <$ty>::saturating_sub(self, rhs) }
            fn saturating_mul(self, rhs: Self) -> Self { <$ty>::saturating_mul(self, rhs) }
            fn bit_or(self, rhs: Self) -> Self { self | rhs }
            fn bit_xor(self, rhs: Self) -> Self { self ^ rhs }
            fn bit_and(self, rhs: Self) -> Self { self & rhs }
            fn wrapping_shl(self, rhs: u32) -> Self { <$ty>::wrapping_shl(self, rhs) }
            fn wrapping_shr(self, rhs: u32) -> Self { <$ty>::wrapping_shr(self, rhs) }
            fn valid_shift(self) -> Option<u32> {
                (self >= 0 && (self as u64) < (<$ty>::BITS as u64)).then_some(self as u32)
            }
            fn from_val(val: &Val) -> Option<Self> {
                if let Val::$variant(v) = val { Some(*v) } else { None }
            }
            fn into_val(self) -> Val { Val::$variant(self) }
        }
    };
    ($ty:ty, $variant:ident, unsigned) => {
        impl WidthInt for $ty {
            const ZERO: Self = 0;
            fn checked_add(self, rhs: Self) -> Option<Self> { <$ty>::checked_add(self, rhs) }
            fn checked_sub(self, rhs: Self) -> Option<Self> { <$ty>::checked_sub(self, rhs) }
            fn checked_mul(self, rhs: Self) -> Option<Self> { <$ty>::checked_mul(self, rhs) }
            fn checked_div(self, rhs: Self) -> Option<Self> { <$ty>::checked_div(self, rhs) }
            fn checked_rem(self, rhs: Self) -> Option<Self> { <$ty>::checked_rem(self, rhs) }
            fn checked_neg(self) -> Option<Self> { None }
            fn wrapping_add(self, rhs: Self) -> Self { <$ty>::wrapping_add(self, rhs) }
            fn wrapping_sub(self, rhs: Self) -> Self { <$ty>::wrapping_sub(self, rhs) }
            fn wrapping_mul(self, rhs: Self) -> Self { <$ty>::wrapping_mul(self, rhs) }
            fn wrapping_neg(self) -> Self { <$ty>::wrapping_neg(self) }
            fn saturating_add(self, rhs: Self) -> Self { <$ty>::saturating_add(self, rhs) }
            fn saturating_sub(self, rhs: Self) -> Self { <$ty>::saturating_sub(self, rhs) }
            fn saturating_mul(self, rhs: Self) -> Self { <$ty>::saturating_mul(self, rhs) }
            fn bit_or(self, rhs: Self) -> Self { self | rhs }
            fn bit_xor(self, rhs: Self) -> Self { self ^ rhs }
            fn bit_and(self, rhs: Self) -> Self { self & rhs }
            fn wrapping_shl(self, rhs: u32) -> Self { <$ty>::wrapping_shl(self, rhs) }
            fn wrapping_shr(self, rhs: u32) -> Self { <$ty>::wrapping_shr(self, rhs) }
            fn valid_shift(self) -> Option<u32> {
                ((self as u64) < (<$ty>::BITS as u64)).then_some(self as u32)
            }
            fn from_val(val: &Val) -> Option<Self> {
                if let Val::$variant(v) = val { Some(*v) } else { None }
            }
            fn into_val(self) -> Val { Val::$variant(self) }
        }
    };
}

impl_width_int!(i8, I8, signed);
impl_width_int!(i16, I16, signed);
impl_width_int!(i32, I32, signed);
impl_width_int!(i64, Int, signed);
impl_width_int!(isize, Isize, signed);
impl_width_int!(u8, U8, unsigned);
impl_width_int!(u16, U16, unsigned);
impl_width_int!(u32, U32, unsigned);
impl_width_int!(u64, U64, unsigned);
impl_width_int!(usize, Usize, unsigned);

fn dispatch_int_method(kind: IntMethodKind, recv: Val, rhs: Option<Val>, result_ty: &Type) -> Result<Val, Flow> {
    match recv {
        Val::Int(x) => eval_int_method(kind, x, rhs, result_ty),
        Val::I8(x) => eval_int_method(kind, x, rhs, result_ty),
        Val::I16(x) => eval_int_method(kind, x, rhs, result_ty),
        Val::I32(x) => eval_int_method(kind, x, rhs, result_ty),
        Val::Isize(x) => eval_int_method(kind, x, rhs, result_ty),
        Val::U8(x) => eval_int_method(kind, x, rhs, result_ty),
        Val::U16(x) => eval_int_method(kind, x, rhs, result_ty),
        Val::U32(x) => eval_int_method(kind, x, rhs, result_ty),
        Val::U64(x) => eval_int_method(kind, x, rhs, result_ty),
        Val::Usize(x) => eval_int_method(kind, x, rhs, result_ty),
        _ => Err(Flow::Trap("internal: integer method on non-integer".to_owned())),
    }
}

fn eval_int_method<T: WidthInt>(kind: IntMethodKind, x: T, rhs: Option<Val>, result_ty: &Type) -> Result<Val, Flow> {
    let option_val = |opt: Option<T>| -> Result<Val, Flow> {
        let Type::Enum(_, eid) = result_ty else {
            return Err(Flow::Trap("internal: checked_* result not enum".to_owned()));
        };
        Ok(match opt {
            Some(v) => Val::Enum(*eid, 0, vec![v.into_val()]),
            None => Val::Enum(*eid, 1, Vec::new()),
        })
    };
    let get_rhs = || -> Result<T, Flow> {
        rhs.as_ref()
            .and_then(T::from_val)
            .ok_or_else(|| Flow::Trap("internal: integer method rhs mismatch".to_owned()))
    };
    match kind {
        IntMethodKind::WrappingAdd => Ok(x.wrapping_add(get_rhs()?).into_val()),
        IntMethodKind::WrappingSub => Ok(x.wrapping_sub(get_rhs()?).into_val()),
        IntMethodKind::WrappingMul => Ok(x.wrapping_mul(get_rhs()?).into_val()),
        IntMethodKind::WrappingNeg => Ok(x.wrapping_neg().into_val()),
        IntMethodKind::SaturatingAdd => Ok(x.saturating_add(get_rhs()?).into_val()),
        IntMethodKind::SaturatingSub => Ok(x.saturating_sub(get_rhs()?).into_val()),
        IntMethodKind::SaturatingMul => Ok(x.saturating_mul(get_rhs()?).into_val()),
        IntMethodKind::CheckedAdd => option_val(x.checked_add(get_rhs()?)),
        IntMethodKind::CheckedSub => option_val(x.checked_sub(get_rhs()?)),
        IntMethodKind::CheckedMul => option_val(x.checked_mul(get_rhs()?)),
        IntMethodKind::CheckedDiv => option_val(x.checked_div(get_rhs()?)),
        IntMethodKind::CheckedRem => option_val(x.checked_rem(get_rhs()?)),
        IntMethodKind::CheckedNeg => option_val(x.checked_neg()),
    }
}

fn eval_width_binop<T: WidthInt>(x: T, y: T, op: &Kind, line: usize, col: usize) -> Result<Val, Flow> {
    match op {
        Kind::Plus => x.checked_add(y).map(T::into_val).ok_or_else(|| trap("integer overflow", line, col)),
        Kind::Minus => x.checked_sub(y).map(T::into_val).ok_or_else(|| trap("integer overflow", line, col)),
        Kind::Star => x.checked_mul(y).map(T::into_val).ok_or_else(|| trap("integer overflow", line, col)),
        Kind::Slash => {
            if y == T::ZERO { return Err(trap("division by zero", line, col)); }
            x.checked_div(y).map(T::into_val).ok_or_else(|| trap("integer overflow", line, col))
        }
        Kind::Percent => {
            if y == T::ZERO { return Err(trap("division by zero", line, col)); }
            x.checked_rem(y).map(T::into_val).ok_or_else(|| trap("integer overflow", line, col))
        }
        Kind::Pipe => Ok(x.bit_or(y).into_val()),
        Kind::Caret => Ok(x.bit_xor(y).into_val()),
        Kind::Amp => Ok(x.bit_and(y).into_val()),
        Kind::Shl => {
            let s = y.valid_shift().ok_or_else(|| trap("shift count out of range", line, col))?;
            Ok(x.wrapping_shl(s).into_val())
        }
        Kind::Shr => {
            let s = y.valid_shift().ok_or_else(|| trap("shift count out of range", line, col))?;
            Ok(x.wrapping_shr(s).into_val())
        }
        Kind::EqEq => Ok(Val::Bool(x == y)),
        Kind::NotEq => Ok(Val::Bool(x != y)),
        Kind::Less => Ok(Val::Bool(x < y)),
        Kind::LessEq => Ok(Val::Bool(x <= y)),
        Kind::Greater => Ok(Val::Bool(x > y)),
        Kind::GreaterEq => Ok(Val::Bool(x >= y)),
        _ => Err(Flow::Trap("internal: int op".to_owned())),
    }
}

fn drain_exec_queue(env: &mut Env, eid: usize) -> Result<i64, Flow> {
    let mut completed: i64 = 0;
    while let Some(mut task) = env.execs[eid].pop_front() {
        let mut yielded = false;
        while let Some(frame) = task.stack.pop() {
            match frame {
                TaskFrame::Block(stmts, mut idx) => {
                    while idx < stmts.len() {
                        let stmt = &stmts[idx];
                        idx += 1;
                        match stmt {
                            Statement::TaskYield => {
                                task.stack.push(TaskFrame::Block(stmts, idx));
                                yielded = true;
                                break;
                            }
                            Statement::If(cond, then_b, else_b) => {
                                let b = match eval_expr(env, cond, &mut task.locals, &mut None, false) {
                                    Ok(Val::Bool(b)) => b,
                                    Ok(_) => return Err(Flow::Trap("internal: non-bool if condition".to_owned())),
                                    Err(f) => return Err(f),
                                };
                                let branch = if b { then_b.clone() } else { else_b.clone() };
                                task.stack.push(TaskFrame::Block(stmts, idx));
                                task.stack.push(TaskFrame::Block(branch, 0));
                                break;
                            }
                            Statement::While(cond, body) => {
                                let cond_c = cond.clone();
                                let body_c = body.clone();
                                task.stack.push(TaskFrame::Block(stmts, idx));
                                task.stack.push(TaskFrame::While(cond_c, body_c));
                                break;
                            }
                            other => {
                                match eval_stmts(env, std::slice::from_ref(other), &mut task.locals, &mut None) {
                                    Flow::Next => {}
                                    Flow::Return(_) => { task.stack.clear(); break; }
                                    Flow::Break => {
                                        while let Some(f) = task.stack.pop() {
                                            if matches!(f, TaskFrame::While(..)) { break; }
                                        }
                                        break;
                                    }
                                    Flow::Continue(_) => { break; }
                                    Flow::Exit(c) => return Err(Flow::Exit(c)),
                                    Flow::Trap(m) => return Err(Flow::Trap(m)),
                                }
                            }
                        }
                    }
                    if yielded { break; }
                }
                TaskFrame::While(cond, body) => {
                    let b = match eval_expr(env, &cond, &mut task.locals, &mut None, false) {
                        Ok(Val::Bool(b)) => b,
                        Ok(_) => return Err(Flow::Trap("internal: non-bool while condition".to_owned())),
                        Err(f) => return Err(f),
                    };
                    if b {
                        task.stack.push(TaskFrame::While(cond, body.clone()));
                        task.stack.push(TaskFrame::Block(body, 0));
                    }
                }
            }
        }
        if yielded {
            env.execs[eid].push_back(task);
        } else {
            completed += 1;
        }
    }
    Ok(completed)
}
