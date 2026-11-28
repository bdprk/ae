use super::*;

fn audit_kind(tokens: &[Token], i: usize) -> &Kind {
    tokens.get(i).map(|t| &t.kind).unwrap_or(&AUDIT_EOF)
}

// `start` points at the parameter list, `end` just past the closing brace.
// Only declarations with at least one constraint are audited: an unconstrained
// generic keeps ADR-33 behaviour, where the per-instantiation check reports the
// error. Unproven cases are skipped rather than rejected, so the check cannot
// break a valid program; correctness still rests on the full type check.
#[allow(clippy::too_many_arguments)]
pub(super) fn audit_generic_fn(
    file: &str, source: &str, tokens: &[Token], start: usize, end: usize,
    name: &str, params: &[String], bounds: &[Vec<Bound>], declared_at: usize,
    ctx: &CompileContext,
) -> Result<(), ae_span::Diagnostic> {
    if !bounds.iter().any(|b| !b.is_empty()) { return Ok(()); }
    let at = |i: usize| audit_kind(tokens, i);
    if *at(start) != Kind::LParen { return Ok(()); }
    let mut depth = 0usize;
    let mut rparen = None;
    for i in start..end {
        match at(i) {
            Kind::LParen => depth += 1,
            Kind::RParen => { depth -= 1; if depth == 0 { rparen = Some(i); break; } }
            _ => {}
        }
    }
    let Some(rparen) = rparen else { return Ok(()); };

    // Bind each parameter variable to a type-parameter index. Concrete types,
    // `&T` and compound types are deliberately left unknown, so an operator can
    // never be attributed to the wrong type parameter.
    let mut vars: HashMap<String, Option<usize>> = HashMap::new();
    let mut piece = start + 1;
    let mut pieces = Vec::new();
    for i in (start + 1)..rparen {
        if *at(i) == Kind::Comma { pieces.push((piece, i)); piece = i + 1; }
    }
    pieces.push((piece, rparen));
    for (a, b) in pieces {
        let Some(colon) = (a..b).find(|&i| *at(i) == Kind::Colon) else { continue; };
        if colon == a { continue; }
        let Kind::Ident(var) = at(colon - 1) else { continue; };
        let bound_to = if b == colon + 2 {
            match at(colon + 1) {
                Kind::Ident(t) => params.iter().position(|p| p == t),
                _ => None,
            }
        } else { None };
        vars.insert(var.clone(), bound_to);
    }

    let Some(body_start) = ((rparen + 1)..end).find(|&i| *at(i) == Kind::LBrace) else { return Ok(()); };
    let mut depth = 0usize;
    let mut body_end = None;
    for i in body_start..end {
        match at(i) {
            Kind::LBrace => depth += 1,
            Kind::RBrace => { depth -= 1; if depth == 0 { body_end = Some(i); break; } }
            _ => {}
        }
    }
    let Some(body_end) = body_end else { return Ok(()); };

    let mut demands: Vec<(usize, Capability)> = Vec::new();
    let mut i = body_start + 1;
    while i < body_end {
        match at(i) {
            Kind::Let | Kind::Var => {
                if let Kind::Ident(var) = at(i + 1) {
                    let mut taint = None;
                    let mut unknown = false;
                    if let Some(eq) = ((i + 2)..body_end).find(|&j| *at(j) == Kind::Equal) {
                        for j in (eq + 1)..body_end {
                            match at(j) {
                                Kind::Semi | Kind::RBrace => break,
                                Kind::LBrace => { unknown = true; break; }
                                Kind::Ident(n) => if let Some(Some(t)) = vars.get(n) {
                                    if taint.is_some() && taint != Some(*t) { unknown = true; }
                                    taint = Some(*t);
                                },
                                _ => {}
                            }
                        }
                    } else { unknown = true; }
                    vars.insert(var.clone(), if unknown { None } else { taint });
                }
            }
            Kind::For => {
                if let Kind::Ident(var) = at(i + 1) { vars.insert(var.clone(), None); }
            }
            _ => {}
        }
        // Only an identifier immediately adjacent to the operator is treated as
        // an operand of a type parameter. Anything else (call results, indices,
        // fields, literals) is left to the per-instantiation check.
        if let Some(cap) = Capability::from_op(at(i)) {
            for neighbour in [i.wrapping_sub(1), i + 1] {
                if neighbour < body_start + 1 || neighbour >= body_end { continue; }
                if let Kind::Ident(n) = at(neighbour)
                    && let Some(Some(t)) = vars.get(n) { demands.push((*t, cap)); }
            }
        }
        i += 1;
    }

    for (index, cap) in demands {
        let declared = &bounds[index];
        if declared.is_empty() || declared.iter().any(|b| matches!(b, Bound::Callable(..))) { continue; }
        let effective: Vec<Type> = AUDIT_TYPES.iter().copied()
            .filter(|ty| declared.iter().all(|b| b.allows(*ty, &[], &[])))
            .collect();
        let bad: Vec<String> = effective.iter().copied()
            .filter(|ty| !cap.supports(*ty))
            .map(|ty| ty.name(ctx))
            .collect();
        if bad.is_empty() { continue; }
        let constraint_text = declared.iter().map(|b| b.name()).collect::<Vec<_>>().join(" + ");
        let effective_text = effective.iter().map(|t| t.name(ctx)).collect::<Vec<_>>().join(", ");
        let end = tokens.binary_search_by_key(&declared_at, |token| token.offset)
            .ok().map_or(declared_at, |index| tokens[index].end);
        return Err(ae_syntax::diagnostic_range(file, source, declared_at..end, &format!(
            "generic function '{name}' declares {}: {} but its body applies {} to {}; the constraint still admits {}, which does not support it ({} allows {}; current effective set is {})",
            params[index], constraint_text, cap.symbol(), params[index], bad.join(", "),
            cap.symbol(), cap.supported_types(), effective_text)));
    }
    Ok(())
}

// `fn pick[T](a: T, b: T) -> T { ... }` before any call site picks arguments.
// The declaration keeps its own token slice and source text so an instantiation
// can be parsed later, even from another file (ADR-31). ADR-41: both are shared
// per source file with `Rc`, so N generic declarations in one file do not make
// N full copies of the token stream (an O(N^2) front-end cost).
