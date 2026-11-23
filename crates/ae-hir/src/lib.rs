//! HIR layer over the shared syntax tree (P1.3).
//!
//! The first slice keeps the grammar's nodes by reference and gives every
//! item, statement, expression and pattern a stable `HirId` in source order.
//! That is the identity layer later passes consume; owning cloned nodes is a
//! following slice. Closure bodies come from the resolver's parsed
//! `ClosureBody` list, so they are real statement trees rather than opaque
//! ranges. Trait bodies are parsed by `ae-syntax` (slice 2), so
//! [`HirModule::opaque_bodies`] is empty for the corpus; the field stays as
//! the P1.3 exit gate's observable.

use ae_syntax::declaration::{Declaration, DeclarationKind};
use ae_syntax::expression::Expression;
use ae_syntax::pattern::Pattern;
use ae_syntax::statement::{ElseBranch, Statement, StatementKind};
use std::collections::HashMap;
use std::ops::Range;

pub use ae_resolve::DefId;
use ae_resolve::resolve::ExpressionType;

pub mod dump;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HirId(pub u32);

pub struct HirModule<'a> {
    pub file: &'a str,
    pub items: Vec<HirItem<'a>>,
    /// Bodies the syntax tree still records only as byte ranges, named by
    /// owner. The P1.3 exit gate requires this list to be empty.
    pub opaque_bodies: Vec<OpaqueBody>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpaqueBody {
    pub owner: String,
    pub span: Range<usize>,
}

pub enum HirItem<'a> {
    Function(HirFunction<'a>),
    Struct(HirItemHeader<'a>),
    Enum(HirItemHeader<'a>),
    Trait(HirTrait<'a>),
    Impl(HirItemHeader<'a>),
}

pub struct HirItemHeader<'a> {
    pub id: HirId,
    /// The resolver's definition for this item, when resolution ran.
    pub def: Option<DefId>,
    pub name: String,
    pub span: Range<usize>,
    pub declaration: &'a Declaration,
}

pub struct HirTrait<'a> {
    pub header: HirItemHeader<'a>,
    pub methods: Vec<HirTraitMethod>,
}

pub struct HirTraitMethod {
    pub id: HirId,
    pub name: String,
    pub span: Range<usize>,
    pub params: Vec<(String, Range<usize>)>,
    pub returns: Range<usize>,
}

pub struct HirFunction<'a> {
    pub header: HirItemHeader<'a>,
    pub def: Option<DefId>,
    /// Parameter names with their name ranges (the type spelling stays a
    /// byte range until the ae-ty interner owns it).
    pub params: Vec<(String, Range<usize>)>,
    pub returns: Option<Range<usize>>,
    pub body: Option<HirBlock<'a>>,
    /// Closure bodies reached inside this function, in source order.
    pub closures: Vec<HirClosure<'a>>,
}

pub struct HirClosure<'a> {
    pub id: HirId,
    pub span: Range<usize>,
    pub has_binding_conflict: bool,
    pub body: HirBlock<'a>,
}

pub struct HirBlock<'a> {
    pub span: Range<usize>,
    pub statements: Vec<HirStmt<'a>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HirStmtKind {
    Binding,
    Assign,
    PointerStore,
    Print,
    Assert,
    Return,
    If,
    While,
    ForRange,
    ForIn,
    Match,
    Break,
    Continue,
    Unsafe,
    Expr,
}

pub struct HirStmt<'a> {
    pub id: HirId,
    /// The local this statement declares, for binding and for statements.
    pub def: Option<DefId>,
    pub kind: HirStmtKind,
    pub span: Range<usize>,
    pub source: &'a Statement,
    pub expressions: Vec<HirExpr<'a>>,
    pub statements: Vec<HirStmt<'a>>,
    pub patterns: Vec<HirPat<'a>>,
}

pub struct HirExpr<'a> {
    pub id: HirId,
    /// The resolver's scalar result for this expression's byte range.
    pub ty: Option<ExpressionType>,
    pub kind: &'static str,
    pub span: Range<usize>,
    pub source: &'a Expression,
    pub children: Vec<HirExpr<'a>>,
}

pub struct HirPat<'a> {
    pub id: HirId,
    /// The local a binding pattern names.
    pub def: Option<DefId>,
    pub kind: &'static str,
    pub span: Range<usize>,
    pub source: &'a Pattern,
    pub children: Vec<HirPat<'a>>,
}

