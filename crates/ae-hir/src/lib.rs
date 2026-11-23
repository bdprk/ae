//! HIR layer over the shared syntax tree (P1.3).
//!
//! The first slice keeps the grammar's nodes by reference and gives every
//! item, statement, expression and pattern a stable `HirId` in source order.
//! That is the identity layer later passes consume; owning cloned nodes is a
//! following slice. Closure bodies come from the resolver's parsed
//! `ClosureBody` list, so they are real statement trees rather than opaque
//! ranges. Trait bodies are still opaque and are reported explicitly in
//! [`HirModule::opaque_bodies`], which the P1.3 exit gate drains to zero.

pub mod dump;

use ae_syntax::declaration::{Declaration, DeclarationKind};
use ae_syntax::expression::Expression;
use ae_syntax::pattern::Pattern;
use ae_syntax::statement::{ElseBranch, Statement, StatementKind};
use std::ops::Range;

pub use ae_resolve::DefId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
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
    Trait(HirItemHeader<'a>),
    Impl(HirItemHeader<'a>),
}

pub struct HirItemHeader<'a> {
    pub id: HirId,
    pub name: String,
    pub span: Range<usize>,
    pub declaration: &'a Declaration,
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
    pub kind: HirStmtKind,
    pub span: Range<usize>,
    pub source: &'a Statement,
    pub expressions: Vec<HirExpr<'a>>,
    pub statements: Vec<HirStmt<'a>>,
    pub patterns: Vec<HirPat<'a>>,
}

pub struct HirExpr<'a> {
    pub id: HirId,
    pub kind: &'static str,
    pub span: Range<usize>,
    pub source: &'a Expression,
    pub children: Vec<HirExpr<'a>>,
}

pub struct HirPat<'a> {
    pub id: HirId,
    pub kind: &'static str,
    pub span: Range<usize>,
    pub source: &'a Pattern,
    pub children: Vec<HirPat<'a>>,
}

/// Build the HIR view for one parsed module. With a resolution, functions
/// carry their `DefId` and closure bodies; without one, the syntax-only view
/// still has stable ids.
pub fn build<'a>(module: &'a ae_resolve::Module, resolution: Option<&'a ae_resolve::resolve::Resolution>) -> HirModule<'a> {
    let mut ids = Ids(0);
    let mut opaque_bodies = Vec::new();
    let mut items = Vec::new();
    for (index, declaration) in module.declarations.iter().enumerate() {
        match &declaration.kind {
            DeclarationKind::Function { .. } => {
                let unit = module.functions.iter().position(|unit| unit.declaration.range == declaration.range);
                if let Some(unit) = unit {
                    items.push(HirItem::Function(function_item(declaration, unit, module, resolution, &mut ids)));
                }
            }
            DeclarationKind::Struct { .. } => items.push(HirItem::Struct(header(declaration, &mut ids))),
            DeclarationKind::Enum { .. } => items.push(HirItem::Enum(header(declaration, &mut ids))),
            DeclarationKind::Trait => {
                if let Some(span) = declaration.body.clone() {
                    opaque_bodies.push(OpaqueBody { owner: declaration.name.clone(), span });
                }
                items.push(HirItem::Trait(header(declaration, &mut ids)));
            }
            DeclarationKind::Impl { .. } => {
                items.push(HirItem::Impl(header(declaration, &mut ids)));
                for (unit, function) in module.functions.iter().enumerate() {
                    if function.impl_owner == Some(index) {
                        let declaration = &module.functions[unit].declaration;
                        items.push(HirItem::Function(function_item(declaration, unit, module, resolution, &mut ids)));
                    }
                }
            }
        }
    }
    HirModule { file: &module.file, items, opaque_bodies }
}


fn function_item<'a>(
    declaration: &'a Declaration,
    unit: usize,
    module: &'a ae_resolve::Module,
    resolution: Option<&'a ae_resolve::resolve::Resolution>,
    ids: &mut Ids,
) -> HirFunction<'a> {
    let DeclarationKind::Function { parameters, returns, .. } = &declaration.kind else {
        unreachable!("only function declarations reach function_item")
    };
    let header = header(declaration, ids);
    let occurrence = resolution.and_then(|res| res.functions.get(unit));
    let body = module.functions[unit].body.as_deref()
        .map(|statements| block(declaration.body.clone().unwrap_or(declaration.range.clone()), statements, ids));
    let mut closures = Vec::new();
    if let Some(occurrence) = occurrence {
        for closure in &occurrence.closure_bodies {
            let body = block(closure.start..closure.end, closure.statements.as_ref(), ids);
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

struct Ids(u32);

impl Ids {
    fn next(&mut self) -> HirId {
        let id = HirId(self.0);
        self.0 += 1;
        id
    }
}

fn header<'a>(declaration: &'a Declaration, ids: &mut Ids) -> HirItemHeader<'a> {
    HirItemHeader {
        id: ids.next(),
        name: declaration.name.clone(),
        span: declaration.range.clone(),
        declaration,
    }
}

fn block<'a>(span: Range<usize>, statements: &'a [Statement], ids: &mut Ids) -> HirBlock<'a> {
    HirBlock { span, statements: statements.iter().map(|statement| statement_node(statement, ids)).collect() }
}

