//! Deterministic MIR pretty-printer (`format_body` and `Display` impls).

use std::fmt::{self, Write as _};

use ae_ty::TyKind;

use crate::ir::{
    AggregateKind, AssertMessage, BasicBlockId, BinOp, Body, BorrowKind, CastKind, ConstValue,
    Constant, LocalId, Mutability, Operand, Place, ProjectionElem, Rvalue, Statement,
    StatementKind, Terminator, TerminatorKind, UnOp,
};

/// Formats a MIR [`Body`] into a deterministic, human-readable string.
pub fn format_body(body: &Body) -> String {
    let mut out = String::new();
    let ret_ty = body
        .locals
        .first()
        .map_or_else(|| "()".to_owned(), |l| format_ty(l.ty));

    let _ = write!(out, "fn {}(", body.name);
    for i in 0..body.arg_count {
        if i > 0 {
            out.push_str(", ");
        }
        let local_idx = i + 1;
        if let Some(decl) = body.locals.get(local_idx) {
            let _ = write!(out, "_{local_idx}: {}", format_ty(decl.ty));
        } else {
            let _ = write!(out, "_{local_idx}: <missing>");
        }
    }
    let _ = writeln!(out, ") -> {ret_ty} {{");

    for (idx, decl) in body.locals.iter().enumerate() {
        let mut_kw = match decl.mutability {
            Mutability::Mut => "mut ",
            Mutability::Not => "",
        };
        let _ = write!(out, "    let {mut_kw}_{idx}: {};", format_ty(decl.ty));
        if let Some(name) = &decl.name {
            let _ = write!(out, " // {name}");
        }
        out.push('\n');
    }

    for (idx, bb) in body.basic_blocks.iter().enumerate() {
        if !body.locals.is_empty() || idx > 0 {
            out.push('\n');
        }
        let _ = writeln!(out, "    bb{idx}: {{");
        for stmt in &bb.statements {
            let _ = writeln!(out, "        {stmt};");
        }
        match &bb.terminator {
            Some(term) => {
                let _ = writeln!(out, "        {term};");
            }
            None => {
                out.push_str("        <missing terminator>;\n");
            }
        }
        out.push_str("    }\n");
    }

    out.push('}');
    out
}

pub fn format_ty(ty: TyKind) -> String {
    match ty {
        TyKind::Int(w) => w.name().to_owned(),
        TyKind::Float(w) => w.name().to_owned(),
        TyKind::Bool => "bool".to_owned(),
        TyKind::Char => "char".to_owned(),
        TyKind::Unit => "()".to_owned(),
        TyKind::String => "String".to_owned(),
        TyKind::StrView => "str".to_owned(),
        TyKind::Vector(id) => format!("Vec#{id}"),
        TyKind::VectorRef(id) => format!("&Vec#{id}"),
        TyKind::MapOf(id) => format!("Map#{id}"),
        TyKind::ArrayOf(id, len) => format!("[Array#{id}; {len}]"),
        TyKind::Slice(id) => format!("&[Slice#{id}]"),
        TyKind::SliceMut(id) => format!("&mut [Slice#{id}]"),
        TyKind::Struct(id) => format!("Struct#{id}"),
        TyKind::StructRef(id) => format!("&Struct#{id}"),
        TyKind::StructMut(id) => format!("&mut Struct#{id}"),
        TyKind::Enum(name, id) => format!("enum {name}#{id}"),
        TyKind::Tokens => "Tokens".to_owned(),
        TyKind::Closure(id) => format!("Closure#{id}"),
        TyKind::Fn(id) => format!("Fn#{id}"),
        TyKind::Ptr(id) => format!("*const #{id}"),
        TyKind::PtrMut(id) => format!("*mut #{id}"),
        TyKind::Thread(id) => format!("Thread#{id}"),
        TyKind::Atomic => "Atomic".to_owned(),
        TyKind::Shared(id) => format!("Shared#{id}"),
        TyKind::Mutex => "Mutex".to_owned(),
        TyKind::Guard => "Guard".to_owned(),
        TyKind::Chan => "Chan".to_owned(),
        TyKind::Exec => "Exec".to_owned(),
        TyKind::TaskScope => "TaskScope".to_owned(),
        TyKind::Pool => "Pool".to_owned(),
    }
}

impl fmt::Display for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_body(self))
    }
}

impl fmt::Display for BasicBlockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bb{}", self.0)
    }
}

impl fmt::Display for LocalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "_{}", self.0)
    }
}

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = format!("{}", self.local);
        for elem in &self.projection {
            match elem {
                ProjectionElem::Deref => out = format!("(*{out})"),
                ProjectionElem::Field(idx) => {
                    let _ = write!(out, ".{idx}");
                }
                ProjectionElem::Index(idx) => {
                    let _ = write!(out, "[{idx}]");
                }
                ProjectionElem::Downcast(variant) => {
                    let _ = write!(out, " as variant#{variant}");
                }
            }
        }
        f.write_str(&out)
    }
}

impl fmt::Display for Constant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.value {
            ConstValue::Unit => f.write_str("const ()"),
            ConstValue::Bool(b) => write!(f, "const {b}"),
            ConstValue::Char(c) => write!(f, "const {c:?}"),
            ConstValue::Int(n) => write!(f, "const {n}_{}", format_ty(self.ty)),
            ConstValue::Float(bits) => write!(f, "const {}_{}", bits.to_f64(), format_ty(self.ty)),
            ConstValue::String(s) => write!(f, "const {s:?}"),
            ConstValue::FnDef(def) => write!(f, "const fn#{}", def.0),
        }
    }
}