/// Build the HIR view for one parsed module. With a resolution, items and
/// locals carry their `DefId` and functions carry their closure bodies;
/// without one, the syntax-only view still has stable ids.
pub fn build<'a>(module: &'a ae_resolve::Module, resolution: Option<&'a ae_resolve::resolve::Resolution>) -> HirModule<'a> {
    let mut ids = Ids(0);
    let opaque_bodies = Vec::new();
    let mut items = Vec::new();
    let locals: HashMap<usize, DefId> = resolution.map(|resolution| resolution.defs.iter()
        .filter(|entry| entry.kind.is_local())
        .map(|entry| (entry.range.start, entry.id))
        .collect()).unwrap_or_default();
    let facts: HashMap<usize, ExpressionType> = resolution.map(|resolution| resolution.functions.iter()
        .flat_map(|function| function.expression_types.iter())
        .map(|(range, kind)| (range.start, *kind))
        .collect()).unwrap_or_default();
    for (index, declaration) in module.declarations.iter().enumerate() {
        match &declaration.kind {
            DeclarationKind::Function { .. } => {
                let unit = module.functions.iter().position(|unit| unit.declaration.range == declaration.range);
                if let Some(unit) = unit {
                    items.push(HirItem::Function(function_item(declaration, unit, module, resolution, &locals, &facts, &mut ids)));
                }
            }
            DeclarationKind::Struct { .. } => items.push(HirItem::Struct(header(declaration, resolution, &mut ids))),
            DeclarationKind::Enum { .. } => items.push(HirItem::Enum(header(declaration, resolution, &mut ids))),
            DeclarationKind::Trait { methods } => {
                let header = header(declaration, resolution, &mut ids);
                let methods = methods.iter().map(|method| HirTraitMethod {
                    id: ids.next(),
                    name: method.name.clone(),
                    span: method.range.clone(),
                    params: method.parameters.iter().map(|parameter| (parameter.name.clone(), parameter.name_range.clone())).collect(),
                    returns: method.returns.clone(),
                }).collect();
                items.push(HirItem::Trait(HirTrait { header, methods }));
            }
            DeclarationKind::Impl { .. } => {
                items.push(HirItem::Impl(header(declaration, resolution, &mut ids)));
                for (unit, function) in module.functions.iter().enumerate() {
                    if function.impl_owner == Some(index) {
                        let declaration = &module.functions[unit].declaration;
                        items.push(HirItem::Function(function_item(declaration, unit, module, resolution, &locals, &facts, &mut ids)));
                    }
                }
            }
        }
    }
    HirModule { file: &module.file, items, opaque_bodies }
}

struct Ids(u32);

impl Ids {
    fn next(&mut self) -> HirId {
        let id = HirId(self.0);
        self.0 += 1;
        id
    }
}

fn header<'a>(
    declaration: &'a Declaration,
    resolution: Option<&'a ae_resolve::resolve::Resolution>,
    ids: &mut Ids,
) -> HirItemHeader<'a> {
    let def = resolution.and_then(|resolution| resolution.defs.iter()
        .find(|entry| !entry.kind.is_local()
            && entry.name == declaration.name
            && entry.range.start >= declaration.range.start
            && entry.range.end <= declaration.range.end)
        .map(|entry| entry.id));
    HirItemHeader {
        id: ids.next(),
        def,
        name: declaration.name.clone(),
        span: declaration.range.clone(),
        declaration,
    }
}

fn function_item<'a>(
    declaration: &'a Declaration,
    unit: usize,
    module: &'a ae_resolve::Module,
    resolution: Option<&'a ae_resolve::resolve::Resolution>,
    locals: &HashMap<usize, DefId>,
    facts: &HashMap<usize, ExpressionType>,
    ids: &mut Ids,
) -> HirFunction<'a> {
    let DeclarationKind::Function { parameters, returns, .. } = &declaration.kind else {
        unreachable!("only function declarations reach function_item")
    };
    let header = header(declaration, resolution, ids);
    let occurrence = resolution.and_then(|res| res.functions.get(unit));
    let body = module.functions[unit].body.as_deref()
        .map(|statements| block(declaration.body.clone().unwrap_or(declaration.range.clone()), statements, locals, facts, ids));
    let mut closures = Vec::new();
    if let Some(occurrence) = occurrence {
        for closure in &occurrence.closure_bodies {
            let body = block(closure.start..closure.end, closure.statements.as_ref(), locals, facts, ids);
            closures.push(HirClosure {
                id: ids.next(),
                span: closure.start..closure.end,
                has_binding_conflict: closure.has_binding_conflict,
                body,
            });
        }
    }
    HirFunction {
        header,
        def: occurrence.map(|occurrence| occurrence.function_def),
        params: parameters.iter().map(|parameter| (parameter.name.clone(), parameter.name_range.clone())).collect(),
        returns: returns.clone(),
        body,
        closures,
    }
}

fn block<'a>(span: Range<usize>, statements: &'a [Statement], locals: &HashMap<usize, DefId>, facts: &HashMap<usize, ExpressionType>, ids: &mut Ids) -> HirBlock<'a> {
    HirBlock { span, statements: statements.iter().map(|statement| statement_node(statement, locals, facts, ids)).collect() }
}

