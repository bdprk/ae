use std::cell::OnceCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
mod parser;
mod closures;
mod tree;
mod cfg;
mod movewalk;
mod borrowscope;
mod drops;
mod mir;
pub mod interp;
mod expression;
mod types;
mod declarations;
pub use loader::{PackageImports, with_package_imports};
mod loader;
use loader::{load_program, parse_source};
pub use loader::{check_path, check_path_diagnostic, describe_path, FrontendDiagnostic, FrontendError, SourceUnit};
mod capability_audit;
use capability_audit::audit_generic_fn;
mod codegen;
mod codegen_mir;
mod c_output;
pub(crate) use ae_syntax::{Kind, Token, lex};
#[allow(unused_imports)]
use codegen::*;
use c_output::*;



#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Type { Int, Bool, F64, String, StrView, Vec, StrVec, VecF, VecRef, StrVecRef, VecFRef, Map, MapF, Array(usize), Struct(usize), StructRef(usize), StructMut(usize), Enum(&'static str, usize), Tokens, Closure(usize), Fn(usize), Ptr(usize), PtrMut(usize), Thread(usize), Atomic, Shared(usize), Mutex, Guard, Chan, Exec, TaskScope, Pool }
// The pointee registry for `Type::Ptr` (M2.12): Type is Copy, so the
// pointee rides an id exactly like the closure/fn signature registries do.
thread_local! {
    static PTR_POINTEES: std::cell::RefCell<Vec<Type>> = const { std::cell::RefCell::new(Vec::new()) };
}
// The extern "C" registry (M2.12 slice 4): the id a call site carries maps
// to the C symbol the declaration names. The code emitter and the interpreter
// both read it; the parser fills it.
thread_local! {
    static EXTERN_NAMES: std::cell::RefCell<BTreeMap<usize, (String, Vec<Type>, Type)>> = const { std::cell::RefCell::new(BTreeMap::new()) };
}
fn register_extern(id: usize, c_name: String, params: Vec<Type>, ret: Type) {
    EXTERN_NAMES.with(|map| { map.borrow_mut().insert(id, (c_name, params, ret)); });
}
fn extern_name(id: usize) -> Option<String> {
    EXTERN_NAMES.with(|map| map.borrow().get(&id).map(|(name, _, _)| name.clone()))
}
// Whether the id is a declared `extern "C" fn` (no body): calling one is the
// unchecked operation the enclosing `unsafe` block exists to legalize, so
// the W0702 scan consults this alongside `is_unsafe_fn`. Both registries
// fill during the signature pass, which completes before any body parses.
pub(super) fn is_extern_fn(id: usize) -> bool {
    EXTERN_NAMES.with(|map| map.borrow().contains_key(&id))
}
// Every declared extern in function ID order, for stable prototype emission.
fn extern_declarations() -> Vec<(String, Vec<Type>, Type)> {
    EXTERN_NAMES.with(|map| map.borrow().values().cloned().collect())
}
// The `#[no_mangle]` exposures (M2.12 U7): an `extern "C" fn` WITH a body,
// emitted under its own name with external linkage so a C symbol can call
// it. The registry maps the ae function id to the C symbol; the call
// sites and the emitter both read it.
thread_local! {
    static EXPOSURE_NAMES: std::cell::RefCell<HashMap<usize, String>> = std::cell::RefCell::new(HashMap::new());
}
pub(super) fn register_exposure(id: usize, c_name: String) {
    EXPOSURE_NAMES.with(|map| { map.borrow_mut().insert(id, c_name); });
}
pub(super) fn exposure_name(id: usize) -> Option<String> {
    EXPOSURE_NAMES.with(|map| map.borrow().get(&id).cloned())
}
// The unsafe-fn registry (M2.12 slice 5): an `unsafe fn` is a normal
// function in codegen — its call sites are what require `unsafe`.
thread_local! {
    static UNSAFE_FNS: std::cell::RefCell<std::collections::HashSet<usize>> = std::cell::RefCell::new(std::collections::HashSet::new());
}
pub(super) fn register_unsafe_fn(id: usize) {
    UNSAFE_FNS.with(|set| { set.borrow_mut().insert(id); });
}
pub(super) fn is_unsafe_fn(id: usize) -> bool {
    UNSAFE_FNS.with(|set| set.borrow().contains(&id))
}
// Intern a pointee type; identical pointees share one id.
fn intern_pointee(pointee: Type) -> usize {
    PTR_POINTEES.with(|pointees| {
        let mut pointees = pointees.borrow_mut();
        if let Some(index) = pointees.iter().position(|t| *t == pointee) { return index; }
        pointees.push(pointee);
        pointees.len() - 1
    })
}
// The pointee of an interned pointer type. Runs without a parser handy, like
// `fn_sig` (ADR-0189).
fn ptr_pointee(id: usize) -> Type {
    PTR_POINTEES.with(|pointees| pointees.borrow()[id])
}
impl Type {
    fn scalar_c_layout(self) -> Option<(i64, i64)> {
        match self {
            Type::Int | Type::F64 => Some((8, 8)),
            Type::Bool => Some((1, 1)),
            _ => None,
        }
    }
    fn name(self) -> String {
        match self {
            Type::Int => "i64".to_owned(),
            Type::Bool => "bool".to_owned(),
            Type::F64 => "f64".to_owned(),
            Type::String => "string".to_owned(),
            Type::StrView => "string view".to_owned(),
            Type::Vec => "vec".to_owned(),
            Type::StrVec => "vec[string]".to_owned(),
            Type::VecF => "vec[f64]".to_owned(),
            Type::VecRef => "&vec".to_owned(),
            Type::StrVecRef => "&vec[string]".to_owned(),
            Type::VecFRef => "&vec[f64]".to_owned(),
            // One map shape for now: string keys, i64 values.
            Type::Map => "map[string, i64]".to_owned(),
            Type::MapF => "map[string, f64]".to_owned(),
            Type::Array(n) => format!("&[i64; {n}]"),
            Type::Struct(id) => format!("struct #{}", id),
            // A read-only borrow of a struct (ADR-0184). It names the same
            // layout as `Struct(id)`; the `&` is what keeps it from moving.
            Type::StructRef(id) => format!("&struct #{}", id),
            // The exclusive mutable borrow (ADR-0185): same layout, plus the
            // right to write through the pointer while the borrow is live.
            Type::StructMut(id) => format!("&mut struct #{}", id),
            Type::Enum(name, _) => name.to_owned(),
            // A single-pass cursor over a string; it owns no memory and only
            // `for` consumes it (ADR-53).
            Type::Tokens => "token cursor".to_owned(),
            // A closure value names its generated function; the captures are
            // hidden parameters, so the value itself has no storage.
            Type::Closure(_) => "closure".to_owned(),
            // A function type: `fn(params) -> ret`, interned in the shared
            // signature registry. Only non-capturing closures can be passed
            // as one (ADR-0189).
            Type::Fn(sig) => {
                let (params, ret) = closures::fn_sig(sig);
                format!("fn({}) -> {}", params.iter().map(|t| t.name()).collect::<Vec<_>>().join(", "), ret.name())
            }
            // A raw pointer (M2.12): an address typed by pointee layout only.
            Type::Ptr(id) => format!("*const {}", ptr_pointee(id).name()),
            Type::PtrMut(id) => format!("*mut {}", ptr_pointee(id).name()),
            // A thread handle (ADR-0214 / spec/concurrency.md): owns its heap
            // slot; the return type rides the interned slot, not the spelling.
            Type::Thread(id) => format!("Thread#{}", id),
            // A copy-semantics handle to one process-lifetime cell
            // (spec/concurrency.md §8): copying shares, nothing moves.
            Type::Atomic => "atomic".to_owned(),
            // A copy-semantics handle to one process-lifetime box holding an
            // owned value (ADR-0216): copying shares, reads copy out.
            Type::Shared(id) => format!("shared[{}]", shared_types_get(id).name()),
            Type::Mutex => "mutex".to_owned(),
            Type::Guard => "guard".to_owned(),
            Type::Chan => "chan".to_owned(),
            Type::Exec => "exec".to_owned(),
            Type::TaskScope => "task_scope".to_owned(),
            Type::Pool => "pool".to_owned(),
        }
    }
    fn c_temp(self) -> String {
        match self {
            Type::Bool => "bool ".to_owned(),
            Type::F64 => "double ".to_owned(),
            Type::String => "ae_string ".to_owned(),
            Type::StrView => "ae_strview ".to_owned(),
            Type::Vec => "ae_vec ".to_owned(),
            Type::StrVec => "ae_strvec ".to_owned(),
            Type::VecF => "ae_f64vec ".to_owned(),
            Type::Map => "ae_map ".to_owned(),
            Type::MapF => "ae_mapf ".to_owned(),
            Type::Struct(k) => format!("ae_t{k} "),
            // A borrow has no storage of its own, so a temporary of these
            // types is never declared; the arms exist so a stray use fails
            // the build instead of formatting a bad C type.
            Type::StructRef(k) => format!("ae_t{k}* "),
            Type::StructMut(k) => format!("ae_t{k}* "),
            Type::Enum(_, k) => format!("ae_e{k} "),
            Type::Tokens => "ae_tokens ".to_owned(),
            // Never declared: a closure has no storage, and this arm exists
            // so a stray use fails the build instead of printing a bad type.
            Type::Closure(_) => "void ".to_owned(),
            Type::Fn(_) => "void ".to_owned(),
            Type::Ptr(id) => format!("const {}* ", ptr_pointee(id).c_temp().trim_end()),
            Type::PtrMut(id) => format!("{}* ", ptr_pointee(id).c_temp().trim_end()),
            Type::Thread(_) => "void *".to_owned(),
            Type::Atomic => "ae_atomic ".to_owned(),
            Type::Shared(_) => "ae_shared ".to_owned(),
            Type::Mutex => "ae_mutex ".to_owned(),
            Type::Guard => "ae_guard ".to_owned(),
            Type::Chan => "ae_chan ".to_owned(),
            Type::Exec => "ae_exec ".to_owned(),
            Type::TaskScope => "ae_task_scope ".to_owned(),
            Type::Pool => "ae_pool ".to_owned(),
            _ => "int64_t ".to_owned(),
        }
    }
    // Owned types carry heap resources: they move, and every live slot is
    // released on scope exit and control-flow jumps. Enums own their payload
    // slots, which may hold strings.
    fn owned(self) -> bool {
        matches!(self, Type::String | Type::Vec | Type::StrVec | Type::VecF | Type::Map | Type::MapF | Type::Struct(_) | Type::Enum(..) | Type::Thread(_) | Type::Guard | Type::TaskScope)
        // A raw pointer is a plain value (M2.12): copying it moves nothing,
        // so it neither moves nor releases.
    }
}
enum MoveKind { Var(usize), Field, StrVecElement }
// Names owned by the prelude rather than by any file on disk.
const PRELUDE: &str = "<prelude>";
// Cap on enum layouts per program, prelude and instantiations included. Each
// layout is a tag enum plus one struct per payload variant, so an unbounded
// instantiation count is a real code-size hazard (ADR-21).
const MAX_ENUM_TYPES: usize = 256;
const MAX_STRUCT_TYPES: usize = 256;
// The interned element types of `shared[T]` handles, indexed by Type::Shared
// (ADR-0216). A thread_local like FN_IDS: one compile per process.
thread_local! {
    static SHARED_TYPES: std::cell::RefCell<Vec<Type>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn intern_shared(elem: Type) -> usize {
    SHARED_TYPES.with(|all| {
        let mut all = all.borrow_mut();
        if let Some(index) = all.iter().position(|existing| *existing == elem) {
            return index;
        }
        all.push(elem);
        all.len() - 1
    })
}

fn shared_types_get(id: usize) -> Type {
    SHARED_TYPES.with(|all| all.borrow()[id])
}

// Built-in names are reserved so their semantics stay unambiguous.
// `pub` is reserved too: it is only meaningful before a top-level item.
const RESERVED: &[&str] = &["len", "push", "pop", "vec", "read_file", "write_file", "bytes", "contains", "slice", "split", "args", "join", "slice_view", "lines", "count_lines_with", "try_read_file", "try_write_file", "parse_int", "to_string", "try_list_dir", "path_exists", "path_is_dir", "path_join", "path_name", "path_parent", "try_file_info", "try_copy_file", "try_rename_path", "try_remove_file", "try_create_dir", "try_remove_dir", "map_new", "f64map_new", "map_put", "map_get", "map_has", "map_remove", "map_inc", "keys", "utf8_len", "utf8_prefix", "pub", "trim", "starts_with", "ends_with", "find", "sort", "reverse", "now_ms", "now_s", "env", "try_append_file", "exit", "read_stdin", "cwd", "json_get", "json_int", "json_bool", "json_len", "json_has", "strvec_new", "f64vec_new", "run_command", "set_cwd", "to_f64", "to_i64", "c_size_of", "c_align_of", "c_offset_of", "tokenize", "sqrt", "abs", "floor", "ceil", "round", "exp", "log", "sin", "cos", "pow", "atan2", "min", "max", "spawn", "wait", "cell_new", "cell_load", "cell_store", "cell_add", "cell_cas", "shared_new", "shared_get", "shared_set", "mutex_new", "mutex_lock", "chan_new", "chan_send", "chan_try_send", "chan_recv", "chan_select", "chan_close", "exec_new", "task_spawn", "task_yield", "exec_run", "scope_new", "scope_spawn", "pool_new", "pool_spawn", "pool_join"];
// The math builtins (ADR-60): ae name, C symbol, arity. Every argument and
// the result is f64, and each call maps 1:1 onto C libm, so domain errors are
// IEEE NaN/-inf instead of language-level failures — unlike the checked `/`
// operator, whose contract is part of the language itself. `abs` maps to
// `fabs`: C's plain `abs` is the integer function in <stdlib.h>.
const MATH_FNS: &[(&str, &str, usize)] = &[
    ("sqrt", "sqrt", 1), ("abs", "fabs", 1), ("floor", "floor", 1),
    ("ceil", "ceil", 1), ("round", "round", 1), ("exp", "exp", 1),
    ("log", "log", 1), ("sin", "sin", 1), ("cos", "cos", 1),
    ("pow", "pow", 2), ("atan2", "atan2", 2),
    // min/max map to runtime symbols, not libm: the NaN-ignoring semantics
    // and the ±0.0 tie rule must be bit-identical on both platforms (ADR-64).
    ("min", "ae_f64_min", 2), ("max", "ae_f64_max", 2),
];
// Provisional prelude: fallible operations report failure as values instead of
// terminating the process (§4.5). These enums are registered before any user
// type, so a program can name them without importing anything. They are
// expected to move into a stdlib module once generics and a module search path
// exist (ADR-11).
fn prelude_enums() -> (Vec<EnumDef>, HashMap<String, usize>) {
    let defs: Vec<(&'static str, Vec<(&'static str, Vec<Type>)>)> = vec![
        ("ReadResult", vec![("Ok", vec![Type::String]), ("Err", vec![Type::String])]),
        ("WriteResult", vec![("Ok", vec![Type::Int]), ("Err", vec![Type::String])]),
        ("IntResult", vec![("Ok", vec![Type::Int]), ("Err", vec![Type::String])]),
        ("DirResult", vec![("Ok", vec![Type::StrVec]), ("Err", vec![Type::String])]),
        // Metadata: byte size, modification time in seconds since the Unix
        // epoch, and whether the target is a directory.
        ("FileResult", vec![("Ok", vec![Type::Int, Type::Int, Type::Bool]), ("Err", vec![Type::String])]),
        // Operations with no value to report: success carries nothing.
        ("StatusResult", vec![("Ok", vec![]), ("Err", vec![Type::String])]),
        // JSON answers: a document field can be a flag, so booleans need their
        // own result shape (there is no "true" placeholder like 0 or "").
        ("BoolResult", vec![("Ok", vec![Type::Bool]), ("Err", vec![Type::String])]),
        // A finished child process: a non-zero status is a result, not an
        // error, so the status travels in Ok next to the output it produced.
        ("CommandResult", vec![("Ok", vec![Type::Int, Type::String]), ("Err", vec![Type::String])]),
        // map[string, f64] lookups (ADR-65). Appended last so every existing
        // prelude enum keeps its id; the runtime names it ae_e8.
        ("FloatResult", vec![("Ok", vec![Type::F64]), ("Err", vec![Type::String])]),
    ];
    let mut enums = Vec::new();
    let mut names = HashMap::new();
    for (name, variants) in defs {
        names.insert(name.to_owned(), enums.len());
        enums.push(EnumDef { name, variants: variants.into_iter().map(|(v, fields)| (v.to_owned(), fields)).collect(), file: PRELUDE.to_owned(), public: true, pattern: name.to_owned() });
    }
    (enums, names)
}

