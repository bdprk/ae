//! Deterministic text form of a resolution. `ae emit-resolve` prints this,
//! and `tests/resolve` snapshots pin it: every def and occurrence on its own
//! line, ids in table order, occurrences in walk order, no timestamps and no
//! absolute paths.

use crate::module::Module;
use crate::resolve::Resolution;
use crate::{DefEntry, DefId, DefKind, Role};

pub fn dump(module: &Module, resolution: &Resolution) -> String {
    let mut out = String::new();
    out.push_str(&format!("resolve {}\n", module.file));
    if module.use_paths.is_empty() {
        out.push_str("use headers: none\n");
    } else {
        out.push_str("use headers:\n");
        for path in &module.use_paths {
            out.push_str(&format!("  use \"{path}\"\n"));
        }
    }
    out.push_str("items:\n");
    for entry in &resolution.defs {
        if !entry.kind.is_local() {
            out.push_str(&def_line(entry));
        }
    }
    // Type spellings, grouped by owner in first-seen order (the entries
    // themselves are in source order). An owner without spellings is
    // skipped; a name shows only where one exists (parameters, fields).
    out.push_str("types:
");
    let mut grouped: Vec<(crate::DefId, Vec<&crate::types::SpellingEntry>)> = Vec::new();
    for entry in &resolution.spellings {
        match grouped.last_mut() {
            Some((owner, group)) if *owner == entry.owner => group.push(entry),
            _ => grouped.push((entry.owner, vec![entry])),
        }
    }
    for (owner, group) in &grouped {
        let DefId(owner_id) = *owner;
        let name = resolution.defs.get(owner_id as usize).map(|entry| entry.name.clone()).unwrap_or_default();
        out.push_str(&format!("#{} {name}:
", owner_id));
        for entry in group {
            let label = if entry.name.is_empty() {
                String::new()
            } else {
                format!(" {} ", entry.name)
            };
            out.push_str(&format!(
                "  {}..{} {}{}-> {}
",
                entry.spelled.range.start, entry.spelled.range.end,
                entry.role.name(), label,
                entry.spelled.spelling.describe(),
            ));
        }
    }
    function_blocks(&mut out, resolution);
    // R3b: imported modules' bodies are walked too, each with its own file
    // tag and def table. The section is omitted when nothing is imported, so
    // single-file snapshots keep their exact text.
    if !resolution.imported.is_empty() {
        out.push_str("imported bodies:\n");
        for (file, imported) in &resolution.imported {
            out.push_str(&format!("file {file}:\n"));
            function_blocks(&mut out, imported);
        }
    }
    if resolution.diagnostics.is_empty() {
        out.push_str("diagnostics: none\n");
    } else {
        out.push_str("diagnostics:\n");
        for diagnostic in &resolution.diagnostics {
            out.push_str(&format!("  {diagnostic}\n"));
        }
    }
    out
}

fn function_blocks(out: &mut String, resolution: &Resolution) {
    for function in &resolution.functions {
        let name = def_name(resolution, function.function_def);
        out.push_str(&format!("locals in #{} {name}:\n", function.function_def.0));
        let mut any = false;
        for entry in &resolution.defs {
            if entry.kind.is_local() && entry.owner == Some(function.function_def) {
                out.push_str(&def_line(entry));
                any = true;
            }
        }
        if !any {
            out.push_str("  none\n");
        }
        out.push_str(&format!("occurrences in #{} {name}:\n", function.function_def.0));
        if function.occurrences.is_empty() {
            out.push_str("  none\n");
        }
        for occurrence in &function.occurrences {
            // A write or borrow with no target is the resolver's E0101 case
            // (nothing live owns the name); a value position with no target
            // is the builtin/prelude/imported surface this single-file pass
            // does not own yet.
            let target = match occurrence.target {
                Some(DefId(id)) => format!("#{id}"),
                None if matches!(occurrence.role, Role::Write | Role::Borrow) => "unresolved".to_owned(),
                None => "external".to_owned(),
            };
            out.push_str(&format!(
                "  {}..{} {} {} -> {target}\n",
                occurrence.range.start, occurrence.range.end,
                role_name(occurrence.role), occurrence.name,
            ));
        }
    }
}

fn def_name(resolution: &Resolution, id: DefId) -> String {
    resolution.defs[id.0 as usize].name.clone()
}

fn def_line(entry: &DefEntry) -> String {
    let mut line = format!("#{} {} {} @{}..{}", entry.id.0, kind_name(entry.kind), entry.name, entry.range.start, entry.range.end);
    if let Some(DefId(owner)) = entry.owner {
        line.push_str(&format!(" owner=#{owner}"));
    }
    if entry.mutable {
        line.push_str(" mutable");
    }
    if entry.kind.is_local() {
        // The inferred shape (ADR-0229 slice 1): a bound shape or the
        // honest `deferred` for initializers the subset cannot type yet.
        match &entry.inferred {
            Some(shape) => line.push_str(&format!(" = {}", shape.describe())),
            None => line.push_str(" = deferred"),
        }
    }
    line.push('\n');
    line
}

fn kind_name(kind: DefKind) -> &'static str {
    match kind {
        DefKind::Function => "function",
        DefKind::Struct => "struct",
        DefKind::Enum => "enum",
        DefKind::Variant => "variant",
        DefKind::Trait => "trait",
        DefKind::Impl => "impl",
        DefKind::Parameter => "parameter",
        DefKind::ClosureParameter => "closure-parameter",
        DefKind::Binding => "binding",
        DefKind::ForBinding => "for-binding",
        DefKind::PatternBinding => "pattern-binding",
    }
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Read => "read",
        Role::Call => "call",
        Role::Construct => "construct",
        Role::Borrow => "borrow",
        Role::Write => "write",
        Role::DerefWrite => "deref-write",
        Role::Qualifier => "qualifier",
    }
}
