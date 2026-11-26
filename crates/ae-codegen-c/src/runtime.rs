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
         #include <stdatomic.h>\n\
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

// Extern bridge (ADR-0238 stage 3): the pure backend ships the small set of
// runtime symbols the language's `extern "C"` corpus links against, instead
// of depending on the main runtime's translation unit. A declaration naming
// a symbol outside this set still fails closed, so an unsupported extern is
// a compile-time diagnostic rather than a link error.
static inline int64_t ae_extern_probe(int64_t v) { return v * 2 + 1; }
static inline int64_t *ae_std_alloc(int64_t size, int64_t line, int64_t column) {
    (void)line;
    (void)column;
    if (size < 0) ae_trap("allocation failed");
    void *out = malloc((size_t)size + 1u);
    if (!out) ae_trap("allocation failed");
    return (int64_t *)out;
}
static inline int64_t *ae_std_free(int64_t *block) {
    // The wrapper returns the block value so a call site can bind it (the
    // legacy bridge does the same); the address is not dereferenced again.
    const uintptr_t address = (uintptr_t)block;
    free(block);
    return (int64_t *)address;
}
"#);

    // Owned strings (ADR-0238 stage 1): one C type carries the bytes, the
    // length, and whether this value must free them. A string literal is a
    // static view (`owned = false`); every allocating operation returns an
    // owned buffer. Releases are idempotent because they clear the flag and
    // the pointer, so the conservative release lists the frontend emits can
    // run on every path that reaches them.
    out.push_str(r#"typedef struct { const char *ptr; int64_t len; bool owned; } ae_str;

static inline char *ae_str_alloc(int64_t len) {
    char *out = (char *)malloc((size_t)len + 1u);
    if (!out) ae_trap("out of memory");
    return out;
}
static inline ae_str ae_str_from_parts(const char *ptr, int64_t len, bool owned) {
    ae_str value = { ptr, len, owned };
    return value;
}
static inline void ae_str_release(ae_str *slot) {
    if (slot->owned && slot->ptr) free((void *)slot->ptr);
    slot->ptr = NULL;
    slot->len = 0;
    slot->owned = false;
}
// Move a value out of a slot without releasing it: the destination owns the
// buffer, and the (conservative) release of the moved-from slot sees empty.
static inline ae_str ae_str_take(ae_str *slot) {
    ae_str value = *slot;
    slot->ptr = NULL;
    slot->len = 0;
    slot->owned = false;
    return value;
}
// Borrowed face of a string: shares the bytes, never releases them. Used for
// field/element reads and `slice_view`, where the owner stays responsible.
static inline ae_str ae_str_view(ae_str value) {
    value.owned = false;
    return value;
}
static inline bool ae_str_eq(ae_str a, ae_str b) {
    if (a.len != b.len) return false;
    if (a.len <= 8) {
        for (int64_t i = 0; i < a.len; i++) {
            if (a.ptr[i] != b.ptr[i]) return false;
        }
        return true;
    }
    return memcmp(a.ptr, b.ptr, (size_t)a.len) == 0;
}
static inline ae_str ae_str_alloc_copy(const char *ptr, int64_t len) {
    char *out = ae_str_alloc(len);
    if (len > 0) memcpy(out, ptr, (size_t)len);
    out[len] = 0;
    return ae_str_from_parts(out, len, true);
}
static inline ae_str ae_str_clone(ae_str value) {
    return ae_str_alloc_copy(value.ptr, value.len);
}
// Concatenation consumes both operands, like the main runtime: an owned
// operand is released after its bytes are copied, a view or literal is only
// read. Callers hand over owned values (taken from their slot) or clones, so
// every input is either disposable or untouched-by-ownership.
static inline ae_str ae_str_concat(ae_str a, ae_str b) {
    if (a.len < 0 || b.len < 0 || a.len > INT64_MAX - b.len) ae_trap("string too long");
    const int64_t total = a.len + b.len;
    char *out = ae_str_alloc(total);
    if (a.len > 0) memcpy(out, a.ptr, (size_t)a.len);
    if (b.len > 0) memcpy(out + a.len, b.ptr, (size_t)b.len);
    out[total] = 0;
    ae_str_release(&a);
    ae_str_release(&b);
    return ae_str_from_parts(out, total, true);
}
static inline int64_t ae_str_len(ae_str value) { return value.len; }
static inline void ae_print_str(ae_str value) {
    if (value.len > 0) fwrite(value.ptr, 1, (size_t)value.len, stdout);
    fputc(10, stdout);
}
static inline bool ae_utf8_continue_byte(unsigned char byte) { return (byte & 0xC0) == 0x80; }
// Same rule as the main runtime: len/slice count bytes, but a slice may not
// end in the middle of a character. Malformed input bytes are left alone.
static inline void ae_str_check_bounds(ae_str value, int64_t start, int64_t end) {
    if (start < 0 || end < start || end > value.len) ae_trap("string index out of bounds");
    if ((start > 0 && start < value.len && ae_utf8_continue_byte((unsigned char)value.ptr[start]))
        || (end > 0 && end < value.len && ae_utf8_continue_byte((unsigned char)value.ptr[end]))) {
        ae_trap("string index splits a UTF-8 character");
    }
}
static inline ae_str ae_str_slice_view(ae_str value, int64_t start, int64_t end) {
    ae_str_check_bounds(value, start, end);
    return ae_str_from_parts(value.ptr + start, end - start, false);
}
static inline ae_str ae_str_slice(ae_str value, int64_t start, int64_t end) {
    ae_str_check_bounds(value, start, end);
    return ae_str_alloc_copy(value.ptr + start, end - start);
}
static inline ae_str ae_str_from_i64(int64_t value) {
    char buffer[24];
    const int length = snprintf(buffer, sizeof buffer, "%" PRId64, value);
    return ae_str_alloc_copy(buffer, (int64_t)length);
}
static inline ae_str ae_str_from_u64(uint64_t value) {
    char buffer[24];
    const int length = snprintf(buffer, sizeof buffer, "%" PRIu64, value);
    return ae_str_alloc_copy(buffer, (int64_t)length);
}
static inline ae_str ae_str_from_char(uint32_t value) {
    char buffer[4];
    const size_t length = ae_utf8_encode(value, buffer);
    return ae_str_alloc_copy(buffer, (int64_t)length);
}
static inline ae_str ae_str_from_f64(double value) {
    char buffer[96];
    const size_t length = ae_f64_text(value, buffer, sizeof buffer);
    return ae_str_alloc_copy(buffer, (int64_t)length);
}

// Generic vectors (ADR-0238 stage 2): element bytes plus, for owned element
// kinds, the thunk that releases one element in place. Copy elements pass
// NULL and the vector only moves bytes and frees its buffer. Pop, overwrite
// and scope release run each element's own release exactly once.
typedef struct {
    void *data;
    int64_t len;
    int64_t cap;
    int64_t elem_size;
    void (*drop)(void *);
} ae_vec;

static inline ae_vec ae_vec_new(int64_t elem_size, void (*drop)(void *)) {
    ae_vec value;
    value.data = NULL;
    value.len = 0;
    value.cap = 0;
    value.elem_size = elem_size > 0 ? elem_size : 1;
    value.drop = drop;
    return value;
}
static inline void ae_vec_reserve(ae_vec *v, int64_t want) {
    if (want <= v->cap) return;
    int64_t cap = v->cap > 0 ? v->cap : 4;
    while (cap < want) {
        if (cap > INT64_MAX / 2) ae_trap("vector too large");
        cap *= 2;
    }
    if ((size_t)cap > SIZE_MAX / (size_t)v->elem_size) ae_trap("vector too large");
    void *data = realloc(v->data, (size_t)cap * (size_t)v->elem_size);
    if (!data) ae_trap("out of memory");
    v->data = data;
    v->cap = cap;
}
static inline void *ae_vec_at(ae_vec *v, int64_t index) {
    if (index < 0 || index >= v->len) ae_trap("array index out of bounds");
    return (char *)v->data + (size_t)index * (size_t)v->elem_size;
}
static inline void ae_vec_push(ae_vec *v, const void *elem) {
    ae_vec_reserve(v, v->len + 1);
    memcpy((char *)v->data + (size_t)v->len * (size_t)v->elem_size, elem, (size_t)v->elem_size);
    v->len += 1;
}
static inline void ae_vec_store(ae_vec *v, int64_t index, const void *elem) {
    void *slot = ae_vec_at(v, index);
    if (v->drop) v->drop(slot);
    memcpy(slot, elem, (size_t)v->elem_size);
}
static inline void ae_vec_pop(ae_vec *v) {
    if (v->len == 0) ae_trap("pop from empty vector");
    v->len -= 1;
    if (v->drop) v->drop((char *)v->data + (size_t)v->len * (size_t)v->elem_size);
}
static inline void ae_vec_release(ae_vec *v) {
    if (v->drop) {
        for (int64_t i = 0; i < v->len; i++) {
            v->drop((char *)v->data + (size_t)i * (size_t)v->elem_size);
        }
    }
    free(v->data);
    v->data = NULL;
    v->len = 0;
    v->cap = 0;
}
static inline ae_vec ae_vec_take(ae_vec *v) {
    ae_vec value = *v;
    v->data = NULL;
    v->len = 0;
    v->cap = 0;
    v->drop = NULL;
    return value;
}
static inline int64_t ae_vec_len(const ae_vec *v) { return v->len; }
static inline void ae_str_release_elem(void *p) { ae_str_release((ae_str *)p); }
static inline void ae_vec_release_elem(void *p) { ae_vec_release((ae_vec *)p); }
// `bytes(s)`: one i64 element per byte, so the values print as numbers.
static inline ae_vec ae_vec_bytes(ae_str s) {
    ae_vec v = ae_vec_new((int64_t)sizeof(int64_t), NULL);
    ae_vec_reserve(&v, s.len);
    for (int64_t i = 0; i < s.len; i++) {
        int64_t byte = (int64_t)(unsigned char)s.ptr[i];
        ae_vec_push(&v, &byte);
    }
    return v;
}

// Slices (ADR-0236): a read-only fat pointer over a vector, an array, or
// another slice. The element storage stays with the source; a slice is never
// released. `ae_slice` also carries mutable windows: the same header, with
// writes bounds-checked at the access site.
typedef struct {
    void *data;
    int64_t len;
    int64_t elem_size;
} ae_slice;

static inline ae_slice ae_slice_view(void *data, int64_t len, int64_t elem_size) {
    ae_slice value;
    value.data = data;
    value.len = len;
    value.elem_size = elem_size > 0 ? elem_size : 1;
    return value;
}
static inline int64_t ae_slice_len(ae_slice s) { return s.len; }
static inline void *ae_slice_at(ae_slice s, int64_t index) {
    if (index < 0 || index >= s.len) ae_trap("array index out of bounds");
    return (char *)s.data + (size_t)index * (size_t)s.elem_size;
}
static inline void *ae_slice_at_mut(ae_slice *s, int64_t index) {
    if (index < 0 || index >= s->len) ae_trap("array index out of bounds");
    return (char *)s->data + (size_t)index * (size_t)s->elem_size;
}
static inline ae_slice ae_slice_range(ae_slice s, int64_t start, int64_t end) {
    if (start < 0 || end < start || end > s.len) ae_trap("array index out of bounds");
    return ae_slice_view(
        (char *)s.data + (size_t)start * (size_t)s.elem_size,
        end - start,
        s.elem_size
    );
}

// ---- Vector ordering and string joins -------------------------------------
// Only the element kinds the accepted corpus sorts get a comparator; a
// vector of another element kind still fails closed in the emitter.
static inline int ae_vec_cmp_i64(const void *a, const void *b) {
    const int64_t x = *(const int64_t *)a;
    const int64_t y = *(const int64_t *)b;
    return (x > y) - (x < y);
}
static inline int ae_vec_cmp_f64(const void *a, const void *b) {
    const double x = *(const double *)a;
    const double y = *(const double *)b;
    return (x > y) - (x < y);
}
static inline int ae_vec_cmp_str(const void *a, const void *b) {
    const ae_str x = *(const ae_str *)a;
    const ae_str y = *(const ae_str *)b;
    const int64_t common = x.len < y.len ? x.len : y.len;
    const int order = common > 0 ? memcmp(x.ptr, y.ptr, (size_t)common) : 0;
    if (order != 0) return order;
    return (x.len > y.len) - (x.len < y.len);
}
static inline void ae_vec_sort_i64(ae_vec *v) { qsort(v->data, (size_t)v->len, (size_t)v->elem_size, ae_vec_cmp_i64); }
static inline void ae_vec_sort_f64(ae_vec *v) { qsort(v->data, (size_t)v->len, (size_t)v->elem_size, ae_vec_cmp_f64); }
static inline void ae_vec_sort_str(ae_vec *v) { qsort(v->data, (size_t)v->len, (size_t)v->elem_size, ae_vec_cmp_str); }
static inline void ae_vec_reverse(ae_vec *v) {
    if (v->len < 2) return;
    char *tmp = (char *)malloc((size_t)v->elem_size);
    if (!tmp) ae_trap("out of memory");
    for (int64_t i = 0; i < v->len / 2; i++) {
        char *a = (char *)v->data + (size_t)i * (size_t)v->elem_size;
        char *b = (char *)v->data + (size_t)(v->len - 1 - i) * (size_t)v->elem_size;
        memcpy(tmp, a, (size_t)v->elem_size);
        memcpy(a, b, (size_t)v->elem_size);
        memcpy(b, tmp, (size_t)v->elem_size);
    }
    free(tmp);
}
static inline ae_str ae_vec_join(const ae_vec *v, ae_str sep) {
    if (v->len == 0) return ae_str_alloc_copy("", 0);
    if (sep.len != 0 && v->len - 1 > INT64_MAX / sep.len) ae_trap("string too long");
    int64_t total = sep.len * (v->len - 1);
    const ae_str *items = (const ae_str *)v->data;
    for (int64_t i = 0; i < v->len; i++) {
        if (total > INT64_MAX - items[i].len) ae_trap("string too long");
        total += items[i].len;
    }
    char *out = ae_str_alloc(total);
    int64_t at = 0;
    for (int64_t i = 0; i < v->len; i++) {
        if (i > 0) {
            if (sep.len > 0) memcpy(out + at, sep.ptr, (size_t)sep.len);
            at += sep.len;
        }
        if (items[i].len > 0) memcpy(out + at, items[i].ptr, (size_t)items[i].len);
        at += items[i].len;
    }
    out[at] = 0;
    return ae_str_from_parts(out, total, true);
}
// count_lines_with(text, pattern): newline-delimited lines containing the
// pattern. Single pass, no allocations.
static inline int ae_line_contains(ae_str line, ae_str pattern) {
    if (pattern.len == 0) return 1;
    if (pattern.len > line.len) return 0;
    const char first = pattern.ptr[0];
    int64_t i = 0;
    while (i + pattern.len <= line.len) {
        const char *hit = memchr(line.ptr + i, first, (size_t)(line.len - i - pattern.len + 1));
        if (!hit) return 0;
        i = (int64_t)(hit - line.ptr);
        if (memcmp(line.ptr + i, pattern.ptr, (size_t)pattern.len) == 0) return 1;
        i += 1;
    }
    return 0;
}
static inline int64_t ae_count_lines_with(ae_str text, ae_str pattern) {
    if (text.len == 0) return 0;
    int64_t count = 0;
    const char *p = text.ptr;
    int64_t remain = text.len;
    while (remain > 0) {
        const char *nl = (const char *)memchr(p, 0x0a, (size_t)remain);
        const int64_t line_len = nl ? (int64_t)(nl - p) : remain;
        if (ae_line_contains(ae_str_from_parts(p, line_len, false), pattern)) count += 1;
        if (!nl) break;
        remain -= line_len + 1;
        p += line_len + 1;
    }
    return count;
}
static inline bool ae_str_starts_with(ae_str s, ae_str prefix) {
    return prefix.len <= s.len && (prefix.len == 0 || memcmp(s.ptr, prefix.ptr, (size_t)prefix.len) == 0);
}
static inline bool ae_str_ends_with(ae_str s, ae_str suffix) {
    return suffix.len <= s.len
        && (suffix.len == 0 || memcmp(s.ptr + (s.len - suffix.len), suffix.ptr, (size_t)suffix.len) == 0);
}
static inline bool ae_str_contains(ae_str haystack, ae_str needle) {
    if (needle.len == 0) return true;
    if (haystack.len < needle.len) return false;
    return strstr(haystack.ptr, needle.ptr) != NULL;
}
static inline int64_t ae_str_find(ae_str haystack, ae_str needle) {
    if (needle.len == 0) return 0;
    if (needle.len > haystack.len) return -1;
    if (needle.len == 1) {
        const char *hit = (const char *)memchr(haystack.ptr, needle.ptr[0], (size_t)haystack.len);
        return hit == NULL ? -1 : (int64_t)(hit - haystack.ptr);
    }
    const char first = needle.ptr[0];
    for (int64_t i = 0; i + needle.len <= haystack.len; ) {
        const char *hit = (const char *)memchr(
            haystack.ptr + i,
            first,
            (size_t)(haystack.len - needle.len - i + 1)
        );
        if (!hit) return -1;
        i = (int64_t)(hit - haystack.ptr);
        if (memcmp(haystack.ptr + i, needle.ptr, (size_t)needle.len) == 0) return i;
        i += 1;
    }
    return -1;
}

// Wrapping/saturating/checked integer helpers (the `wrapping_*`,
// `saturating_*` and `checked_*` methods). GCC and Clang both provide the
// overflow builtins, so the helper family is one macro set instantiated per
// width below; matching the checked semantics of the main runtime.
#define AE_WRAP_BIN_S(T, U, OP, a, b) do { U u = (U)((U)(a) OP (U)(b)); T r; memcpy(&r, &u, sizeof(r)); return r; } while (0)
#define AE_WRAP_NEG_S(T, U, a) do { U u = (U)(0 - (U)(a)); T r; memcpy(&r, &u, sizeof(r)); return r; } while (0)
#define AE_SAT_ADD_S(T, MIN, MAX, a, b) do { T r; if (__builtin_add_overflow((a), (b), &r)) return (b) > 0 ? (MAX) : (MIN); return r; } while (0)
#define AE_SAT_SUB_S(T, MIN, MAX, a, b) do { T r; if (__builtin_sub_overflow((a), (b), &r)) return (b) < 0 ? (MAX) : (MIN); return r; } while (0)
#define AE_SAT_MUL_S(T, MIN, MAX, a, b) do { T r; if (__builtin_mul_overflow((a), (b), &r)) return (((a) > 0) == ((b) > 0)) ? (MAX) : (MIN); return r; } while (0)
#define AE_SAT_ADD_U(T, MAX, a, b) do { T r; if (__builtin_add_overflow((a), (b), &r)) return (MAX); return r; } while (0)
#define AE_SAT_SUB_U(T, MAX, a, b) do { T r; if (__builtin_sub_overflow((a), (b), &r)) return 0; return r; } while (0)
#define AE_SAT_MUL_U(T, MAX, a, b) do { T r; if (__builtin_mul_overflow((a), (b), &r)) return (MAX); return r; } while (0)
#define AE_OPT_ADD_S(T, MIN, MAX, a, b, out) return !__builtin_add_overflow((a), (b), (out))
#define AE_OPT_SUB_S(T, MIN, MAX, a, b, out) return !__builtin_sub_overflow((a), (b), (out))
#define AE_OPT_MUL_S(T, MIN, MAX, a, b, out) return !__builtin_mul_overflow((a), (b), (out))
#define AE_OPT_ADD_U(T, MAX, a, b, out) return !__builtin_add_overflow((a), (b), (out))
#define AE_OPT_SUB_U(T, MAX, a, b, out) return !__builtin_sub_overflow((a), (b), (out))
#define AE_OPT_MUL_U(T, MAX, a, b, out) return !__builtin_mul_overflow((a), (b), (out))
#define AE_OPT_DIV_S(T, MIN, a, b, out) do { if ((b) == 0 || ((a) == (MIN) && (b) == -1)) return false; *(out) = (T)((a) / (b)); return true; } while (0)
#define AE_OPT_MOD_S(T, MIN, a, b, out) do { if ((b) == 0 || ((a) == (MIN) && (b) == -1)) return false; *(out) = (T)((a) % (b)); return true; } while (0)
#define AE_OPT_NEG_S(T, MIN, a, out) do { if ((a) == (MIN)) return false; *(out) = (T)(-(a)); return true; } while (0)
#define AE_OPT_DIV_U(T, MAX, a, b, out) do { if ((b) == 0) return false; *(out) = (T)((a) / (b)); return true; } while (0)
#define AE_OPT_MOD_U(T, MAX, a, b, out) do { if ((b) == 0) return false; *(out) = (T)((a) % (b)); return true; } while (0)

// parse_int: status 0 = value, 1 = empty/sign-only, 2 = not a base-10
// integer, 3 = out of i64 range. The emitter turns the status into the
// Result enum's Ok/Err payload.
typedef struct { int64_t status; int64_t value; } ae_parse_i64_result;
static inline ae_parse_i64_result ae_parse_i64(ae_str text) {
    int64_t at = 0;
    bool negative = false;
    if (text.len > 0 && (text.ptr[0] == '-' || text.ptr[0] == '+')) {
        negative = text.ptr[0] == '-';
        at = 1;
    }
    if (at >= text.len) {
        ae_parse_i64_result bad = { 1, 0 };
        return bad;
    }
    uint64_t magnitude = 0;
    const uint64_t limit = negative ? (uint64_t)INT64_MAX + 1u : (uint64_t)INT64_MAX;
    for (; at < text.len; at++) {
        const char c = text.ptr[at];
        if (c < '0' || c > '9') {
            ae_parse_i64_result bad = { 2, 0 };
            return bad;
        }
        const uint64_t digit = (uint64_t)(c - '0');
        if (magnitude > (limit - digit) / 10u) {
            ae_parse_i64_result bad = { 3, 0 };
            return bad;
        }
        magnitude = magnitude * 10u + digit;
    }
    ae_parse_i64_result out = { 0, negative ? (int64_t)(0u - magnitude) : (int64_t)magnitude };
    return out;
}

// Token cursors (the `tokenize`/`for t in cursor` form): the cursor holds a
// view of the base text and an inline copy of the separator, so a temporary
// separator is safe and iterating allocates nothing. Boundaries match
// `split`: a trailing separator yields a final empty token, and empty input
// yields one empty token.
#define AE_TOKENS_SEP_MAX 16
typedef struct {
    ae_str text;
    char sep[AE_TOKENS_SEP_MAX];
    int64_t sep_len;
    int64_t pos;
    bool done;
    ae_str current;
} ae_tokens;
static inline ae_tokens ae_tokens_new(ae_str text, ae_str sep) {
    if (sep.len <= 0) ae_trap("empty separator");
    if (sep.len > AE_TOKENS_SEP_MAX) ae_trap("token separator longer than 16 bytes");
    ae_tokens cursor;
    cursor.text = text;
    memcpy(cursor.sep, sep.ptr, (size_t)sep.len);
    cursor.sep_len = sep.len;
    cursor.pos = 0;
    cursor.done = false;
    cursor.current = ae_str_from_parts("", 0, false);
    return cursor;
}
static inline void ae_tokens_reset(ae_tokens *cursor) {
    cursor->pos = 0;
    cursor->done = false;
    cursor->current = ae_str_from_parts("", 0, false);
}
static inline bool ae_tokens_next(ae_tokens *cursor) {
    if (cursor->done) return false;
    const int64_t start = cursor->pos;
    int64_t at = -1;
    if (cursor->sep_len == 1) {
        const char *hit = (const char *)memchr(cursor->text.ptr + start, cursor->sep[0], (size_t)(cursor->text.len - start));
        if (hit) at = (int64_t)(hit - cursor->text.ptr);
    } else {
        const char first = cursor->sep[0];
        for (int64_t i = start; i + cursor->sep_len <= cursor->text.len; ) {
            const char *hit = (const char *)memchr(
                cursor->text.ptr + i,
                first,
                (size_t)(cursor->text.len - cursor->sep_len - i + 1)
            );
            if (!hit) break;
            i = (int64_t)(hit - cursor->text.ptr);
            if (memcmp(hit, cursor->sep, (size_t)cursor->sep_len) == 0) {
                at = i;
                break;
            }
            i += 1;
        }
    }
    if (at < 0) {
        cursor->current = ae_str_from_parts(cursor->text.ptr + start, cursor->text.len - start, false);
        cursor->pos = cursor->text.len;
        cursor->done = true;
    } else {
        cursor->current = ae_str_from_parts(cursor->text.ptr + start, at - start, false);
        cursor->pos = at + cursor->sep_len;
    }
    return true;
}
static inline ae_str ae_tokens_current(const ae_tokens *cursor) { return cursor->current; }

// Atomic cells (spec/concurrency.md §8): one process-lifetime C11 atomic
// behind a copy-semantics handle. Copying the struct shares the cell;
// nothing frees it; every operation is seq_cst.
typedef struct { _Atomic int64_t *cell; } ae_atomic;
static inline ae_atomic ae_cell_new(int64_t seed) {
    _Atomic int64_t *cell = (_Atomic int64_t *)malloc(sizeof(_Atomic int64_t));
    if (!cell) ae_trap("out of memory");
    atomic_store_explicit(cell, seed, memory_order_seq_cst);
    ae_atomic value = { cell };
    return value;
}
static inline int64_t ae_cell_load(ae_atomic a) {
    return atomic_load_explicit(a.cell, memory_order_seq_cst);
}
static inline void ae_cell_store(ae_atomic a, int64_t value) {
    atomic_store_explicit(a.cell, value, memory_order_seq_cst);
}
static inline int64_t ae_cell_add(ae_atomic a, int64_t value) {
    return atomic_fetch_add_explicit(a.cell, value, memory_order_seq_cst);
}
static inline bool ae_cell_cas(ae_atomic a, int64_t expected, int64_t replacement) {
    int64_t observed = expected;
    return atomic_compare_exchange_strong_explicit(
        a.cell,
        &observed,
        replacement,
        memory_order_seq_cst,
        memory_order_seq_cst
    );
}
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
        emit_int_method_helpers(out, ity);
    }
}

