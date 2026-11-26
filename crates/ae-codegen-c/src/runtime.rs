//! Portable C11 runtime preamble and checked integer arithmetic helpers.

use std::fmt::Write as _;

use ae_ty::IntTy;

use crate::types::int_c_bounds;

/// Emits standard C11 headers, `ae_unit_t`, `ae_trap`, and checked integer helpers.
pub(crate) fn emit_preamble(out: &mut String) {
    out.push_str(
        "#include <inttypes.h>\n\
         #include <math.h>\n\
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

    // Printers and process exit. The formats mirror the legacy runtime: one
    // canonical spelling per value, so fixture stdout compares byte-for-byte
    // with the main backend.
    out.push_str(r#"static inline void ae_print_i64(int64_t value) { printf("%" PRId64, value); fputc(10, stdout); }
static inline void ae_print_u64(uint64_t value) { printf("%" PRIu64, value); fputc(10, stdout); }
static inline void ae_print_bool(bool value) { fputs(value ? "true" : "false", stdout); fputc(10, stdout); }
static inline void ae_print_cstr(const char *text) { fputs(text, stdout); fputc(10, stdout); }

static inline size_t ae_utf8_encode(uint32_t cp, char out[4]) {
    if (cp <= 0x7Fu) {
        out[0] = (char)cp;
        return 1;
    } else if (cp <= 0x7FFu) {
        out[0] = (char)(0xC0u | (cp >> 6));
        out[1] = (char)(0x80u | (cp & 0x3Fu));
        return 2;
    } else if (cp <= 0xFFFFu) {
        out[0] = (char)(0xE0u | (cp >> 12));
        out[1] = (char)(0x80u | ((cp >> 6) & 0x3Fu));
        out[2] = (char)(0x80u | (cp & 0x3Fu));
        return 3;
    } else {
        out[0] = (char)(0xF0u | (cp >> 18));
        out[1] = (char)(0x80u | ((cp >> 12) & 0x3Fu));
        out[2] = (char)(0x80u | ((cp >> 6) & 0x3Fu));
        out[3] = (char)(0x80u | (cp & 0x3Fu));
        return 4;
    }
}
static inline void ae_print_char(uint32_t value) {
    char buffer[4];
    const size_t length = ae_utf8_encode(value, buffer);
    fwrite(buffer, 1, length, stdout);
    fputc(10, stdout);
}

static inline size_t ae_f64_fixed(double value, char *out, size_t cap) {
    for (int decimals = 0; decimals <= 17; decimals++) {
        const int written = snprintf(out, cap, "%.*f", decimals, value);
        if (written < 0 || (size_t)written >= cap) return 0;
        if (strtod(out, NULL) != value) continue;
        size_t length = (size_t)written;
        char *dot = memchr(out, '.', length);
        if (dot) {
            char *end = out + length;
            while (end > dot + 2 && end[-1] == '0') end -= 1;
            length = (size_t)(end - out);
        }
        return length;
    }
    return 0;
}
static inline size_t ae_f64_scientific(double value, char *out, size_t cap) {
    for (int digits = 1; digits <= 17; digits++) {
        const int written = snprintf(out, cap, "%.*e", digits - 1, value);
        if (written < 0 || (size_t)written >= cap) return 0;
        if (strtod(out, NULL) == value) return (size_t)written;
    }
    return 0;
}
static inline size_t ae_f64_exponent_form(char *text, size_t length) {
    char *marker = memchr(text, 'e', length);
    if (!marker) marker = memchr(text, 'E', length);
    if (!marker) return length;
    const size_t mantissa = (size_t)(marker - text);
    char sign = '+';
    size_t at = mantissa + 1;
    if (at < length && (text[at] == '+' || text[at] == '-')) { sign = text[at]; at += 1; }
    while (at < length && text[at] == '0') at += 1;
    size_t cursor = mantissa;
    text[cursor++] = 'e';
    text[cursor++] = sign;
    if (at == length) {
        text[cursor++] = '0';
    } else {
        while (at < length) text[cursor++] = text[at++];
    }
    return cursor;
}
static inline size_t ae_f64_text(double value, char *out, size_t cap) {
    if (isnan(value)) { memcpy(out, "nan", 3); return 3; }
    if (isinf(value)) {
        const char *text = value < 0.0 ? "-inf" : "inf";
        const size_t length = strlen(text);
        memcpy(out, text, length);
        return length;
    }
    char fixed[64];
    char scientific[64];
    const size_t fixed_length = (value > -1e17 && value < 1e17) ? ae_f64_fixed(value, fixed, sizeof fixed) : 0;
    const size_t scientific_length = ae_f64_scientific(value, scientific, sizeof scientific);
    char *chosen = fixed;
    size_t length = fixed_length;
    if (scientific_length > 0 && (fixed_length == 0 || scientific_length < fixed_length)) {
        chosen = scientific;
        length = scientific_length;
    }
    if (length == 0) { memcpy(out, "nan", 3); return 3; }
    length = ae_f64_exponent_form(chosen, length);
    const bool plain = !memchr(chosen, '.', length) && !memchr(chosen, 'e', length);
    memcpy(out, chosen, length);
    if (plain && length + 2 <= cap) {
        out[length++] = '.';
        out[length++] = '0';
    }
    return length;
}
static inline void ae_print_f64(double value) {
    char buffer[96];
    const size_t length = ae_f64_text(value, buffer, sizeof buffer);
    fwrite(buffer, 1, length, stdout);
    fputc(10, stdout);
}
_Noreturn static inline void ae_exit(int64_t code) { exit((int)code); }
"#);

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
