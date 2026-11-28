//! Deterministic text form of the resolver's type facts (P1.2). `ae
//! emit-types` prints this, and `tests/types` snapshots pin it: signatures,
//! item spellings, local shapes and the checked/deferred/rejected scalar
//! result for every expression the walker recorded. Ids are table order,
//! expression lines are range order, and no timestamp or absolute path is
//! included.

use crate::module::Module;
use crate::resolve::{ExpressionType, Resolution};
use crate::{DefEntry, DefId, DefKind};

pub fn dump(module: &Module, resolution: &Resolution) -> String {
    let mut out = String::new();
    out.push_str(&format!("types {}\n", module.file));

    let mut item_spellings = String::new();
    let mut current_owner = None;
    for entry in &resolution.spellings {
        let owner_kind = resolution.defs.get(entry.owner.0 as usize).map(|def| def.kind);
        if owner_kind == Some(DefKind::Function) {
            continue;
        }
        if current_owner != Some(entry.owner) {
            let name = resolution.defs.get(entry.owner.0 as usize).map(|def| def.name.clone()).unwrap_or_default();
            item_spellings.push_str(&format!("#{} {name}\n", entry.owner.0));
            current_owner = Some(entry.owner);
        }
        let name = if entry.name.is_empty() { String::new() } else { format!(" {} ", entry.name) };
        item_spellings.push_str(&format!("  {}{}-> {}\n", entry.role.name(), name, entry.spelled.spelling.describe()));
    }
    if item_spellings.is_empty() {
        out.push_str("item spellings: none\n");
    } else {
        out.push_str("item spellings:\n");
        out.push_str(&item_spellings);
    }

    for function in &resolution.functions {
        let name = def_name(resolution, function.function_def);
        let DefId(id) = function.function_def;
        out.push_str(&format!("fn #{id} {name}\n"));

        let parameters: Vec<&crate::types::SpellingEntry> = resolution.spellings.iter()
            .filter(|entry| entry.owner == function.function_def && entry.role == crate::types::SpellingRole::Parameter)
            .collect();
        if parameters.is_empty() {
            out.push_str("  params: none\n");
        } else {
            out.push_str("  params:\n");
            for parameter in parameters {
                let label = if parameter.name.is_empty() { "?".to_owned() } else { parameter.name.clone() };
                out.push_str(&format!("    {label}: {}\n", parameter.spelled.spelling.describe()));
            }
        }
        match resolution.spellings.iter()
            .find(|entry| entry.owner == function.function_def && entry.role == crate::types::SpellingRole::Return)
        {
            Some(entry) => out.push_str(&format!("  return: {}\n", entry.spelled.spelling.describe())),
            None => out.push_str("  return: (none)\n"),
        }

        let mut any_local = false;
        for entry in &resolution.defs {
            if entry.kind.is_local() && entry.owner == Some(function.function_def) {
                if !any_local {
                    out.push_str("  locals:\n");
                    any_local = true;
                }
                out.push_str(&format!("    #{} {} = {}\n", entry.id.0, entry.name, local_shape(entry)));
            }
        }
        if !any_local {
            out.push_str("  locals: none\n");
        }

        let mut facts: Vec<(usize, usize, &ExpressionType)> = function.expression_types.iter()
            .map(|(range, kind)| (range.start, range.end, kind))
            .collect();
        facts.sort_by_key(|(start, end, _)| (*start, *end));
        if facts.is_empty() {
            out.push_str("  expressions: none\n");
        } else {
            out.push_str("  expressions:\n");
            for (start, end, kind) in facts {
                out.push_str(&format!("    {start}..{end} {}\n", fact(kind)));
            }
        }
    }
    out
}

fn local_shape(entry: &DefEntry) -> String {
    match &entry.inferred {
        Some(shape) => shape.describe(),
        None => "deferred".to_owned(),
    }
}

fn fact(kind: &ExpressionType) -> String {
    match kind {
        ExpressionType::Checked(scalar) => format!("checked {}", scalar.spelling()),
        ExpressionType::Rejected(error) => format!("rejected {}", error.message()),
        ExpressionType::Deferred => "deferred".to_owned(),
    }
}

fn def_name(resolution: &Resolution, id: DefId) -> String {
    resolution.defs[id.0 as usize].name.clone()
}

