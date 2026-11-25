#![allow(clippy::result_large_err)]

use std::cell::OnceCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
pub(crate) use ae_ty::Type;
pub(crate) use ae_ty::{FloatTy, IntTy};
mod parser;
mod route_marks;
mod route_body_expr;
mod route_body_stmt;
mod lower_scan;
mod lower_body;
mod lower_stmt;
mod lower_loop;
mod lower_expr;
mod lower_builtin;
mod lower_builtin_io;
mod lower_builtin_stmt;
mod lower_call;
mod lower_check;
mod lower_shared;
mod lower_match;
#[cfg(test)]
mod parser_tests;
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
pub use loader::{PackageImports, check_path, check_path_with_imports, check_path_diagnostic, check_path_diagnostic_with_imports, describe_path, FrontendDiagnostic, FrontendError, SourceUnit};
mod loader;
use loader::{load_program, load_program_with_imports, parse_source};
mod capability_audit;
use capability_audit::audit_generic_fn;
mod codegen;
mod codegen_mir;
mod c_output;
pub(crate) use ae_syntax::{Kind, Token, lex};
#[allow(unused_imports)]
use codegen::*;
use c_output::*;



pub(super) const PRELUDE_I64_VEC_ELEM_ID: usize = 0;
pub(super) const PRELUDE_STR_VEC_ELEM_ID: usize = 1;
pub(super) const PRELUDE_F64_VEC_ELEM_ID: usize = 2;
pub(super) const PRELUDE_I64_MAP_ID: usize = 0;
pub(super) const PRELUDE_F64_MAP_ID: usize = 1;
pub(super) const PRELUDE_I64_ARRAY_ELEM_ID: usize = 0;

#[derive(Debug, Clone)]
pub struct CompileContext {
    types: ae_ty::TypeInterner,
    extern_names: BTreeMap<usize, (String, Vec<Type>, Type)>,
    exposure_names: HashMap<usize, String>,
    unsafe_fns: HashSet<usize>,
    closure_fn_ids: Vec<usize>,
    spawned_threads: Vec<closures::SpawnedThread>,
    closure_borrow_captures: Vec<Vec<(usize, Type)>>,
    package_imports: PackageImports,
}

impl Default for CompileContext {
    fn default() -> Self {
        Self::new()
    }
}

impl CompileContext {
    pub fn new() -> Self {
        let mut ctx = Self {
            types: ae_ty::TypeInterner::default(),
            extern_names: BTreeMap::new(),
            exposure_names: HashMap::new(),
            unsafe_fns: HashSet::new(),
            closure_fn_ids: Vec::new(),
            spawned_threads: Vec::new(),
            closure_borrow_captures: Vec::new(),
            package_imports: PackageImports::default(),
        };
        let i64_id = ctx.intern_vector_elem(Type::Int(IntTy::I64));
        let str_id = ctx.intern_vector_elem(Type::String);
        let f64_id = ctx.intern_vector_elem(Type::Float(FloatTy::F64));
        debug_assert_eq!(i64_id, PRELUDE_I64_VEC_ELEM_ID);
        debug_assert_eq!(str_id, PRELUDE_STR_VEC_ELEM_ID);
        debug_assert_eq!(f64_id, PRELUDE_F64_VEC_ELEM_ID);
        let i64_map_id = ctx.intern_map_types(Type::String, Type::Int(IntTy::I64));
        let f64_map_id = ctx.intern_map_types(Type::String, Type::Float(FloatTy::F64));
        debug_assert_eq!(i64_map_id, PRELUDE_I64_MAP_ID);
        debug_assert_eq!(f64_map_id, PRELUDE_F64_MAP_ID);
        let i64_arr_id = ctx.intern_array_elem(Type::Int(IntTy::I64));
        debug_assert_eq!(i64_arr_id, PRELUDE_I64_ARRAY_ELEM_ID);
        ctx
    }

    pub fn with_package_imports(package_imports: PackageImports) -> Self {
        Self { package_imports, ..Self::new() }
    }

    fn register_extern(&mut self, id: usize, c_name: String, params: Vec<Type>, ret: Type) {
        self.extern_names.insert(id, (c_name, params, ret));
    }

    fn extern_name(&self, id: usize) -> Option<String> {
        self.extern_names.get(&id).map(|(name, _, _)| name.clone())
    }

    fn is_extern_fn(&self, id: usize) -> bool {
        self.extern_names.contains_key(&id)
    }

    fn extern_declarations(&self) -> Vec<(String, Vec<Type>, Type)> {
        self.extern_names.values().cloned().collect()
    }

    fn register_exposure(&mut self, id: usize, c_name: String) {
        self.exposure_names.insert(id, c_name);
    }

    fn exposure_name(&self, id: usize) -> Option<String> {
        self.exposure_names.get(&id).cloned()
    }

    fn register_unsafe_fn(&mut self, id: usize) {
        self.unsafe_fns.insert(id);
    }

    fn is_unsafe_fn(&self, id: usize) -> bool {
        self.unsafe_fns.contains(&id)
    }

    fn intern_pointee(&mut self, pointee: Type) -> usize {
        self.types.intern_pointee(pointee)
    }

    fn find_pointee(&self, pointee: Type) -> Option<usize> {
        self.types.find_pointee(pointee)
    }

    fn ptr_pointee(&self, id: usize) -> Type {
        self.types.pointee(id)
    }

    fn intern_shared(&mut self, elem: Type) -> usize {
        self.types.intern_shared(elem)
    }

    /// ADR-0235: the element type of a generic vector.
    fn intern_vector_elem(&mut self, elem: Type) -> usize {
        self.types.intern_vector_elem(elem)
    }

    fn find_vector_elem(&self, elem: Type) -> Option<usize> {
        (0..self.types.vector_elem_count()).find(|&id| self.types.vector_elem(id) == elem)
    }

    fn vector_elem(&self, id: usize) -> Type {
        self.types.vector_elem(id)
    }

    fn vector_elem_count(&self) -> usize {
        self.types.vector_elem_count()
    }

    fn intern_map_types(&mut self, key: Type, value: Type) -> usize {
        self.types.intern_map_types(key, value)
    }

    fn map_types(&self, id: usize) -> (Type, Type) {
        self.types.map_types(id)
    }

    fn find_map_types(&self, key: Type, value: Type) -> Option<usize> {
        self.types.find_map_types(key, value)
    }

    fn intern_array_elem(&mut self, elem: Type) -> usize {
        self.types.intern_array_elem(elem)
    }

    fn array_elem(&self, id: usize) -> Type {
        self.types.array_elem(id)
    }

    fn find_array_elem(&self, elem: Type) -> Option<usize> {
        self.types.find_array_elem(elem)
    }

    fn shared_types_get(&self, id: usize) -> Type {
        self.types.shared(id)
    }

    fn fn_sig(&self, id: usize) -> (Vec<Type>, Type) {
        self.types.fn_sig(id)
    }

    /// The signature an interned id names, or `None` when the id does not
    /// belong to this compilation (a synthetic test expression, or a walker
    /// meeting an id it did not create). The walkers use this so a foreign id
    /// cannot panic.
    fn fn_sig_checked(&self, id: usize) -> Option<(Vec<Type>, Type)> {
        self.types.fn_sig_checked(id)
    }

    fn intern_fn_sig(&mut self, params: Vec<Type>, ret: Type) -> usize {
        self.types.intern_fn_sig(params, ret)
    }

    fn closure_fn_id(&self, cid: usize) -> usize {
        self.closure_fn_ids[cid]
    }

    fn record_spawned(&mut self, entry: closures::SpawnedThread) {
        if !self.spawned_threads.contains(&entry) {
            self.spawned_threads.push(entry);
        }
    }

    fn take_spawned(&mut self) -> Vec<closures::SpawnedThread> {
        std::mem::take(&mut self.spawned_threads)
    }

    fn closure_borrow_captures(&self, cid: usize) -> Vec<(usize, Type)> {
        self.closure_borrow_captures[cid].clone()
    }

    fn push_closure(&mut self, fn_id: usize, borrow_captures: Vec<(usize, Type)>) {
        self.closure_borrow_captures.push(borrow_captures);
        self.closure_fn_ids.push(fn_id);
    }

