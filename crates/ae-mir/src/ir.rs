//! Core MIR data structures (P3.1).
//!
//! Pure control-flow graph representation independent of syntax and HIR.
//! Types come exclusively from `ae-ty` and source positions from `ae-span`.

use std::collections::{BTreeMap, BTreeSet};

use ae_span::Span;
use ae_ty::{DefId, TyKind};

/// Index into [`Body::basic_blocks`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BasicBlockId(pub u32);

impl BasicBlockId {
    pub const ENTRY: Self = Self(0);

    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Index into [`Body::locals`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalId(pub u32);

impl LocalId {
    /// Conventional return place `_0`.
    pub const RETURN_PLACE: Self = Self(0);

    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Complete MIR body for one function or closure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Body {
    pub name: String,
    pub def: Option<DefId>,
    /// Number of parameters (`_1..=arg_count`). `_0` is the return place.
    pub arg_count: usize,
    pub locals: Vec<LocalDecl>,
    pub basic_blocks: Vec<BasicBlockData>,
    /// Synthetic `FnDef` ids of builtin intrinsic calls, mapped to their
    /// names (`print`, `exit`, `vec_push`, ...). A backend emits these
    /// itself; they are not source functions.
    pub intrinsics: BTreeMap<DefId, String>,
    /// Pointee types for the raw-pointer interner ids the body names
    /// (`pointee id -> type`), so a standalone backend can type a deref.
    pub pointees: BTreeMap<usize, TyKind>,
    /// Function-pointer signatures (`sig id -> (params, return)`), so a
    /// standalone backend can spell and call an indirect callee.
    pub fn_sigs: BTreeMap<usize, (Vec<TyKind>, TyKind)>,
    /// Declared `extern "C"` callees (`def id -> symbol`). A standalone
    /// backend cannot resolve these through its own function map.
    pub externs: BTreeMap<DefId, String>,
    /// Payload field types per variant for every enum the program declares
    /// (`enum id -> variant -> fields`). Carried so a standalone backend can
    /// lay out a union member even when the body never constructs that
    /// variant (a match arm still binds its payload).
    pub enum_layouts: BTreeMap<usize, Vec<Vec<TyKind>>>,
    /// User `Drop` implementations (`struct id -> (function, return type, live
    /// field name)`). A release helper calls the user function before
    /// releasing fields, and a struct with a drop function keeps a live flag
    /// so a conservative release of a moved-from or already-released slot
    /// calls it once. The frontend picks the flag's name because it knows the
    /// declared field names.
    pub drop_fns: BTreeMap<usize, (DefId, TyKind, String)>,
    /// Nominal types that copy instead of moving (`#[derive(Copy)]`): a
    /// binding of such a type stays usable after a by-value use.
    pub copy_structs: BTreeSet<usize>,
    pub copy_enums: BTreeSet<usize>,
    /// Vector element interner id -> element type, so a standalone backend
    /// can size the storage, type an index, and pick a drop thunk.
    pub vector_elems: BTreeMap<usize, TyKind>,
    /// Slice element interner id -> element type, for the same reasons.
    pub slice_elems: BTreeMap<usize, TyKind>,
    /// Map interner id -> (key type, value type), so a standalone backend can
    /// lay out one map family and type its reads.
    pub map_types: BTreeMap<usize, (TyKind, TyKind)>,
    /// Struct id -> declared field types, so a backend that constructs a
    /// runtime-built payload (FileInfo, CommandOutput) knows its layout
    /// without inferring it from field accesses.
    pub struct_layouts: BTreeMap<usize, Vec<TyKind>>,
    pub span: Option<Span>,
}

impl Body {
    pub fn new(name: impl Into<String>, arg_count: usize) -> Self {
        Self {
            name: name.into(),
            def: None,
            arg_count,
            locals: Vec::new(),
            basic_blocks: Vec::new(),
            intrinsics: BTreeMap::new(),
            pointees: BTreeMap::new(),
            fn_sigs: BTreeMap::new(),
            externs: BTreeMap::new(),
            enum_layouts: BTreeMap::new(),
            drop_fns: BTreeMap::new(),
            copy_structs: BTreeSet::new(),
            copy_enums: BTreeSet::new(),
            vector_elems: BTreeMap::new(),
            slice_elems: BTreeMap::new(),
            map_types: BTreeMap::new(),
            struct_layouts: BTreeMap::new(),
            span: None,
        }
    }

    pub fn local_decl(&self, id: LocalId) -> Option<&LocalDecl> {
        self.locals.get(id.index())
    }

    pub fn block(&self, id: BasicBlockId) -> Option<&BasicBlockData> {
        self.basic_blocks.get(id.index())
    }
}

/// Mutability of a local slot or borrow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mutability {
    Not,
    Mut,
}

/// Role of a local variable within a [`Body`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LocalKind {
    ReturnPlace,
    Arg,
    Var,
    Temp,
}

/// Declaration of a local slot (`_0`, `_1`, ...).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalDecl {
    pub ty: TyKind,
    pub mutability: Mutability,
    pub kind: LocalKind,
    pub name: Option<String>,
    pub span: Option<Span>,
}

