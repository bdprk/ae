//! Stable definition identity shared by the resolver, HIR and compiler (P1.2).

/// One definition in the flat def table. Locals and items share the table so
/// an occurrence needs a single id to point at its owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DefId(pub u32);