    fn package_imports(&self) -> &PackageImports {
        &self.package_imports
    }
}
pub(crate) trait TypeDisplay {
    fn name(self, ctx: &CompileContext) -> String;
    fn c_temp(self, ctx: &CompileContext) -> String;
}
// The concrete enum lives in `ae-ty` (P1.2); the context-dependent
// spelling/layout lookups stay with the compiler's registries as a trait.
impl TypeDisplay for Type {
    fn name(self, ctx: &CompileContext) -> String {
        match self {
            Type::Int(width) => width.name().to_owned(),
            Type::Bool => "bool".to_owned(),
            Type::Float(width) => width.name().to_owned(),
            Type::Char => "char".to_owned(),
            Type::Unit => "()".to_owned(),
            Type::String => "string".to_owned(),
            Type::StrView => "string view".to_owned(),
            // Generic containers (ADR-0235): one spelling from the interned
            // element type, whatever it is.
            Type::Vector(id) => format!("vec[{}]", ctx.vector_elem(id).name(ctx)),
            Type::VectorRef(id) => format!("&vec[{}]", ctx.vector_elem(id).name(ctx)),
            Type::MapOf(id) => {
                let (k, v) = ctx.map_types(id);
                format!("map[{}, {}]", k.name(ctx), v.name(ctx))
            }
            Type::ArrayOf(elem, n) => format!("&[{}; {n}]", ctx.array_elem(elem).name(ctx)),
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
                let (params, ret) = ctx.fn_sig(sig);
                format!("fn({}) -> {}", params.iter().map(|t| t.name(ctx)).collect::<Vec<_>>().join(", "), ret.name(ctx))
            }
            // A raw pointer (M2.12): an address typed by pointee layout only.
            Type::Ptr(id) => format!("*const {}", ctx.ptr_pointee(id).name(ctx)),
            Type::PtrMut(id) => format!("*mut {}", ctx.ptr_pointee(id).name(ctx)),
            // A thread handle (ADR-0214 / spec/concurrency.md): owns its heap
            // slot; the return type rides the interned slot, not the spelling.
            Type::Thread(id) => format!("Thread#{}", id),
            // A copy-semantics handle to one process-lifetime cell
            // (spec/concurrency.md §8): copying shares, nothing moves.
            Type::Atomic => "atomic".to_owned(),
            // A copy-semantics handle to one process-lifetime box holding an
            // owned value (ADR-0216): copying shares, reads copy out.
            Type::Shared(id) => format!("shared[{}]", ctx.shared_types_get(id).name(ctx)),
            Type::Mutex => "mutex".to_owned(),
            Type::Guard => "guard".to_owned(),
            Type::Chan => "chan".to_owned(),
            Type::Exec => "exec".to_owned(),
            Type::TaskScope => "task_scope".to_owned(),
            Type::Pool => "pool".to_owned(),
        }
    }
    fn c_temp(self, ctx: &CompileContext) -> String {
        match self {
            Type::Bool => "bool ".to_owned(),
            Type::Float(FloatTy::F64) => "double ".to_owned(),
            Type::String => "ae_string ".to_owned(),
            Type::StrView => "ae_strview ".to_owned(),
            Type::Vector(id) => format!("{} ", c_output::vector_family(ctx, id)),
            // A borrow has no storage of its own; the declaration form exists
            // for a loop variable bound to an owned element's storage
            // (ADR-0235 slice 4), which is a pointer to the element.
            Type::VectorRef(id) => format!("const {} *", c_output::vector_family(ctx, id)),
            Type::MapOf(id) => format!("{} ", c_output::map_family(ctx, id)),
            Type::Struct(k) => format!("ae_t{k} "),
            // A borrow has no storage of its own, so a temporary of these
            // types is never declared; the arms exist so a stray use fails
            // the build instead of formatting a bad C type. The read-only
            // form keeps `const` (a `for` element binding takes the address
            // of a const element place).
            Type::StructRef(k) => format!("const ae_t{k}* "),
            Type::StructMut(k) => format!("ae_t{k}* "),
            Type::Enum(_, k) => format!("ae_e{k} "),
            Type::Tokens => "ae_tokens ".to_owned(),
            // Never declared: a closure has no storage, and this arm exists
            // so a stray use fails the build instead of printing a bad type.
            Type::Closure(_) => "void ".to_owned(),
            Type::Fn(sig) => format!("ae_fn_t{sig} "),
            Type::Ptr(id) => format!("const {}* ", ctx.ptr_pointee(id).c_temp(ctx).trim_end()),
            Type::PtrMut(id) => format!("{}* ", ctx.ptr_pointee(id).c_temp(ctx).trim_end()),
            Type::Thread(_) => "void *".to_owned(),
            Type::Atomic => "ae_atomic ".to_owned(),
            Type::Shared(_) => "ae_shared ".to_owned(),
            Type::Mutex => "ae_mutex ".to_owned(),
            Type::Guard => "ae_guard ".to_owned(),
            Type::Chan => "ae_chan ".to_owned(),
            Type::Exec => "ae_exec ".to_owned(),
            Type::TaskScope => "ae_task_scope ".to_owned(),
            Type::Pool => "ae_pool ".to_owned(),
            Type::Int(width) => format!("{} ", width.c_name()),
            Type::Float(width) => format!("{} ", width.c_name()),
            _ => "int64_t ".to_owned(),
        }
    }
}

// Names owned by the prelude rather than by any file on disk.
const PRELUDE: &str = "<prelude>";
// The prelude payload structs (FileInfo, CommandOutput) occupy the first
// struct ids and do not count against the generic-instantiation budget.
pub(super) const PRELUDE_STRUCT_COUNT: usize = 2;
// Cap on enum layouts per program, prelude and instantiations included. Each
// layout is a tag enum plus one struct per payload variant, so an unbounded
// instantiation count is a real code-size hazard (ADR-21).
const MAX_ENUM_TYPES: usize = 256;
const MAX_STRUCT_TYPES: usize = 256;

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
// The one generic result declaration every preset family specializes. The
// prelude enums and the generic registry share this shape (P2.4).
fn result_generic_enum() -> GenericEnum {
    GenericEnum {
        name: "Result".to_owned(),
        params: vec!["T".to_owned(), "E".to_owned()],
        variants: vec![
            ("Ok".to_owned(), vec![Payload::Param(0)]),
            ("Err".to_owned(), vec![Payload::Param(1)]),
        ],
        file: PRELUDE.to_owned(),
        public: true,
        is_copy: false,
    }
}

