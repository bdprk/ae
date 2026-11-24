//! The C backend over the MIR block graph (M2.3).
//!
//! The structured statement tree is emitted as `if`/`while` blocks by
//! `codegen`; this module emits the same program as the MIR's basic blocks:
//! each block becomes a C label, statements stay in their original order, and
//! the terminator becomes an explicit `goto`, `if (...) goto ...; else ...`,
//! `return`, or `ae_exit`. Nothing about ownership changes: the
//! statements carry the same releases, moves, and runtime backstops the tree
//! emitter consumes, so the runtime behavior is identical by construction.
//!
//! Token iteration resets its cursor before entering the loop. The head
//! advances the cursor and binds the next view before selecting an edge.

use super::*;
use super::cfg::CfgTerminator;
use super::mir::{self, MirBlock, MirFunction};

// The MIR path is the default; `AE_CODEGEN=tree` forces the structured
// tree emitter for the whole program, which is how the differential tests
// compare the two backends and how a regression can be rolled back without a
// rebuild of the decision logic.
pub(super) fn enabled() -> bool {
    !matches!(std::env::var("AE_CODEGEN").as_deref(), Ok("tree"))
}

// Emit one function body as labelled blocks. The caller owns the signature and
// the trailing backstops; this writes everything between them.
pub(super) fn emit_function_body(function: &Function, out: &mut String, ctx: &CompileContext) -> Result<(), String> {
    let mut mir = mir::lower_function(function, ctx);
    // The same optimization pipeline the printer uses: constant folding and
    // CFG simplification decide which blocks are live. A pruned block is
    // unreachable, so not emitting it changes nothing.
    mir::optimize_function(&mut mir);
    // Drop elaboration (M2.4) removes the releases the graph proves
    // unnecessary and keeps every release it cannot prove redundant, where
    // the idempotent release itself decides at runtime.
    mir::elaborate_drops(&mut mir);
    mir::validate(&mir, ctx)?;
    let mut next = 0usize;
    let mut needs_tail = false;
    for block in &mir.blocks {
        out.push_str(&format!("  block_{}: ;\n", block.id));
        emit_block(&mir, block, out, &mut next, &mut needs_tail, ctx);
    }
    // `main`'s pending tail is not necessarily the last block created — a
    // loop's exit block is created while the loop lowers, but any arm or join
    // block can come after it in the vector. Fallthrough in C goes to the
    // next label in *emission* order, so the implicit exit must be an
    // explicit jump to a label placed just before the caller's flush-and-
    // return, not a reliance on block order.
    if needs_tail {
        out.push_str("  block_tail: ;\n");
    }
    Ok(())
}

fn emit_block(function: &MirFunction, block: &MirBlock, out: &mut String, next: &mut usize, needs_tail: &mut bool, ctx: &CompileContext) {
    let mut statements: &[Statement] = &block.statements;
    // A `return`/`exit` carries the scope's releases as a trailing `Frees`
    // statement (the CFG lowering puts them in the block, before the
    // terminator). They must run after the returned expression is evaluated —
    // the return may move a value out of one of those slots — so the terminator
    // emits them exactly where the tree backend does.
    let mut frees: &[(usize, Type)] = &[];
    if matches!(block.terminator, CfgTerminator::Return(_) | CfgTerminator::Exit(_))
        && let Some(Statement::Frees(ids)) = statements.last() {
            frees = ids;
            statements = &statements[..statements.len() - 1];
        }
    emit_statements(statements, out, next, ctx);
    match &block.terminator {
        CfgTerminator::Goto(target) => out.push_str(&format!("    goto block_{target};\n")),
        CfgTerminator::Branch(condition, yes, no) => {
            // The condition keeps its source form: emitting it once, here,
            // preserves short-circuit evaluation and any call it contains.
            let cond = emit_expr(condition, out, next, false, ctx);
            out.push_str(&format!("    if ({}) goto block_{yes}; else goto block_{no};\n", cond.text));
        }
        CfgTerminator::Return(expr) => {
            let result = codegen::emit_value_before_cleanup(expr, frees, out, next, true, ctx);
            if result.is_empty() {
                out.push_str("    return;\n");
            } else {
                out.push_str(&format!("    return {result};\n"));
            }
        }
        CfgTerminator::Exit(code) => {
            let result = codegen::emit_value_before_cleanup(code, frees, out, next, false, ctx);
            out.push_str(&format!("    ae_exit({result});\n"));
        }
        // Enum dispatch: one arm block per variant, in variant order, so each
        // case maps onto the layout's 0-based tag constants. A `match` is
        // exhaustive, so every variant has a target and no default is
        // needed; the arm bodies are other blocks, so the switch contains
        // only gotos and nothing inside it can `break` an enclosing loop.
        CfgTerminator::Switch(subject, eid, targets) => {
            out.push_str(&format!("    switch (v{subject}.tag) {{\n"));
            for (variant, target) in targets.iter().enumerate() {
                out.push_str(&format!("    case AE_E{eid}_{variant}: goto block_{target};\n"));
            }
            out.push_str("    }\n");
        }
        // `main`'s implicit exit jumps to the tail label; any other function
        // reaching its tail failed to return a value.
        CfgTerminator::Unreachable => {
            if function.is_main {
                out.push_str("    goto block_tail;\n");
                *needs_tail = true;
            } else {
                out.push_str("    ae_fail(\"function exited without return\", 0, 0);\n");
            }
        }
    }
}