/// Wrapping, saturating and checked helpers for one width, used by the
/// `wrapping_*`/`saturating_*`/`checked_*` integer methods.
fn emit_int_method_helpers(out: &mut String, ity: IntTy) {
    let t = ity.c_name();
    let n = ity.name();
    let (min_expr, max_expr) = int_c_bounds(ity);
    if ity.signed() {
        let u = match ity {
            IntTy::I8 => "uint8_t",
            IntTy::I16 => "uint16_t",
            IntTy::I32 => "uint32_t",
            IntTy::I64 => "uint64_t",
            IntTy::Isize => "uintptr_t",
            _ => unreachable!(),
        };
        let _ = writeln!(out, "static inline {t} ae_wrap_add_{n}({t} a, {t} b) {{ AE_WRAP_BIN_S({t}, {u}, +, a, b); }}");
        let _ = writeln!(out, "static inline {t} ae_wrap_sub_{n}({t} a, {t} b) {{ AE_WRAP_BIN_S({t}, {u}, -, a, b); }}");
        let _ = writeln!(out, "static inline {t} ae_wrap_mul_{n}({t} a, {t} b) {{ AE_WRAP_BIN_S({t}, {u}, *, a, b); }}");
        let _ = writeln!(out, "static inline {t} ae_wrap_neg_{n}({t} a) {{ AE_WRAP_NEG_S({t}, {u}, a); }}");
        let _ = writeln!(out, "static inline {t} ae_sat_add_{n}({t} a, {t} b) {{ AE_SAT_ADD_S({t}, {min_expr}, {max_expr}, a, b); }}");
        let _ = writeln!(out, "static inline {t} ae_sat_sub_{n}({t} a, {t} b) {{ AE_SAT_SUB_S({t}, {min_expr}, {max_expr}, a, b); }}");
        let _ = writeln!(out, "static inline {t} ae_sat_mul_{n}({t} a, {t} b) {{ AE_SAT_MUL_S({t}, {min_expr}, {max_expr}, a, b); }}");
        let _ = writeln!(out, "static inline bool ae_opt_add_{n}({t} a, {t} b, {t} *out) {{ AE_OPT_ADD_S({t}, {min_expr}, {max_expr}, a, b, out); }}");
        let _ = writeln!(out, "static inline bool ae_opt_sub_{n}({t} a, {t} b, {t} *out) {{ AE_OPT_SUB_S({t}, {min_expr}, {max_expr}, a, b, out); }}");
        let _ = writeln!(out, "static inline bool ae_opt_mul_{n}({t} a, {t} b, {t} *out) {{ AE_OPT_MUL_S({t}, {min_expr}, {max_expr}, a, b, out); }}");
        let _ = writeln!(out, "static inline bool ae_opt_div_{n}({t} a, {t} b, {t} *out) {{ AE_OPT_DIV_S({t}, {min_expr}, a, b, out); }}");
        let _ = writeln!(out, "static inline bool ae_opt_mod_{n}({t} a, {t} b, {t} *out) {{ AE_OPT_MOD_S({t}, {min_expr}, a, b, out); }}");
        let _ = writeln!(out, "static inline bool ae_opt_neg_{n}({t} a, {t} *out) {{ AE_OPT_NEG_S({t}, {min_expr}, a, out); }}");
    } else {
        let _ = writeln!(out, "static inline {t} ae_wrap_add_{n}({t} a, {t} b) {{ AE_WRAP_BIN_S({t}, {t}, +, a, b); }}");
        let _ = writeln!(out, "static inline {t} ae_wrap_sub_{n}({t} a, {t} b) {{ AE_WRAP_BIN_S({t}, {t}, -, a, b); }}");
        let _ = writeln!(out, "static inline {t} ae_wrap_mul_{n}({t} a, {t} b) {{ AE_WRAP_BIN_S({t}, {t}, *, a, b); }}");
        let _ = writeln!(out, "static inline {t} ae_wrap_neg_{n}({t} a) {{ AE_WRAP_NEG_S({t}, {t}, a); }}");
        let _ = writeln!(out, "static inline {t} ae_sat_add_{n}({t} a, {t} b) {{ AE_SAT_ADD_U({t}, {max_expr}, a, b); }}");
        let _ = writeln!(out, "static inline {t} ae_sat_sub_{n}({t} a, {t} b) {{ AE_SAT_SUB_U({t}, {max_expr}, a, b); }}");
        let _ = writeln!(out, "static inline {t} ae_sat_mul_{n}({t} a, {t} b) {{ AE_SAT_MUL_U({t}, {max_expr}, a, b); }}");
        let _ = writeln!(out, "static inline bool ae_opt_add_{n}({t} a, {t} b, {t} *out) {{ AE_OPT_ADD_U({t}, {max_expr}, a, b, out); }}");
        let _ = writeln!(out, "static inline bool ae_opt_sub_{n}({t} a, {t} b, {t} *out) {{ AE_OPT_SUB_U({t}, {max_expr}, a, b, out); }}");
        let _ = writeln!(out, "static inline bool ae_opt_mul_{n}({t} a, {t} b, {t} *out) {{ AE_OPT_MUL_U({t}, {max_expr}, a, b, out); }}");
        let _ = writeln!(out, "static inline bool ae_opt_div_{n}({t} a, {t} b, {t} *out) {{ AE_OPT_DIV_U({t}, {max_expr}, a, b, out); }}");
        let _ = writeln!(out, "static inline bool ae_opt_mod_{n}({t} a, {t} b, {t} *out) {{ AE_OPT_MOD_U({t}, {max_expr}, a, b, out); }}");
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