// The preset result families: alias name plus the `[T, E]` arguments of their
// `Result` instantiation. The order fixes the prelude enum ids and the C
// runtime's macro ids; the runtime alias block is generated from the same
// list. `FileInfo` (struct id 0) and `CommandOutput` (struct id 1) are the
// prelude payload structs registered before any source.
pub(super) fn prelude_result_families() -> [(&'static str, [Type; 2]); 8] {
    [
        ("ReadResult", [Type::String, Type::String]),
        ("IntResult", [Type::Int(IntTy::I64), Type::String]),
        ("DirResult", [Type::Vector(PRELUDE_STR_VEC_ELEM_ID), Type::String]),
        ("FileResult", [Type::Struct(0), Type::String]),
        ("StatusResult", [Type::Unit, Type::String]),
        ("BoolResult", [Type::Bool, Type::String]),
        ("CommandResult", [Type::Struct(1), Type::String]),
        ("FloatResult", [Type::Float(FloatTy::F64), Type::String]),
    ]
}

// Substitute one family's arguments into the generic Result's variants.
fn specialize_result_variants(def: &GenericEnum, args: &[Type; 2]) -> Vec<(String, Vec<Type>)> {
    def.variants.iter().map(|(name, fields)| {
        let fields = fields.iter().map(|payload| match payload {
            Payload::Param(index) => args[*index],
            Payload::Concrete(ty) => *ty,
            Payload::Nested(_) => unreachable!("the prelude Result has no nested payloads"),
        }).collect();
        (name.clone(), fields)
    }).collect()
}

fn prelude_enums() -> (Vec<EnumDef>, HashMap<String, usize>) {
    // Every preset family is an instantiation of the one generic Result; the
    // definitions derive its Ok/Err variants with the family's arguments, so
    // no payload-specific variant list exists (Hard Rule 1).
    let result = result_generic_enum();
    let mut enums = Vec::new();
    let mut names = HashMap::new();
    for (name, args) in prelude_result_families() {
        names.insert(name.to_owned(), enums.len());
        let variants = specialize_result_variants(&result, &args);
        enums.push(EnumDef { name, variants, file: PRELUDE.to_owned(), public: true, pattern: name.to_owned(), aliases: vec!["Result"], is_copy: false });
    }
    // The write family's Ok/Err payloads are identical to the integer family's;
    // one instantiation serves both names (P2.4 slice 3).
    let int_eid = names["IntResult"];
    enums[int_eid].aliases.push("WriteResult");
    names.insert("WriteResult".to_owned(), int_eid);
    (enums, names)
}

// ADR-0234: the payload structs of the two multi-value result families are
// prelude types, registered before any user struct so their ids (0 and 1) are
// fixed for every compilation and the route scan can read the variants'
// shapes while it parses. User struct ids therefore start at 2.
pub(super) fn prelude_payload_structs() -> Vec<StructDef> {
    vec![
        StructDef {
            name: "FileInfo".to_owned(),
            fields: vec![
                ("size".to_owned(), Type::Int(IntTy::I64)),
                ("mtime".to_owned(), Type::Int(IntTy::I64)),
                ("is_dir".to_owned(), Type::Bool),
            ],
            file: PRELUDE.to_owned(),
            public: false,
            repr_c: false,
            is_copy: true,
            drop_fn: None,
            drop_returns_unit: false,
        },
        StructDef {
            name: "CommandOutput".to_owned(),
            fields: vec![
                ("status".to_owned(), Type::Int(IntTy::I64)),
                ("output".to_owned(), Type::String),
            ],
            file: PRELUDE.to_owned(),
            public: false,
            repr_c: false,
            is_copy: false,
            drop_fn: None,
            drop_returns_unit: false,
        },
    ]
}

// `Option[T]` and `Result[T, E]` ship with the prelude: "a value that may be
// absent" and "fallibility as a value" are the two generic shapes every
// program needs, and building them in keeps programs from each declaring a
// slightly different `Maybe` or `Outcome` (Hard Rule 1; P2.4 slice 1).
fn prelude_generics() -> Generics {
    let mut generics = Generics::default();
    generics.names.insert("Option".to_owned(), 0);
    generics.defs.push(GenericEnum {
        name: "Option".to_owned(),
        params: vec!["T".to_owned()],
        variants: vec![("Some".to_owned(), vec![Payload::Param(0)]), ("None".to_owned(), Vec::new())],
        file: PRELUDE.to_owned(),
        public: true,
        is_copy: false,
    });
    generics.names.insert("Result".to_owned(), 1);
    generics.defs.push(result_generic_enum());
    generics
}


// String literals keep their source text with escapes uninterpreted: the bytes
// between the quotes are already valid C string body, so emission can reuse them.
#[allow(clippy::vec_box)]
#[derive(Debug, Clone)]
enum Expr { Number(i64), FloatLit(f64), ToF64(Box<Expr>, usize, usize), ToI64(Box<Expr>, Type, usize, usize), FloatToString(Box<Expr>, usize, usize), BoolLit(bool), StrLit(String, usize, usize), Variable(usize, Type, usize, usize), Call(usize, Vec<Argument>, Type), Index(usize, usize, Box<Expr>, usize, usize, Type), VecLit(Vec<Expr>, Type, usize, usize), VecIndex(usize, Box<Expr>, usize, usize, bool, Type), Len(Box<Expr>, usize, usize), ReadFile(Box<Expr>, usize, usize), WriteFile(Box<Expr>, Box<Expr>, usize, usize), Bytes(Box<Expr>, usize, usize), Contains(Box<Expr>, Box<Expr>, usize, usize), Slice(Box<Expr>, Box<Expr>, Box<Expr>, usize, usize), Split(Box<Expr>, Box<Expr>, usize, usize), Args(usize, usize), Join(usize, Box<Expr>, usize, usize), StrVecIndex(usize, Box<Expr>, usize, usize, bool), SliceView(usize, Option<String>, Box<Expr>, Box<Expr>, usize, usize),
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
    BorrowAs(Box<Expr>, bool, usize, usize), #[allow(dead_code)] DerefPtr(Box<Expr>, usize, usize), #[allow(dead_code)] PtrOffset(Box<Expr>, Box<Expr>, bool, usize, usize), #[allow(dead_code)] PtrCast(Box<Expr>, bool, usize, usize, usize), #[allow(dead_code)] PtrDropInPlace(Box<Expr>, Type, usize, usize), #[allow(dead_code)] PtrWrite(Box<Expr>, Box<Expr>, Type, usize, usize), ScalarCast(Box<Expr>, Type, Type, usize, usize), CharLit(char), Unit, IntMethod(IntMethodKind, Box<Expr>, Option<Box<Expr>>, Type, usize, usize), FnRef(usize, usize), FnExprCall(Box<Expr>, Vec<Argument>, Type, usize),
    // P2.4: an element read through a non-slot receiver (`r.cells[0]`, a
    // struct's vector field): the base expression is the vector lvalue, the
    // index is a value, and the element type names the storage element. The
    // result is a value for Copy elements and a view for owned ones.
    VecIndexOf(Box<Expr>, Box<Expr>, usize, usize, Type) }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IntMethodKind {
    WrappingAdd,
    WrappingSub,
    WrappingMul,
    WrappingNeg,
    SaturatingAdd,
    SaturatingSub,
    SaturatingMul,
    CheckedAdd,
    CheckedSub,
    CheckedMul,
    CheckedDiv,
    CheckedRem,
    CheckedNeg,
}

impl IntMethodKind {
    pub(super) fn from_name(name: &str) -> Option<Self> {
        match name {
            "wrapping_add" => Some(Self::WrappingAdd),
            "wrapping_sub" => Some(Self::WrappingSub),
            "wrapping_mul" => Some(Self::WrappingMul),
            "wrapping_neg" => Some(Self::WrappingNeg),
            "saturating_add" => Some(Self::SaturatingAdd),
            "saturating_sub" => Some(Self::SaturatingSub),
            "saturating_mul" => Some(Self::SaturatingMul),
            "checked_add" => Some(Self::CheckedAdd),
            "checked_sub" => Some(Self::CheckedSub),
            "checked_mul" => Some(Self::CheckedMul),
            "checked_div" => Some(Self::CheckedDiv),
            "checked_rem" => Some(Self::CheckedRem),
            "checked_neg" => Some(Self::CheckedNeg),
            _ => None,
        }
    }

    pub(super) fn mir_name(self) -> &'static str {
        match self {
            Self::WrappingAdd => "wrapping_add",
            Self::WrappingSub => "wrapping_sub",
            Self::WrappingMul => "wrapping_mul",
            Self::WrappingNeg => "wrapping_neg",
            Self::SaturatingAdd => "saturating_add",
            Self::SaturatingSub => "saturating_sub",
            Self::SaturatingMul => "saturating_mul",
            Self::CheckedAdd => "checked_add",
            Self::CheckedSub => "checked_sub",
            Self::CheckedMul => "checked_mul",
            Self::CheckedDiv => "checked_div",
            Self::CheckedRem => "checked_rem",
            Self::CheckedNeg => "checked_neg",
        }
    }

    pub(super) fn is_unary(self) -> bool {
        matches!(self, Self::WrappingNeg | Self::CheckedNeg)
    }

    pub(super) fn is_checked(self) -> bool {
        matches!(
            self,
            Self::CheckedAdd | Self::CheckedSub | Self::CheckedMul | Self::CheckedDiv | Self::CheckedRem | Self::CheckedNeg
        )
    }
}
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
fn map_kind_of(ty: Type, ctx: &CompileContext) -> MapKind {
    if let Type::MapOf(id) = ty && ctx.map_types(id).1 == Type::Float(FloatTy::F64) {
        return MapKind::Float;
    }
    MapKind::Int
}
fn map_element_ty(kind: MapKind) -> Type {
    if kind == MapKind::Float { Type::Float(FloatTy::F64) } else { Type::Int(IntTy::I64) }
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
enum Statement { Return(Expr, Vec<(usize, Type)>), Declare(usize, Expr), Array(usize, Vec<Expr>, Type), Assign(usize, Expr, bool), Store(usize, usize, Expr, Expr, usize, usize, Type), Sort(usize, bool, usize, usize), Reverse(usize, bool, usize, usize), StrVecPush(usize, Expr, usize, usize, bool), StrVecPop(usize, usize, usize), StrVecStore(usize, Expr, Expr, usize, usize, bool), GVecPush(usize, Expr, Type, usize, usize), GVecPop(usize, Type, usize, usize), GVecStore(usize, Expr, Expr, Type, usize, usize), FieldStore(usize, Vec<(String, Type)>, Type, Expr, Type), Print(Printed), Assert(Expr, usize, usize), If(Expr, Vec<Statement>, Vec<Statement>), While(Expr, Vec<Statement>), For(usize, usize, Vec<Statement>, usize, usize, usize), ForTokens(usize, usize, Vec<Statement>, usize, usize, usize), ResetTokens(usize, usize), AdvanceTokens(usize, usize, usize, usize), Match(usize, Type, Vec<MatchArm>), BindPayload(usize, usize, usize, usize, Type), TakePropagationPayload(usize, Type, Vec<Type>, Option<(usize, Type)>), MapPut(MapPlace, Expr, Expr, usize, usize), Propagate(Expr, Type, Vec<Type>, Option<(usize, Type)>, Vec<(usize, Type)>), Break, Continue(Option<usize>), Exit(Expr, Vec<(usize, Type)>), Frees(Vec<(usize, Type)>), Unsafe(Vec<Statement>), PtrStore(usize, Expr, usize, usize), AtomicStore(Box<Expr>, Box<Expr>), SharedSet(Box<Expr>, Box<Expr>, usize), ChanClose(Box<Expr>), TaskSpawn(Box<Expr>, Vec<Expr>, Box<Expr>, usize, usize, usize), TaskYield, ScopeSpawn(Box<Expr>, Vec<Expr>, Box<Expr>, usize, usize, usize), PoolSpawn(Box<Expr>, Vec<Expr>, Box<Expr>, usize, usize, usize),
    // P2.4: vector operations on a struct's vector field (`r.cells`).
    // The statement carries the root slot, the field name and the root's
    // declared type (a `&mut` root reads through its pointer), so the
    // codegen can address the field lvalue and the interpreter can
    // mutate the field value in place.
    VecPushField(usize, Vec<(String, Type)>, Type, Expr, Type, bool, usize, usize),
    VecPopField(usize, Vec<(String, Type)>, Type, Type, usize, usize),
    VecStoreField(usize, Vec<(String, Type)>, Type, Expr, Expr, Type, bool, usize, usize),
    VecSortField(usize, Vec<(String, Type)>, Type, Type, bool, usize, usize) }
#[derive(Debug)]
#[derive(Clone)]
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
pub(super) struct StructDef { name: String, fields: Vec<(String, Type)>, file: String, public: bool, repr_c: bool, is_copy: bool, drop_fn: Option<usize>, drop_returns_unit: bool }

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
                Type::Int(IntTy::I64) | Type::Float(FloatTy::F64) | Type::Bool => ty.scalar_c_layout()?,
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
pub(super) struct EnumDef {
    name: &'static str,
    variants: Vec<(String, Vec<Type>)>,
    file: String,
    public: bool,
    pattern: String,
    /// Alternative declaration names a pattern may use for this type. The
    /// preset result families become `Result[payload, string]` instantiations
    /// (P2.4), so `Result::Ok` and the historical family name both match the
    /// same value; user enums carry no aliases.
    aliases: Vec<&'static str>,
    is_copy: bool,
}

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
                    Type::Int(IntTy::I64) | Type::Float(FloatTy::F64) | Type::Bool => ty.scalar_c_layout().unwrap_or((8, 8)),
                    // The unit payload occupies no storage (the C field is one
                    // `uint8_t`, but it never participates in a layout query).
                    Type::Unit => (0, 1),
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
struct GenericEnum { name: String, params: Vec<String>, variants: Vec<(String, Vec<Payload>)>, file: String, public: bool, is_copy: bool }
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
struct GenericStruct { name: String, params: Vec<String>, fields: Vec<(String, Payload)>, file: String, public: bool, is_copy: bool }

pub(super) fn is_copy_type(ty: Type, structs: &[StructDef], enums: &[EnumDef]) -> bool {
    match ty {
        Type::Int(_) | Type::Float(_) | Type::Bool | Type::Char | Type::Unit => true,
        Type::Ptr(_) | Type::PtrMut(_) => true,
        Type::StructRef(_) | Type::VectorRef(_) | Type::StrView => true,
        Type::Fn(_) => true,
        Type::Atomic | Type::Shared(_) => true,
        Type::Struct(sid) => structs.get(sid).is_some_and(|defn| defn.is_copy),
        Type::Enum(_, eid) => enums.get(eid).is_some_and(|defn| defn.is_copy),
        _ => false,
    }
}

pub(super) fn is_owned_type(ty: Type, structs: &[StructDef], enums: &[EnumDef]) -> bool {
    ty.owned() && !is_copy_type(ty, structs, enums)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CallableKind { Fn, FnMut, FnOnce }
impl CallableKind {
    fn name(self) -> &'static str {
        match self { CallableKind::Fn => "Fn", CallableKind::FnMut => "FnMut", CallableKind::FnOnce => "FnOnce" }
    }
    fn satisfies(self, required: CallableKind) -> bool {
        matches!(
            (self, required),
            (CallableKind::Fn, _)
                | (CallableKind::FnMut, CallableKind::FnMut | CallableKind::FnOnce)
                | (CallableKind::FnOnce, CallableKind::FnOnce)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bound { Add, Eq, Ord, Copy, Callable(CallableKind, usize) }
impl Bound {
    fn name(self) -> &'static str {
        match self {
            Bound::Add => "Add", Bound::Eq => "Eq", Bound::Ord => "Ord", Bound::Copy => "Copy",
            Bound::Callable(kind, _) => kind.name(),
        }
    }
    fn allows(self, ty: Type, structs: &[StructDef], enums: &[EnumDef]) -> bool {
        match self {
            Bound::Copy => is_copy_type(ty, structs, enums),
            Bound::Eq => matches!(ty, Type::Int(IntTy::I64) | Type::Bool | Type::String | Type::Float(FloatTy::F64)),
            Bound::Ord => matches!(ty, Type::Int(IntTy::I64) | Type::Float(FloatTy::F64)),
            Bound::Add => matches!(ty, Type::Int(IntTy::I64) | Type::String | Type::Float(FloatTy::F64)),
            Bound::Callable(kind, sig) => matches!(kind, CallableKind::Fn | CallableKind::FnMut | CallableKind::FnOnce) && match ty {
                Type::Fn(ty_sig) => ty_sig == sig,
                _ => false,
            },
        }
    }
    fn allows_with_generics(self, ty: Type, structs: &[StructDef], enums: &[EnumDef], generics: &Generics) -> bool {
        match self {
            Bound::Callable(kind, sig) => match ty {
                Type::Fn(ty_sig) => ty_sig == sig,
                Type::Struct(sid) => generics.closure_envs.get(&sid)
                    .is_some_and(|env| env.sig == sig && env.kind.satisfies(kind)),
                _ => false,
            },
            other => other.allows(ty, structs, enums),
        }
    }
    fn allowed_types(self) -> &'static str {
        match self {
            Bound::Copy => "a Copy type (primitive scalar, raw pointer, shared reference, fn, atomic, shared, or #[derive(Copy)] struct/enum)",
            Bound::Eq => "i64, bool, string, f64",
            Bound::Ord => "i64, f64",
            Bound::Add => "i64, string, f64",
            Bound::Callable(CallableKind::Fn, _) => "a function pointer or non-mutating, non-consuming closure",
            Bound::Callable(CallableKind::FnMut, _) => "a function pointer or non-consuming closure",
            Bound::Callable(CallableKind::FnOnce, _) => "a function pointer or closure with matching signature",
        }
    }
}

// ADR-40. A generic declaration that writes constraints must not admit a type
// that its body cannot use. This is a diagnostic, not a proof: every
// instantiation is still parsed and fully type-checked (ADR-33). The audit can
// only reject a declaration; it never lets unchecked code through.
const AUDIT_TYPES: &[Type] = &[
    Type::Int(IntTy::I64), Type::Bool, Type::Float(FloatTy::F64), Type::String, Type::StrView,
    Type::Vector(PRELUDE_I64_VEC_ELEM_ID), Type::Vector(PRELUDE_STR_VEC_ELEM_ID), Type::Vector(PRELUDE_F64_VEC_ELEM_ID),
    Type::VectorRef(PRELUDE_I64_VEC_ELEM_ID), Type::VectorRef(PRELUDE_STR_VEC_ELEM_ID), Type::VectorRef(PRELUDE_F64_VEC_ELEM_ID),
    Type::MapOf(PRELUDE_I64_MAP_ID), Type::MapOf(PRELUDE_F64_MAP_ID),
    Type::ArrayOf(PRELUDE_I64_ARRAY_ELEM_ID, 0), Type::Struct(0), Type::StructRef(0), Type::StructMut(0), Type::Enum("audit", 0),
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
            Capability::Add => matches!(ty, Type::Int(IntTy::I64) | Type::Float(FloatTy::F64) | Type::String | Type::StrView),
            Capability::Arith => matches!(ty, Type::Int(IntTy::I64) | Type::Float(FloatTy::F64)),
            Capability::Modulo => matches!(ty, Type::Int(IntTy::I64)),
            Capability::Ord => matches!(ty, Type::Int(IntTy::I64) | Type::Float(FloatTy::F64)),
            Capability::Eq => matches!(ty, Type::Int(IntTy::I64) | Type::Float(FloatTy::F64) | Type::Bool | Type::String | Type::StrView),
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
struct Generics { defs: Vec<GenericEnum>, names: HashMap<String, usize>, done: HashMap<String, usize>, struct_defs: Vec<GenericStruct>, struct_names: HashMap<String, usize>, struct_done: HashMap<String, usize>, struct_origin: HashMap<usize, (usize, Vec<Type>)>, enum_origin: HashMap<usize, (usize, Vec<Type>)>, fn_defs: Vec<GenericFn>, fn_names: HashMap<String, usize>, fn_done: HashMap<String, Instance>, fn_in_progress: std::collections::HashSet<String>, next_fn_id: usize, traits: Vec<TraitDef>, trait_names: HashMap<String, usize>, impls: std::collections::HashSet<(usize, String)>, inherent: HashMap<String, MethodDef>, generic_impls: HashMap<String, GenericImplDef>, generic_impl_done: HashMap<String, Instance>, impl_in_progress: std::collections::HashSet<String>, closures: Vec<ClosureDef>, closure_envs: HashMap<usize, ClosureEnvInfo>, impl_fns: HashMap<usize, ImplFnInfo>, threads: Vec<Type>, nested_shapes: Vec<NestedShape>, instantiation_trace: Vec<(&'static str, String, String)> }
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
pub(super) struct ClosureEnvInfo {
    pub(super) env_sid: usize,
    pub(super) wrapper_fn_id: usize,
    pub(super) kind: CallableKind,
    pub(super) sig: usize,
}

#[derive(Debug, Clone)]
pub(super) struct ImplFnInfo {
    pub(super) env_sid: usize,
    pub(super) wrapper_fn_id: usize,
    pub(super) kind: CallableKind,
    pub(super) sig: usize,
}

#[derive(Debug, Clone)]
struct ClosureDef {
    fn_id: usize,
    params: Vec<Type>,
    ret: Type,
    value_captures: Vec<(usize, Type)>,
    borrow_captures: Vec<(usize, Type)>,
    mut_captures: Vec<(usize, Type)>,
    kind: CallableKind,
    env: Option<ClosureEnvInfo>,
    file: String,
}

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

/// A source-spelled type argument of a generic instance, kept for the
/// stateless field walk: a value type, or a nested generic struct instance
/// (`Pair[string, Pair[string, string]]`). No layout is instantiated here.
#[derive(Clone, Debug)]
enum ShapeArg { Value(Type), StructInstance { gid: usize, args: Vec<ShapeArg> } }

#[derive(Default)]
struct ResolvedExpressions {
    local_bindings: HashMap<usize, std::ops::Range<usize>>,
    direct_calls: HashSet<usize>,
    unresolved_calls: HashSet<usize>,
    /// Calls whose spelling no declaration answers (not a function, generic
    /// function, trait method or reserved builtin). The scan admits them so
    /// the lowering can report the typed route's `unknown function 'name'`
    /// (E0207) at the callee, before any argument or return-type question.
    unknown_calls: HashSet<usize>,
    specialization_arguments: HashMap<usize, std::sync::Arc<[std::ops::Range<usize>]>>,
    specialized_call_arguments: HashMap<usize, Rc<[ae_syntax::expression::Expression]>>,
    specialized_struct_fields: HashMap<usize, (usize, Rc<[ae_syntax::expression::FieldInitializer]>)>,
    struct_constructors: HashSet<usize>,
    enum_variants: HashSet<usize>,
    pointer_cast_targets: HashMap<usize, bool>,
    closure_headers: HashMap<usize, std::sync::Arc<ae_syntax::expression::Expression>>,
    values: HashMap<usize, Rc<ae_syntax::expression::Expression>>,
    pointer_store_names: HashMap<usize, String>,
    match_patterns: HashMap<usize, ae_syntax::pattern::Pattern>,
    scalar_bodies: HashMap<usize, std::sync::Arc<[ae_syntax::statement::Statement]>>,
    scalar_definitions: HashMap<usize, ae_resolve::DefId>,
    scalar_reads: HashMap<usize, ae_resolve::DefId>,
    scalar_pointer_receivers: HashSet<usize>,
    scalar_address_casts: HashSet<usize>,
    vector_definitions: HashSet<ae_resolve::DefId>,
    owned_vectors: HashSet<ae_resolve::DefId>,
    /// A local bound from a generic struct literal or constructor, kept as
    /// its template and source-spelled type arguments: the field walk types
    /// one field's payload from them without instantiating a layout
    /// (stateless, so the index does not depend on parse order).
    generic_struct_bindings: HashMap<ae_resolve::DefId, (usize, Vec<ShapeArg>)>,
    owned_text_bindings: HashSet<ae_resolve::DefId>,
    enum_bindings: HashSet<ae_resolve::DefId>,
    enum_constructors: HashSet<usize>,
    token_cursor_definitions: HashSet<ae_resolve::DefId>,
    scalar_expression_types: HashMap<std::ops::Range<usize>, ae_resolve::resolve::ExpressionType>,
    scalar_writes: HashMap<usize, ae_resolve::DefId>,
    /// Reads of an owned string through a closure capture slot.
    closure_capture_reads: HashSet<usize>,
    array_definitions: HashSet<ae_resolve::DefId>,
    struct_definitions: HashSet<ae_resolve::DefId>,
    /// The struct name each struct-typed local or parameter spells, resolved
    /// through the registry: a `Named` spelling reaches this module's def
    /// table, while an imported type (`Unknown`) names the struct directly.
    struct_definition_names: HashMap<ae_resolve::DefId, String>,
    /// Struct-typed bindings that hold the struct by value (spelled `Named`,
    /// not borrowed). `return binding;` moves one out of the body.
    owned_struct_bindings: HashSet<ae_resolve::DefId>,
    /// Locals bound to a struct *borrow* (`r: &Rect`): a value-position read
    /// is the typed route's "cannot store a borrowed struct in a variable"
    /// error, so the scan admits the read for the binding arm to answer.
    struct_borrow_reads: HashSet<ae_resolve::DefId>,
    /// Statement-level errors the shared module parse recovered from, keyed
    /// by the body's opening brace (the same key a collected body uses). The
    /// deny switch reports the shared diagnostic for that body; the default
    /// route still falls back to the typed parser, whose message matches.
    shared_body_errors: HashMap<usize, ae_span::Diagnostic>,
    struct_field_reads: HashSet<usize>,
    struct_field_chains: HashSet<usize>,
    /// String-typed field reads: a view, never a move. Lowered in concat
    /// operands, print arguments, and call arguments (a builtin or a
    /// `&string` parameter only borrows it).
    struct_field_views: HashSet<usize>,
    /// Field reads of a copy-semantics handle (`atomic`/`shared`/`mutex`):
    /// admissible where a handle is consumed (`cell_load(t.cancelled)`).
    struct_field_handles: HashSet<usize>,
    /// Field store targets on a mutable struct receiver (`r.n = v`): the
    /// base is a `&mut` view, so the write joins the body route exactly as
    /// an impl method's `self.n = v` does.
    struct_field_writes: HashSet<usize>,
    drop_free_constructions: HashSet<usize>,
    /// Constructions of a file-local struct that declares `fn drop(&mut
    /// self)` and whose fields are all plain: the generated `ae_tN_release`
    /// calls the user's drop (then clears the handle), so the body route owns
    /// the value exactly as the typed route does.
    drop_constructions: HashSet<usize>,
    /// Constructions of a file-local struct whose fields are scalar or
    /// `string`: the literal itself is pure, and every owned field value
    /// moves into it (the generated `ae_tN_release` releases them later).
    owned_constructions: HashSet<usize>,
    /// Locals that own a string by value: a `string` parameter or a binding
    /// initialized from a text literal. A construction may move one into an
    /// owned field.
    owned_string_bindings: HashSet<ae_resolve::DefId>,
    /// Local bindings and parameters whose spelling is a map (`Composite`
    /// family "map"): map builtins and `m[key] = v` stores borrow them.
    map_definitions: HashSet<ae_resolve::DefId>,
    /// Locals whose spelling is `fn(T, …) -> R`: a call through the value
    /// lowers as `Expr::FnCall` on the binding's slot.
    fn_value_bindings: HashSet<ae_resolve::DefId>,
    /// Bindings initialized by a closure literal: a call on such a name is a
    /// closure call (captures read at the call site), not a plain function.
    closure_bindings: HashSet<ae_resolve::DefId>,
    /// Bindings initialized by an explicit generic call (`let x = f[T](..)`) or
    /// a method call (`let x = r.shifted(..)`): the concrete return type exists
    /// only at lowering time, so field and method use of such a local is
    /// admitted permissively there.
    generic_call_bindings: HashSet<ae_resolve::DefId>,
    /// Call sites whose callee is a local binding (a function value): the
    /// callee offset names the def.
    call_definitions: HashMap<usize, ae_resolve::DefId>,
    /// Stores of an owned value into a `string` field through a `&mut`
    /// receiver (`p.buf = ...`): the codegen releases the old value first
    /// and consumes the right-hand side.
    owned_field_writes: HashSet<usize>,
}

struct Parser<'a> { file: String, source: &'a str, tokens: Rc<Vec<Token>>, shared_source: Rc<str>, pos: usize, names: HashMap<String, (usize, bool, Type)>, binding_ranges: HashMap<usize, std::ops::Range<usize>>, capture_origins: HashMap<usize, usize>, resolved_expressions: Rc<OnceCell<ResolvedExpressions>>, next_var: usize, expression_depth: usize, expression_nodes: usize, block_depth: usize, loop_depth: usize, loop_scopes: Vec<usize>, loop_labels: Vec<Option<usize>>, next_label: usize, unsafe_depth: usize, functions: &'a mut SharedFunctions, structs: &'a mut Vec<StructDef>, struct_names: &'a mut HashMap<String, usize>, enums: &'a mut Vec<EnumDef>, enum_names: &'a mut HashMap<String, usize>, generics: &'a mut Generics, ctx: &'a mut CompileContext, id_order: Vec<usize>, in_main: bool, return_type: Type, moved: std::collections::HashSet<usize>, borrows: Vec<(usize, usize, BorrowKind)>, deferred_write_conflicts: Vec<(usize, usize, String, &'static str, Option<String>)>, deferred_borrow_spawns: Vec<(usize, Vec<usize>, usize)>, call_borrows: Vec<(usize, usize)>, mut_borrows: Vec<(usize, usize)>, scopes: Vec<Vec<(usize, Type)>>, owned_params: Vec<(usize, Type)>, subst: HashMap<String, Type>, trait_bounds: HashMap<String, Vec<usize>>, receiver: Option<Type>, impl_self: Option<Type>, inherent_impl: bool, shadow_ok: bool, impl_fn_id: Option<usize>, fn_once_type_params: std::collections::HashSet<String>, fn_once_slots: std::collections::HashSet<usize>, pending: Vec<Function> }
impl Parser<'_> {
    pub(super) fn is_owned(&self, ty: Type) -> bool {
        is_owned_type(ty, self.structs, self.enums)
    }
}
#[derive(Debug, Default)]
struct Program { files: Vec<Vec<Function>>, structs: Vec<StructDef>, enums: Vec<EnumDef>, ctx: CompileContext }

#[allow(clippy::too_many_arguments)]
fn make_parser<'a>(file: &str, source: &'a str, tokens: Rc<Vec<Token>>, functions: &'a mut SharedFunctions, structs: &'a mut Vec<StructDef>, struct_names: &'a mut HashMap<String, usize>, enums: &'a mut Vec<EnumDef>, enum_names: &'a mut HashMap<String, usize>, generics: &'a mut Generics, ctx: &'a mut CompileContext) -> Parser<'a> {
    Parser { file: file.to_owned(), source, tokens, shared_source: Rc::from(source), pos: 0, names: HashMap::new(), binding_ranges: HashMap::new(), capture_origins: HashMap::new(), resolved_expressions: Rc::default(), next_var: 0, expression_depth: 0, expression_nodes: 0, block_depth: 0, loop_depth: 0, loop_scopes: Vec::new(), loop_labels: Vec::new(), next_label: 0, unsafe_depth: 0, functions, structs, struct_names, enums, enum_names, generics, ctx, id_order: Vec::new(), in_main: false, return_type: Type::Int(IntTy::I64), moved: std::collections::HashSet::new(), borrows: Vec::new(), deferred_write_conflicts: Vec::new(), deferred_borrow_spawns: Vec::new(), call_borrows: Vec::new(), mut_borrows: Vec::new(), scopes: Vec::new(), owned_params: Vec::new(), subst: HashMap::new(), trait_bounds: HashMap::new(), receiver: None, impl_self: None, inherent_impl: false, shadow_ok: false, impl_fn_id: None, fn_once_type_params: std::collections::HashSet::new(), fn_once_slots: std::collections::HashSet::new(), pending: Vec::new() }
}

#[allow(dead_code)] // exercised by in-memory unit tests
pub fn check(source: &str) -> Result<(), String> {
    check_source_diagnostic(source).map_err(|diagnostic| diagnostic.to_string())
}

pub(crate) fn check_source_diagnostic(source: &str) -> Result<(), FrontendDiagnostic> {
    // The front end recurses over nesting (expressions, blocks, generic
    // instances, trait resolution). A pathological input — a malformed
    // generic body whose enclosing function still parses to a complete
    // `main`, for instance — can drive that depth past a 2 MB thread's
    // stack, which aborts the process instead of producing a diagnostic.
    // A dedicated large stack keeps every malformed input a diagnostic;
    // the structural fix (an explicit depth limit at the HIR cutover)
    // belongs to phase P1.
    const FRONTEND_STACK: usize = 16 * 1024 * 1024;
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(FRONTEND_STACK)
            .spawn_scoped(scope, || check_source_here(source))
            .expect("spawn the front-end worker")
            .join()
            .expect("the front-end worker returns a diagnostic, never a panic")
    })
}