// `Option[T]` ships with the prelude: "a value that may be absent" is the one
// generic shape every program needs, and building it in keeps programs from
// each declaring a slightly different `Maybe`.
fn prelude_generics() -> Generics {
    let mut generics = Generics::default();
    generics.names.insert("Option".to_owned(), 0);
    generics.defs.push(GenericEnum {
        name: "Option".to_owned(),
        params: vec!["T".to_owned()],
        variants: vec![("Some".to_owned(), vec![Payload::Param(0)]), ("None".to_owned(), Vec::new())],
        file: PRELUDE.to_owned(),
        public: true,
    });
    generics
}


// String literals keep their source text with escapes uninterpreted: the bytes
// between the quotes are already valid C string body, so emission can reuse them.
#[derive(Debug, Clone)]
enum Expr { Number(i64), FloatLit(f64), ToF64(Box<Expr>, usize, usize), ToI64(Box<Expr>, Type, usize, usize), FloatToString(Box<Expr>, usize, usize), BoolLit(bool), StrLit(String, usize, usize), Variable(usize, Type, usize, usize), Call(usize, Vec<Argument>, Type), Index(usize, usize, Box<Expr>, usize, usize), VecLit(Vec<Expr>, Type, usize, usize), VecIndex(usize, Box<Expr>, usize, usize, bool, Type), Len(Box<Expr>, usize, usize), ReadFile(Box<Expr>, usize, usize), WriteFile(Box<Expr>, Box<Expr>, usize, usize), Bytes(Box<Expr>, usize, usize), Contains(Box<Expr>, Box<Expr>, usize, usize), Slice(Box<Expr>, Box<Expr>, Box<Expr>, usize, usize), Split(Box<Expr>, Box<Expr>, usize, usize), Args(usize, usize), Join(usize, Box<Expr>, usize, usize), StrVecIndex(usize, Box<Expr>, usize, usize, bool), SliceView(usize, Option<String>, Box<Expr>, Box<Expr>, usize, usize),
    // Lazy token cursor: (base variable, base field, separator, line, column).
    Tokens(usize, Option<String>, Box<Expr>, usize, usize), Lines(Box<Expr>, usize, usize), CountLinesWith(Box<Expr>, Box<Expr>, usize, usize), StructLit(usize, Vec<Expr>), EnumLit(&'static str, usize, usize, Vec<Expr>), TryReadFile(Box<Expr>, Type, usize, usize), TryListDir(Box<Expr>, Type, usize, usize), PathExists(Box<Expr>, usize, usize), PathIsDir(Box<Expr>, usize, usize), TryWriteFile(Box<Expr>, Box<Expr>, Type, usize, usize), ParseInt(Box<Expr>, Type, usize, usize), ToString(Box<Expr>, usize, usize), PathJoin(Box<Expr>, Box<Expr>, usize, usize), PathName(Box<Expr>, usize, usize), PathParent(Box<Expr>, usize, usize), TryFileInfo(Box<Expr>, Type, usize, usize), TryCopyFile(Box<Expr>, Box<Expr>, Type, usize, usize), TryRenamePath(Box<Expr>, Box<Expr>, Type, usize, usize), TryRemoveFile(Box<Expr>, Type, usize, usize), TryRemoveDir(Box<Expr>, Type, usize, usize), TryCreateDir(Box<Expr>, Type, usize, usize), SetCwd(Box<Expr>, Type, usize, usize), MapNew(usize, usize, MapKind), MapGet(MapPlace, Box<Expr>, Type, usize, usize), MapHas(MapPlace, Box<Expr>, usize, usize), MapRemove(MapPlace, Box<Expr>, usize, usize), MapInc(MapPlace, Box<Expr>, Box<Expr>, usize, usize), MapKeys(MapPlace, usize, usize), Utf8Len(Box<Expr>, usize, usize), Utf8Prefix(Box<Expr>, Box<Expr>, usize, usize), Trim(Box<Expr>, usize, usize), StartsWith(Box<Expr>, Box<Expr>), EndsWith(Box<Expr>, Box<Expr>),
    TryAppendFile(Box<Expr>, Box<Expr>, Type, usize, usize), Env(Box<Expr>, Type, usize, usize), JsonGet(Box<Expr>, Box<Expr>, Type, usize, usize), JsonInt(Box<Expr>, Box<Expr>, Type, usize, usize), JsonBool(Box<Expr>, Box<Expr>, Type, usize, usize), JsonLen(Box<Expr>, Box<Expr>, Type, usize, usize), JsonHas(Box<Expr>, Box<Expr>, usize, usize), Find(Box<Expr>, Box<Expr>, Type, usize, usize), ReadStdin(Type, usize, usize), Cwd(Type, usize, usize), StrVecNew(usize, usize), F64VecNew(usize, usize), RunCommand(Box<Expr>, usize, Type, usize, usize), NowMs, NowS, Field(Box<Expr>, String, Type), StrEq(Box<Expr>, Box<Expr>, bool), Neg(Box<Expr>, usize, usize), Not(Box<Expr>),
    // Math builtin (ADR-60): index into MATH_FNS, arguments, source position.
    MathCall(u8, Vec<Box<Expr>>, usize, usize), Binary(Box<Expr>, Kind, Box<Expr>, usize, usize, Type),
    // A closure literal names its generated function and stores nothing
    // (ADR-0188): the declaration emits no C, and the call site reads the
    // captures at the moment of the call. Passed as a function value, the
    // emitted pointer is the literal's function id (ADR-0189).
    ClosureLit(usize), FnCall(usize, Vec<Argument>, Type, usize),
    // ADR-0214 / spec/concurrency.md: spawn moves its capture slots (the
    // leading expressions, in capture order) into a heap context the thread
    // consumes, and wait blocks on the handle and hands back the spawned
    // callable's owned return. The callable is a closure literal or a
    // zero-parameter function value; the return type rides the variant so
    // wait's result is typed without a second lookup.
    Spawn(Vec<Expr>, Box<Expr>, usize, usize, usize), Wait(Box<Expr>, Type, usize, usize),
    // spec/concurrency.md §8: copy-semantics handles over one process-
    // lifetime i64 cell. Every operation is C11 seq_cst; the store is a
    // statement, the rest are expressions. The handle never moves.
    AtomicNew(Box<Expr>), AtomicLoad(Box<Expr>), AtomicAdd(Box<Expr>, Box<Expr>), AtomicCas(Box<Expr>, Box<Expr>, Box<Expr>),
    SharedNew(Box<Expr>, usize), SharedGet(Box<Expr>, usize),
    MutexNew, MutexLock(Box<Expr>),
    ChanNew(Box<Expr>), ChanSend(Box<Expr>, Box<Expr>), ChanTrySend(Box<Expr>, Box<Expr>), ChanRecv(Box<Expr>), ChanSelect(Box<Expr>, Box<Expr>),
    // M4.2 / ADR-0218: single-worker deterministic FIFO task executor.
    ExecNew, ExecRun(Box<Expr>),
    // M4.2.04 / M4.2.07 / ADR-0220: task scope and work-stealing pool. The
    // cancel token moved to std/sync (ADR-0224 R2.4).
    ScopeNew,
    PoolNew(Box<Expr>), PoolJoin(Box<Expr>),
    // M3.2.01 / M3.2.02 / ADR-0221: scripted byte streams and buffered readers.
    // A raw pointer (M2.12 slice 2/3): `&expr as *const T` and
    // `&mut expr as *mut T` take the address of a local; the bool records
    // mutability. `p.deref()` reads through it, inside `unsafe` only.
    #[allow(dead_code)]
    BorrowAs(Box<Expr>, bool, usize, usize), #[allow(dead_code)] DerefPtr(Box<Expr>, usize, usize), #[allow(dead_code)] PtrOffset(Box<Expr>, Box<Expr>, bool, usize, usize), #[allow(dead_code)] PtrCast(Box<Expr>, bool, usize, usize, usize), #[allow(dead_code)] PtrDropInPlace(Box<Expr>, Type, usize, usize), #[allow(dead_code)] PtrWrite(Box<Expr>, Box<Expr>, Type, usize, usize) }
// Which value type a map holds: the builtins dispatch on it (ADR-65).
#[derive(Debug, Clone, Copy, PartialEq)]
enum MapKind { Int, Float }
// A map reachable without a move: either a variable slot or a field of one.
#[derive(Debug, Clone)]
enum MapPlace { Var(usize, MapKind), Field(usize, String, MapKind) }
impl MapPlace {
    fn text(&self) -> String {
        match self {
            MapPlace::Var(id, _) => format!("v{id}"),
            MapPlace::Field(id, name, _) => format!("v{id}.{name}"),
        }
    }
    fn slot(&self) -> usize {
        match self {
            MapPlace::Var(id, _) | MapPlace::Field(id, _, _) => *id,
        }
    }
    fn f64_value(&self) -> bool {
        matches!(self, MapPlace::Var(_, MapKind::Float) | MapPlace::Field(_, _, MapKind::Float))
    }
}
fn map_kind_of(ty: Type) -> MapKind {
    if ty == Type::MapF { MapKind::Float } else { MapKind::Int }
}
fn map_element_ty(kind: MapKind) -> Type {
    if kind == MapKind::Float { Type::F64 } else { Type::Int }
}
#[derive(Debug, Clone)]
enum Argument { Value(Expr), Borrow(usize, bool), View(Expr), FnPtr(usize) } // Borrow bool: pass &slot vs array (decays); View: read-only string borrow; FnPtr: an internal function ID (ADR-0189)
#[derive(Debug, Clone)]
enum Printed { Expr(Expr), Text(String) }
#[derive(Debug, Clone)]
// `BindPayload` exists only in lowered statement lists (M2.3): the CFG's match
// lowering materializes each arm's payload bindings as these statements at the
// top of the arm's block, so the MIR backend can emit them from the block
// graph. The parser never produces one; the tree form of a match keeps its
// bindings on the `MatchArm`.
enum Statement { Return(Expr, Vec<(usize, Type)>), Declare(usize, Expr), Array(usize, Vec<Expr>), Assign(usize, Expr, bool), Store(usize, usize, Expr, Expr, usize, usize), VecPush(usize, Expr, usize, usize), Sort(usize, bool, usize, usize), Reverse(usize, bool, usize, usize), VecPop(usize, usize, usize), VecStore(usize, Expr, Expr, usize, usize), StrVecPush(usize, Expr, usize, usize, bool), StrVecPop(usize, usize, usize), StrVecStore(usize, Expr, Expr, usize, usize, bool), VecFPush(usize, Expr, usize, usize), VecFPop(usize, usize, usize), VecFStore(usize, Expr, Expr, usize, usize), FieldStore(usize, String, Type, Expr, Type), Print(Printed), Assert(Expr, usize, usize), If(Expr, Vec<Statement>, Vec<Statement>), While(Expr, Vec<Statement>), For(usize, usize, Vec<Statement>, usize, usize, usize), ForTokens(usize, usize, Vec<Statement>, usize, usize, usize), ResetTokens(usize, usize), AdvanceTokens(usize, usize, usize, usize), Match(usize, Type, Vec<MatchArm>), BindPayload(usize, usize, usize, usize, Type), TakePropagationPayload(usize, Type, Vec<Type>, Option<(usize, Type)>), MapPut(MapPlace, Expr, Expr, usize, usize), Propagate(Expr, Type, Vec<Type>, Option<(usize, Type)>, Vec<(usize, Type)>), Break, Continue(Option<usize>), Exit(Expr, Vec<(usize, Type)>), Frees(Vec<(usize, Type)>), Unsafe(Vec<Statement>), PtrStore(usize, Expr, usize, usize), AtomicStore(Box<Expr>, Box<Expr>), SharedSet(Box<Expr>, Box<Expr>, usize), ChanClose(Box<Expr>), TaskSpawn(Box<Expr>, Vec<Expr>, Box<Expr>, usize, usize, usize), TaskYield, ScopeSpawn(Box<Expr>, Vec<Expr>, Box<Expr>, usize, usize, usize), PoolSpawn(Box<Expr>, Vec<Expr>, Box<Expr>, usize, usize, usize) }
#[derive(Debug)]
struct Function { id: usize, params: Vec<(usize, Type)>, body: Vec<Statement>, is_main: bool, returns: Type }
fn returns(stmts: &[Statement]) -> bool {
    stmts.iter().any(|s| match s {
        Statement::Return(..) => true,
        // Terminating the process is a valid way to end a function that never
        // produces a value.
        Statement::Exit(..) => true,
        Statement::If(_, yes, no) => returns(yes) && returns(no),
        // An `unsafe` block is a scoping container: a `return` inside it
        // returns from the enclosing function exactly as a bare one does.
        Statement::Unsafe(body) => returns(body),
        // Exhaustive coverage means the match always runs one arm, so the
        // match returns whenever every arm does.
        Statement::Match(_, _, arms) => !arms.is_empty() && arms.iter().all(|arm| returns(&arm.body)),
        _ => false,
    })
}
#[derive(Debug, Clone)]
struct StructDef { name: String, fields: Vec<(String, Type)>, file: String, public: bool, repr_c: bool, drop_fn: Option<usize> }

impl StructDef {
    // The C layout of a `#[repr(C)]` struct: each field sits at its natural
    // alignment (C padding rules), the struct's alignment is the maximum
    // field alignment, and the size rounds up to that alignment (trailing
    // padding). A field of owned type cannot appear — the attribute parse
    // rejects it — so scalars and nested `#[repr(C)]` structs cover all
    // shapes. Returns None for a struct without the attribute: its layout
    // is the compiler's choice, not a promise.
    fn c_layout(&self, structs: &[StructDef]) -> Option<(i64, i64, Vec<i64>)> {
        if !self.repr_c { return None; }
        let (mut offset, mut max_align) = (0i64, 1i64);
        let mut offsets = Vec::with_capacity(self.fields.len());
        for (_, ty) in &self.fields {
            let (size, align) = match ty {
                Type::Int | Type::F64 | Type::Bool => ty.scalar_c_layout()?,
                Type::Struct(k) => {
                    let (inner_size, inner_align, _) = structs[*k].c_layout(structs)?;
                    (inner_size, inner_align)
                }
                _ => return None,
            };
            offset = (offset + align - 1) / align * align;
            offsets.push(offset);
            offset += size;
            max_align = max_align.max(align);
        }
        let size = (offset + max_align - 1) / max_align * max_align;
        Some((size, max_align, offsets))
    }
}
// Enum layouts are closed: variants carry a fixed field list, and the first
// version restricts payload fields to i64/bool/string so no enum can contain
// itself and every payload has a trivial release.
#[derive(Debug, Clone)]
struct EnumDef { name: &'static str, variants: Vec<(String, Vec<Type>)>, file: String, public: bool, pattern: String }

impl EnumDef {
    fn c_layout(&self, structs: &[StructDef]) -> (i64, i64) {
        let tag_size = 4i64;
        let tag_align = 4i64;
        let mut max_payload_size = 0i64;
        let mut max_payload_align = 1i64;
        for (_, fields) in &self.variants {
            let (mut v_offset, mut v_max_align) = (0i64, 1i64);
            for ty in fields {
                let (sz, al) = match ty {
                    Type::Int | Type::F64 | Type::Bool => ty.scalar_c_layout().unwrap_or((8, 8)),
                    Type::String => (16, 8),
                    Type::Struct(k) => structs[*k].c_layout(structs).map(|(s, a, _)| (s, a)).unwrap_or((8, 8)),
                    _ => (8, 8),
                };
                v_offset = (v_offset + al - 1) / al * al;
                v_offset += sz;
                v_max_align = v_max_align.max(al);
            }
            let v_size = (v_offset + v_max_align - 1) / v_max_align * v_max_align;
            max_payload_size = max_payload_size.max(v_size);
            max_payload_align = max_payload_align.max(v_max_align);
        }
        if max_payload_size == 0 {
            max_payload_size = 1;
        }
        let total_align = tag_align.max(max_payload_align);
        let union_offset = (tag_size + max_payload_align - 1) / max_payload_align * max_payload_align;
        let total_size = (union_offset + max_payload_size + total_align - 1) / total_align * total_align;
        (total_size, total_align)
    }
}
// A payload type in a generic declaration: either fixed, or one of the
// declaration's type parameters. Substitution replaces every Param with a
// concrete argument, so Param never reaches code generation.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Payload { Concrete(Type), Param(usize), Nested(usize) }
// The shape behind `Payload::Nested`: a generic declaration (struct or enum)
// and the argument payloads it was spelled with, at least one of which is
// itself a type parameter of the enclosing declaration. Recorded at
// declaration-parse time; instantiated recursively per argument list.
#[derive(Debug, Clone)]
struct NestedShape { gid: usize, is_struct: bool, args: Vec<Payload> }
// `enum Option[T] { Some(T), None }` before it is instantiated. Kept apart from
// EnumDef because a declaration has no layout: only instantiations do.
#[derive(Debug, Clone)]
struct GenericEnum { name: String, params: Vec<String>, variants: Vec<(String, Vec<Payload>)>, file: String, public: bool }
// A trait declares the method signatures every impl must provide. The trait's
// type parameter is always spelled `T`; `Payload::Param(0)` marks the positions
// that stand for it, and every other type must be concrete. V1 deliberately
// has no generic impls and no default method bodies.
#[derive(Debug, Clone)]
struct TraitMethod { name: String, params: Vec<Payload>, returns: Payload }
#[derive(Debug, Clone)]
struct TraitDef { name: String, methods: Vec<TraitMethod>, file: String, public: bool }
// `struct Pair[K, V] { left: K, right: V }` before it is instantiated. Same
// reason as GenericEnum: a declaration is a shape, not a layout.
#[derive(Debug, Clone)]
struct GenericStruct { name: String, params: Vec<String>, fields: Vec<(String, Payload)>, file: String, public: bool }
// A constraint on a generic function's type parameter is a *shape check* at
// the call site, not a proof about the generic body (ADR-33). The body is still
// parsed and type-checked once per instantiation, so a constraint can only
// reject an argument earlier and with a better message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bound { Add, Eq, Ord, Copy }
impl Bound {
    fn name(self) -> &'static str {
        match self { Bound::Add => "Add", Bound::Eq => "Eq", Bound::Ord => "Ord", Bound::Copy => "Copy" }
    }
    fn allows(self, ty: Type) -> bool {
        match self {
            Bound::Copy => matches!(ty, Type::Int | Type::Bool | Type::F64),
            Bound::Eq => matches!(ty, Type::Int | Type::Bool | Type::String | Type::F64),
            Bound::Ord => matches!(ty, Type::Int | Type::F64),
            Bound::Add => matches!(ty, Type::Int | Type::String | Type::F64),
        }
    }
    fn allowed_types(self) -> &'static str {
        match self {
            Bound::Copy => "i64, bool, f64",
            Bound::Eq => "i64, bool, string, f64",
            Bound::Ord => "i64, f64",
            Bound::Add => "i64, string, f64",
        }
    }
}

// ADR-40. A generic declaration that writes constraints must not admit a type
// that its body cannot use. This is a diagnostic, not a proof: every
// instantiation is still parsed and fully type-checked (ADR-33). The audit can
// only reject a declaration; it never lets unchecked code through.
const AUDIT_TYPES: &[Type] = &[
    Type::Int, Type::Bool, Type::F64, Type::String, Type::StrView,
    Type::Vec, Type::StrVec, Type::VecF, Type::VecRef, Type::StrVecRef, Type::VecFRef, Type::Map, Type::MapF,
    Type::Array(0), Type::Struct(0), Type::StructRef(0), Type::StructMut(0), Type::Enum("audit", 0),
];

// Capabilities mirror the binary-operator rules in `expression`. They live
// next to `Bound::allows` so adding a scalar type cannot silently desync the
// constraint whitelist and the real type checker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capability { Add, Arith, Modulo, Ord, Eq, Logic }
impl Capability {
    fn from_op(op: &Kind) -> Option<Capability> {
        match op {
            Kind::Plus => Some(Capability::Add),
            Kind::Minus | Kind::Star | Kind::Slash => Some(Capability::Arith),
            Kind::Percent => Some(Capability::Modulo),
            Kind::Less | Kind::LessEq | Kind::Greater | Kind::GreaterEq => Some(Capability::Ord),
            Kind::EqEq | Kind::NotEq => Some(Capability::Eq),
            Kind::And | Kind::Or => Some(Capability::Logic),
            _ => None,
        }
    }
    fn symbol(self) -> &'static str {
        match self {
            Capability::Add => "'+'", Capability::Arith => "arithmetic",
            Capability::Modulo => "'%'", Capability::Ord => "ordering comparison",
            Capability::Eq => "equality comparison", Capability::Logic => "'&&' / '||'",
        }
    }
    fn supports(self, ty: Type) -> bool {
        match self {
            Capability::Add => matches!(ty, Type::Int | Type::F64 | Type::String | Type::StrView),
            Capability::Arith => matches!(ty, Type::Int | Type::F64),
            Capability::Modulo => matches!(ty, Type::Int),
            Capability::Ord => matches!(ty, Type::Int | Type::F64),
            Capability::Eq => matches!(ty, Type::Int | Type::F64 | Type::Bool | Type::String | Type::StrView),
            Capability::Logic => matches!(ty, Type::Bool),
        }
    }
    fn supported_types(self) -> &'static str {
        match self {
            Capability::Add => "i64, f64, string",
            Capability::Arith => "i64, f64",
            Capability::Modulo => "i64",
            Capability::Ord => "i64, f64",
            Capability::Eq => "i64, f64, bool, string",
            Capability::Logic => "bool",
        }
    }
}

