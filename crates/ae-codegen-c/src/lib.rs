//! Standalone pure MIR to C11 backend for the AE compiler (P3.2).
//!
//! Emits portable C11 directly from validated [`ae_mir::Body`] graphs without
//! depending on `ae-syntax`, `ae-hir`, or `ae-compiler`.

mod emit;
mod runtime;
mod types;

use std::fmt;

use ae_layout::TargetSpec;
use ae_mir::{Body, MirValidationError, validate};
use ae_ty::DefId;

use crate::emit::{build_fn_name_map, emit_fn_body, emit_fn_decl, emit_main_wrapper};
use crate::runtime::emit_preamble;
use crate::types::ModuleTypes;

/// Configuration for C11 code generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmitConfig {
    pub target: TargetSpec,
    pub emit_main_wrapper: bool,
}

impl Default for EmitConfig {
    fn default() -> Self {
        Self {
            target: TargetSpec::host(),
            emit_main_wrapper: false,
        }
    }
}

/// Structured error returned when MIR cannot be lowered to C11.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodegenError {
    InvalidMir {
        body_name: String,
        errors: Vec<MirValidationError>,
    },
    UnsupportedType {
        body_name: String,
        detail: String,
    },
    UnsupportedFeature {
        body_name: String,
        detail: String,
    },
    UnresolvedCall {
        body_name: String,
        def_id: DefId,
    },
    InvalidEntry {
        detail: String,
    },
    TypeCycle {
        detail: String,
    },
}

impl fmt::Display for CodegenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMir { body_name, errors } => {
                write!(
                    f,
                    "invalid MIR body `{body_name}` ({} validation error(s))",
                    errors.len()
                )
            }
            Self::UnsupportedType { body_name, detail } => {
                write!(f, "body `{body_name}`: unsupported type: {detail}")
            }
            Self::UnsupportedFeature { body_name, detail } => {
                write!(f, "body `{body_name}`: unsupported feature: {detail}")
            }
            Self::UnresolvedCall { body_name, def_id } => {
                write!(
                    f,
                    "body `{body_name}`: unresolved call to DefId({})",
                    def_id.0
                )
            }
            Self::InvalidEntry { detail } => write!(f, "invalid entry point: {detail}"),
            Self::TypeCycle { detail } => write!(f, "type cycle: {detail}"),
        }
    }
}

impl std::error::Error for CodegenError {}

/// Emits a standalone C11 translation unit for a single MIR [`Body`].
pub fn emit_body_c(body: &Body, config: &EmitConfig) -> Result<String, CodegenError> {
    emit_module_c(std::slice::from_ref(body), config)
}

/// Emits a standalone C11 translation unit for a module of MIR [`Body`] functions.
pub fn emit_module_c(bodies: &[Body], config: &EmitConfig) -> Result<String, CodegenError> {
    for body in bodies {
        if let Err(errors) = validate(body) {
            return Err(CodegenError::InvalidMir {
                body_name: body.name.clone(),
                errors,
            });
        }
    }

    let types = ModuleTypes::collect(bodies);
    let (fn_names, def_map) = build_fn_name_map(bodies);

    let mut out = String::new();
    emit_preamble(&mut out);
    // Ownership helpers name the user drop functions, so the function name
    // map is built first; forward declarations for the bodies still follow
    // the typedefs.
    types.emit_typedefs(&mut out, &def_map)?;

    for (body, c_name) in bodies.iter().zip(fn_names.iter()) {
        emit_fn_decl(&mut out, body, c_name);
        out.push_str(";\n");
    }
    if !bodies.is_empty() {
        out.push('\n');
    }
    // Spawn entry trampolines call the lifted functions, so they follow the
    // forward declarations and precede the definitions.
    types.emit_task_entries(&mut out, &def_map)?;

    for (body, c_name) in bodies.iter().zip(fn_names.iter()) {
        emit_fn_body(&mut out, body, c_name, &types, &def_map)?;
    }

    if config.emit_main_wrapper {
        emit_main_wrapper(&mut out, bodies, &fn_names, config)?;
    }

    Ok(out)
}