fn check_source_here(source: &str) -> Result<(), FrontendDiagnostic> {
    let mut shared: SharedFunctions = HashMap::new();
    let mut structs: Vec<StructDef> = prelude_payload_structs();
    let mut struct_names: HashMap<String, usize> = HashMap::new();
    // Prelude result types are registered first, so their ids are stable.
    let (mut enums, mut enum_names) = prelude_enums();
    let mut generics = prelude_generics();
    let mut ctx = CompileContext::new();
    types::seed_preset_result_instantiations(&mut generics, &enum_names, &ctx);
    let mut next_id = 0usize;
    parse_source("", source, &mut shared, &mut structs, &mut struct_names, &mut enums, &mut enum_names, &mut generics, &mut ctx, &mut next_id).map(|_| ())
}
#[allow(dead_code)] // exercised by in-memory unit tests
pub fn compile(source: &str) -> Result<String, String> {
    compile_with_context(source).map(|(c, _)| c)
}

pub fn compile_with_context(source: &str) -> Result<(String, CompileContext), String> {
    let mut shared: SharedFunctions = HashMap::new();
    let mut structs: Vec<StructDef> = prelude_payload_structs();
    let mut struct_names: HashMap<String, usize> = HashMap::new();
    // Prelude result types are registered first, so their ids are stable.
    let (mut enums, mut enum_names) = prelude_enums();
    let mut generics = prelude_generics();
    let mut ctx = CompileContext::new();
    types::seed_preset_result_instantiations(&mut generics, &enum_names, &ctx);
    let mut next_id = 0usize;
    let functions = parse_source("", source, &mut shared, &mut structs, &mut struct_names, &mut enums, &mut enum_names, &mut generics, &mut ctx, &mut next_id).map_err(|diagnostic| diagnostic.to_string())?;
    let mut program = Program { files: vec![functions], structs, enums, ctx };
    let generated = emit_program(&mut program)?;
    Ok((generated.full, program.ctx))
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

pub fn compile_path_with_imports(path: &Path, imports: &PackageImports) -> Result<String, String> {
    compile_path_parts_with_imports(path, imports).map(|(runtime, user)| runtime + &user)
}

/// The build driver's incremental view: `(runtime_prefix, program_suffix)`.
/// Concatenating the two reproduces `compile_path` exactly, so `emit-c` stays
/// the authority for what a program means.
pub fn compile_path_parts(path: &Path) -> Result<(String, String), String> {
    let mut program = load_program(path)?;
    let generated = emit_program(&mut program)?;
    let GeneratedC { full, runtime_len } = generated;
    let user = full[runtime_len..].to_owned();
    let mut runtime = full;
    runtime.truncate(runtime_len);
    Ok((runtime, user))
}

pub fn compile_path_parts_with_imports(path: &Path, imports: &PackageImports) -> Result<(String, String), String> {
    let mut program = load_program_with_imports(path, imports)?;
    let generated = emit_program(&mut program)?;
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

pub fn tree_path_with_imports(path: &Path, imports: &PackageImports) -> Result<String, String> {
    tree::tree_path_with_imports(path, imports)
}

/// The control-flow graph of each function: blocks whose contents are the
/// statement tree's own statements, printed as text (`ae emit-cfg`).
pub fn cfg_path(path: &Path) -> Result<String, String> {
    cfg::cfg_path(path)
}

pub fn cfg_path_with_imports(path: &Path, imports: &PackageImports) -> Result<String, String> {
    cfg::cfg_path_with_imports(path, imports)
}

/// The MIR: places, rvalues, and explicit temporaries over the CFG, printed
/// as text (`ae emit-mir`).
pub fn mir_path(path: &Path) -> Result<String, String> {
    mir::mir_path(path)
}

pub fn mir_path_with_imports(path: &Path, imports: &PackageImports) -> Result<String, String> {
    mir::mir_path_with_imports(path, imports)
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

pub fn interpret_path_with_imports(path: &Path, imports: &PackageImports) -> Result<(String, u8), (String, String)> {
    interp::interpret_path_with_imports(path, imports)
}

/// Compile a file and report what the output depends on, in one pass. This is
/// the only entry point that exposes the unit list; the plain `compile_path`
/// forms stay the authority for what a program means.
pub fn compile_path_tracked(path: &Path) -> Result<TrackedC, String> {
    compile_path_tracked_with_imports(path, &PackageImports::new())
}

pub fn compile_path_tracked_with_imports(path: &Path, imports: &PackageImports) -> Result<TrackedC, String> {
    let t0 = std::time::Instant::now();
    let (mut program, units) = loader::load_with_units_and_imports(path, imports).map_err(String::from)?;
    let t1 = std::time::Instant::now();
    let GeneratedC { full, runtime_len } = emit_program(&mut program)?;
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

    // ADR-0235 slice 1: the generic vector constructor names its interned
    // element type and maps onto the C family that currently carries it.
    #[test]
    fn generic_vector_constructor_names_and_c_families() {
        let mut ctx = CompileContext::new();
        let int = ctx.intern_vector_elem(Type::Int(IntTy::I64));
        let number = Type::Vector(int);
        assert_eq!(number.name(&ctx), "vec[i64]");
        assert_eq!(c_output::c_type_field(number, &ctx), "ae_gvec");
        assert!(number.owned(), "a vector owns its storage");
        let text = ctx.intern_vector_elem(Type::String);
        assert_eq!(Type::Vector(text).name(&ctx), "vec[string]");
        assert_eq!(c_output::c_type_field(Type::Vector(text), &ctx), "ae_strvec");
        let float = ctx.intern_vector_elem(Type::Float(FloatTy::F64));
        assert_eq!(c_output::c_type_field(Type::Vector(float), &ctx), "ae_gvec");
        let view = Type::VectorRef(text);
        assert_eq!(view.name(&ctx), "&vec[string]");
        assert_eq!(c_output::c_type_field(view, &ctx), "const ae_strvec *");
        assert!(!view.owned(), "a borrow owns nothing");
        let bool_id = ctx.intern_vector_elem(Type::Bool);
        let bool_vec = Type::Vector(bool_id);
        assert_eq!(bool_vec.name(&ctx), "vec[bool]");
        assert_eq!(c_output::vector_family(&ctx, bool_id), "ae_gvec");
        assert_eq!(bool_vec.c_temp(&ctx), "ae_gvec ");
    }

    // ADR-0235 slice 2: generic map and array constructors name their
    // interned types and map onto the C families that currently carry them.
    #[test]
    fn generic_map_and_array_constructors_names_and_c_families() {
        let mut ctx = CompileContext::new();
        let m_int_id = ctx.intern_map_types(Type::String, Type::Int(IntTy::I64));
        let m_int = Type::MapOf(m_int_id);
        assert_eq!(m_int.name(&ctx), "map[string, i64]");
        assert_eq!(c_output::map_family(&ctx, m_int_id), "ae_map");
        assert!(m_int.owned(), "a map owns its storage");
        let m_f64_id = ctx.intern_map_types(Type::String, Type::Float(FloatTy::F64));
        let m_f64 = Type::MapOf(m_f64_id);
        assert_eq!(m_f64.name(&ctx), "map[string, f64]");
        assert_eq!(c_output::map_family(&ctx, m_f64_id), "ae_mapf");
        assert_eq!(ctx.find_map_types(Type::String, Type::Float(FloatTy::F64)), Some(m_f64_id));

        let arr_elem = ctx.intern_array_elem(Type::Int(IntTy::I64));
        let arr = Type::ArrayOf(arr_elem, 4);
        assert_eq!(arr.name(&ctx), "&[i64; 4]");
        assert!(!arr.owned(), "a borrowed array owns nothing");
        assert_eq!(ctx.find_array_elem(Type::Int(IntTy::I64)), Some(arr_elem));
    }

    // P2.4: every preset result family derives from the one generic Result
    // declaration, and the two multi-value families name the prelude payload
    // structs (ADR-0234). This pins the family order the C ids depend on.
    #[test]
    fn prelude_result_families_are_result_instantiations() {
        let (enums, names) = prelude_enums();
        let result = result_generic_enum();
        for (index, (alias, args)) in prelude_result_families().iter().enumerate() {
            assert_eq!(enums[index].name, *alias);
            assert_eq!(names[*alias], index);
            let variants = specialize_result_variants(&result, args);
            assert_eq!(enums[index].variants, variants);
            assert_eq!(variants[0].1, vec![args[0]]);
            assert_eq!(variants[1].1, vec![Type::String]);
            assert!(enums[index].aliases.contains(&"Result"));
        }
        assert_eq!(names["WriteResult"], names["IntResult"]);
        let structs = prelude_payload_structs();
        assert_eq!(structs[0].name, "FileInfo");
        assert_eq!(structs[1].name, "CommandOutput");
        assert!(matches!(prelude_result_families()[3].1[0], Type::Struct(0)));
        assert!(matches!(prelude_result_families()[6].1[0], Type::Struct(1)));
    }

    // The audit's P0.2 acceptance: two programs compiled one after the
    // other must not answer from each other's registries. Function ids
    // restart at 0 for every program, so a leaked `unsafe fn` at id 0
    // would make the second program's safe function id 0 unsafe.
    #[test]
    fn compilation_registries_do_not_leak_between_programs() {
        // Program A: an `unsafe fn` at id 0 (E0704 wants the doc comment).
        let a = "/// Preconditions: v is a valid i64; single-threaded context.\nunsafe fn risky(v: i64) -> i64 { return v * 2; }\nfn main() { unsafe { print(risky(1)); } }";
        let (_, ctx_a) = compile_with_context(a).expect("program A compiles");
        assert!(ctx_a.is_unsafe_fn(0), "program A registers its unsafe fn at id 0");
        // Program B: id 0 is an ordinary safe function. Each compilation owns
        // its own CompileContext, so Program B starts from empty registries.
        let b = "fn safe(v: i64) -> i64 { return v + 1; }\nfn main() { print(safe(1)); }";
        let (_, ctx_b) = compile_with_context(b).expect("program B compiles");
        assert!(!ctx_b.is_unsafe_fn(0), "the unsafe marker leaked from program A");

        // Pointees: a program with an address cast fills the registry; the
        // next one starts from empty.
        let c = "fn main() { var v = 1; let raw = &mut v as *mut i64; print(2); }";
        let (_, ctx_c) = compile_with_context(c).expect("program C compiles");
        assert!(ctx_c.types.pointee_count() > 0, "the cast registered a pointee");
        let (_, ctx_plain) = compile_with_context("fn main() { print(3); }").expect("plain program compiles");
        assert_eq!(ctx_plain.types.pointee_count(), 0, "the pointee registry carried entries across programs");

        // Extern names: same rule for the C-symbol map.
        let d = "/// Preconditions: size is a positive byte count; the block is released once.\nextern \"C\" fn ae_std_alloc(size: i64, line: i64, column: i64) -> *mut i64;\nfn main() { unsafe { let _slot = ae_std_alloc(8, 1, 1); } print(1); }";
        let (_, ctx_d) = compile_with_context(d).expect("program D compiles");
        assert!(ctx_d.extern_name(0).is_some(), "the extern declaration registered");
        let (_, ctx_plain2) = compile_with_context("fn main() { print(4); }").expect("plain program 2 compiles");
        assert_eq!(ctx_plain2.extern_name(0), None, "an extern entry leaked from the previous program");
    }

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
            "fn main() { let a = [\"hi\"]; }",
            "fn main() { let a = [1, true]; }",
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
    #[test] fn i8_u8_frontend_accepts_and_rejects_width_rules() {
        let parse_ok = |source: &str| -> Result<(), String> {
            let mut shared: SharedFunctions = HashMap::new();
            let mut structs: Vec<StructDef> = Vec::new();
            let mut struct_names: HashMap<String, usize> = HashMap::new();
            let (mut enums, mut enum_names) = prelude_enums();
            let mut generics = prelude_generics();
            let mut ctx = CompileContext::new();
            let mut next_id = 0usize;
            let functions = parse_source(
                "", source, &mut shared, &mut structs, &mut struct_names,
                &mut enums, &mut enum_names, &mut generics, &mut ctx, &mut next_id,
            ).map_err(|d| d.to_string())?;
            for function in &functions {
                let mut mir_fn = mir::lower_function(function, &ctx);
                mir::optimize_function(&mut mir_fn);
                mir::elaborate_drops(&mut mir_fn);
                mir::validate(&mir_fn, &ctx)?;
            }
            Ok(())
        };

        let valid = "
            struct Pair { a: i8, b: u8 }
            fn step(x: i8, y: u8) -> i8 {
                let n = -x;
                let s = n + (1 as i8);
                let b = (y & (15 as u8)) << (1 as u8);
                let ok = x < s && y >= b;
                let wide = s as i64;
                let flt = y as f64;
                let back_i = flt as i8;
                let back_u = wide as u8;
                let p = Pair { a: (-128) as i8, b: 255 as u8 };
                if ok { return back_i + (back_u as i8) + p.a; }
                return 127 as i8;
            }
            fn main() {
                let r = step(10 as i8, 20 as u8);
                print(r);
            }
        ";
        assert!(parse_ok(valid).is_ok(), "{:?}", parse_ok(valid));

        for bad in [
            "fn main() { let x = 128 as i8; }",
            "fn main() { let x = (-129) as i8; }",
            "fn main() { let x = 256 as u8; }",
            "fn main() { let x = (-1) as u8; }",
            "fn main() { let a = 1 as i8; let b = 2; let c = a + b; }",
            "fn main() { let a = 1 as i8; let b = 2 as u8; let c = a + b; }",
            "fn main() { let a = 1 as u8; let b = -a; }",
            "fn main() { let a = 1 as i8; let b = 2 as u8; let c = a < b; }",
        ] {
            assert!(parse_ok(bad).is_err(), "expected error for: {bad}");
        }
    }
    #[test] fn all_int_widths_frontend_accepts_and_rejects_width_rules() {
        let parse_ok = |source: &str| -> Result<(), String> {
            let mut shared: SharedFunctions = HashMap::new();
            let mut structs: Vec<StructDef> = Vec::new();
            let mut struct_names: HashMap<String, usize> = HashMap::new();
            let (mut enums, mut enum_names) = prelude_enums();
            let mut generics = prelude_generics();
            let mut ctx = CompileContext::new();
            let mut next_id = 0usize;
            let functions = parse_source(
                "", source, &mut shared, &mut structs, &mut struct_names,
                &mut enums, &mut enum_names, &mut generics, &mut ctx, &mut next_id,
            ).map_err(|d| d.to_string())?;
            for function in &functions {
                let mut mir_fn = mir::lower_function(function, &ctx);
                mir::optimize_function(&mut mir_fn);
                mir::elaborate_drops(&mut mir_fn);
                mir::validate(&mir_fn, &ctx)?;
            }
            Ok(())
        };

        let valid = "
            struct AllInts {
                a: i16, b: i32, c: isize,
                d: u16, e: u32, f: u64, g: usize,
            }
            fn take_all(p: AllInts) -> usize {
                let x = -p.a + (1 as i16);
                let y = p.b << (1 as i32);
                let z = -p.c - (1 as isize);
                let u = p.d + (1 as u16);
                let v = p.e & (7 as u32);
                let w = p.f ^ (3 as u64);
                let s = p.g + (x as usize) + (y as usize) + (z as usize) + (u as usize) + (v as usize) + (w as usize);
                let f = s as f64;
                return f as usize;
            }
            fn main() {
                let p = AllInts {
                    a: (-32768) as i16, b: 2147483647 as i32, c: (-1) as isize,
                    d: 65535 as u16, e: 4294967295 as u32, f: 9223372036854775807 as u64, g: 0 as usize,
                };
                print(take_all(p));
            }
        ";
        assert!(parse_ok(valid).is_ok(), "{:?}", parse_ok(valid));

        for bad in [
            "fn main() { let x = 32768 as i16; }",
            "fn main() { let x = (-32769) as i16; }",
            "fn main() { let x = (-1) as u16; }",
            "fn main() { let x = 65536 as u16; }",
            "fn main() { let x = 2147483648 as i32; }",
            "fn main() { let x = (-2147483649) as i32; }",
            "fn main() { let x = (-1) as u32; }",
            "fn main() { let x = 4294967296 as u32; }",
            "fn main() { let x = (-1) as u64; }",
            "fn main() { let x = (-1) as usize; }",
            "fn main() { let a = 1 as isize; let b = 2; let c = a + b; }",
            "fn main() { let a = 1 as usize; let b = 2 as u64; let c = a + b; }",
            "fn main() { let a = 1 as u16; let b = -a; }",
            "fn main() { let a = 1 as u32; let b = -a; }",
            "fn main() { let a = 1 as u64; let b = -a; }",
            "fn main() { let a = 1 as usize; let b = -a; }",
        ] {
            assert!(parse_ok(bad).is_err(), "expected error for: {bad}");
        }
    }
    #[test] fn wave3a_f32_char_unit_ptr_usize_frontend() {
        let parse_ok = |source: &str| -> Result<(), String> {
            let mut shared: SharedFunctions = HashMap::new();
            let mut structs: Vec<StructDef> = Vec::new();
            let mut struct_names: HashMap<String, usize> = HashMap::new();
            let (mut enums, mut enum_names) = prelude_enums();
            let mut generics = prelude_generics();
            let mut ctx = CompileContext::new();
            let mut next_id = 0usize;
            let functions = parse_source(
                "", source, &mut shared, &mut structs, &mut struct_names,
                &mut enums, &mut enum_names, &mut generics, &mut ctx, &mut next_id,
            ).map_err(|d| d.to_string())?;
            for function in &functions {
                let mut mir_fn = mir::lower_function(function, &ctx);
                mir::optimize_function(&mut mir_fn);
                mir::elaborate_drops(&mut mir_fn);
                mir::validate(&mir_fn, &ctx)?;
            }
            Ok(())
        };

        let valid = "
            fn side_effect(u: ()) {
                let c = 'a';
                let d = '\\u{4E2D}';
                if c < d { return; }
                return ();
            }
            fn take_f32(x: f32) -> f32 {
                let y = -x + (1.5 as f32);
                let z = (y as f64) as f32;
                let n = 10 as f32;
                return z * n;
            }
            fn main() {
                let u = ();
                side_effect(u);
                let f = take_f32(2.5 as f32);
                print(f);
                let c = (65 as u32) as char;
                let code = c as u32;
                print(c);
                print(code);
                var v = 42;
                unsafe {
                    let p = &mut v as *mut i64;
                    let addr = p as usize;
                    let q = addr as *mut i64;
                    *q = 99;
                }
                return;
            }
        ";
        assert!(parse_ok(valid).is_ok(), "{:?}", parse_ok(valid));

        for bad in [
            "fn main() { let a = 1.0 as f32; let b = a + 1.0; }",
            "fn main() { let a = 1.0 as f32; let b = a % (2.0 as f32); }",
            "fn main() { let c = 'a' + 'b'; }",
            "fn main() { let c = -'a'; }",
            "fn main() { let c = 'a' as i64; }",
            "fn main() { let c = 65 as char; }",
            "fn main() { let x = 1; let p = &x as *const i64; let a = p as usize; }",
            "fn main() { let a = 0 as usize; let p = a as *const i64; }",
        ] {
            assert!(parse_ok(bad).is_err(), "expected error for: {bad}");
        }
    }
    #[test] fn wave3b_int_methods_frontend() {
        let parse_ok = |source: &str| -> Result<(), String> {
            let mut shared: SharedFunctions = HashMap::new();
            let mut structs: Vec<StructDef> = Vec::new();
            let mut struct_names: HashMap<String, usize> = HashMap::new();
            let (mut enums, mut enum_names) = prelude_enums();
            let mut generics = prelude_generics();
            let mut ctx = CompileContext::new();
            let mut next_id = 0usize;
            let functions = parse_source(
                "", source, &mut shared, &mut structs, &mut struct_names,
                &mut enums, &mut enum_names, &mut generics, &mut ctx, &mut next_id,
            ).map_err(|d| d.to_string())?;
            for function in &functions {
                let mut mir_fn = mir::lower_function(function, &ctx);
                mir::optimize_function(&mut mir_fn);
                mir::elaborate_drops(&mut mir_fn);
                mir::validate(&mir_fn, &ctx)?;
            }
            Ok(())
        };

        let valid = "
            fn add_checked(a: i8, b: i8) -> Option[i8] {
                let v = a.checked_add(b)?;
                return Option[i8]::Some(v);
            }
            fn main() {
                let a = 127 as i8;
                let w1 = a.wrapping_add(1 as i8);
                let w2 = ((-128) as i8).wrapping_neg();
                let s1 = a.saturating_add(10 as i8);
                let u = (5 as u8).saturating_sub(20 as u8);
                let wu = (1 as u64).wrapping_neg();
                let res = add_checked(a, 1 as i8);
                match res {
                    Option::Some(v) => { print(v); }
                    Option::None => { print(w1); print(w2); print(s1); print(u); print(wu); }
                }
            }
        ";
        assert!(parse_ok(valid).is_ok(), "{:?}", parse_ok(valid));

        for bad in [
            "fn main() { let x = (1 as i8).checked_add(1 as i64); }",
            "fn main() { let x = (1 as i8).wrapping_add(1 as u8); }",
            "fn main() { let x = (1 as i8).wrapping_add(); }",
            "fn main() { let x = (1 as u8).checked_neg(); }",
            "fn main() { let x = (1.0).wrapping_add(1.0); }",
            "fn main() { let x = true.wrapping_add(false); }",
        ] {
            assert!(parse_ok(bad).is_err(), "expected error for: {bad}");
        }
    }
    #[test] fn p2_2_copy_semantics_frontend() {
        let parse_ok = |source: &str| -> Result<(), String> {
            let mut shared: SharedFunctions = HashMap::new();
            let mut structs: Vec<StructDef> = Vec::new();
            let mut struct_names: HashMap<String, usize> = HashMap::new();
            let (mut enums, mut enum_names) = prelude_enums();
            let mut generics = prelude_generics();
            let mut ctx = CompileContext::new();
            let mut next_id = 0usize;
            let functions = parse_source(
                "", source, &mut shared, &mut structs, &mut struct_names,
                &mut enums, &mut enum_names, &mut generics, &mut ctx, &mut next_id,
            ).map_err(|d| d.to_string())?;
            for function in &functions {
                let mut mir_fn = mir::lower_function(function, &ctx);
                mir::optimize_function(&mut mir_fn);
                mir::elaborate_drops(&mut mir_fn);
                mir::validate(&mir_fn, &ctx)?;
            }
            Ok(())
        };

        let valid = "
            #[derive(Copy)]
            struct Point { x: i64, y: i64 }

            #[repr(C)]
            #[derive(Copy)]
            struct Handle { fd: i32 }

            #[derive(Copy)]
            enum Tag { A(i64), B }

            #[derive(Copy)]
            struct Pair[A, B] { first: A, second: B }

            fn take_point(p: Point) -> i64 { return p.x + p.y; }
            fn dup[T: Copy](x: T) -> T { return x; }

            fn main() {
                let p = Point { x: 1, y: 2 };
                let p2 = p;
                let s1 = take_point(p);
                let s2 = take_point(p2);
                let p3 = dup[Point](p);
                let h = Handle { fd: 3 as i32 };
                let h2 = h;
                let t = Tag::A(10);
                let t2 = t;
                let pair = Pair[i8, u8] { first: 1 as i8, second: 2 as u8 };
                let pair2 = pair;
                let n = dup[i8](1 as i8);
                print(s1 + s2 + p2.x + (h2.fd as i64) + (n as i64));
                match t2 { Tag::A(v) => { print(v); } Tag::B => { print(0); } }
                print(pair2.first);
            }
        ";
        assert!(parse_ok(valid).is_ok(), "{:?}", parse_ok(valid));

        for bad in [
            "#[derive(Copy)]\nstruct Bad { s: string }\nfn main() {}",
            "#[derive(Copy)]\nenum Bad { S(string) }\nfn main() {}",
            "#[derive(Copy)]\nstruct DropCopy { x: i64 }\nimpl DropCopy { fn drop(&mut self) -> i64 { return 0; } }\nfn main() {}",
            "#[derive(Copy)]\nfn bad() {}\nfn main() {}",
            "#[derive(Copy)]\nstruct Pair[T] { v: T }\nfn main() { let p = Pair[string] { v: \"a\" }; }",
        ] {
            assert!(parse_ok(bad).is_err(), "expected error for: {bad}");
        }
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
        let (plain, len) = prune_runtime(prefix, "", &[], &[], "", "int main(void) { ae_plain(); return 0; }\n");
        assert!(plain[..len].contains(WIN32_HEADER_OMITTED), "header should be dropped");
        assert!(!plain[..len].contains(WIN32_HEADER_INCLUDE), "include line must be gone");
        assert!(!plain.contains("ae_win"), "unreachable Win32 function must be pruned");

        let (win, len) = prune_runtime(prefix, "", &[], &[], "", "int main(void) { ae_win(); return 0; }\n");
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
        let ctx = CompileContext::new();
        for &ty in AUDIT_TYPES {
            if Bound::Add.allows(ty, &[], &[]) { assert!(Capability::Add.supports(ty), "Add admits {}", ty.name(&ctx)); }
            if Bound::Ord.allows(ty, &[], &[]) { assert!(Capability::Ord.supports(ty), "Ord admits {}", ty.name(&ctx)); }
            if Bound::Eq.allows(ty, &[], &[]) { assert!(Capability::Eq.supports(ty), "Eq admits {}", ty.name(&ctx)); }
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
        let mut ctx = CompileContext::new();
        let mut next_id = 0usize;
        let source = "fn f[T: Add](a: T, b: T) -> T { return a + b; }\n\
fn g[U: Ord](a: U, b: U) -> U { return a + b; }\n\
fn h[V: Eq](a: V, b: V) -> bool { return a == b; }\n\
fn main() { print(f[i64](1, 2)); print(g[i64](3, 4)); print(h[bool](true, true)); }";
        parse_source("", source, &mut shared, &mut structs, &mut struct_names,
            &mut enums, &mut enum_names, &mut generics, &mut ctx, &mut next_id).unwrap();
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

    #[test]
    fn p2_2_drop_trait_and_order_frontend() {
        use super::*;
        let parse_with_structs = |source: &str| -> Result<(Vec<Function>, Vec<StructDef>), String> {
            let mut shared: SharedFunctions = HashMap::new();
            let mut structs: Vec<StructDef> = Vec::new();
            let mut struct_names: HashMap<String, usize> = HashMap::new();
            let (mut enums, mut enum_names) = prelude_enums();
            let mut generics = prelude_generics();
            let mut ctx = CompileContext::new();
            let mut next_id = 0usize;
            let functions = parse_source(
                "", source, &mut shared, &mut structs, &mut struct_names,
                &mut enums, &mut enum_names, &mut generics, &mut ctx, &mut next_id,
            ).map_err(|d| d.to_string())?;
            Ok((functions, structs))
        };

        // 1. `impl Drop for T` with omitted return (`-> ()`) sets `drop_returns_unit = true`.
        let (_, structs) = parse_with_structs("
            struct Guard { id: i64 }
            impl Drop for Guard {
                fn drop(&mut self) { print(self.id); }
            }
            fn main() {
                let a = Guard { id: 1 };
                let b = Guard { id: 2 };
            }
        ").expect("impl Drop with omitted return");
        assert!(structs[0].drop_fn.is_some());
        assert!(structs[0].drop_returns_unit);

        // 2. `impl Drop for T` with explicit `-> ()` sets `drop_returns_unit = true`.
        let (_, structs) = parse_with_structs("
            struct Guard { id: i64 }
            impl Drop for Guard {
                fn drop(&mut self) -> () { print(self.id); }
            }
            fn main() { let g = Guard { id: 1 }; }
        ").expect("impl Drop with -> ()");
        assert!(structs[0].drop_fn.is_some());
        assert!(structs[0].drop_returns_unit);

        // 3. Legacy `fn drop(&mut self) -> i64` keeps `drop_returns_unit = false`.
        let (_, structs) = parse_with_structs("
            struct Guard { id: i64 }
            impl Guard {
                fn drop(&mut self) -> i64 { print(self.id); return 0; }
            }
            fn main() { let g = Guard { id: 1 }; }
        ").expect("legacy drop -> i64");
        assert!(structs[0].drop_fn.is_some());
        assert!(!structs[0].drop_returns_unit);

        // 4. Reverse declaration order in `Statement::Frees` (later `b` freed before earlier `a`).
        let (functions, _) = parse_with_structs("
            struct Guard { id: i64 }
            impl Drop for Guard {
                fn drop(&mut self) { print(self.id); }
            }
            fn main() {
                let a = Guard { id: 1 };
                let b = Guard { id: 2 };
            }
        ").expect("reverse drop order");
        let main_fn = functions.iter().find(|f| f.id == 1).expect("main");
        let frees = main_fn.body.iter().find_map(|s| match s {
            Statement::Frees(items) => Some(items),
            _ => None,
        }).expect("main frees");
        assert_eq!(frees.len(), 2);
        assert!(frees[0].0 > frees[1].0, "expected reverse declaration order, got {frees:?}");

        // 5. Rejections.
        for bad in [
            "trait Drop { fn drop(&mut self); } fn main() {}",
            "#[derive(Copy)] struct C { x: i64 } impl Drop for C { fn drop(&mut self) {} } fn main() {}",
            "struct G { x: i64 } impl G { fn drop(&mut self) {} } impl Drop for G { fn drop(&mut self) {} } fn main() {}",
            "struct G { x: i64 } impl Drop for G { pub fn drop(&mut self) {} } fn main() {}",
            "struct G { x: i64 } impl Drop for G { fn drop(&mut self) {} } fn main() { var g = G { x: 1 }; g.drop(); }",
            "struct G { x: i64 } impl Drop for G { fn drop(&mut self) {} } fn main() { var g = G { x: 1 }; G::drop(&mut g); }",
        ] {
            assert!(parse_with_structs(bad).is_err(), "expected error for: {bad}");
        }
    }

    #[test]
    fn p2_3_fn_ptrs_frontend() {
        use super::*;
        let parse_ok = |source: &str| -> Result<Vec<Function>, String> {
            let mut shared: SharedFunctions = HashMap::new();
            let mut structs: Vec<StructDef> = Vec::new();
            let mut struct_names: HashMap<String, usize> = HashMap::new();
            let (mut enums, mut enum_names) = prelude_enums();
            let mut generics = prelude_generics();
            let mut ctx = CompileContext::new();
            let mut next_id = 0usize;
            let functions = parse_source(
                "", source, &mut shared, &mut structs, &mut struct_names,
                &mut enums, &mut enum_names, &mut generics, &mut ctx, &mut next_id,
            ).map_err(|d| d.to_string())?;
            for function in &functions {
                let mut mir_fn = mir::lower_function(function, &ctx);
                mir::optimize_function(&mut mir_fn);
                mir::elaborate_drops(&mut mir_fn);
                mir::validate(&mir_fn, &ctx)?;
            }
            Ok(functions)
        };

        let valid = "
            fn double(x: i64) -> i64 { return x * 2; }
            fn noop(x: i64) { print(x); }
            #[derive(Copy)]
            struct Holder { cb: fn(i64) -> i64, side: fn(i64) }
            struct Wrapper[T] { val: T }
            enum Action { Run(fn(i64) -> i64), None }
            fn pick(flag: bool) -> fn(i64) -> i64 {
                if flag { return double; }
                return |x: i64| -> i64 { return x + 10; };
            }
            fn higher() -> fn(i64) -> fn(i64) -> i64 {
                return pick_wrap;
            }
            fn pick_wrap(x: i64) -> fn(i64) -> i64 {
                return pick(x > 0);
            }
            fn main() {
                var f = double;
                let a = f(3);
                f = |x: i64| -> i64 { return x + 1; };
                let b = f(3);
                let h = Holder { cb: double, side: noop };
                let h2 = h;
                let c = h.cb(4);
                let d = (h2.cb)(5);
                (h.side)(a + b + c + d);
                let e = pick(false)(7);
                let w = Wrapper[fn(i64) -> i64] { val: double };
                let opt = Option[fn(i64) -> i64]::Some(double);
                let act = Action::Run(|x: i64| -> i64 { return x * 3; });
                print(e + w.val(2));
                match opt {
                    Option::Some(g) => { print(g(1)); }
                    Option::None => { print(0); }
                }
                match act {
                    Action::Run(g) => { print(g(2)); }
                    Action::None => { print(0); }
                }
            }
        ";
        parse_ok(valid).expect("valid fn ptr program");

        for bad in [
            "fn main() { let k = 1; let h = Holder { cb: |x: i64| -> i64 { return x + k; } }; } struct Holder { cb: fn(i64) -> i64 }",
            "unsafe fn risky(x: i64) -> i64 { return x; } fn main() { let f = risky; }",
            "fn takes_bool(b: bool) -> i64 { return 0; } struct Holder { cb: fn(i64) -> i64 } fn main() { let h = Holder { cb: takes_bool }; }",
            "fn bad(cb: fn(&string) -> &string) -> i64 { return 0; } fn main() {}",
        ] {
            assert!(parse_ok(bad).is_err(), "expected error for: {bad}");
        }
    }

    #[test]
    fn p2_3_first_class_closures_frontend() {
        use super::*;
        let parse_ok = |source: &str| -> Result<Vec<Function>, String> {
            let mut shared: SharedFunctions = HashMap::new();
            let mut structs: Vec<StructDef> = Vec::new();
            let mut struct_names: HashMap<String, usize> = HashMap::new();
            let (mut enums, mut enum_names) = prelude_enums();
            let mut generics = prelude_generics();
            let mut ctx = CompileContext::new();
            let mut next_id = 0usize;
            let functions = parse_source(
                "", source, &mut shared, &mut structs, &mut struct_names,
                &mut enums, &mut enum_names, &mut generics, &mut ctx, &mut next_id,
            ).map_err(|d| d.to_string())?;
            for function in &functions {
                let mut mir_fn = mir::lower_function(function, &ctx);
                mir::optimize_function(&mut mir_fn);
                mir::elaborate_drops(&mut mir_fn);
                mir::validate(&mir_fn, &ctx)?;
            }
            Ok(functions)
        };

        let valid = "
            fn double(x: i64) -> i64 { return x * 2; }
            fn apply[F: Fn(i64) -> i64](f: F, x: i64) -> i64 { return f(x); }
            fn apply_copy[F: Fn(i64) -> i64 + Copy](f: F, x: i64) -> i64 { return f(x) + f(x); }
            fn run_mut[F: FnMut(i64) -> i64](var f: F, x: i64) -> i64 { return f(x); }
            fn run_once[F: FnOnce() -> string](f: F) -> string { return f(); }
            fn make_adder(base: i64) -> impl Fn(i64) -> i64 {
                return |x: i64| -> i64 { return x + base; };
            }
            fn main() {
                let offset = 10;
                let add_off = |x: i64| -> i64 { return x + offset; };
                print(apply(add_off, 5));
                print(apply(|x: i64| -> i64 { return x + offset; }, 7));
                print(apply(double, 3));
                print(apply_copy(add_off, 2));
                var total = 1;
                let inc = |x: i64| -> i64 { total = total + x; return total; };
                print(run_mut(inc, 4));
                let s = \"owned\";
                let take = || -> string { return s; };
                print(run_once(take));
                let s2 = \"borrowed\";
                print(apply(|x: i64| -> i64 { return len(s2) + x; }, 3));
                let adder = make_adder(20);
                print(adder(5));
                print(apply(adder, 6));
            }
        ";
        parse_ok(valid).expect("valid first-class closure program");

        for bad in [
            // Calling FnOnce parameter twice -> E0102
            "fn bad_once[F: FnOnce() -> i64](f: F) -> i64 { return f() + f(); } fn main() { let s = \"hi\"; print(bad_once(|| -> i64 { let t = s; return len(t); })); }",
            // Returning closure that borrows a &string parameter -> escaping borrow
            "fn bad_ret(s: &string) -> impl Fn() -> i64 { return || -> i64 { return len(s); }; } fn main() {}",
            // Mutating an immutable `let` capture -> rejected
            "fn main() { let x = 0; let f = || -> i64 { x = 1; return x; }; }",
        ] {
            assert!(parse_ok(bad).is_err(), "expected error for: {bad}");
        }
    }
}