static AUDIT_EOF: Kind = Kind::Eof;
// Borrow instead of cloning: the audit walks every token of a declaration, and
// cloning `Kind::Ident(String)` on each visit would add a heap allocation per
// identifier for a check that must stay cheap on the front-end path.
#[derive(Debug, Clone)]
struct GenericFn { name: String, bindings: Option<Rc<[ae_syntax::declaration::TypedName]>>, params: Vec<String>, bounds: Vec<Vec<Bound>>, trait_bounds: Vec<Vec<usize>>, start: usize, tokens: Rc<Vec<Token>>, source: Rc<str>, file: String, public: bool }
// An instantiated generic function: its id, parameter types and return type.
type Instance = (usize, Vec<Type>, Type);
// Instantiated generic enums, structs and functions, keyed by declaration and
// arguments (ADR-21, ADR-30, ADR-31). `next_fn_id` is the one id space shared
// by ordinary and instantiated functions, so C names never collide.
#[derive(Default, Clone)]
struct Generics { defs: Vec<GenericEnum>, names: HashMap<String, usize>, done: HashMap<String, usize>, struct_defs: Vec<GenericStruct>, struct_names: HashMap<String, usize>, struct_done: HashMap<String, usize>, struct_origin: HashMap<usize, (usize, Vec<Type>)>, enum_origin: HashMap<usize, (usize, Vec<Type>)>, fn_defs: Vec<GenericFn>, fn_names: HashMap<String, usize>, fn_done: HashMap<String, Instance>, fn_in_progress: std::collections::HashSet<String>, next_fn_id: usize, traits: Vec<TraitDef>, trait_names: HashMap<String, usize>, impls: std::collections::HashSet<(usize, String)>, inherent: HashMap<String, MethodDef>, generic_impls: HashMap<String, GenericImplDef>, generic_impl_done: HashMap<String, Instance>, impl_in_progress: std::collections::HashSet<String>, closures: Vec<ClosureDef>, threads: Vec<Type>, nested_shapes: Vec<NestedShape>, instantiation_trace: Vec<(&'static str, String, String)> }
#[derive(Debug, Clone)]
struct MatchArm { variant: usize, bindings: Vec<(usize, Type, usize)>, body: Vec<Statement> }

// A function signature as seen by importers. `file` and `public` let a call
// site reject a name that exists but was not exported (ADR-18).
#[derive(Debug, Clone)]
struct Signature { id: usize, params: Vec<Type>, ret: Type, file: String, public: bool, extern_c: bool }

// The type an inherent impl block targets (ADR-0186). Both forms resolve
// methods through the same (type, method) name space, and `Self` inside the
// block stands for exactly this type. Enum receivers are by value only.
#[derive(Debug, Clone, Copy)]
enum ImplTarget { Struct(usize), Enum(usize) }
fn sid_of(target: &ImplTarget) -> usize {
    match target { ImplTarget::Struct(sid) => *sid, ImplTarget::Enum(_) => unreachable!("enum receivers are by value only") }
}

// A method declared inside a generic impl block (ADR-0187). Only the shape
// the call site needs is stored: the name, how the receiver was spelled, and
// where the signature starts. The signature and body are parsed once per
// (method, argument list) pair with the impl's parameters substituted —
// exactly how a generic function instantiates — so signatures may use the
// parameters anywhere a type can appear.
#[derive(Debug, Clone)]
struct GenericImplMethod { name: String, receiver: Option<ReceiverKind>, body_start: usize, tokens: Rc<Vec<Token>>, source: Rc<str> }
// How a generic impl method receives its receiver; the receiver's concrete
// type is built from the instantiation, never spelled in the signature.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ReceiverKind { ByValue, Shared, Mutable }
// One generic impl block: `impl Pair[K, V] { ... }`. Coherence keeps it in
// the file that declares the struct, one per generic struct.
#[derive(Debug, Clone)]
struct GenericImplDef { params: Vec<String>, methods: Vec<GenericImplMethod>, file: String }

// A closure literal's contract (ADR-0188). The generated function takes the
// captured variables as hidden leading parameters, then the visible ones:
// a value capture moves its outer variable at each call (the callee owns the
// copy), a borrow capture forwards a read-only borrow of storage that stays
// with the enclosing scope. A closure value itself has no storage — it is a
// name for the function id — so it cannot be stored, returned, or moved.
#[derive(Debug, Clone)]
struct ClosureDef { fn_id: usize, params: Vec<Type>, ret: Type, value_captures: Vec<(usize, Type)>, borrow_captures: Vec<(usize, Type)> }

// One entry of an inherent `impl` block. `receiver` records the type the first
// parameter was bound to — the struct by value for `self`, a read-only borrow
// for `&self` — and `None` marks an associated function. The call form follows
// from it: a method is reached through a value, an associated function through
// the type name, and a borrowed receiver is auto-borrowed at the call site.
#[derive(Debug, Clone)]
struct MethodDef { signature: Signature, receiver: Option<Type> }

type SharedFunctions = HashMap<String, Signature>;

// The kind of a persistent borrow the parser registers. A **View** borrow
// hangs off a view binding (`slice_view`/`tokenize`) whose NLL region dies
// after the view's last use, so a write conflict on it can be deferred to
// the graph's adjudication. A **Container** borrow is the whole-statement
// shape — a for's container, a match's subject, a strvec element read, a
// closure capture — whose region runs to the scope's end and whose
// rejections stay with the parser.
#[derive(Clone, Copy, PartialEq)]
enum BorrowKind { View, Container }

#[derive(Clone, Copy)]
enum IndexedStatement {
    Binding { name_start: usize, name_end: usize, mutable: bool },
    For { binding_start: usize, binding_end: usize, is_range: bool },
    Print(usize),
    Assert(usize),
    Break,
    Continue,
}

#[derive(Default)]
struct ResolvedExpressions {
    local_bindings: HashMap<usize, std::ops::Range<usize>>,
    direct_calls: HashSet<usize>,
    unresolved_calls: HashSet<usize>,
    specialization_arguments: HashMap<usize, std::sync::Arc<[std::ops::Range<usize>]>>,
    specialized_call_arguments: HashMap<usize, Rc<[ae_syntax::expression::Expression]>>,
    specialized_struct_fields: HashMap<usize, (usize, Rc<[ae_syntax::expression::FieldInitializer]>)>,
    struct_constructors: HashSet<usize>,
    enum_variants: HashSet<usize>,
    pointer_cast_targets: HashMap<usize, bool>,
    closure_headers: HashMap<usize, std::sync::Arc<ae_syntax::expression::Expression>>,
    values: HashMap<usize, Rc<ae_syntax::expression::Expression>>,
    statements: HashMap<usize, (IndexedStatement, usize)>,
    pointer_store_names: HashMap<usize, String>,
    match_patterns: HashMap<usize, ae_syntax::pattern::Pattern>,
    scalar_bodies: HashMap<usize, std::sync::Arc<[ae_syntax::statement::Statement]>>,
    scalar_definitions: HashMap<usize, ae_resolve::DefId>,
    scalar_reads: HashMap<usize, ae_resolve::DefId>,
    scalar_pointer_receivers: HashSet<usize>,
    scalar_address_casts: HashSet<usize>,
    numeric_vector_definitions: HashSet<ae_resolve::DefId>,
    owned_numeric_vectors: HashSet<ae_resolve::DefId>,
    scalar_expression_types: HashMap<std::ops::Range<usize>, ae_resolve::resolve::ExpressionType>,
    scalar_writes: HashMap<usize, ae_resolve::DefId>,
    array_definitions: HashSet<ae_resolve::DefId>,
    struct_definitions: HashSet<ae_resolve::DefId>,
    struct_field_reads: HashSet<usize>,
    struct_field_chains: HashSet<usize>,
    drop_free_constructions: HashSet<usize>,
}

struct Parser<'a> { file: String, source: &'a str, tokens: Rc<Vec<Token>>, shared_source: Rc<str>, pos: usize, names: HashMap<String, (usize, bool, Type)>, binding_ranges: HashMap<usize, std::ops::Range<usize>>, resolved_expressions: Rc<OnceCell<ResolvedExpressions>>, next_var: usize, expression_depth: usize, expression_nodes: usize, block_depth: usize, loop_depth: usize, loop_scopes: Vec<usize>, loop_labels: Vec<Option<usize>>, next_label: usize, unsafe_depth: usize, functions: &'a mut SharedFunctions, structs: &'a mut Vec<StructDef>, struct_names: &'a mut HashMap<String, usize>, enums: &'a mut Vec<EnumDef>, enum_names: &'a mut HashMap<String, usize>, generics: &'a mut Generics, id_order: Vec<usize>, in_main: bool, return_type: Type, moved: std::collections::HashSet<usize>, borrows: Vec<(usize, usize, BorrowKind)>, deferred_write_conflicts: Vec<(usize, usize, String, &'static str, Option<String>)>, deferred_borrow_spawns: Vec<(usize, Vec<usize>, usize)>, call_borrows: Vec<(usize, usize)>, mut_borrows: Vec<(usize, usize)>, plus_operand_depth: usize, scopes: Vec<Vec<(usize, Type)>>, subst: HashMap<String, Type>, trait_bounds: HashMap<String, Vec<usize>>, receiver: Option<Type>, impl_self: Option<Type>, shadow_ok: bool, pending: Vec<Function> }
#[derive(Debug, Default)]
struct Program { files: Vec<Vec<Function>>, structs: Vec<StructDef>, enums: Vec<EnumDef> }

fn make_parser<'a>(file: &str, source: &'a str, tokens: Rc<Vec<Token>>, functions: &'a mut SharedFunctions, structs: &'a mut Vec<StructDef>, struct_names: &'a mut HashMap<String, usize>, enums: &'a mut Vec<EnumDef>, enum_names: &'a mut HashMap<String, usize>, generics: &'a mut Generics) -> Parser<'a> {
    Parser { file: file.to_owned(), source, tokens, shared_source: Rc::from(source), pos: 0, names: HashMap::new(), binding_ranges: HashMap::new(), resolved_expressions: Rc::default(), next_var: 0, expression_depth: 0, expression_nodes: 0, block_depth: 0, loop_depth: 0, loop_scopes: Vec::new(), loop_labels: Vec::new(), next_label: 0, unsafe_depth: 0, functions, structs, struct_names, enums, enum_names, generics, id_order: Vec::new(), in_main: false, return_type: Type::Int, moved: std::collections::HashSet::new(), borrows: Vec::new(), deferred_write_conflicts: Vec::new(), deferred_borrow_spawns: Vec::new(), call_borrows: Vec::new(), mut_borrows: Vec::new(), plus_operand_depth: 0, scopes: Vec::new(), subst: HashMap::new(), trait_bounds: HashMap::new(), receiver: None, impl_self: None, shadow_ok: false, pending: Vec::new() }
}

#[allow(dead_code)] // exercised by in-memory unit tests
pub fn check(source: &str) -> Result<(), String> {
    check_source_diagnostic(source).map_err(|diagnostic| diagnostic.to_string())
}

pub(crate) fn check_source_diagnostic(source: &str) -> Result<(), FrontendDiagnostic> {
    let mut shared: SharedFunctions = HashMap::new();
    let mut structs: Vec<StructDef> = Vec::new();
    let mut struct_names: HashMap<String, usize> = HashMap::new();
    // Prelude result types are registered first, so their ids are stable.
    let (mut enums, mut enum_names) = prelude_enums();
    let mut generics = prelude_generics();
    let mut next_id = 0usize;
    parse_source("", source, &mut shared, &mut structs, &mut struct_names, &mut enums, &mut enum_names, &mut generics, &mut next_id).map(|_| ())
}
#[allow(dead_code)] // exercised by in-memory unit tests
pub fn compile(source: &str) -> Result<String, String> {
    let mut shared: SharedFunctions = HashMap::new();
    let mut structs: Vec<StructDef> = Vec::new();
    let mut struct_names: HashMap<String, usize> = HashMap::new();
    // Prelude result types are registered first, so their ids are stable.
    let (mut enums, mut enum_names) = prelude_enums();
    let mut generics = prelude_generics();
    let mut next_id = 0usize;
    let functions = parse_source("", source, &mut shared, &mut structs, &mut struct_names, &mut enums, &mut enum_names, &mut generics, &mut next_id).map_err(|diagnostic| diagnostic.to_string())?;
    emit_program(Program { files: vec![functions], structs, enums }).map(|g| g.full)
}
/// One-paragraph explanation for each stable diagnostic code, shared by
/// `ae explain` and any editor that wants the text without the compiler.
/// Wrong code means the catalog must be updated, never that the code is reused.
pub fn explain(code: &str) -> Option<&'static str> {
    Some(match code {
        "E0001" => "The source contains a byte that starts no token the lexer knows. Check for a stray symbol, a brace from another language, or a character that needs to be inside a string.",
        "E0002" => "A string literal uses an escape the language does not define. The supported escapes are \\n, \\t, \\r, \\\\, and \\\".",
        "E0003" => "A string literal is not closed before the end of the file. Add the missing quote.",
        "E0004" => "A string literal contains a raw newline or NUL byte. Write \\n instead of a literal line break.",
        "E0005" => "An integer literal does not fit in i64. Use a smaller constant or compute the value at run time.",
        "E0006" => "A float literal does not parse as a finite decimal number. Check the digits and the exponent.",
        "E0007" => "A float literal overflows to infinity. The value cannot be represented as an f64 constant.",
        "E0008" => "A nonzero float literal is too small and would round to zero. Use a larger magnitude or a different unit.",
        "E0101" => "A name is used that no binding in scope declares. Check the spelling and that the declaration is in an enclosing block, or before this use.",
        "E0102" => "A value is used, borrowed, or assigned after ownership of it moved elsewhere. Clone or rebuild the value, or restructure the code so the moved value is not needed afterwards.",
        "E0103" => "A binding declared with let is assigned or modified. Declare it with var if it is meant to change; parameters are also immutable.",
        "E0104" => "Two bindings with the same name exist in one scope. Rename one of them; inner blocks may shadow, but a scope cannot bind a name twice.",
        "E0010" => "The parser expected a specific token and found another. The message names both; usually a missing or extra bracket, comma, or semicolon earlier in the line is the real cause.",
        "E0011" => "An expression was required but the parser found something that cannot start one. Check for a missing operand, a doubled operator, or a statement written where a value belongs.",
        "E0201" => "An index expression is not i64. Array and vector indices must be integers; convert an f64 with to_i64 instead of relying on truncation.",
        "E0202" => "Comparison operands do not form one comparable pair. Both sides must be i64, both f64, or both string or string view, and only == and != accept strings.",
        "E0203" => "An arithmetic operator received an operand that is neither i64 nor f64 (unary minus is the same rule). Arithmetic never converts implicitly; use to_f64 or to_i64 explicitly.",
        "E0204" => "A return expression's type differs from the declared return type. Return position has no implicit conversions.",
        "E0205" => "A condition is not boolean. if, while and assert require a bool; comparisons produce bool, integers and strings do not.",
        "E0206" => "print does not accept this type. Print scalars, strings, string views, or a value with a Display impl; containers must be formatted element by element.",
        "E0207" => "The named function or method is not declared. Check the spelling and that the declaration is in scope or in the imported module.",
        "E0700" => "A raw pointer is dereferenced or written outside an unsafe block. Wrap the operation in unsafe and document the invariant that makes it sound.",
        "E0701" => "An unsafe fn or a declared extern is called outside an unsafe block. Wrap the call in unsafe; the caller takes responsibility for the function's preconditions.",
        "E0704" => "An unsafe fn has no doc comment above it. Write a /// comment naming what the caller must guarantee (validity, aliasing, thread context); presence is checked, truth is the author's job.",
        _ => return None,
    })
}

