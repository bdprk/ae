//! Import following: `use "path"` headers resolved against the file system
//! with a cycle guard. The CLI and the differential gate both use
//! [`load_module_transitive`] so they see the same import surface the
//! compiler's loader does, and [`resolve_with_imports`] limits visibility
//! to the imports' `pub` items under free names.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ae_syntax::Token;

use crate::module::{parse_module, parse_module_tokens, Module};
use crate::resolve::Resolution;

/// Load one module plus the transitive closure of its `use` targets, in
/// depth-first order, deduplicated by canonical path. Cycles are cut by the
/// visited set; the closure is capped at 64 files so a malformed project
/// cannot walk the disk forever.
pub fn load_module_transitive(path: &Path) -> Result<(Module, Vec<(String, Module)>), String> {
    let (module, imports, _) = load_tokenized_module(path)?;
    Ok((module, imports))
}

type TokenizedModule = (Module, Vec<(String, Module)>, Vec<Token>);

fn load_tokenized_module(path: &Path) -> Result<TokenizedModule, String> {
    let source = std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let file = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    let tokens = ae_syntax::lex_diagnostic(&source).map_err(|diagnostic| diagnostic.to_string())?;
    let module = parse_module_tokens(&file, &source, &tokens).map_err(|diagnostic| diagnostic.to_string())?;
    let mut visited: HashSet<PathBuf> = HashSet::new();
    if let Ok(canonical) = path.canonicalize() {
        visited.insert(canonical);
    }
    let mut imports = Vec::new();
    let base = path.parent().map(|parent| parent.to_path_buf()).unwrap_or_default();
    load_headers(&module, &base, &mut visited, &mut imports, 0)?;
    Ok((module, imports, tokens))
}

fn load_headers(
    module: &Module,
    base: &Path,
    visited: &mut HashSet<PathBuf>,
    imports: &mut Vec<(String, Module)>,
    depth: usize,
) -> Result<(), String> {
    if depth >= 64 {
        return Err("use headers nest more than 64 files deep".to_owned());
    }
    for use_path in &module.use_paths {
        let target = base.join(use_path);
        let canonical = target.canonicalize().map_err(|error| format!("{}: {error}", target.display()))?;
        if visited.contains(&canonical) {
            continue;
        }
        visited.insert(canonical);
        let source = std::fs::read_to_string(&target).map_err(|error| format!("{}: {error}", target.display()))?;
        let file = target.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| target.display().to_string());
        let imported = parse_module(&file, &source).map_err(|diagnostic| format!("{}: {diagnostic}", target.display()))?;
        let nested_base = target.parent().map(|parent| parent.to_path_buf()).unwrap_or_default();
        load_headers(&imported, &nested_base, visited, imports, depth + 1)?;
        imports.push((use_path.clone(), imported));
    }
    Ok(())
}

/// Load and resolve in one call: the entry module plus everything its
/// headers pull in, `pub` items of the imports visible under free names.
pub fn load_and_resolve(path: &Path) -> Result<(Module, Resolution), String> {
    let (module, imports, tokens) = load_tokenized_module(path)?;
    let resolution = crate::resolve::resolve_with_tokens(&module, &imports, &tokens);
    Ok((module, resolution))
}

pub fn resolve_loaded(module: &Module, imports: &[(String, Module)]) -> Resolution {
    crate::resolve::resolve_with_imports(module, imports)
}