impl LocalDecl {
    pub fn new(ty: TyKind, mutability: Mutability, kind: LocalKind) -> Self {
        Self {
            ty,
            mutability,
            kind,
            name: None,
            span: None,
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn with_span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }
}

/// One basic block in the CFG: sequential statements followed by a terminator.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct BasicBlockData {
    pub statements: Vec<Statement>,
    pub terminator: Option<Terminator>,
}

impl BasicBlockData {
    pub fn new(statements: Vec<Statement>, terminator: Option<Terminator>) -> Self {
        Self {
            statements,
            terminator,
        }
    }
}

/// Memory location: a base local plus zero or more projections.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Place {
    pub local: LocalId,
    pub projection: Vec<ProjectionElem>,
}

impl Place {
    pub fn from_local(local: LocalId) -> Self {
        Self {
            local,
            projection: Vec::new(),
        }
    }

    pub fn with_projection(mut self, projection: Vec<ProjectionElem>) -> Self {
        self.projection = projection;
        self
    }

    pub fn project(mut self, elem: ProjectionElem) -> Self {
        self.projection.push(elem);
        self
    }
}

impl From<LocalId> for Place {
    fn from(local: LocalId) -> Self {
        Self::from_local(local)
    }
}

/// One step in a [`Place`] projection path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProjectionElem {
    Deref,
    Field(usize),
    Index(LocalId),
    Downcast(usize),
}

/// A non-branching MIR statement with an optional source span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statement {
    pub span: Option<Span>,
    pub kind: StatementKind,
}

impl Statement {
    pub fn new(kind: StatementKind) -> Self {
        Self { span: None, kind }
    }

    pub fn with_span(kind: StatementKind, span: Span) -> Self {
        Self {
            span: Some(span),
            kind,
        }
    }
}

/// Operations executed sequentially within a basic block.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum StatementKind {
    Assign(Place, Rvalue),
    StorageLive(LocalId),
    StorageDead(LocalId),
    SetDiscriminant { place: Place, variant_index: usize },
    Drop(Place),
    Nop,
}

/// Borrow kind for [`Rvalue::Ref`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BorrowKind {
    Shared,
    Mut,
}

/// Binary operators in MIR (short-circuit `&&`/`||` lower to control flow).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitXor,
    BitAnd,
    BitOr,
    Shl,
    Shr,
    Eq,
    Lt,
    Le,
    Ne,
    Ge,
    Gt,
}

impl BinOp {
    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            Self::Eq | Self::Lt | Self::Le | Self::Ne | Self::Ge | Self::Gt
        )
    }
}

/// Unary operators in MIR.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnOp {
    Not,
    Neg,
}

/// Explicit cast classification for [`Rvalue::Cast`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CastKind {
    IntToInt,
    IntToFloat,
    FloatToInt,
    FloatToFloat,
    PtrToPtr,
    Misc,
}

/// Aggregate kind for [`Rvalue::Aggregate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateKind {
    Tuple,
    /// A fixed array literal: its element type and the compiler's
    /// interned element id (the id names the `ArrayOf` C typedef).
    Array(TyKind, usize),
    Struct(usize),
    Enum(usize, usize),
    Closure(usize),
}