pub fn compile_path(path: &Path) -> Result<String, String> {
    compile_path_parts(path).map(|(runtime, user)| runtime + &user)
}
/// The build driver's incremental view: `(runtime_prefix, program_suffix)`.
/// Concatenating the two reproduces `compile_path` exactly, so `emit-c` stays
/// the authority for what a program means.
pub fn compile_path_parts(path: &Path) -> Result<(String, String), String> {
    let generated = emit_program(load_program(path)?)?;
    let GeneratedC { full, runtime_len } = generated;
    let user = full[runtime_len..].to_owned();
    let mut runtime = full;
    runtime.truncate(runtime_len);
    Ok((runtime, user))
}

/// A compilation result together with the exact source units behind it.
/// The driver caches `runtime + user` and later re-checks every unit's digest;
/// nothing here decides whether the output is current.
pub struct TrackedC { pub runtime: String, pub user: String, pub units: Vec<SourceUnit> }

/// The lowered statement tree, printed as text: `ae emit-tree`.
pub fn tree_path(path: &Path) -> Result<String, String> {
    tree::tree_path(path)
}

/// The control-flow graph of each function: blocks whose contents are the
/// statement tree's own statements, printed as text (`ae emit-cfg`).
pub fn cfg_path(path: &Path) -> Result<String, String> {
    cfg::cfg_path(path)
}