fn statement_node<'a>(statement: &'a Statement, locals: &HashMap<usize, DefId>, facts: &HashMap<usize, ExpressionType>, ids: &mut Ids) -> HirStmt<'a> {
    let id = ids.next();
    let def = match &statement.kind {
        StatementKind::Binding { name_range, .. } => locals.get(&name_range.start).copied(),
        StatementKind::ForRange { binding_range, .. } | StatementKind::ForIn { binding_range, .. } => locals.get(&binding_range.start).copied(),
        _ => None,
    };
    let mut expressions = Vec::new();
    let mut statements = Vec::new();
    let mut patterns = Vec::new();
    let kind = match &statement.kind {
        StatementKind::Binding { initializer, .. } => {
            expressions.push(expression_node(initializer, facts, ids));
            HirStmtKind::Binding
        }
        StatementKind::Assign { target, value, .. } => {
            expressions.push(expression_node(target, facts, ids));
            expressions.push(expression_node(value, facts, ids));
            HirStmtKind::Assign
        }
        StatementKind::PointerStore { value, .. } => {
            expressions.push(expression_node(value, facts, ids));
            HirStmtKind::PointerStore
        }
        StatementKind::Print(value) => {
            expressions.push(expression_node(value, facts, ids));
            HirStmtKind::Print
        }
        StatementKind::Assert(value) => {
            expressions.push(expression_node(value, facts, ids));
            HirStmtKind::Assert
        }
        StatementKind::Return(value) => {
            expressions.push(expression_node(value, facts, ids));
            HirStmtKind::Return
        }
        StatementKind::If { condition, then_body, else_body } => {
            expressions.push(expression_node(condition, facts, ids));
            statements.extend(then_body.iter().map(|statement| statement_node(statement, locals, facts, ids)));
            match else_body {
                Some(ElseBranch::Block(body)) => {
                    statements.extend(body.iter().map(|statement| statement_node(statement, locals, facts, ids)));
                }
                Some(ElseBranch::If(statement)) => statements.push(statement_node(statement, locals, facts, ids)),
                None => {}
            }
            HirStmtKind::If
        }
        StatementKind::While { condition, body } => {
            expressions.push(expression_node(condition, facts, ids));
            statements.extend(body.iter().map(|statement| statement_node(statement, locals, facts, ids)));
            HirStmtKind::While
        }
        StatementKind::ForRange { start, end, body, .. } => {
            expressions.push(expression_node(start, facts, ids));
            expressions.push(expression_node(end, facts, ids));
            statements.extend(body.iter().map(|statement| statement_node(statement, locals, facts, ids)));
            HirStmtKind::ForRange
        }
        StatementKind::ForIn { iterable, body, .. } => {
            expressions.push(expression_node(iterable, facts, ids));
            statements.extend(body.iter().map(|statement| statement_node(statement, locals, facts, ids)));
            HirStmtKind::ForIn
        }
        StatementKind::Match { subject, arms } => {
            expressions.push(expression_node(subject, facts, ids));
            for arm in arms {
                patterns.push(pattern_node(&arm.pattern, locals, ids));
                statements.extend(arm.body.iter().map(|statement| statement_node(statement, locals, facts, ids)));
            }
            HirStmtKind::Match
        }
        StatementKind::Unsafe(body) => {
            statements.extend(body.iter().map(|statement| statement_node(statement, locals, facts, ids)));
            HirStmtKind::Unsafe
        }
        StatementKind::Expr { value, .. } => {
            expressions.push(expression_node(value, facts, ids));
            HirStmtKind::Expr
        }
        StatementKind::Break => HirStmtKind::Break,
        StatementKind::Continue => HirStmtKind::Continue,
    };
    HirStmt { id, def, kind, span: statement.range.clone(), source: statement, expressions, statements, patterns }
}

