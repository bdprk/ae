//! Deterministic text form of the HIR view. `ae emit-hir` prints this and
//! `tests/hir` snapshots pin it: items, statements, expressions and patterns
//! in source order, each with its stable HirId and byte span, plus the
//! opaque bodies the syntax tree still records only as ranges.

use crate::{HirExpr, HirItem, HirModule, HirPat, HirStmt, HirStmtKind};

pub fn dump(module: &HirModule<'_>) -> String {
    let mut out = String::new();
    out.push_str(&format!("hir {}\n", module.file));
    if module.items.is_empty() {
        out.push_str("items: none\n");
    }
    for item in &module.items {
        match item {
            HirItem::Function(function) => {
                out.push_str(&format!("item #{} function {} {}..{}\n",
                    function.header.id.0, function.header.name,
                    function.header.span.start, function.header.span.end));
                if function.params.is_empty() {
                    out.push_str("  params: none\n");
                } else {
                    out.push_str("  params:\n");
                    for (name, span) in &function.params {
                        out.push_str(&format!("    {name}: {}..{}\n", span.start, span.end));
                    }
                }
                match &function.returns {
                    Some(span) => out.push_str(&format!("  return: {}..{}\n", span.start, span.end)),
                    None => out.push_str("  return: (none)\n"),
                }
                match &function.body {
                    Some(body) => {
                        out.push_str(&format!("  body: {}..{}\n", body.span.start, body.span.end));
                        statements(&body.statements, 2, &mut out);
                    }
                    None => out.push_str("  body: (none)\n"),
                }
                for closure in &function.closures {
                    out.push_str(&format!("  closure #{} {}..{} conflict={}\n",
                        closure.id.0, closure.span.start, closure.span.end,
                        if closure.has_binding_conflict { "yes" } else { "no" }));
                    statements(&closure.body.statements, 2, &mut out);
                }
            }
            HirItem::Struct(header) => out.push_str(&format!("item #{} struct {} {}..{}\n",
                header.id.0, header.name, header.span.start, header.span.end)),
            HirItem::Enum(header) => out.push_str(&format!("item #{} enum {} {}..{}\n",
                header.id.0, header.name, header.span.start, header.span.end)),
            HirItem::Trait(header) => out.push_str(&format!("item #{} trait {} {}..{}\n",
                header.id.0, header.name, header.span.start, header.span.end)),
            HirItem::Impl(header) => out.push_str(&format!("item #{} impl {} {}..{}\n",
                header.id.0, header.name, header.span.start, header.span.end)),
        }
    }
    if module.opaque_bodies.is_empty() {
        out.push_str("opaque bodies: none\n");
    } else {
        out.push_str("opaque bodies:\n");
        for body in &module.opaque_bodies {
            out.push_str(&format!("  {} {}..{}\n", body.owner, body.span.start, body.span.end));
        }
    }
    out
}

fn statements(statements: &[HirStmt<'_>], depth: usize, out: &mut String) {
    if statements.is_empty() {
        out.push_str(&format!("{}statements: none\n", "  ".repeat(depth)));
        return;
    }
    for statement in statements {
        dump_statement(statement, depth, out);
    }
}

fn dump_statement(statement: &HirStmt<'_>, depth: usize, out: &mut String) {
    out.push_str(&format!("{}stmt #{} {} {}..{}\n",
        "  ".repeat(depth), statement.id.0, kind(statement.kind),
        statement.span.start, statement.span.end));
    for expression in &statement.expressions {
        dump_expression(expression, depth + 1, out);
    }
    for pattern in &statement.patterns {
        dump_pattern(pattern, depth + 1, out);
    }
    statements(&statement.statements, depth + 1, out);
}

fn dump_expression(expression: &HirExpr<'_>, depth: usize, out: &mut String) {
    out.push_str(&format!("{}expr #{} {} {}..{}\n",
        "  ".repeat(depth), expression.id.0, expression.kind,
        expression.span.start, expression.span.end));
    for child in &expression.children {
        dump_expression(child, depth + 1, out);
    }
}

fn dump_pattern(pattern: &HirPat<'_>, depth: usize, out: &mut String) {
    out.push_str(&format!("{}pat #{} {} {}..{}\n",
        "  ".repeat(depth), pattern.id.0, pattern.kind,
        pattern.span.start, pattern.span.end));
    for child in &pattern.children {
        dump_pattern(child, depth + 1, out);
    }
}

fn kind(kind: HirStmtKind) -> &'static str {
    match kind {
        HirStmtKind::Binding => "binding",
        HirStmtKind::Assign => "assign",
        HirStmtKind::PointerStore => "pointer-store",
        HirStmtKind::Print => "print",
        HirStmtKind::Assert => "assert",
        HirStmtKind::Return => "return",
        HirStmtKind::If => "if",
        HirStmtKind::While => "while",
        HirStmtKind::ForRange => "for-range",
        HirStmtKind::ForIn => "for-in",
        HirStmtKind::Match => "match",
        HirStmtKind::Break => "break",
        HirStmtKind::Continue => "continue",
        HirStmtKind::Unsafe => "unsafe",
        HirStmtKind::Expr => "expr",
    }
}