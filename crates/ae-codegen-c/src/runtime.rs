//! Portable C11 runtime preamble and checked integer arithmetic helpers.

use std::fmt::Write as _;

use ae_ty::IntTy;

use crate::types::int_c_bounds;

/// Emits standard C11 headers, `ae_unit_t`, `ae_trap`, and checked integer helpers.
pub(crate) fn emit_preamble(out: &mut String) {
    out.push_str(
        "#include <math.h>\n\
         #include <stdbool.h>\n\
         #include <stddef.h>\n\
         #include <stdint.h>\n\
         #include <stdio.h>\n\
         #include <stdlib.h>\n\
         #include <string.h>\n\n\
         typedef struct { uint8_t _unused; } ae_unit_t;\n\n\
         _Noreturn static inline void ae_trap(const char *msg) {\n\
             fprintf(stderr, \"ae trap: %s\\n\", msg);\n\
             abort();\n\
         }\n\n",
    );

    let int_tys = [
        IntTy::I8,
        IntTy::I16,
        IntTy::I32,
        IntTy::I64,
        IntTy::Isize,
        IntTy::U8,
        IntTy::U16,
        IntTy::U32,
        IntTy::U64,
        IntTy::Usize,
    ];

    for ity in int_tys {
        emit_int_helpers(out, ity);
    }
}

fn emit_int_helpers(out: &mut String, ity: IntTy) {
    let name = ity.name();
    let c_ty = ity.c_name();
    let bits = ity.bits();
    let (min_expr, max_expr) = int_c_bounds(ity);

    let mask_expr = if bits == 64 {
        "UINT64_MAX".to_owned()
    } else {
        format!("((UINT64_C(1) << {bits}) - 1)")
    };

    if ity.signed() {
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_add_{name}({c_ty} a, {c_ty} b) {{\n\
                 if ((b > 0 && a > {max_expr} - b) || (b < 0 && a < {min_expr} - b)) {{\n\
                     ae_trap(\"integer addition overflow\");\n\
                 }}\n\
                 return ({c_ty})((uint64_t)a + (uint64_t)b);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_sub_{name}({c_ty} a, {c_ty} b) {{\n\
                 if ((b < 0 && a > {max_expr} + b) || (b > 0 && a < {min_expr} + b)) {{\n\
                     ae_trap(\"integer subtraction overflow\");\n\
                 }}\n\
                 return ({c_ty})((uint64_t)a - (uint64_t)b);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_mul_{name}({c_ty} a, {c_ty} b) {{\n\
                 if (a > 0) {{\n\
                     if (b > 0 && a > {max_expr} / b) {{ ae_trap(\"integer multiplication overflow\"); }}\n\
                     if (b < 0 && b < {min_expr} / a) {{ ae_trap(\"integer multiplication overflow\"); }}\n\
                 }} else if (a < 0) {{\n\
                     if (b > 0 && a < {min_expr} / b) {{ ae_trap(\"integer multiplication overflow\"); }}\n\
                     if (b < 0 && b < {max_expr} / a) {{ ae_trap(\"integer multiplication overflow\"); }}\n\
                 }}\n\
                 return ({c_ty})((int64_t)a * (int64_t)b);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_div_{name}({c_ty} a, {c_ty} b) {{\n\
                 if (b == 0) {{ ae_trap(\"division by zero\"); }}\n\
                 if (a == {min_expr} && b == -1) {{ ae_trap(\"integer division overflow\"); }}\n\
                 return ({c_ty})(a / b);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_rem_{name}({c_ty} a, {c_ty} b) {{\n\
                 if (b == 0) {{ ae_trap(\"remainder by zero\"); }}\n\
                 if (a == {min_expr} && b == -1) {{ ae_trap(\"integer remainder overflow\"); }}\n\
                 return ({c_ty})(a % b);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_shl_{name}({c_ty} a, {c_ty} b) {{\n\
                 if (b < 0 || b >= {bits}) {{ ae_trap(\"shift left overflow\"); }}\n\
                 uint64_t raw = ((uint64_t)(int64_t)a) << (unsigned)b;\n\
                 uint64_t mask = {mask_expr};\n\
                 raw &= mask;\n\
                 uint64_t sign_bit = UINT64_C(1) << ({bits} - 1);\n\
                 if ((raw & sign_bit) != 0) {{\n\
                     raw |= ~mask;\n\
                 }}\n\
                 return ({c_ty})(int64_t)raw;\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_shr_{name}({c_ty} a, {c_ty} b) {{\n\
                 if (b < 0 || b >= {bits}) {{ ae_trap(\"shift right overflow\"); }}\n\
                 if (a >= 0) {{\n\
                     return ({c_ty})((uint64_t)a >> (unsigned)b);\n\
                 }}\n\
                 return ({c_ty})(int64_t)(~((~(uint64_t)(int64_t)a) >> (unsigned)b));\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_neg_{name}({c_ty} a) {{\n\
                 if (a == {min_expr}) {{ ae_trap(\"integer negation overflow\"); }}\n\
                 return ({c_ty})(-(uint64_t)(int64_t)a);\n\
             }}\n"
        );
    } else {
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_add_{name}({c_ty} a, {c_ty} b) {{\n\
                 if (a > {max_expr} - b) {{ ae_trap(\"integer addition overflow\"); }}\n\
                 return ({c_ty})(a + b);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_sub_{name}({c_ty} a, {c_ty} b) {{\n\
                 if (a < b) {{ ae_trap(\"integer subtraction overflow\"); }}\n\
                 return ({c_ty})(a - b);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_mul_{name}({c_ty} a, {c_ty} b) {{\n\
                 if (b != 0 && a > {max_expr} / b) {{ ae_trap(\"integer multiplication overflow\"); }}\n\
                 return ({c_ty})(a * b);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_div_{name}({c_ty} a, {c_ty} b) {{\n\
                 if (b == 0) {{ ae_trap(\"division by zero\"); }}\n\
                 return ({c_ty})(a / b);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_rem_{name}({c_ty} a, {c_ty} b) {{\n\
                 if (b == 0) {{ ae_trap(\"remainder by zero\"); }}\n\
                 return ({c_ty})(a % b);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_shl_{name}({c_ty} a, {c_ty} b) {{\n\
                 if (b >= {bits}) {{ ae_trap(\"shift left overflow\"); }}\n\
                 uint64_t raw = ((uint64_t)a) << (unsigned)b;\n\
                 uint64_t mask = {mask_expr};\n\
                 return ({c_ty})(raw & mask);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_shr_{name}({c_ty} a, {c_ty} b) {{\n\
                 if (b >= {bits}) {{ ae_trap(\"shift right overflow\"); }}\n\
                 return ({c_ty})(a >> (unsigned)b);\n\
             }}\n"
        );
    }
}