/// The MIR: places, rvalues, and explicit temporaries over the CFG, printed
/// as text (`ae emit-mir`).
pub fn mir_path(path: &Path) -> Result<String, String> {
    mir::mir_path(path)
}

/// Which C backend the driver will pick (M2.3). Part of the frontend cache's
/// query key: the two backends produce different C for the same sources.
pub fn codegen_mode() -> &'static str {
    if codegen_mir::enabled() { "mir" } else { "tree" }
}

/// Execute a program directly inside the compiler's MIR/CFG interpreter
/// (`ae test --interp` / `ae run --interp`).
pub fn interpret_path(path: &Path) -> Result<(String, u8), (String, String)> {
    interp::interpret_path(path)
}

/// Compile a file and report what the output depends on, in one pass. This is
/// the only entry point that exposes the unit list; the plain `compile_path`
/// forms stay the authority for what a program means.
pub fn compile_path_tracked(path: &Path) -> Result<TrackedC, String> {
    let t0 = std::time::Instant::now();
    let (program, units) = loader::load_with_units(path).map_err(String::from)?;
    let t1 = std::time::Instant::now();
    let GeneratedC { full, runtime_len } = emit_program(program)?;
    let t2 = std::time::Instant::now();
    if std::env::var_os("AE_PHASE_SPLIT").is_some() {
        eprintln!("ae phase: load_ms={:.3} emit_ms={:.3}", (t1-t0).as_secs_f64()*1000.0, (t2-t1).as_secs_f64()*1000.0);
    }
    let user = full[runtime_len..].to_owned();
    let mut runtime = full;
    runtime.truncate(runtime_len);
    Ok(TrackedC { runtime, user, units })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn rejects_invalid_semantics() {
        for s in ["fn main() { let x = 1; x = 2; }", "fn main() { print(x); }", "fn main() { let x = x; }", "fn main() { var x = 1; let x = 2; }", "fn main() { print(9223372036854775808); }", "fn main() { print(1) }", "fn main() {} junk"] { assert!(compile(s).is_err(), "{s}"); }
    }
    #[test] fn rejects_type_errors() {
        for s in [
            "fn main() { if 1 { print(1); } }",
            "fn main() { while 1 { break; } }",
            "fn main() { print(true + 1); }",
            "fn main() { if true < false { print(1); } }",
            "fn main() { if 1 < true { print(1); } }",
            "fn main() { let x = 1; x = true; }",
            "fn main() { let b = true; b = 1; }",
            "fn main() { print(!1); }",
            "fn main() { print(-true); }",
            "fn main() { assert(1); }",
            "fn main() { let a = [true]; }",
            "fn main() { let x = 1 && true; }",
            "fn main() { if 1 == 1 && 2 { print(1); } }",
        ] { assert!(compile(s).is_err(), "{s}"); }
    }
    #[test] fn accepts_typed_programs() {
        for s in [
            "fn main() { let flag = true; if flag && !false { print(true); } }",
            "fn main() { print(true == true); print(false != true); }",
            "fn is_even(n: i64) -> bool { return n % 2 == 0; } fn main() { print(is_even(4) || is_even(3)); }",
            "fn pick(flag: bool, a: i64, b: i64) -> i64 { if flag { return a; } else { return b; } } fn main() { print(pick(1 < 2, 3, 4)); }",
            "fn main() { print(\"hello, 世界\"); print(true); print(1 < 2 && 2 < 3); assert(1 == 1); }",
        ] { assert!(compile(s).is_ok(), "{s}"); }
    }
    #[test] fn rejects_invalid_string_literals() {
        for s in ["fn main() { print(\"bad \\q\"); }", "fn main() { print(\"open }", "fn main() { print(\"\n\"); }"] { assert!(compile(s).is_err(), "{s}"); }
    }
    #[test] fn diagnostics_include_unicode_aware_position() {
        let e = compile("// 中文\nfn main() { print(@); }").unwrap_err();
        assert!(e.starts_with("2:19:"), "{e}");
    }
    #[test] fn malformed_inputs_do_not_panic() {
        // Deterministic corpus of arbitrary UTF-8 tokens and punctuation.
        let alphabet = ["a", "0", " ", "\n", "{", "}", "[", "]", "(", ")", ";", "+", "=", "/", "\"", "\\", "中", "🙂", "\0"];
        let mut state = 0x953c_16a7_4d82_13efu64;
        for _ in 0..5000 {
            let mut text = String::new();
            state ^= state << 13; state ^= state >> 7; state ^= state << 17;
            let len = (state as usize) % 150;
            for _ in 0..len {
                state ^= state << 13; state ^= state >> 7; state ^= state << 17;
                text.push_str(alphabet[(state as usize) % alphabet.len()]);
            }
            let result = std::panic::catch_unwind(|| compile(&text));
            assert!(result.is_ok(), "compiler panicked on {text:?}");
        }
    }
    #[test] fn token_positions_handle_unicode_and_newlines() {
        let tokens = lex("// 😀\nfn main() {\n print(1 + 2);\n}").unwrap();
        let plus = tokens.iter().find(|t| t.kind == Kind::Plus).unwrap();
        assert_eq!((plus.line, plus.column), (3, 10));
        let eof = tokens.last().unwrap();
        assert_eq!((eof.line, eof.column), (4, 2));
    }
    #[test] fn arithmetic_and_variables_compile() {
        assert!(compile("fn main() { let x = 2; var y = x + 3 * 4; y = y / 2; print(y); }").is_ok());
    }
    #[test]
    fn runtime_units_attach_leading_comments_to_the_function() {
        // A `//` comment (and the blank lines above it) belongs to the function
        // below, so pruning that function also drops its explanation instead of
        // leaving it in every generated program.
        let units = runtime_units("// explanation\n\nstatic void ae_x(void) { }\n");
        let function = units.iter().find(|(name, _)| name.is_some()).expect("function unit");
        assert!(function.1.starts_with("// explanation"), "{}", function.1);
        // A comment above code that is not a function stays in the always-kept
        // text, otherwise the file header would vanish with the first pruned
        // function.
        let units = runtime_units("// header\n#include <stdio.h>\n");
        assert!(units.iter().all(|(name, _)| name.is_none()));
        assert!(units[0].1.contains("// header"));
        // A multi-line block comment is not moved into a function unit.
        let units = runtime_units("/* banner\n   here */\nstatic void ae_y(void) { }\n");
        let function = units.iter().find(|(name, _)| name.is_some()).expect("function unit");
        assert!(!function.1.contains("banner"), "{}", function.1);
    }

    #[test] fn runtime_units_name_marker_regions_across_their_functions() {
        // `// unit: <name>` turns the following non-function text into a prune
        // unit with that name; the name spans the region's functions, because
        // globals are often interrupted by the functions serving them, and
        // `// unit: -` ends the region.
        let units = runtime_units(concat!(
            "static int ae_one(void) { return 1; }\n",
            "// unit: ae_state\n",
            "// Region doc.\n",
            "static int ae_state_g = 0;\n",
            "static int ae_get(void) { return ae_state_g; }\n",
            "// unit: -\n",
            "static int ae_loose = 5;\n",
            "static int ae_last(void) { return ae_loose; }\n",
        ));
        let named: Vec<&str> = units.iter().filter_map(|(name, _)| name.as_deref()).collect();
        assert_eq!(named, ["ae_one", "ae_state", "ae_get", "ae_last"], "{named:?}");
        let region = units.iter().find(|(name, _)| name.as_deref() == Some("ae_state")).unwrap();
        assert!(region.1.contains("// Region doc.") && region.1.contains("ae_state_g = 0"), "{}", region.1);
        let loose = units.iter().find(|(name, body)| name.is_none() && body.contains("ae_loose")).unwrap();
        assert!(loose.1.contains("ae_loose = 5"), "{}", loose.1);
        // The marker lines themselves never reach the output.
        assert!(!units.iter().any(|(_, body)| body.contains("// unit:")), "{named:?}");
    }

    #[test] fn prune_runtime_drops_unneeded_windows_header() {
        let prefix = concat!(
            "#ifdef _WIN32\n",
            "#ifndef WIN32_LEAN_AND_MEAN\n",
            "#define WIN32_LEAN_AND_MEAN\n",
            "#endif\n",
            "#include <windows.h>\n",
            "#endif\n",
            "static void ae_plain(void) { }\n",
            "static void ae_win(void) { /* win32-header */ }\n",
        );
        let (plain, len) = prune_runtime(prefix, "", &[], "int main(void) { ae_plain(); return 0; }\n");
        assert!(plain[..len].contains(WIN32_HEADER_OMITTED), "header should be dropped");
        assert!(!plain[..len].contains(WIN32_HEADER_INCLUDE), "include line must be gone");
        assert!(!plain.contains("ae_win"), "unreachable Win32 function must be pruned");

        let (win, len) = prune_runtime(prefix, "", &[], "int main(void) { ae_win(); return 0; }\n");
        assert!(win[..len].contains(WIN32_HEADER_INCLUDE), "header must stay for Win32 functions");
        assert!(win[..len].contains(WIN32_HEADER_MARKER));
    }
    #[test] fn every_win32_dependent_runtime_function_is_marked() {
        // Audit only: this token list does not drive the build. A future Win32
        // helper added without the marker either keeps the header (harmless) or
        // fails to compile with a missing declaration, which the generated-C
        // suite catches. A missing marker can never change runtime behaviour.
        const WIN32_DECLS: &[&str] = &[
            "wchar_t", "MultiByteToWideChar", "WideCharToMultiByte", "CP_UTF8",
            "FILETIME", "GetSystemTimeAsFileTime", "GetEnvironmentVariableW",
            "GetCurrentDirectoryW", "GetFileAttributesW", "WIN32_FIND_DATAW",
            "WIN32_FILE_ATTRIBUTE_DATA", "FindFirstFileW", "CreateProcessW",
            "STARTUPINFOW", "GetLastError", "RemoveDirectoryW", "DeleteFileW",
            "CreateDirectoryW", "MoveFileExW", "SetCurrentDirectoryW",
            "GetStdHandle", "GetConsoleMode", "WriteConsoleW",
            "GetCommandLineW", "CommandLineToArgvW", "LocalFree", "lstrlenW",
            "CreateThread", "WaitForSingleObject", "CloseHandle", "ExitThread",
            "CreateMutexA", "ReleaseMutex",
            "InitializeSRWLock", "AcquireSRWLockExclusive", "ReleaseSRWLockExclusive",
            "InitializeConditionVariable", "SleepConditionVariableSRW",
            "WakeConditionVariable", "WakeAllConditionVariable", "Sleep",
            "ConvertThreadToFiber", "CreateFiber", "SwitchToFiber", "DeleteFiber", "ConvertFiberToThread",
        ];
        let sources = [("runtime.c", include_str!("../runtime.c")), ("runtime_ex.c", include_str!("../runtime_ex.c"))];
        assert!(sources[0].1.contains(WIN32_HEADER_INCLUDE), "include line must still exist in runtime.c");
        let mut checked = 0usize;
        let mut declared_users = 0usize;
        for (file, source) in sources {
            for (name, text) in runtime_units(source) {
                let Some(name) = name else { continue; };
                if !WIN32_DECLS.iter().any(|needle| text.contains(needle)) { continue; }
                checked += 1;
                // Either the header is kept for this function, or the runtime
                // declares the entry point itself -- and then the declaration has
                // to be there, or the program builds without a prototype.
                assert!(text.contains(WIN32_HEADER_MARKER) || text.contains(WIN32_DECLARED_MARKER),
                    "{file}: {name} uses a Win32 declaration but carries neither marker");
                if text.contains(WIN32_DECLARED_MARKER) && !text.contains(WIN32_HEADER_MARKER) {
                    declared_users += 1;
                }
            }
        }
        assert!(checked >= 16, "expected at least 16 Win32-dependent functions, found {checked}");
        assert!(declared_users >= 3, "expected the declared-marker functions to exist, found {declared_users}");
        // The other direction: every symbol the runtime declares for itself is
        // audited above, so a hand-written declaration cannot slip in unmarked.
        // The declarations live inside the functions that call them (top-level
        // text is always kept, so a program that never prints would carry them),
        // which is why this scan trims the indentation.
        let mut declared = 0usize;
        for line in sources[0].1.lines().map(|line| line.trim_start()) {
            if !line.starts_with("extern ") { continue; }
            let name = line[..line.find('(').expect("declaration has a parameter list")]
                .rsplit(' ').next().unwrap().trim_start_matches('*');
            assert!(WIN32_DECLS.contains(&name), "self-declared Win32 symbol not in the audit list: {name}");
            declared += 1;
        }
        assert_eq!(declared, 29, "expected twenty-nine self-declared Win32 entry points");
    }

    #[test]
    fn capability_tables_match_the_real_operator_rules() {
        // ADR-40: a constraint whitelist must be a subset of the types that
        // actually support the operation, or the declaration would promise
        // something the per-instantiation type checker cannot deliver.
        for &ty in AUDIT_TYPES {
            if Bound::Add.allows(ty) { assert!(Capability::Add.supports(ty), "Add admits {}", ty.name()); }
            if Bound::Ord.allows(ty) { assert!(Capability::Ord.supports(ty), "Ord admits {}", ty.name()); }
            if Bound::Eq.allows(ty) { assert!(Capability::Eq.supports(ty), "Eq admits {}", ty.name()); }
        }
    }

    #[test]
    fn capability_audit_agrees_with_per_instantiation_checks() {
        // Differential test: with no constraint the audit steps aside and the
        // per-instantiation check decides. bool + bool must still fail while
        // i64, f64 and string pass.
        let body = "fn f[T](a: T, b: T) -> T { return a + b; }";
        for (ty, args, expected) in [
            ("i64", "1, 2", true), ("f64", "1.0, 2.0", true),
            ("string", "\"x\", \"y\"", true), ("bool", "true, false", false),
        ] {
            let source = format!("{body} fn main() {{ print(f[{ty}]({args})); }}");
            assert_eq!(compile(&source).is_ok(), expected, "{ty}: {source}");
        }
        // Copy still admits bool, so the declaration is rejected even when the
        // function is never instantiated: the error moves to the declaration.
        let weak = "fn f[T: Copy](a: T, b: T) -> T { return a + b; } fn main() { print(1); }";
        let err = compile(weak).unwrap_err();
        assert!(err.contains("declares T: Copy") && err.contains("bool"), "{err}");
        assert!(err.contains("1:5:"), "diagnostic must point at the constraint: {err}");
        // Narrowing the effective set to the supported types fixes it.
        assert!(compile("fn f[T: Add](a: T, b: T) -> T { return a + b; } fn main() { print(f[i64](1, 2)); }").is_ok());
        assert!(compile("fn f[T: Ord](a: T, b: T) -> T { return a + b; } fn main() { print(f[f64](1.0, 2.0)); }").is_ok());
    }

    #[test]
    fn capability_audit_rejects_weak_constraints_without_false_positives() {
        for source in [
            "fn f[T: Add](a: T, b: T) -> T { return a - b; } fn main() { print(1); }",
            "fn f[T: Add](a: T, b: T) -> T { return a % b; } fn main() { print(1); }",
            "fn f[T: Eq](a: T, b: T) -> bool { return a && b; } fn main() { print(1); }",
            "fn f[T: Copy](a: T, b: T) -> bool { return a < b; } fn main() { print(1); }",
            "fn f[T: Copy](a: T, b: T) -> T { let y = a; return y + b; } fn main() { print(1); }",
        ] {
            assert!(compile(source).is_err(), "weak constraint accepted: {source}");
        }
        for source in [
            "fn f[T: Add](a: T, b: T) -> T { return a + b; } fn main() { print(f[i64](1, 2)); }",
            "fn f[T: Ord](a: T, b: T) -> T { let t = a + b; if t > a { return t; } return b; } fn main() { print(f[i64](1, 2)); }",
            "fn f[T: Eq](a: T, b: T) -> bool { return a == b; } fn main() { print(f[bool](true, false)); }",
            "fn f[T: Copy](a: T) -> T { return a; } fn main() { print(f[i64](1)); }",
            // An operator on a concrete-typed variable is never attributed to T.
            "fn f[T: Eq](a: T, n: i64) -> i64 { return n + 1; } fn main() { print(f[i64](1, 2)); }",
            // An unconstrained generic keeps ADR-33 behaviour.
            "fn f[T](a: T, b: T) -> T { return a + b; } fn main() { print(f[i64](1, 2)); }",
        ] {
            if let Err(err) = compile(source) { panic!("valid declaration rejected: {source}: {err}"); }
        }
    }

    #[test]
    fn generic_declarations_share_one_token_buffer_per_file() {
        // ADR-41: before this change every declaration cloned the whole token
        // stream and source text, which is O(N^2) in the number of declarations.
        let mut shared = HashMap::new();
        let mut structs = Vec::new();
        let mut struct_names = HashMap::new();
        let (mut enums, mut enum_names) = prelude_enums();
        let mut generics = prelude_generics();
        let mut next_id = 0usize;
        let source = "fn f[T: Add](a: T, b: T) -> T { return a + b; }\n\
fn g[U: Ord](a: U, b: U) -> U { return a + b; }\n\
fn h[V: Eq](a: V, b: V) -> bool { return a == b; }\n\
fn main() { print(f[i64](1, 2)); print(g[i64](3, 4)); print(h[bool](true, true)); }";
        parse_source("", source, &mut shared, &mut structs, &mut struct_names,
            &mut enums, &mut enum_names, &mut generics, &mut next_id).unwrap();
        assert_eq!(generics.fn_defs.len(), 3, "expected three generic declarations");
        let tokens = Rc::clone(&generics.fn_defs[0].tokens);
        let text = Rc::clone(&generics.fn_defs[0].source);
        for def in &generics.fn_defs[1..] {
            assert!(Rc::ptr_eq(&tokens, &def.tokens), "each declaration copied the token stream");
            assert!(Rc::ptr_eq(&text, &def.source), "each declaration copied the source text");
        }
        assert_eq!(&*text, source);
    }
}