fn statement_node<'a>(statement: &'a Statement, ids: &mut Ids) -> HirStmt<'a> {
    let id = ids.next();
    let mut expressions = Vec::new();
    let mut statements = Vec::new();
    let mut patterns = Vec::new();
    let kind = match &statement.kind {
        StatementKind::Binding { initializer, .. } => {
            expressions.push(expression_node(initializer, ids));
            HirStmtKind::Binding
        }
        StatementKind::Assign { target, value, .. } => {
            expressions.push(expression_node(target, ids));
            expressions.push(expression_node(value, ids));
            HirStmtKind::Assign
        }
        StatementKind::PointerStore { value, .. } => {
            expressions.push(expression_node(value, ids));
            HirStmtKind::PointerStore
        }
        StatementKind::Print(value) => {
            expressions.push(expression_node(value, ids));
            HirStmtKind::Print
        }
        StatementKind::Assert(value) => {
            expressions.push(expression_node(value, ids));
            HirStmtKind::Assert
        }
        StatementKind::Return(value) => {
            expressions.push(expression_node(value, ids));
            HirStmtKind::Return
        }
        StatementKind::If { condition, then_body, else_body } => {
            expressions.push(expression_node(condition, ids));
            statements.extend(then_body.iter().map(|statement| statement_node(statement, ids)));
            match else_body {
                Some(ElseBranch::Block(body)) => {
                    statements.extend(body.iter().map(|statement| statement_node(statement, ids)));
                }
                Some(ElseBranch::If(statement)) => statements.push(statement_node(statement, ids)),
                None => {}
            }
            HirStmtKind::If
        }
        StatementKind::While { condition, body } => {
            expressions.push(expression_node(condition, ids));
            statements.extend(body.iter().map(|statement| statement_node(statement, ids)));
            HirStmtKind::While
        }
        StatementKind::ForRange { start, end, body, .. } => {
            expressions.push(expression_node(start, ids));
            expressions.push(expression_node(end, ids));
            statements.extend(body.iter().map(|statement| statement_node(statement, ids)));
            HirStmtKind::ForRange
        }
        StatementKind::ForIn { iterable, body, .. } => {
            expressions.push(expression_node(iterable, ids));
            statements.extend(body.iter().map(|statement| statement_node(statement, ids)));
            HirStmtKind::ForIn
        }
        StatementKind::Match { subject, arms } => {
            expressions.push(expression_node(subject, ids));
            for arm in arms {
                patterns.push(pattern_node(&arm.pattern, ids));
                statements.extend(arm.body.iter().map(|statement| statement_node(statement, ids)));
            }
            HirStmtKind::Match
        }
        StatementKind::Unsafe(body) => {
            statements.extend(body.iter().map(|statement| statement_node(statement, ids)));
            HirStmtKind::Unsafe
        }
        StatementKind::Expr { value, .. } => {
            expressions.push(expression_node(value, ids));
            HirStmtKind::Expr
        }
        StatementKind::Break => HirStmtKind::Break,
        StatementKind::Continue => HirStmtKind::Continue,
    };
    HirStmt { id, kind, span: statement.range.clone(), source: statement, expressions, statements, patterns }
}

fn expression_node<'a>(expression: &'a Expression, ids: &mut Ids) -> HirExpr<'a> {
    use ae_syntax::expression::ExpressionKind;
    let id = ids.next();
    let mut children = Vec::new();
    let kind = match &expression.kind {
        ExpressionKind::Atom(_) => "atom",
        ExpressionKind::Group(base) => {
            children.push(expression_node(base, ids));
            "group"
        }
        ExpressionKind::Propagate(base) => {
            children.push(expression_node(base, ids));
            "propagate"
        }
        ExpressionKind::AddressCast { .. } => "address-cast",
        ExpressionKind::PointerCast { base, .. } => {
            children.push(expression_node(base, ids));
            "pointer-cast"
        }
        ExpressionKind::BorrowArgument { .. } => "borrow-argument",
        ExpressionKind::Array(elements) | ExpressionKind::Vector(elements) => {
            children.extend(elements.iter().map(|element| expression_node(element, ids)));
            if matches!(expression.kind, ExpressionKind::Array(_)) { "array" } else { "vector" }
        }
        ExpressionKind::Struct { target, fields } => {
            children.push(expression_node(target, ids));
            children.extend(fields.iter().map(|field| expression_node(&field.value, ids)));
            "struct"
        }
        ExpressionKind::Call { callee, arguments } => {
            children.push(expression_node(callee, ids));
            children.extend(arguments.iter().map(|argument| expression_node(argument, ids)));
            "call"
        }
        ExpressionKind::Index { base, index } => {
            children.push(expression_node(base, ids));
            children.push(expression_node(index, ids));
            "index"
        }
        ExpressionKind::Field { base, .. } => {
            children.push(expression_node(base, ids));
            "field"
        }
        ExpressionKind::Specialization { base, .. } => {
            children.push(expression_node(base, ids));
            "specialization"
        }
        ExpressionKind::BracketedCallTarget { base, .. } => {
            children.push(expression_node(base, ids));
            "bracketed-call-target"
        }
        ExpressionKind::Qualified { base, .. } => {
            children.push(expression_node(base, ids));
            "qualified"
        }
        ExpressionKind::Closure { .. } => "closure",
        ExpressionKind::Prefix { operand, .. } => {
            children.push(expression_node(operand, ids));
            "prefix"
        }
        ExpressionKind::Binary { left, right, .. } => {
            children.push(expression_node(left, ids));
            children.push(expression_node(right, ids));
            "binary"
        }
    };
    HirExpr { id, kind, span: expression.range.clone(), source: expression, children }
}

fn pattern_node<'a>(pattern: &'a Pattern, ids: &mut Ids) -> HirPat<'a> {
    use ae_syntax::pattern::PatternKind;
    let id = ids.next();
    let mut children = Vec::new();
    let kind = match &pattern.kind {
        PatternKind::Binding(_) => "binding",
        PatternKind::Variant { payload, .. } => {
            if let Some(payload) = payload {
                children.extend(payload.iter().map(|pattern| pattern_node(pattern, ids)));
            }
            "variant"
        }
    };
    HirPat { id, kind, span: pattern.range.clone(), source: pattern, children }
}