fn expression_node<'a>(expression: &'a Expression, facts: &HashMap<usize, ExpressionType>, ids: &mut Ids) -> HirExpr<'a> {
    use ae_syntax::expression::ExpressionKind;
    let id = ids.next();
    let ty = facts.get(&expression.range.start).copied();
    let mut children = Vec::new();
    let kind = match &expression.kind {
        ExpressionKind::Atom(_) => "atom",
        ExpressionKind::Group(base) => {
            children.push(expression_node(base, facts, ids));
            "group"
        }
        ExpressionKind::Propagate(base) => {
            children.push(expression_node(base, facts, ids));
            "propagate"
        }
        ExpressionKind::AddressCast { .. } => "address-cast",
        ExpressionKind::PointerCast { base, .. } => {
            children.push(expression_node(base, facts, ids));
            "pointer-cast"
        }
        ExpressionKind::BorrowArgument { .. } => "borrow-argument",
        ExpressionKind::Array(elements) | ExpressionKind::Vector(elements) => {
            children.extend(elements.iter().map(|element| expression_node(element, facts, ids)));
            if matches!(expression.kind, ExpressionKind::Array(_)) { "array" } else { "vector" }
        }
        ExpressionKind::Struct { target, fields } => {
            children.push(expression_node(target, facts, ids));
            children.extend(fields.iter().map(|field| expression_node(&field.value, facts, ids)));
            "struct"
        }
        ExpressionKind::Call { callee, arguments } => {
            children.push(expression_node(callee, facts, ids));
            children.extend(arguments.iter().map(|argument| expression_node(argument, facts, ids)));
            "call"
        }
        ExpressionKind::Index { base, index } => {
            children.push(expression_node(base, facts, ids));
            children.push(expression_node(index, facts, ids));
            "index"
        }
        ExpressionKind::Field { base, .. } => {
            children.push(expression_node(base, facts, ids));
            "field"
        }
        ExpressionKind::Specialization { base, .. } => {
            children.push(expression_node(base, facts, ids));
            "specialization"
        }
        ExpressionKind::BracketedCallTarget { base, .. } => {
            children.push(expression_node(base, facts, ids));
            "bracketed-call-target"
        }
        ExpressionKind::Qualified { base, .. } => {
            children.push(expression_node(base, facts, ids));
            "qualified"
        }
        ExpressionKind::Closure { .. } => "closure",
        ExpressionKind::Prefix { operator: _, operand } => {
            children.push(expression_node(operand, facts, ids));
            "prefix"
        }
        ExpressionKind::Binary { left, right, .. } => {
            children.push(expression_node(left, facts, ids));
            children.push(expression_node(right, facts, ids));
            "binary"
        }
    };
    HirExpr { id, ty, kind, span: expression.range.clone(), source: expression, children }
}

fn pattern_node<'a>(pattern: &'a Pattern, locals: &HashMap<usize, DefId>, ids: &mut Ids) -> HirPat<'a> {
    use ae_syntax::pattern::PatternKind;
    let id = ids.next();
    let mut children = Vec::new();
    let kind = match &pattern.kind {
        PatternKind::Binding(_) => "binding",
        PatternKind::Variant { payload, .. } => {
            if let Some(payload) = payload {
                children.extend(payload.iter().map(|pattern| pattern_node(pattern, locals, ids)));
            }
            "variant"
        }
    };
    let def = locals.get(&pattern.range.start).copied();
    HirPat { id, def, kind, span: pattern.range.clone(), source: pattern, children }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_trait_bodies_are_not_opaque() {
        let source = "trait Area { fn area(shape: T) -> i64; }
struct Square { side: i64 }
impl Area for Square { fn area(shape: Square) -> i64 { return shape.side * shape.side; } }
fn main() { let square = Square { side: 3 }; for i in 0..2 { print(i); } }
";
        let module = ae_resolve::parse_module("case.ae", source).unwrap();
        let hir = build(&module, None);
        assert!(hir.opaque_bodies.is_empty(), "trait bodies must be structured");
        let mut ids = Vec::new();
        for item in &hir.items {
            match item {
                HirItem::Function(function) => {
                    ids.push(function.header.id);
                    if let Some(body) = &function.body { collect_statements(&body.statements, &mut ids); }
                    for closure in &function.closures { collect_statements(&closure.body.statements, &mut ids); }
                }
                HirItem::Trait(trait_item) => {
                    ids.push(trait_item.header.id);
                    ids.extend(trait_item.methods.iter().map(|method| method.id));
                }
                HirItem::Struct(header) | HirItem::Enum(header) | HirItem::Impl(header) => ids.push(header.id),
            }
        }
        let unique: std::collections::HashSet<_> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "HirIds repeat");
        let max = ids.iter().map(|id| id.0).max().unwrap();
        assert_eq!(max as usize + 1, ids.len(), "HirIds are not dense");
    }

    fn collect_statements(statements: &[HirStmt<'_>], ids: &mut Vec<HirId>) {
        for statement in statements {
            ids.push(statement.id);
            for expression in &statement.expressions {
                collect_expression(expression, ids);
            }
            for pattern in &statement.patterns {
                collect_pattern(pattern, ids);
            }
            collect_statements(&statement.statements, ids);
        }
    }

    fn collect_expression(expression: &HirExpr<'_>, ids: &mut Vec<HirId>) {
        ids.push(expression.id);
        for child in &expression.children {
            collect_expression(child, ids);
        }
    }

    fn collect_pattern(pattern: &HirPat<'_>, ids: &mut Vec<HirId>) {
        ids.push(pattern.id);
        for child in &pattern.children {
            collect_pattern(child, ids);
        }
    }
}