impl fmt::Display for Operand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Copy(p) => write!(f, "copy {p}"),
            Self::Move(p) => write!(f, "move {p}"),
            Self::Constant(c) => write!(f, "{c}"),
        }
    }
}

impl fmt::Display for BinOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Add => "Add",
            Self::Sub => "Sub",
            Self::Mul => "Mul",
            Self::Div => "Div",
            Self::Rem => "Rem",
            Self::BitXor => "BitXor",
            Self::BitAnd => "BitAnd",
            Self::BitOr => "BitOr",
            Self::Shl => "Shl",
            Self::Shr => "Shr",
            Self::Eq => "Eq",
            Self::Lt => "Lt",
            Self::Le => "Le",
            Self::Ne => "Ne",
            Self::Ge => "Ge",
            Self::Gt => "Gt",
        };
        f.write_str(name)
    }
}

impl fmt::Display for UnOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Not => f.write_str("Not"),
            Self::Neg => f.write_str("Neg"),
        }
    }
}

impl fmt::Display for CastKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::IntToInt => "IntToInt",
            Self::IntToFloat => "IntToFloat",
            Self::FloatToInt => "FloatToInt",
            Self::FloatToFloat => "FloatToFloat",
            Self::PtrToPtr => "PtrToPtr",
            Self::Misc => "Misc",
        };
        f.write_str(name)
    }
}

impl fmt::Display for Rvalue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Use(op) => write!(f, "{op}"),
            Self::Ref { borrow_kind, place } => match borrow_kind {
                BorrowKind::Shared => write!(f, "&{place}"),
                BorrowKind::Mut => write!(f, "&mut {place}"),
            },
            Self::BinaryOp(op, lhs, rhs) => write!(f, "{op}({lhs}, {rhs})"),
            Self::UnaryOp(op, operand) => write!(f, "{op}({operand})"),
            Self::Cast(kind, op, ty) => write!(f, "{op} as {} ({kind})", format_ty(*ty)),
            Self::Aggregate(kind, ops) => {
                match kind {
                    AggregateKind::Tuple => f.write_str("(")?,
                    AggregateKind::Array(ty) => write!(f, "[{}; ", format_ty(*ty))?,
                    AggregateKind::Struct(id) => write!(f, "Struct#{id} {{ ")?,
                    AggregateKind::Enum(id, variant) => {
                        write!(f, "Enum#{id}::variant#{variant}(")?
                    }
                    AggregateKind::Closure(id) => write!(f, "Closure#{id}(")?,
                }
                for (i, op) in ops.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{op}")?;
                }
                match kind {
                    AggregateKind::Tuple
                    | AggregateKind::Enum(..)
                    | AggregateKind::Closure(_) => f.write_str(")"),
                    AggregateKind::Array(_) => f.write_str("]"),
                    AggregateKind::Struct(_) => f.write_str(" }"),
                }
            }
            Self::Len(place) => write!(f, "Len({place})"),
            Self::Discriminant(place) => write!(f, "discriminant({place})"),
        }
    }
}

impl fmt::Display for Statement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            StatementKind::Assign(place, rvalue) => write!(f, "{place} = {rvalue}"),
            StatementKind::StorageLive(local) => write!(f, "StorageLive({local})"),
            StatementKind::StorageDead(local) => write!(f, "StorageDead({local})"),
            StatementKind::SetDiscriminant {
                place,
                variant_index,
            } => write!(f, "discriminant({place}) = {variant_index}"),
            StatementKind::Drop(place) => write!(f, "drop({place})"),
            StatementKind::Nop => f.write_str("nop"),
        }
    }
}

impl fmt::Display for Terminator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            TerminatorKind::Goto { target } => write!(f, "goto -> {target}"),
            TerminatorKind::SwitchInt { discr, targets } => {
                write!(f, "switchInt({discr}) -> [")?;
                for (val, target) in &targets.cases {
                    write!(f, "{val}: {target}, ")?;
                }
                write!(f, "otherwise: {}]", targets.otherwise)
            }
            TerminatorKind::Call {
                func,
                args,
                destination,
                target,
            } => {
                write!(f, "{destination} = {func}(")?;
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{arg}")?;
                }
                match target {
                    Some(bb) => write!(f, ") -> {bb}"),
                    None => f.write_str(") -> diverge"),
                }
            }
            TerminatorKind::Drop {
                place,
                target,
                unwind,
            } => match unwind {
                Some(u) => write!(f, "drop({place}) -> [return: {target}, unwind: {u}]"),
                None => write!(f, "drop({place}) -> {target}"),
            },
            TerminatorKind::Assert {
                cond,
                expected,
                msg,
                target,
            } => {
                write!(f, "assert({cond}, expected: {expected}, ")?;
                match msg {
                    AssertMessage::BoundsCheck { len, index } => {
                        write!(f, "bounds({index} < {len})")?;
                    }
                    AssertMessage::DivisionByZero(op) => write!(f, "div_by_zero({op})")?,
                    AssertMessage::RemainderByZero(op) => write!(f, "rem_by_zero({op})")?,
                    AssertMessage::Overflow(op, l, r) => write!(f, "overflow({op}, {l}, {r})")?,
                    AssertMessage::Custom(s) => write!(f, "{s:?}")?,
                }
                write!(f, ") -> {target}")
            }
            TerminatorKind::Return => f.write_str("return"),
            TerminatorKind::Unreachable => f.write_str("unreachable"),
        }
    }
}