/// Right-hand side of an assignment statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rvalue {
    Use(Operand),
    Ref {
        borrow_kind: BorrowKind,
        place: Place,
    },
    /// A raw address of a place (`&v as *const T`): the same C expression as
    /// `Ref`, but it creates no borrow loan — `unsafe` sees the address and
    /// the place stays writable (ADR-0237).
    AddressOf(Place),
    BinaryOp(BinOp, Operand, Operand),
    UnaryOp(UnOp, Operand),
    Cast(CastKind, Operand, TyKind),
    Aggregate(AggregateKind, Vec<Operand>),
    Len(Place),
    Discriminant(Place),
}

/// Input value to an rvalue or terminator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operand {
    Copy(Place),
    Move(Place),
    Constant(Constant),
}

/// IEEE-754 float bits stored as `u64` so constants support `Eq` and `Hash`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConstFloat(pub u64);

impl ConstFloat {
    pub fn from_f64(value: f64) -> Self {
        Self(value.to_bits())
    }

    pub fn to_f64(self) -> f64 {
        f64::from_bits(self.0)
    }
}

/// Literal or static item value carried by [`Constant`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ConstValue {
    Unit,
    Bool(bool),
    Char(char),
    Int(i128),
    Float(ConstFloat),
    String(String),
    FnDef(DefId),
}

/// Compile-time constant operand with type and optional source span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Constant {
    pub span: Option<Span>,
    pub ty: TyKind,
    pub value: ConstValue,
}

impl Constant {
    pub fn new(ty: TyKind, value: ConstValue) -> Self {
        Self {
            span: None,
            ty,
            value,
        }
    }

    pub fn with_span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }
}

/// Branch targets for [`TerminatorKind::SwitchInt`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchTargets {
    pub cases: Vec<(u128, BasicBlockId)>,
    pub otherwise: BasicBlockId,
}

impl SwitchTargets {
    pub fn new(cases: Vec<(u128, BasicBlockId)>, otherwise: BasicBlockId) -> Self {
        Self { cases, otherwise }
    }

    /// Convenience constructor for a boolean branch (`0` -> `false_bb`, otherwise -> `true_bb`).
    pub fn if_bool(true_bb: BasicBlockId, false_bb: BasicBlockId) -> Self {
        Self {
            cases: vec![(0, false_bb)],
            otherwise: true_bb,
        }
    }

    pub fn all_targets(&self) -> impl Iterator<Item = BasicBlockId> + '_ {
        self.cases
            .iter()
            .map(|&(_, target)| target)
            .chain(std::iter::once(self.otherwise))
    }
}

/// Diagnostic payload for [`TerminatorKind::Assert`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssertMessage {
    BoundsCheck { len: Operand, index: Operand },
    DivisionByZero(Operand),
    RemainderByZero(Operand),
    Overflow(BinOp, Operand, Operand),
    Custom(String),
}

/// Control-flow terminator at the end of a basic block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Terminator {
    pub span: Option<Span>,
    pub kind: TerminatorKind,
}

impl Terminator {
    pub fn new(kind: TerminatorKind) -> Self {
        Self { span: None, kind }
    }

    pub fn with_span(kind: TerminatorKind, span: Span) -> Self {
        Self {
            span: Some(span),
            kind,
        }
    }
}

/// Control-flow edge kinds leaving a basic block.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum TerminatorKind {
    Goto {
        target: BasicBlockId,
    },
    SwitchInt {
        discr: Operand,
        targets: SwitchTargets,
    },
    Call {
        func: Operand,
        args: Vec<Operand>,
        destination: Place,
        target: Option<BasicBlockId>,
    },
    Drop {
        place: Place,
        target: BasicBlockId,
        unwind: Option<BasicBlockId>,
    },
    Assert {
        cond: Operand,
        expected: bool,
        msg: AssertMessage,
        target: BasicBlockId,
    },
    Return,
    Unreachable,
}

impl TerminatorKind {
    /// Returns all outgoing basic-block successors of this terminator.
    pub fn successors(&self) -> Vec<BasicBlockId> {
        match self {
            Self::Goto { target } => vec![*target],
            Self::SwitchInt { targets, .. } => targets.all_targets().collect(),
            Self::Call { target, .. } => target.iter().copied().collect(),
            Self::Drop { target, unwind, .. } => {
                let mut out = vec![*target];
                if let Some(u) = unwind {
                    out.push(*u);
                }
                out
            }
            Self::Assert { target, .. } => vec![*target],
            Self::Return | Self::Unreachable => Vec::new(),
        }
    }
}