#[cfg(test)]
mod explain_tests {
    use super::explain;
    use std::collections::BTreeSet;

    /// Every code the compiler or lexer assigns must have text to explain it.
    /// The scan reads string literals shaped like a diagnostic code; a code that
    /// exists only in a comment is ignored, a code assigned without an entry fails.
    #[test]
    fn every_assigned_code_has_an_explanation() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut pending = vec![root.join("src"), root.join("crates")];
        let mut codes = BTreeSet::new();
        while let Some(directory) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&directory) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() { pending.push(path); continue; }
                if path.extension().and_then(|ext| ext.to_str()) != Some("rs") { continue; }
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                let mut at = 0;
                while let Some(found) = text[at..].find("\"E") {
                    let start = at + found + 1;
                    let rest = &text[start..];
                    let bytes = rest.as_bytes();
                    if bytes.len() > 5
                        && bytes[1..5].iter().all(u8::is_ascii_digit)
                        && !bytes[5].is_ascii_digit()
                    {
                        codes.insert(rest[..5].to_owned());
                    }
                    at = start;
                }
            }
        }
        assert!(codes.len() >= 10, "scan found too few codes: {codes:?}");
        for code in codes {
            assert!(explain(&code).is_some(), "no explanation registered for {code}");
        }
    }
}

