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
         #include <errno.h>\n\
         #include <stdio.h>\n\
         #include <stdlib.h>\n\
         #include <string.h>\n\
         #include <sys/stat.h>\n\
         #ifndef _WIN32\n\
         #include <fcntl.h>\n\
         #include <sys/wait.h>\n\
         #include <unistd.h>\n\
         #endif\n\
         #include <time.h>\n\n\
         typedef struct { uint8_t _unused; } ae_unit_t;\n\n\
         // The positioned failure sink is defined with the concurrency runtime\n\
         // (a child thread records a failure instead of ending the process).\n\
         static _Noreturn void ae_fail_at(const char *msg, size_t line, size_t column);\n\
         _Noreturn static inline void ae_trap(const char *msg) {\n\
             ae_fail_at(msg, 0, 0);\n\
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
    if (size < 0) ae_fail_at("out of memory", (size_t)line, (size_t)column);
    void *out = malloc((size_t)size + 1u);
    if (!out) ae_fail_at("out of memory", (size_t)line, (size_t)column);
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
static inline void ae_str_check_bounds(ae_str value, int64_t start, int64_t end, size_t line, size_t column) {
    if (start < 0 || end < start || end > value.len) ae_fail_at("string index out of bounds", line, column);
    if ((start > 0 && start < value.len && ae_utf8_continue_byte((unsigned char)value.ptr[start]))
        || (end > 0 && end < value.len && ae_utf8_continue_byte((unsigned char)value.ptr[end]))) {
        ae_fail_at("string index splits a UTF-8 character", line, column);
    }
}
static inline ae_str ae_str_slice_view(ae_str value, int64_t start, int64_t end, size_t line, size_t column) {
    ae_str_check_bounds(value, start, end, line, column);
    return ae_str_from_parts(value.ptr + start, end - start, false);
}
static inline ae_str ae_str_slice(ae_str value, int64_t start, int64_t end, size_t line, size_t column) {
    ae_str_check_bounds(value, start, end, line, column);
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
static inline void *ae_vec_at(ae_vec *v, int64_t index, size_t line, size_t column) {
    if (index < 0 || index >= v->len) ae_fail_at("array index out of bounds", line, column);
    return (char *)v->data + (size_t)index * (size_t)v->elem_size;
}
static inline void ae_vec_push(ae_vec *v, const void *elem) {
    ae_vec_reserve(v, v->len + 1);
    memcpy((char *)v->data + (size_t)v->len * (size_t)v->elem_size, elem, (size_t)v->elem_size);
    v->len += 1;
}
static inline void ae_vec_store(ae_vec *v, int64_t index, const void *elem, size_t line, size_t column) {
    void *slot = ae_vec_at(v, index, line, column);
    if (v->drop) v->drop(slot);
    memcpy(slot, elem, (size_t)v->elem_size);
}
static inline void ae_vec_pop(ae_vec *v, size_t line, size_t column) {
    if (v->len == 0) ae_fail_at("pop from empty vector", line, column);
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
static inline void *ae_slice_at(ae_slice s, int64_t index, size_t line, size_t column) {
    if (index < 0 || index >= s.len) ae_fail_at("array index out of bounds", line, column);
    return (char *)s.data + (size_t)index * (size_t)s.elem_size;
}
static inline void *ae_slice_at_mut(ae_slice *s, int64_t index, size_t line, size_t column) {
    if (index < 0 || index >= s->len) ae_fail_at("array index out of bounds", line, column);
    return (char *)s->data + (size_t)index * (size_t)s->elem_size;
}
static inline ae_slice ae_slice_range(ae_slice s, int64_t start, int64_t end, size_t line, size_t column) {
    if (start < 0 || end < start || end > s.len) ae_fail_at("array index out of bounds", line, column);
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

// String splitting, line splitting and UTF-8 helpers. Each token is its own
// owned string (the pure backend's vector stores ae_str elements), so the
// result vector releases them through its element thunk.
static inline int64_t ae_find_sep(const char *base, int64_t from, int64_t len, ae_str sep) {
    if (from >= len || sep.len > len - from) return -1;
    if (sep.len == 1) {
        const char *hit = (const char *)memchr(base + from, sep.ptr[0], (size_t)(len - from));
        return hit == NULL ? -1 : (int64_t)(hit - base);
    }
    const char first = sep.ptr[0];
    for (int64_t i = from; i + sep.len <= len; ) {
        const char *hit = (const char *)memchr(base + i, first, (size_t)(len - sep.len - i + 1));
        if (!hit) return -1;
        i = (int64_t)(hit - base);
        if (memcmp(hit, sep.ptr, (size_t)sep.len) == 0) return i;
        i += 1;
    }
    return -1;
}
static inline ae_vec ae_vec_split(ae_str s, ae_str sep, size_t line, size_t column) {
    if (sep.len == 0) ae_fail_at("empty separator", line, column);
    ae_vec out = ae_vec_new((int64_t)sizeof(ae_str), ae_str_release_elem);
    int64_t start = 0;
    for (;;) {
        const int64_t at = ae_find_sep(s.ptr, start, s.len, sep);
        if (at < 0) {
            ae_str token = ae_str_alloc_copy(s.ptr + start, s.len - start);
            ae_vec_push(&out, &token);
            break;
        }
        ae_str token = ae_str_alloc_copy(s.ptr + start, at - start);
        ae_vec_push(&out, &token);
        start = at + sep.len;
    }
    return out;
}
static inline ae_vec ae_vec_lines(ae_str s) {
    ae_vec out = ae_vec_new((int64_t)sizeof(ae_str), ae_str_release_elem);
    int64_t start = 0;
    while (start < s.len) {
        const char *nl = (const char *)memchr(s.ptr + start, '\n', (size_t)(s.len - start));
        const int64_t end = nl ? (int64_t)(nl - s.ptr) : s.len;
        ae_str token = ae_str_alloc_copy(s.ptr + start, end - start);
        ae_vec_push(&out, &token);
        if (!nl) break;
        start = end + 1;
    }
    return out;
}
static inline int64_t ae_utf8_len(ae_str s) {
    int64_t count = 0;
    for (int64_t i = 0; i < s.len; i++) {
        if (!ae_utf8_continue_byte((unsigned char)s.ptr[i])) count += 1;
    }
    return count;
}
static inline ae_str ae_utf8_prefix(ae_str s, int64_t n, size_t line, size_t column) {
    if (n < 0) ae_fail_at("character index out of bounds", line, column);
    int64_t at = 0;
    int64_t seen = 0;
    while (at < s.len && seen < n) {
        at += 1;
        while (at < s.len && ae_utf8_continue_byte((unsigned char)s.ptr[at])) at += 1;
        seen += 1;
    }
    return ae_str_alloc_copy(s.ptr, at);
}
static inline bool ae_is_space(unsigned char byte) {
    return byte == ' ' || byte == '\t' || byte == '\n' || byte == '\r' || byte == '\v' || byte == '\f';
}
static inline ae_str ae_str_trim(ae_str s) {
    int64_t start = 0;
    int64_t end = s.len;
    while (start < end && ae_is_space((unsigned char)s.ptr[start])) start += 1;
    while (end > start && ae_is_space((unsigned char)s.ptr[end - 1])) end -= 1;
    return ae_str_alloc_copy(s.ptr + start, end - start);
}
static inline double ae_f64_min(double a, double b) {
    if (isnan(a)) return b;
    if (isnan(b)) return a;
    if (a == 0.0 && b == 0.0) return (signbit(a) && !signbit(b)) ? a : b;
    return a < b ? a : b;
}
static inline double ae_f64_max(double a, double b) {
    if (isnan(a)) return b;
    if (isnan(b)) return a;
    if (a == 0.0 && b == 0.0) return (signbit(a) && !signbit(b)) ? b : a;
    return a > b ? a : b;
}

// Program arguments, stdin/file reads, and portable path helpers. A read
// returns a small status struct so the emitter can build the language's
// ReadResult/WriteResult enums; the trapping forms reuse the same helpers and
// call ae_fail_at with their own position.
static int64_t ae_argc_g = 0;
static char **ae_argv_g = NULL;
static inline ae_vec ae_args_value(void) {
    ae_vec out = ae_vec_new((int64_t)sizeof(ae_str), ae_str_release_elem);
    for (int64_t i = 0; i < ae_argc_g; i++) {
        const char *arg = ae_argv_g[i] ? ae_argv_g[i] : "";
        ae_str value = ae_str_alloc_copy(arg, (int64_t)strlen(arg));
        ae_vec_push(&out, &value);
    }
    return out;
}
typedef struct { int64_t status; ae_str text; ae_str error; } ae_read_result;
static inline ae_str ae_io_error(void) {
    const char *text = strerror(errno);
    if (!text) text = "I/O error";
    return ae_str_alloc_copy(text, (int64_t)strlen(text));
}
static inline ae_read_result ae_try_read_file_value(ae_str path) {
    ae_read_result out;
    out.status = 0;
    out.text = ae_str_from_parts("", 0, false);
    out.error = ae_str_from_parts("", 0, false);
    FILE *file = fopen(path.ptr, "rb");
    if (!file) {
        out.status = 1;
        out.error = ae_io_error();
        return out;
    }
    if (fseek(file, 0, SEEK_END) != 0) {
        fclose(file);
        out.status = 1;
        out.error = ae_io_error();
        return out;
    }
    const long size = ftell(file);
    if (size < 0) {
        fclose(file);
        out.status = 1;
        out.error = ae_io_error();
        return out;
    }
    rewind(file);
    char *buffer = ae_str_alloc((int64_t)size);
    const size_t got = fread(buffer, 1, (size_t)size, file);
    fclose(file);
    if (got != (size_t)size) {
        free(buffer);
        out.status = 1;
        out.error = ae_io_error();
        return out;
    }
    buffer[size] = 0;
    out.text = ae_str_from_parts(buffer, (int64_t)size, true);
    return out;
}
static inline ae_read_result ae_try_read_stdin_value(void) {
    ae_read_result out;
    out.status = 0;
    out.error = ae_str_from_parts("", 0, false);
    size_t capacity = 8192;
    size_t length = 0;
    char *buffer = (char *)malloc(capacity);
    if (!buffer) ae_trap("out of memory");
    errno = 0;
    for (;;) {
        if (length == capacity) {
            capacity *= 2;
            char *grown = (char *)realloc(buffer, capacity);
            if (!grown) {
                free(buffer);
                ae_trap("out of memory");
            }
            buffer = grown;
        }
        const size_t got = fread(buffer + length, 1, capacity - length, stdin);
        length += got;
        if (got == 0) {
            if (ferror(stdin)) {
                free(buffer);
                out.status = 1;
                out.text = ae_str_from_parts("", 0, false);
                out.error = ae_io_error();
                return out;
            }
            break;
        }
    }
    buffer[length] = 0;
    out.text = ae_str_from_parts(buffer, (int64_t)length, true);
    return out;
}
static inline ae_str ae_read_file_value(ae_str path, size_t line, size_t column) {
    ae_read_result out = ae_try_read_file_value(path);
    if (out.status != 0) {
        ae_str_release(&out.text);
        ae_str_release(&out.error);
        ae_fail_at("cannot read file", line, column);
    }
    ae_str_release(&out.error);
    return out.text;
}
static inline int64_t ae_write_file_value(ae_str path, ae_str content, size_t line, size_t column) {
    FILE *file = fopen(path.ptr, "wb");
    if (!file) ae_fail_at("cannot write file", line, column);
    const size_t written = fwrite(content.ptr, 1, (size_t)content.len, file);
    if (fclose(file) != 0 || written != (size_t)content.len) {
        ae_fail_at("cannot write file", line, column);
    }
    return (int64_t)written;
}
static inline bool ae_is_separator(char c) { return c == '/' || c == '\\'; }
typedef struct { int64_t status; int64_t written; ae_str error; } ae_write_result;
static inline ae_write_result ae_try_write_file_value(ae_str path, ae_str content) {
    ae_write_result out;
    out.status = 0;
    out.written = 0;
    out.error = ae_str_from_parts("", 0, false);
    FILE *file = fopen(path.ptr, "wb");
    if (!file) {
        out.status = 1;
        out.error = ae_io_error();
        return out;
    }
    const size_t written = fwrite(content.ptr, 1, (size_t)content.len, file);
    if (fclose(file) != 0 || written != (size_t)content.len) {
        out.status = 1;
        out.error = ae_io_error();
        return out;
    }
    out.written = (int64_t)written;
    return out;
}
static inline bool ae_path_is_absolute(ae_str path) {
    if (path.len == 0) return false;
    if (ae_is_separator(path.ptr[0])) return true;
    return path.len >= 2 && path.ptr[1] == ':';
}
static inline ae_str ae_path_join_value(ae_str dir, ae_str name) {
    if (name.len == 0) return ae_str_alloc_copy(dir.ptr, dir.len);
    if (dir.len == 0) return ae_str_alloc_copy(name.ptr, name.len);
    if (ae_path_is_absolute(name)) return ae_str_alloc_copy(name.ptr, name.len);
    const bool separated = ae_is_separator(dir.ptr[dir.len - 1]);
    const int64_t total = dir.len + name.len + (separated ? 0 : 1);
    char *buffer = ae_str_alloc(total);
    memcpy(buffer, dir.ptr, (size_t)dir.len);
    int64_t at = dir.len;
    if (!separated) buffer[at++] = '/';
    memcpy(buffer + at, name.ptr, (size_t)name.len);
    buffer[total] = 0;
    return ae_str_from_parts(buffer, total, true);
}
static inline ae_str ae_path_name_value(ae_str path) {
    int64_t end = path.len;
    while (end > 1 && ae_is_separator(path.ptr[end - 1])) end--;
    int64_t start = end;
    while (start > 0 && !ae_is_separator(path.ptr[start - 1])) start--;
    if (start == end) return ae_str_alloc_copy(path.ptr, end);
    return ae_str_alloc_copy(path.ptr + start, end - start);
}
static inline ae_str ae_path_parent_value(ae_str path) {
    int64_t end = path.len;
    while (end > 1 && ae_is_separator(path.ptr[end - 1])) end--;
    int64_t at = end;
    while (at > 0 && !ae_is_separator(path.ptr[at - 1])) at--;
    if (at == 0) return ae_str_alloc_copy("", 0);
    int64_t stop = at;
    while (stop > 1 && ae_is_separator(path.ptr[stop - 1])) stop--;
    if (stop == 2 && path.ptr[1] == ':' && at > stop) stop = at;
    if (stop == 1 && ae_is_separator(path.ptr[0])) stop = 1;
    return ae_str_alloc_copy(path.ptr, stop);
}
static inline bool ae_path_exists_value(ae_str path) {
    struct stat info;
    return stat(path.ptr, &info) == 0;
}
static inline bool ae_path_is_dir_value(ae_str path) {
    struct stat info;
    if (stat(path.ptr, &info) != 0) return false;
#ifdef _WIN32
    return (info.st_mode & _S_IFDIR) != 0;
#else
    return S_ISDIR(info.st_mode);
#endif
}

// Directory listing, metadata and the mutation family. Results use small
// status structs so the emitter can build DirResult/FileResult/StatusResult.
#ifdef _WIN32
#include <direct.h>
#include <io.h>
#else
#include <dirent.h>
#include <unistd.h>
#endif
static inline int ae_name_compare(const void *a, const void *b) {
    const ae_str x = *(const ae_str *)a;
    const ae_str y = *(const ae_str *)b;
    const int64_t common = x.len < y.len ? x.len : y.len;
    const int order = common > 0 ? memcmp(x.ptr, y.ptr, (size_t)common) : 0;
    if (order != 0) return order;
    return (x.len > y.len) - (x.len < y.len);
}
typedef struct { int64_t status; ae_vec names; ae_str error; } ae_dir_result;
static inline ae_dir_result ae_try_list_dir_value(ae_str path) {
    ae_dir_result out;
    out.status = 0;
    out.error = ae_str_from_parts("", 0, false);
    out.names = ae_vec_new((int64_t)sizeof(ae_str), ae_str_release_elem);
    ae_str pattern = ae_path_join_value(path, ae_str_from_parts("*", 1, false));
#ifdef _WIN32
    struct _finddata_t entry;
    intptr_t walk = _findfirst(pattern.ptr, &entry);
    if (walk == -1) {
        out.status = 1;
        out.error = ae_io_error();
        ae_str_release(&pattern);
        return out;
    }
    do {
        if (strcmp(entry.name, ".") == 0 || strcmp(entry.name, "..") == 0) continue;
        ae_str name = ae_str_alloc_copy(entry.name, (int64_t)strlen(entry.name));
        ae_vec_push(&out.names, &name);
    } while (_findnext(walk, &entry) == 0);
    _findclose(walk);
#else
    DIR *walk = opendir(path.ptr);
    if (!walk) {
        out.status = 1;
        out.error = ae_io_error();
        ae_str_release(&pattern);
        return out;
    }
    struct dirent *entry;
    while ((entry = readdir(walk)) != NULL) {
        if (strcmp(entry->d_name, ".") == 0 || strcmp(entry->d_name, "..") == 0) continue;
        ae_str name = ae_str_alloc_copy(entry->d_name, (int64_t)strlen(entry->d_name));
        ae_vec_push(&out.names, &name);
    }
    closedir(walk);
#endif
    ae_str_release(&pattern);
    if (out.names.len > 1) {
        qsort(out.names.data, (size_t)out.names.len, sizeof(ae_str), ae_name_compare);
    }
    return out;
}
typedef struct {
    int64_t status;
    int64_t size;
    int64_t mtime;
    bool is_dir;
    ae_str error;
} ae_file_info_result;
static inline ae_file_info_result ae_try_file_info_value(ae_str path) {
    ae_file_info_result out;
    out.status = 0;
    out.size = 0;
    out.mtime = 0;
    out.is_dir = false;
    out.error = ae_str_from_parts("", 0, false);
    errno = 0;
    struct stat info;
    if (stat(path.ptr, &info) != 0) {
        out.status = 1;
        out.error = ae_io_error();
        return out;
    }
    out.size = (int64_t)info.st_size;
    out.mtime = (int64_t)info.st_mtime;
#ifdef _WIN32
    out.is_dir = (info.st_mode & _S_IFDIR) != 0;
#else
    out.is_dir = S_ISDIR(info.st_mode);
#endif
    return out;
}
typedef struct { int64_t status; ae_str error; } ae_status_result;
static inline ae_status_result ae_status_ok_value(void) {
    ae_status_result out;
    out.status = 0;
    out.error = ae_str_from_parts("", 0, false);
    return out;
}
static inline ae_status_result ae_status_fail_value(ae_str error) {
    ae_status_result out;
    out.status = 1;
    out.error = error;
    return out;
}
static inline ae_status_result ae_try_copy_file_value(ae_str from, ae_str to) {
    errno = 0;
    FILE *source = fopen(from.ptr, "rb");
    if (!source) return ae_status_fail_value(ae_io_error());
    FILE *target = fopen(to.ptr, "wb");
    if (!target) {
        fclose(source);
        return ae_status_fail_value(ae_io_error());
    }
    bool failed = false;
    char buffer[65536];
    size_t read = 0;
    while ((read = fread(buffer, 1, sizeof buffer, source)) > 0) {
        if (fwrite(buffer, 1, read, target) != read) {
            failed = true;
            break;
        }
    }
    if (!failed && ferror(source)) failed = true;
    if (fclose(source) != 0 && !failed) failed = true;
    if (fclose(target) != 0 && !failed) failed = true;
    if (failed) return ae_status_fail_value(ae_io_error());
    return ae_status_ok_value();
}
static inline ae_status_result ae_try_append_file_value(ae_str path, ae_str content) {
    errno = 0;
    FILE *file = fopen(path.ptr, "ab");
    if (!file) return ae_status_fail_value(ae_io_error());
    const size_t written = fwrite(content.ptr, 1, (size_t)content.len, file);
    if (fclose(file) != 0 || written != (size_t)content.len) {
        return ae_status_fail_value(ae_io_error());
    }
    return ae_status_ok_value();
}
static inline ae_status_result ae_try_rename_path_value(ae_str from, ae_str to) {
    errno = 0;
    if (rename(from.ptr, to.ptr) != 0) return ae_status_fail_value(ae_io_error());
    return ae_status_ok_value();
}
static inline ae_status_result ae_try_remove_file_value(ae_str path) {
    if (ae_path_is_dir_value(path)) {
        return ae_status_fail_value(ae_str_alloc_copy(
            "cannot remove a directory with try_remove_file",
            (int64_t)strlen("cannot remove a directory with try_remove_file")
        ));
    }
    errno = 0;
    if (remove(path.ptr) != 0) return ae_status_fail_value(ae_io_error());
    return ae_status_ok_value();
}
static inline ae_status_result ae_try_remove_dir_value(ae_str path) {
    errno = 0;
#ifdef _WIN32
    if (_rmdir(path.ptr) != 0) return ae_status_fail_value(ae_io_error());
#else
    if (rmdir(path.ptr) != 0) return ae_status_fail_value(ae_io_error());
#endif
    return ae_status_ok_value();
}
static inline ae_read_result ae_try_env_value(ae_str name) {
    ae_read_result out;
    out.status = 0;
    out.text = ae_str_from_parts("", 0, false);
    out.error = ae_str_from_parts("", 0, false);
    const char *value = getenv(name.ptr);
    if (!value) {
        out.status = 1;
        out.error = ae_str_alloc_copy("variable not set", (int64_t)strlen("variable not set"));
        return out;
    }
    out.text = ae_str_alloc_copy(value, (int64_t)strlen(value));
    return out;
}
static inline ae_read_result ae_try_cwd_value(void) {
    ae_read_result out;
    out.status = 0;
    out.error = ae_str_from_parts("", 0, false);
    size_t capacity = 256;
    char *buffer = (char *)malloc(capacity);
    if (!buffer) ae_trap("out of memory");
    for (;;) {
#ifdef _WIN32
        const char *got = _getcwd(buffer, (int)capacity);
#else
        const char *got = getcwd(buffer, capacity);
#endif
        if (got) break;
        if (errno != ERANGE || capacity > ((size_t)-1) / 2) {
            free(buffer);
            out.status = 1;
            out.text = ae_str_from_parts("", 0, false);
            out.error = ae_io_error();
            return out;
        }
        capacity *= 2;
        char *grown = (char *)realloc(buffer, capacity);
        if (!grown) {
            free(buffer);
            ae_trap("out of memory");
        }
        buffer = grown;
    }
    const int64_t length = (int64_t)strlen(buffer);
    for (int64_t i = 0; i < length; i++) {
        if (buffer[i] == '\\') buffer[i] = '/';
    }
    out.text = ae_str_from_parts(buffer, length, true);
    return out;
}
static inline int64_t ae_now_s(void) { return (int64_t)time(NULL); }
static inline int64_t ae_now_ms(void) {
#ifdef _WIN32
    struct { unsigned long low; unsigned long high; } file_time;
    extern void GetSystemTimeAsFileTime(void *);
    GetSystemTimeAsFileTime(&file_time);
    const uint64_t ticks = ((uint64_t)file_time.high << 32) | (uint64_t)file_time.low;
    return (int64_t)(ticks / UINT64_C(10000) - UINT64_C(11644473600000));
#else
    struct timespec ts;
    timespec_get(&ts, TIME_UTC);
    return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
#endif
}
static inline ae_status_result ae_try_create_dir_value(ae_str path) {
    if (path.len == 0) {
        return ae_status_fail_value(ae_str_alloc_copy("empty path", (int64_t)strlen("empty path")));
    }
    errno = 0;
    char *buffer = ae_str_alloc(path.len);
    memcpy(buffer, path.ptr, (size_t)path.len);
    buffer[path.len] = 0;
    ae_status_result out = ae_status_ok_value();
    for (int64_t at = 1; at <= path.len; at++) {
        if (at < path.len && !ae_is_separator(buffer[at])) continue;
        const char saved = buffer[at];
        buffer[at] = 0;
#ifdef _WIN32
        const bool skip = (at == 2 && buffer[1] == ':');
#else
        const bool skip = false;
#endif
        if (!skip) {
            if (!ae_path_is_dir_value(ae_str_from_parts(buffer, at, false))) {
#ifdef _WIN32
                if (_mkdir(buffer) != 0) {
#else
                if (mkdir(buffer, 0777) != 0) {
#endif
                    if (errno != EEXIST) {
                        out = ae_status_fail_value(ae_io_error());
                        buffer[at] = saved;
                        break;
                    }
                }
            }
        }
        buffer[at] = saved;
    }
    free(buffer);
    return out;
}
static inline ae_status_result ae_try_set_cwd_value(ae_str path) {
    errno = 0;
#ifdef _WIN32
    if (_chdir(path.ptr) != 0) return ae_status_fail_value(ae_io_error());
#else
    if (chdir(path.ptr) != 0) return ae_status_fail_value(ae_io_error());
#endif
    return ae_status_ok_value();
}

// ---- Hash maps: string keys, i64-bit values --------------------------------
// Open addressing with linear probing. The value slot stores i64 bits, so the
// same table serves map[string, i64] and map[string, f64] (the emitter
// bit-casts the value); a parallel state array keeps slots packed. A deleted
// slot keeps probing alive until the next rehash.
typedef struct { ae_str key; int64_t value; } ae_map_slot;
typedef struct {
    ae_map_slot *slots;
    unsigned char *states;
    int64_t cap;
    int64_t len;
    int64_t used;
} ae_map;
#define AE_MAP_EMPTY 0
#define AE_MAP_FULL 1
#define AE_MAP_DELETED 2
static inline ae_map ae_map_new(void) {
    ae_map m;
    m.slots = NULL;
    m.states = NULL;
    m.cap = 0;
    m.len = 0;
    m.used = 0;
    return m;
}
static inline uint64_t ae_map_hash(ae_str key) {
    uint64_t hash = UINT64_C(1469598103934665603);
    for (int64_t i = 0; i < key.len; i++) {
        hash ^= (uint64_t)(unsigned char)key.ptr[i];
        hash *= UINT64_C(1099511628211);
    }
    return hash;
}
static inline int64_t ae_map_slot_of(ae_str key, int64_t cap) {
    return (int64_t)(ae_map_hash(key) & (uint64_t)(cap - 1));
}
static inline int64_t ae_map_find(const ae_map *m, ae_str key) {
    if (m->cap == 0) return -1;
    int64_t at = ae_map_slot_of(key, m->cap);
    for (int64_t probes = 0; probes < m->cap; probes++) {
        const unsigned char state = m->states[at];
        if (state == AE_MAP_EMPTY) return -1;
        if (state == AE_MAP_FULL && ae_str_eq(m->slots[at].key, key)) return at;
        at = (at + 1) & (m->cap - 1);
    }
    return -1;
}
static inline void ae_map_grow(ae_map *m) {
    int64_t cap = m->cap == 0 ? 8 : m->cap;
    while ((m->len + 1) * 2 >= cap) {
        if (cap > INT64_MAX / 4) ae_trap("map too large");
        cap = cap * 2;
    }
    if (cap == m->cap && m->used == m->len) return;
    ae_map_slot *slots = (ae_map_slot *)malloc((size_t)cap * sizeof(ae_map_slot));
    unsigned char *states = (unsigned char *)malloc((size_t)cap);
    if (!slots || !states) ae_trap("out of memory");
    memset(states, AE_MAP_EMPTY, (size_t)cap);
    for (int64_t i = 0; i < m->cap; i++) {
        if (m->states[i] != AE_MAP_FULL) continue;
        int64_t at = ae_map_slot_of(m->slots[i].key, cap);
        while (states[at] == AE_MAP_FULL) at = (at + 1) & (cap - 1);
        slots[at] = m->slots[i];
        states[at] = AE_MAP_FULL;
    }
    free(m->slots);
    free(m->states);
    m->slots = slots;
    m->states = states;
    m->cap = cap;
    m->used = m->len;
}
static inline void ae_map_put(ae_map *m, ae_str key, int64_t value) {
    ae_map_grow(m);
    int64_t at = ae_map_slot_of(key, m->cap);
    int64_t reuse = -1;
    for (;;) {
        const unsigned char state = m->states[at];
        if (state == AE_MAP_EMPTY) break;
        if (state == AE_MAP_DELETED) {
            if (reuse < 0) reuse = at;
        } else if (ae_str_eq(m->slots[at].key, key)) {
            m->slots[at].value = value;
            return;
        }
        at = (at + 1) & (m->cap - 1);
    }
    if (reuse >= 0) at = reuse;
    else m->used += 1;
    m->slots[at].key = ae_str_alloc_copy(key.ptr, key.len);
    m->slots[at].value = value;
    m->states[at] = AE_MAP_FULL;
    m->len += 1;
}
static inline int64_t ae_map_inc(ae_map *m, ae_str key, int64_t delta) {
    ae_map_grow(m);
    int64_t at = ae_map_slot_of(key, m->cap);
    int64_t reuse = -1;
    for (;;) {
        const unsigned char state = m->states[at];
        if (state == AE_MAP_EMPTY) break;
        if (state == AE_MAP_DELETED) {
            if (reuse < 0) reuse = at;
        } else if (ae_str_eq(m->slots[at].key, key)) {
            m->slots[at].value += delta;
            return m->slots[at].value;
        }
        at = (at + 1) & (m->cap - 1);
    }
    if (reuse >= 0) at = reuse;
    else m->used += 1;
    m->slots[at].key = ae_str_alloc_copy(key.ptr, key.len);
    m->slots[at].value = delta;
    m->states[at] = AE_MAP_FULL;
    m->len += 1;
    return delta;
}
static inline double ae_map_inc_f64(ae_map *m, ae_str key, double delta) {
    ae_map_grow(m);
    int64_t at = ae_map_slot_of(key, m->cap);
    int64_t reuse = -1;
    for (;;) {
        const unsigned char state = m->states[at];
        if (state == AE_MAP_EMPTY) break;
        if (state == AE_MAP_DELETED) {
            if (reuse < 0) reuse = at;
        } else if (ae_str_eq(m->slots[at].key, key)) {
            double current;
            memcpy(&current, &m->slots[at].value, sizeof current);
            current += delta;
            memcpy(&m->slots[at].value, &current, sizeof current);
            return current;
        }
        at = (at + 1) & (m->cap - 1);
    }
    if (reuse >= 0) at = reuse;
    else m->used += 1;
    m->slots[at].key = ae_str_alloc_copy(key.ptr, key.len);
    memcpy(&m->slots[at].value, &delta, sizeof delta);
    m->states[at] = AE_MAP_FULL;
    m->len += 1;
    return delta;
}
static inline bool ae_map_lookup(const ae_map *m, ae_str key, int64_t *out) {
    const int64_t at = ae_map_find(m, key);
    if (at < 0) return false;
    if (out) *out = m->slots[at].value;
    return true;
}
static inline bool ae_map_has(const ae_map *m, ae_str key) {
    return ae_map_find(m, key) >= 0;
}
static inline bool ae_map_remove(ae_map *m, ae_str key) {
    const int64_t at = ae_map_find(m, key);
    if (at < 0) return false;
    ae_str_release(&m->slots[at].key);
    m->slots[at].value = 0;
    m->states[at] = AE_MAP_DELETED;
    m->len -= 1;
    return true;
}
static inline int64_t ae_map_len(const ae_map *m) { return m->len; }
static inline int ae_map_cmp_key(const void *a, const void *b) {
    const ae_str x = *(const ae_str *)a;
    const ae_str y = *(const ae_str *)b;
    const int64_t common = x.len < y.len ? x.len : y.len;
    const int order = common > 0 ? memcmp(x.ptr, y.ptr, (size_t)common) : 0;
    if (order != 0) return order;
    return (x.len > y.len) - (x.len < y.len);
}
static inline ae_vec ae_map_keys(const ae_map *m) {
    ae_vec out = ae_vec_new((int64_t)sizeof(ae_str), ae_str_release_elem);
    for (int64_t i = 0; i < m->cap; i++) {
        if (m->states[i] != AE_MAP_FULL) continue;
        ae_str key = ae_str_alloc_copy(m->slots[i].key.ptr, m->slots[i].key.len);
        ae_vec_push(&out, &key);
    }
    // keys() is byte-order sorted, like the main runtime.
    if (out.len > 1) qsort(out.data, (size_t)out.len, sizeof(ae_str), ae_map_cmp_key);
    return out;
}
static inline void ae_map_release(ae_map *m) {
    for (int64_t i = 0; i < m->cap; i++) {
        if (m->states[i] == AE_MAP_FULL) ae_str_release(&m->slots[i].key);
    }
    free(m->slots);
    free(m->states);
    m->slots = NULL;
    m->states = NULL;
    m->cap = 0;
    m->len = 0;
    m->used = 0;
}
static inline ae_map ae_map_take(ae_map *m) {
    ae_map out = *m;
    m->slots = NULL;
    m->states = NULL;
    m->cap = 0;
    m->len = 0;
    m->used = 0;
    return out;
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

// to_i64: status 0 = value, 1 = not finite, 2 = outside the i64 range,
// 3 = not an exact integer. The emitter turns the status into IntResult's
// Ok/Err payload.
typedef struct { int64_t status; int64_t value; } ae_f64_to_i64_result;
static inline ae_f64_to_i64_result ae_f64_to_i64_checked(double value) {
    ae_f64_to_i64_result out;
    if (!isfinite(value)) {
        out.status = 1;
        out.value = 0;
        return out;
    }
    if (value < -9223372036854775808.0 || value >= 9223372036854775808.0) {
        out.status = 2;
        out.value = 0;
        return out;
    }
    const int64_t converted = (int64_t)value;
    if ((double)converted != value) {
        out.status = 3;
        out.value = 0;
        return out;
    }
    out.status = 0;
    out.value = converted;
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

// JSON reader (ADR-0238): a path query over a string, with the kind checks
// and distinct "missing", "wrong shape" and "syntax" errors the language
// reports as Result payloads. There is no container type to own or release.
typedef struct { ae_str f0; } ae_json_text;
typedef struct { int64_t f0; } ae_json_number;
typedef struct { bool f0; } ae_json_flag;
typedef struct { int64_t status; union { ae_json_text v0; ae_json_text v1; } u; } ae_json_read_result;
typedef struct { int64_t status; union { ae_json_number v0; ae_json_text v1; } u; } ae_json_int_result;
typedef struct { int64_t status; union { ae_json_flag v0; ae_json_text v1; } u; } ae_json_bool_result;
static inline ae_str ae_json_error(const char *text) { return ae_str_alloc_copy(text, (int64_t)strlen(text)); }

// A path query, not a document model: one call selects one value and reports it
// as text, an integer or a boolean. The document is rescanned per call, which is
// the right trade for config-sized input and keeps JSON out of the type system:
// there is no container type to own, borrow or release.
#define AE_JSON_OK 0
#define AE_JSON_SYNTAX 1
#define AE_JSON_MISSING 2
#define AE_JSON_EXPECTED 3
#define AE_JSON_DEPTH 64
#define AE_JSON_STRING 0
#define AE_JSON_NUMBER 1
#define AE_JSON_BOOL 2
#define AE_JSON_NULL 3
#define AE_JSON_ARRAY 4
#define AE_JSON_OBJECT 5

static inline void ae_json_ws(const char *t, int64_t n, int64_t *i) {
    while (*i < n) {
        const char c = t[*i];
        if (c != ' ' && c != '\t' && c != '\n' && c != '\r') break;
        *i += 1;
    }
}
static inline int ae_json_hex(char c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}
static inline int ae_json_utf8(char *out, unsigned cp) {
    if (cp < 0x80u) { out[0] = (char)cp; return 1; }
    if (cp < 0x800u) { out[0] = (char)(0xC0u | (cp >> 6)); out[1] = (char)(0x80u | (cp & 0x3Fu)); return 2; }
    if (cp < 0x10000u) { out[0] = (char)(0xE0u | (cp >> 12)); out[1] = (char)(0x80u | ((cp >> 6) & 0x3Fu)); out[2] = (char)(0x80u | (cp & 0x3Fu)); return 3; }
    out[0] = (char)(0xF0u | (cp >> 18)); out[1] = (char)(0x80u | ((cp >> 12) & 0x3Fu)); out[2] = (char)(0x80u | ((cp >> 6) & 0x3Fu)); out[3] = (char)(0x80u | (cp & 0x3Fu)); return 4;
}
static inline int ae_json_skip_string(const char *t, int64_t n, int64_t *i) {
    if (*i >= n || t[*i] != '"') return 0;
    int64_t j = *i + 1;
    while (j < n) {
        const unsigned char c = (unsigned char)t[j];
        if (c == '"') { *i = j + 1; return 1; }
        if (c == '\\') { j += 2; continue; }
        if (c < 0x20u) return 0;
        j += 1;
    }
    return 0;
}
static inline int ae_json_skip_value(const char *t, int64_t n, int64_t *i, int depth) {
    if (depth > AE_JSON_DEPTH) return 0;
    ae_json_ws(t, n, i);
    if (*i >= n) return 0;
    const char c = t[*i];
    if (c == '"') return ae_json_skip_string(t, n, i);
    if (c == '{' || c == '[') {
        const char close = c == '{' ? '}' : ']';
        *i += 1;
        ae_json_ws(t, n, i);
        if (*i < n && t[*i] == close) { *i += 1; return 1; }
        for (;;) {
            ae_json_ws(t, n, i);
            if (c == '{') {
                if (!ae_json_skip_string(t, n, i)) return 0;
                ae_json_ws(t, n, i);
                if (*i >= n || t[*i] != ':') return 0;
                *i += 1;
            }
            if (!ae_json_skip_value(t, n, i, depth + 1)) return 0;
            ae_json_ws(t, n, i);
            if (*i >= n) return 0;
            if (t[*i] == ',') { *i += 1; continue; }
            if (t[*i] == close) { *i += 1; return 1; }
            return 0;
        }
    }
    // A bare token: number, true, false or null. Its end is punctuation or space.
    const int64_t start = *i;
    while (*i < n) {
        const char d = t[*i];
        if (d == ',' || d == '}' || d == ']' || d == ' ' || d == '\t' || d == '\r' || d == '\n') break;
        *i += 1;
    }
    return *i > start;
}
// Decodes one JSON string at *i into an owned string, resolving escapes (and
// surrogate pairs) so the language never sees a \uXXXX sequence.
static inline int ae_json_decode(const char *t, int64_t n, int64_t *i, ae_str *out, size_t line, size_t column) {
    if (*i >= n || t[*i] != '"') return 0;
    int64_t j = *i + 1;
    while (j < n) {
        const unsigned char c = (unsigned char)t[j];
        if (c == '"') break;
        if (c == '\\') { j += 2; continue; }
        if (c < 0x20u) return 0;
        j += 1;
    }
    if (j >= n) return 0;
    const int64_t raw = j - (*i + 1);
    char *buffer = (char *)ae_std_alloc((int64_t)((size_t)raw + 1), (int64_t)line, (int64_t)column);
    int64_t s = *i + 1;
    int64_t k = 0;
    while (s < j) {
        const unsigned char c = (unsigned char)t[s];
        if (c != '\\') { buffer[k] = (char)c; k += 1; s += 1; continue; }
        s += 1;
        if (s >= j) { free(buffer); return 0; }
        const char e = t[s];
        s += 1;
        switch (e) {
            case '"': buffer[k] = '"'; k += 1; break;
            case '\\': buffer[k] = '\\'; k += 1; break;
            case '/': buffer[k] = '/'; k += 1; break;
            case 'b': buffer[k] = '\b'; k += 1; break;
            case 'f': buffer[k] = '\f'; k += 1; break;
            case 'n': buffer[k] = '\n'; k += 1; break;
            case 'r': buffer[k] = '\r'; k += 1; break;
            case 't': buffer[k] = '\t'; k += 1; break;
            case 'u': {
                unsigned cp = 0;
                if (s + 4 > j) { free(buffer); return 0; }
                for (int d = 0; d < 4; d++) {
                    const int v = ae_json_hex(t[s + d]);
                    if (v < 0) { free(buffer); return 0; }
                    cp = (cp << 4) | (unsigned)v;
                }
                s += 4;
                if (cp >= 0xD800u && cp <= 0xDBFFu && s + 6 <= j && t[s] == '\\' && t[s + 1] == 'u') {
                    unsigned low = 0;
                    int ok = 1;
                    for (int d = 0; d < 4; d++) {
                        const int v = ae_json_hex(t[s + 2 + d]);
                        if (v < 0) { ok = 0; break; }
                        low = (low << 4) | (unsigned)v;
                    }
                    if (ok && low >= 0xDC00u && low <= 0xDFFFu) {
                        cp = 0x10000u + ((cp - 0xD800u) << 10) + (low - 0xDC00u);
                        s += 6;
                    }
                }
                // A NUL would end the string early for every C consumer.
                if (cp == 0) { free(buffer); return 0; }
                k += ae_json_utf8(buffer + k, cp);
                break;
            }
            default: free(buffer); return 0;
        }
    }
    buffer[k] = 0;
    out->ptr = buffer;
    out->len = k;
    out->owned = true;
    *i = j + 1;
    return 1;
}
static inline int ae_json_kind(const char *t, int64_t n, int64_t start) {
    if (start >= n) return -1;
    const char c = t[start];
    if (c == '"') return AE_JSON_STRING;
    if (c == '{') return AE_JSON_OBJECT;
    if (c == '[') return AE_JSON_ARRAY;
    if (c == 't' || c == 'f') return AE_JSON_BOOL;
    if (c == 'n') return AE_JSON_NULL;
    return AE_JSON_NUMBER;
}
static const char *ae_json_kind_name(int kind) {
    switch (kind) {
        case AE_JSON_STRING: return "a string";
        case AE_JSON_NUMBER: return "a number";
        case AE_JSON_BOOL: return "a boolean";
        case AE_JSON_NULL: return "null";
        case AE_JSON_ARRAY: return "an array";
        case AE_JSON_OBJECT: return "an object";
        default: return "nothing";
    }
}
// Locates one member inside the object at *pos, leaving its value span in
// *vs/*ve. *found separates "no such member" from "not an object at all".
static inline int ae_json_member(const char *t, int64_t n, int64_t *pos, const char *key, int64_t key_len, int64_t *vs, int64_t *ve, int *found, size_t line, size_t column) {
    *found = 0;
    int64_t i = *pos;
    if (i >= n || t[i] != '{') return 0;
    i += 1;
    ae_json_ws(t, n, &i);
    if (i < n && t[i] == '}') { *pos = i + 1; return 1; }
    for (;;) {
        ae_json_ws(t, n, &i);
        if (i >= n || t[i] != '"') return 0;
        // Fast path: unescaped keys can be matched directly without heap allocation.
        int64_t j = i + 1;
        int has_escape = 0;
        while (j < n) {
            const unsigned char ch = (unsigned char)t[j];
            if (ch == '"') break;
            if (ch == '\\') { has_escape = 1; break; }
            if (ch < 0x20u) return 0;
            j += 1;
        }
        int match = 0;
        if (!has_escape) {
            if (j >= n) return 0;
            const int64_t raw_len = j - (i + 1);
            match = raw_len == key_len && (key_len == 0 || memcmp(t + i + 1, key, (size_t)key_len) == 0);
            i = j + 1;
        } else {
            ae_str name;
            if (!ae_json_decode(t, n, &i, &name, line, column)) return 0;
            match = name.len == key_len && memcmp(name.ptr, key, (size_t)key_len) == 0;
            ae_str_release(&name);
        }
        ae_json_ws(t, n, &i);
        if (i >= n || t[i] != ':') return 0;
        i += 1;
        ae_json_ws(t, n, &i);
        const int64_t start = i;
        if (!ae_json_skip_value(t, n, &i, 0)) return 0;
        if (match) { *vs = start; *ve = i; *found = 1; return 1; }
        ae_json_ws(t, n, &i);
        if (i >= n) return 0;
        if (t[i] == ',') { i += 1; continue; }
        if (t[i] == '}') { *pos = i + 1; return 1; }
        return 0;
    }
}
static inline int ae_json_element(const char *t, int64_t n, int64_t *pos, int64_t index, int64_t *vs, int64_t *ve, int *found) {
    *found = 0;
    int64_t i = *pos;
    if (i >= n || t[i] != '[') return 0;
    i += 1;
    ae_json_ws(t, n, &i);
    if (i < n && t[i] == ']') { *pos = i + 1; return 1; }
    int64_t k = 0;
    for (;;) {
        ae_json_ws(t, n, &i);
        const int64_t start = i;
        if (!ae_json_skip_value(t, n, &i, 0)) return 0;
        if (k == index) { *vs = start; *ve = i; *found = 1; return 1; }
        k += 1;
        ae_json_ws(t, n, &i);
        if (i >= n) return 0;
        if (t[i] == ',') { i += 1; continue; }
        if (t[i] == ']') { *pos = i + 1; return 1; }
        return 0;
    }
}
// Validates the path on its own first, so a typo in the path is reported as a
// path error even when the document would have failed on the same segment.
static inline int ae_json_path_check(const char *path, int64_t plen, char *why, size_t why_cap) {
    int64_t p = 0;
    while (p < plen) {
        if (path[p] == '[') {
            const int64_t open = p;
            p += 1;
            int digits = 0;
            while (p < plen && path[p] >= '0' && path[p] <= '9') { p += 1; digits += 1; }
            if (digits == 0 || p >= plen || path[p] != ']') {
                snprintf(why, why_cap, "path index at byte %lld needs digits and a closing ']'", (long long)open);
                return 0;
            }
            p += 1;
            if (p < plen && path[p] == '.') {
                p += 1;
                if (p >= plen) { snprintf(why, why_cap, "path ends with '.'"); return 0; }
            }
            continue;
        }
        const int64_t kstart = p;
        while (p < plen && path[p] != '.' && path[p] != '[') p += 1;
        if (p == kstart) {
            snprintf(why, why_cap, "path has an empty segment at byte %lld", (long long)kstart);
            return 0;
        }
        if (p < plen && path[p] == '.') {
            p += 1;
            if (p >= plen) { snprintf(why, why_cap, "path ends with '.'"); return 0; }
        }
    }
    return 1;
}
// Path grammar: `key`, `key.sub`, `key[2].sub`, `[0].key`. A segment is a run of
// bytes up to the next '.' or '['; keys are compared after unescaping.
static inline int ae_json_locate(const char *t, int64_t n, const char *path, int64_t plen, int64_t *vs, int64_t *ve, int *kind, char *why, size_t why_cap, size_t line, size_t column) {
    if (!ae_json_path_check(path, plen, why, why_cap)) return AE_JSON_SYNTAX;
    int64_t pos = 0;
    ae_json_ws(t, n, &pos);
    const int64_t start0 = pos;
    if (!ae_json_skip_value(t, n, &pos, 0)) {
        snprintf(why, why_cap, "JSON syntax error near byte %lld", (long long)start0);
        return AE_JSON_SYNTAX;
    }
    int64_t start = start0;
    int64_t end = pos;
    int current = ae_json_kind(t, n, start);
    int64_t p = 0;
    while (p < plen) {
        if (path[p] == '[') {
            const int64_t open = p;
            p += 1;
            int64_t index = 0;
            int digits = 0;
            while (p < plen && path[p] >= '0' && path[p] <= '9' && digits < 10) { index = index * 10 + (path[p] - '0'); p += 1; digits += 1; }
            if (digits == 0 || p >= plen || path[p] != ']') {
                snprintf(why, why_cap, "path index at byte %lld needs digits and a closing ']'", (long long)open);
                return AE_JSON_SYNTAX;
            }
            p += 1;
            if (current != AE_JSON_ARRAY) {
                snprintf(why, why_cap, "path segment [%lld] needs an array, found %s", (long long)index, ae_json_kind_name(current));
                return AE_JSON_EXPECTED;
            }
            int64_t cursor = start;
            int found = 0;
            if (!ae_json_element(t, n, &cursor, index, &start, &end, &found)) {
                snprintf(why, why_cap, "JSON syntax error while reading an array element");
                return AE_JSON_SYNTAX;
            }
            if (!found) {
                snprintf(why, why_cap, "path segment [%lld] is past the end of the array", (long long)index);
                return AE_JSON_MISSING;
            }
        } else {
            const int64_t kstart = p;
            while (p < plen && path[p] != '.' && path[p] != '[') p += 1;
            const int64_t klen = p - kstart;
            if (klen == 0) {
                snprintf(why, why_cap, "path has an empty segment at byte %lld", (long long)kstart);
                return AE_JSON_SYNTAX;
            }
            if (current != AE_JSON_OBJECT) {
                snprintf(why, why_cap, "path segment '%.*s' needs an object, found %s", (int)klen, path + kstart, ae_json_kind_name(current));
                return AE_JSON_EXPECTED;
            }
            int64_t cursor = start;
            int found = 0;
            if (!ae_json_member(t, n, &cursor, path + kstart, klen, &start, &end, &found, line, column)) {
                snprintf(why, why_cap, "JSON syntax error while reading a member");
                return AE_JSON_SYNTAX;
            }
            if (!found) {
                snprintf(why, why_cap, "no member named '%.*s'", (int)klen, path + kstart);
                return AE_JSON_MISSING;
            }
        }
        current = ae_json_kind(t, n, start);
        if (p < plen) {
            if (path[p] != '.' && path[p] != '[') {
                snprintf(why, why_cap, "path segment ends unexpectedly at byte %lld", (long long)p);
                return AE_JSON_SYNTAX;
            }
            if (path[p] == '.') {
                p += 1;
                if (p >= plen) {
                    snprintf(why, why_cap, "path ends with '.'");
                    return AE_JSON_SYNTAX;
                }
            }
        }
    }
    *vs = start;
    *ve = end;
    *kind = current;
    return AE_JSON_OK;
}
static inline int ae_json_count(const char *t, int64_t n, int64_t start, int kind) {
    int64_t i = start + 1;
    int64_t count = 0;
    const char close = kind == AE_JSON_OBJECT ? '}' : ']';
    ae_json_ws(t, n, &i);
    if (i < n && t[i] == close) return 0;
    for (;;) {
        ae_json_ws(t, n, &i);
        if (kind == AE_JSON_OBJECT) {
            if (!ae_json_skip_string(t, n, &i)) return -1;
            ae_json_ws(t, n, &i);
            if (i >= n || t[i] != ':') return -1;
            i += 1;
        }
        if (!ae_json_skip_value(t, n, &i, 0)) return -1;
        count += 1;
        ae_json_ws(t, n, &i);
        if (i >= n) return -1;
        if (t[i] == ',') { i += 1; continue; }
        if (t[i] == close) return count;
        return -1;
    }
}
static inline ae_json_read_result ae_json_get(ae_str doc, ae_str path, size_t line, size_t column) {

    int64_t vs = 0;
    int64_t ve = 0;
    int kind = 0;
    char why[192];
    if (ae_json_locate(doc.ptr, doc.len, path.ptr, path.len, &vs, &ve, &kind, why, sizeof why, line, column) != AE_JSON_OK) {
        ae_json_read_result out = { 1, { .v1 = { ae_json_error(why) } } };
        return out;
    }
    ae_str value;
    if (kind == AE_JSON_STRING) {
        int64_t at = vs;
        if (!ae_json_decode(doc.ptr, doc.len, &at, &value, line, column)) {
            ae_json_read_result out = { 1, { .v1 = { ae_json_error("JSON string could not be decoded") } } };
            return out;
        }
    } else {
        value = ae_str_alloc_copy(doc.ptr + vs, ve - vs);
    }
    ae_json_read_result out = { 0, { .v0 = { value } } };
    return out;
}
static inline ae_json_int_result ae_json_int(ae_str doc, ae_str path, size_t line, size_t column) {

    int64_t vs = 0;
    int64_t ve = 0;
    int kind = 0;
    char why[192];
    if (ae_json_locate(doc.ptr, doc.len, path.ptr, path.len, &vs, &ve, &kind, why, sizeof why, line, column) != AE_JSON_OK) {
        ae_json_int_result out = { 1, { .v1 = { ae_json_error(why) } } };
        return out;
    }
    if (kind != AE_JSON_NUMBER) {
        snprintf(why, sizeof why, "'%.*s' is %s, not a number", (int)path.len, path.ptr, ae_json_kind_name(kind));
        ae_json_int_result out = { 1, { .v1 = { ae_json_error(why) } } };
        return out;
    }
    int64_t i = vs;
    int negative = 0;
    if (i < ve && doc.ptr[i] == '-') { negative = 1; i += 1; }
    if (i >= ve) {
        snprintf(why, sizeof why, "'%.*s' is not an integer", (int)path.len, path.ptr);
        ae_json_int_result out = { 1, { .v1 = { ae_json_error(why) } } };
        return out;
    }
    int64_t value = 0;
    for (; i < ve; i++) {
        const char d = doc.ptr[i];
        if (d < '0' || d > '9') {
            // A fraction or exponent is a real number; this reader has no float type.
            snprintf(why, sizeof why, "'%.*s' is not an integer", (int)path.len, path.ptr);
            ae_json_int_result out = { 1, { .v1 = { ae_json_error(why) } } };
            return out;
        }
        const int digit = d - '0';
        if (value > (INT64_MAX - digit) / 10) {
            snprintf(why, sizeof why, "'%.*s' does not fit in i64", (int)path.len, path.ptr);
            ae_json_int_result out = { 1, { .v1 = { ae_json_error(why) } } };
            return out;
        }
        value = value * 10 + digit;
    }
    ae_json_int_result out = { 0, { .v0 = { negative ? -value : value } } };
    return out;
}
static inline ae_json_bool_result ae_json_bool(ae_str doc, ae_str path, size_t line, size_t column) {

    int64_t vs = 0;
    int64_t ve = 0;
    int kind = 0;
    char why[192];
    if (ae_json_locate(doc.ptr, doc.len, path.ptr, path.len, &vs, &ve, &kind, why, sizeof why, line, column) != AE_JSON_OK) {
        ae_json_bool_result out = { 1, { .v1 = { ae_json_error(why) } } };
        return out;
    }
    const int64_t width = ve - vs;
    bool value = false;
    if (kind == AE_JSON_BOOL && width == 4 && memcmp(doc.ptr + vs, "true", 4) == 0) value = true;
    else if (!(kind == AE_JSON_BOOL && width == 5 && memcmp(doc.ptr + vs, "false", 5) == 0)) {
        snprintf(why, sizeof why, "'%.*s' is %s, not a boolean", (int)path.len, path.ptr, ae_json_kind_name(kind));
        ae_json_bool_result out = { 1, { .v1 = { ae_json_error(why) } } };
        return out;
    }
    ae_json_bool_result out = { 0, { .v0 = { value } } };
    return out;
}
static inline ae_json_int_result ae_json_len(ae_str doc, ae_str path, size_t line, size_t column) {

    int64_t vs = 0;
    int64_t ve = 0;
    int kind = 0;
    char why[192];
    if (ae_json_locate(doc.ptr, doc.len, path.ptr, path.len, &vs, &ve, &kind, why, sizeof why, line, column) != AE_JSON_OK) {
        ae_json_int_result out = { 1, { .v1 = { ae_json_error(why) } } };
        return out;
    }
    if (kind != AE_JSON_ARRAY && kind != AE_JSON_OBJECT) {
        snprintf(why, sizeof why, "'%.*s' is %s; only arrays and objects have a member count", (int)path.len, path.ptr, ae_json_kind_name(kind));
        ae_json_int_result out = { 1, { .v1 = { ae_json_error(why) } } };
        return out;
    }
    const int64_t count = ae_json_count(doc.ptr, doc.len, vs, kind);
    if (count < 0) {
        ae_json_int_result out = { 1, { .v1 = { ae_json_error("JSON syntax error while counting members") } } };
        return out;
    }
    ae_json_int_result out = { 0, { .v0 = { count } } };
    return out;
}
static inline bool ae_json_has(ae_str doc, ae_str path, size_t line, size_t column) {

    int64_t vs = 0;
    int64_t ve = 0;
    int kind = 0;
    char why[192];
    return ae_json_locate(doc.ptr, doc.len, path.ptr, path.len, &vs, &ve, &kind, why, sizeof why, line, column) == AE_JSON_OK;
}


// Child processes (ADR-0238): stdout and stderr share one pipe, so the
// child's own write order survives; a second CLOEXEC pipe tells "could not
// start the program" apart from a child that exits 127. A non-zero exit
// status is a result, not an error.
typedef struct { int64_t status; int64_t code; ae_str output; ae_str error; } ae_command_result;
static inline ae_command_result ae_command_ok_value(ae_str output, int64_t code) {
    ae_command_result out;
    out.status = 0;
    out.code = code;
    out.output = output;
    out.error = ae_str_from_parts("", 0, false);
    return out;
}
static inline ae_command_result ae_command_fail_value(ae_str error) {
    ae_command_result out;
    out.status = 1;
    out.code = 0;
    out.output = ae_str_from_parts("", 0, false);
    out.error = error;
    return out;
}
static inline ae_str ae_command_literal(const char *text) {
    return ae_str_alloc_copy(text, (int64_t)strlen(text));
}
static inline ae_str ae_command_status_error(const char *what, unsigned long code) {
    char buffer[160];
    const int length = snprintf(buffer, sizeof buffer, "%s (status %lu)", what, code);
    if (length < 0) return ae_command_literal(what);
    const int64_t used = length < (int)sizeof buffer ? (int64_t)length : (int64_t)sizeof buffer - 1;
    return ae_str_alloc_copy(buffer, used);
}
#ifndef _WIN32
static inline ae_command_result ae_run_command_value(ae_str program, ae_vec *args, size_t line, size_t column) {
    int fds[2];
    if (pipe(fds) != 0) return ae_command_fail_value(ae_io_error());
    int errors[2];
    if (pipe(errors) != 0) {
        const ae_str failure = ae_io_error();
        close(fds[0]);
        close(fds[1]);
        return ae_command_fail_value(failure);
    }
    (void)fcntl(errors[1], F_SETFD, FD_CLOEXEC);
    const pid_t pid = fork();
    if (pid < 0) {
        const ae_str failure = ae_io_error();
        close(fds[0]);
        close(fds[1]);
        close(errors[0]);
        close(errors[1]);
        return ae_command_fail_value(failure);
    }
    if (pid == 0) {
        if (dup2(fds[1], STDOUT_FILENO) < 0 || dup2(fds[1], STDERR_FILENO) < 0) _exit(127);
        close(fds[0]);
        close(fds[1]);
        close(errors[0]);
        char **argv = (char **)malloc(sizeof(char *) * (size_t)(args->len + 2));
        if (!argv) _exit(127);
        argv[0] = (char *)program.ptr;
        for (int64_t i = 0; i < args->len; i++) {
            argv[i + 1] = (char *)((ae_str *)ae_vec_at(args, i, line, column))->ptr;
        }
        argv[args->len + 1] = NULL;
        execvp(program.ptr, argv);
        const int code = errno;
        (void)!write(errors[1], &code, sizeof code);
        _exit(127);
    }
    close(fds[1]);
    close(errors[1]);
    int exec_error = 0;
    ssize_t reported = 0;
    do {
        reported = read(errors[0], &exec_error, sizeof exec_error);
    } while (reported < 0 && errno == EINTR);
    close(errors[0]);
    if (reported == (ssize_t)sizeof exec_error) {
        int ignored = 0;
        close(fds[0]);
        while (waitpid(pid, &ignored, 0) < 0 && errno == EINTR) {}
        return ae_command_fail_value(ae_command_status_error("cannot start the program", (unsigned long)exec_error));
    }
    int64_t capacity = 4096;
    int64_t length = 0;
    char *buffer = (char *)ae_std_alloc(capacity, (int64_t)line, (int64_t)column);
    for (;;) {
        if (length == capacity) {
            char *grown = (char *)ae_std_alloc(capacity * 2, (int64_t)line, (int64_t)column);
            memcpy(grown, buffer, (size_t)length);
            free(buffer);
            buffer = grown;
            capacity = capacity * 2;
        }
        const ssize_t got = read(fds[0], buffer + length, (size_t)(capacity - length));
        if (got < 0) {
            if (errno == EINTR) continue;
            const ae_str failure = ae_io_error();
            free(buffer);
            close(fds[0]);
            return ae_command_fail_value(failure);
        }
        if (got == 0) break;
        length += got;
    }
    close(fds[0]);
    buffer[length] = 0;
    int status = 0;
    for (;;) {
        if (waitpid(pid, &status, 0) >= 0) break;
        if (errno != EINTR) {
            const ae_str failure = ae_io_error();
            free(buffer);
            return ae_command_fail_value(failure);
        }
    }
    // A signaled child is reported the way a shell reports it.
    const int64_t code = WIFEXITED(status) ? (int64_t)WEXITSTATUS(status)
                                           : (int64_t)(128 + (WIFSIGNALED(status) ? WTERMSIG(status) : 0));
    return ae_command_ok_value(ae_str_from_parts(buffer, length, true), code);
}
#else
// The Win32 path declares the few kernel32 entry points it needs instead of
// including <windows.h>, whose prototypes would collide with the fiber,
// thread and lock declarations elsewhere in this preamble.
typedef struct {
    unsigned long cb;
    wchar_t *lpReserved;
    wchar_t *lpDesktop;
    wchar_t *lpTitle;
    unsigned long dwX;
    unsigned long dwY;
    unsigned long dwXSize;
    unsigned long dwYSize;
    unsigned long dwXCountChars;
    unsigned long dwYCountChars;
    unsigned long dwFillAttribute;
    unsigned long dwFlags;
    unsigned short wShowWindow;
    unsigned short cbReserved2;
    unsigned char *lpReserved2;
    void *hStdInput;
    void *hStdOutput;
    void *hStdError;
} ae_startup_info;
typedef struct {
    void *hProcess;
    void *hThread;
    unsigned long process_id;
    unsigned long thread_id;
} ae_process_info;
extern int CreatePipe(void **read_pipe, void **write_pipe, void *attributes, unsigned long size);
extern int SetHandleInformation(void *object, unsigned long mask, unsigned long flags);
extern void *GetStdHandle(unsigned long which);
extern int CreateProcessW(wchar_t *application, wchar_t *command, void *process_attributes,
                          void *thread_attributes, int inherit_handles, unsigned long flags,
                          void *environment, wchar_t *directory, ae_startup_info *startup,
                          ae_process_info *info);
extern int ReadFile(void *file, void *buffer, unsigned long bytes, unsigned long *read, void *overlapped);
extern int GetExitCodeProcess(void *process, unsigned long *code);
extern unsigned long GetLastError(void);
extern int MultiByteToWideChar(unsigned int codepage, unsigned long flags, const char *text,
                               int length, wchar_t *out, int capacity);
extern unsigned long WaitForSingleObject(void *handle, unsigned long milliseconds);
extern int CloseHandle(void *handle);
static inline ae_command_result ae_run_command_value(ae_str program, ae_vec *args, size_t line, size_t column) {
    // Quotes and trailing backslashes are refused instead of escaped: guessing
    // the quoting rules wrong silently changes what the child runs.
    for (int64_t i = 0; i < args->len; i++) {
        const ae_str arg = *(ae_str *)ae_vec_at(args, i, line, column);
        for (int64_t k = 0; k < arg.len; k++) {
            if (arg.ptr[k] == '"') {
                return ae_command_fail_value(ae_command_literal("command arguments must not contain a double quote"));
            }
        }
        if (arg.len > 0 && arg.ptr[arg.len - 1] == '\\') {
            return ae_command_fail_value(ae_command_literal("command arguments must not end with a backslash"));
        }
    }
    for (int64_t k = 0; k < program.len; k++) {
        if (program.ptr[k] == '"') {
            return ae_command_fail_value(ae_command_literal("the program name must not contain a double quote"));
        }
    }
    int64_t width = program.len + 3;
    for (int64_t i = 0; i < args->len; i++) {
        width += ((ae_str *)ae_vec_at(args, i, line, column))->len + 3;
    }
    char *command = (char *)ae_std_alloc(width, (int64_t)line, (int64_t)column);
    int64_t at = 0;
    command[at++] = '"';
    memcpy(command + at, program.ptr, (size_t)program.len);
    at += program.len;
    command[at++] = '"';
    for (int64_t i = 0; i < args->len; i++) {
        const ae_str arg = *(ae_str *)ae_vec_at(args, i, line, column);
        bool spaced = false;
        for (int64_t k = 0; k < arg.len; k++) {
            if (arg.ptr[k] == ' ' || arg.ptr[k] == '\t') { spaced = true; break; }
        }
        command[at++] = ' ';
        if (spaced) command[at++] = '"';
        memcpy(command + at, arg.ptr, (size_t)arg.len);
        at += arg.len;
        if (spaced) command[at++] = '"';
    }
    command[at] = 0;
    const int needed = MultiByteToWideChar(65001, 0, command, -1, NULL, 0);
    wchar_t *wide = NULL;
    if (needed > 0) {
        wide = (wchar_t *)ae_std_alloc((int64_t)(sizeof(wchar_t) * (size_t)needed), (int64_t)line, (int64_t)column);
        if (MultiByteToWideChar(65001, 0, command, -1, wide, needed) <= 0) {
            free(wide);
            wide = NULL;
        }
    }
    free(command);
    if (!wide) return ae_command_fail_value(ae_command_literal("out of memory"));
    void *read_end = NULL;
    void *write_end = NULL;
    if (!CreatePipe(&read_end, &write_end, NULL, 0)) {
        free(wide);
        return ae_command_fail_value(ae_command_status_error("cannot create a pipe", GetLastError()));
    }
    // CreatePipe with no security attributes makes both ends private; the
    // child needs the write end, so mark that one inheritable.
    (void)SetHandleInformation(write_end, 1u, 1u);
    ae_startup_info startup;
    memset(&startup, 0, sizeof startup);
    startup.cb = (unsigned long)sizeof startup;
    startup.dwFlags = 256u; // STARTF_USESTDHANDLES
    startup.hStdInput = GetStdHandle(0xFFFFFFF6u); // STD_INPUT_HANDLE
    startup.hStdOutput = write_end;
    startup.hStdError = write_end;
    ae_process_info process;
    memset(&process, 0, sizeof process);
    const int started = CreateProcessW(NULL, wide, NULL, NULL, 1, 0x08000000u, NULL, NULL, &startup, &process);
    const unsigned long failure = started ? 0 : GetLastError();
    free(wide);
    CloseHandle(write_end);
    if (!started) {
        CloseHandle(read_end);
        return ae_command_fail_value(ae_command_status_error("cannot start the program", failure));
    }
    CloseHandle(process.hThread);
    int64_t capacity = 4096;
    int64_t length = 0;
    char *buffer = (char *)ae_std_alloc(capacity, (int64_t)line, (int64_t)column);
    for (;;) {
        if (length == capacity) {
            char *grown = (char *)ae_std_alloc(capacity * 2, (int64_t)line, (int64_t)column);
            memcpy(grown, buffer, (size_t)length);
            free(buffer);
            buffer = grown;
            capacity = capacity * 2;
        }
        unsigned long got = 0;
        const int64_t room = capacity - length;
        const unsigned long request = (unsigned long)(room > 0x7FFFFFFF ? 0x7FFFFFFF : room);
        if (!ReadFile(read_end, buffer + length, request, &got, NULL)) {
            const unsigned long code = GetLastError();
            if (code == 109u) break; // ERROR_BROKEN_PIPE: the child closed its end
            const ae_str failure = ae_command_status_error("cannot read the program output", code);
            free(buffer);
            CloseHandle(read_end);
            CloseHandle(process.hProcess);
            return ae_command_fail_value(failure);
        }
        if (got == 0) break;
        length += (int64_t)got;
    }
    CloseHandle(read_end);
    buffer[length] = 0;
    (void)WaitForSingleObject(process.hProcess, 0xFFFFFFFFu);
    unsigned long status = 0;
    (void)GetExitCodeProcess(process.hProcess, &status);
    CloseHandle(process.hProcess);
    return ae_command_ok_value(ae_str_from_parts(buffer, length, true), (int64_t)status);
}
#endif
// Cooperative task executor (spec/concurrency.md, ADR-0218): tasks are
// stackful coroutines. Win32 uses fibers; POSIX uses C11 threads with a
// turnstile so exactly one task runs at a time until it yields.
#ifdef _WIN32
extern void *ConvertThreadToFiber(void *param);
extern void *CreateFiber(size_t stack, void (*start)(void *), void *param);
extern void SwitchToFiber(void *fiber);
extern void DeleteFiber(void *fiber);
extern int ConvertFiberToThread(void);
#else
#include <threads.h>
#endif
typedef void (*ae_task_entry_fn)(void *);
typedef struct { void *state; } ae_exec;
typedef struct ae_task_node {
    ae_task_entry_fn entry;
    void *ctx;
#ifdef _WIN32
    void *fiber;
#else
    thrd_t thrd;
    mtx_t mu;
    cnd_t cv;
    int started;
    int turn;
#endif
    int done;
    struct ae_task_node *next;
} ae_task_node;
typedef struct {
    ae_task_node *head;
    ae_task_node *tail;
} ae_exec_state;
#ifdef _WIN32
static _Thread_local void *ae_task_sched_fiber = NULL;
static _Thread_local int ae_task_in_task = 0;
#else
static _Thread_local ae_task_node *ae_task_current = NULL;
#endif
#ifdef _WIN32
static void ae_task_trampoline(void *param) {
    ae_task_node *node = (ae_task_node *)param;
    node->entry(node->ctx);
    node->done = 1;
    SwitchToFiber(ae_task_sched_fiber);
}
#else
static int ae_task_trampoline_posix(void *param) {
    ae_task_node *node = (ae_task_node *)param;
    ae_task_current = node;
    mtx_lock(&node->mu);
    while (!node->turn) cnd_wait(&node->cv, &node->mu);
    mtx_unlock(&node->mu);
    node->entry(node->ctx);
    mtx_lock(&node->mu);
    node->done = 1;
    node->turn = 0;
    cnd_signal(&node->cv);
    mtx_unlock(&node->mu);
    ae_task_current = NULL;
    return 0;
}
#endif
static inline void ae_task_yield(void) {
#ifdef _WIN32
    if (ae_task_in_task && ae_task_sched_fiber) {
        SwitchToFiber(ae_task_sched_fiber);
    }
#else
    ae_task_node *node = ae_task_current;
    if (!node) return;
    mtx_lock(&node->mu);
    node->turn = 0;
    cnd_signal(&node->cv);
    while (!node->turn) cnd_wait(&node->cv, &node->mu);
    mtx_unlock(&node->mu);
#endif
}
static inline ae_exec ae_exec_new(void) {
    ae_exec_state *s = (ae_exec_state *)calloc(1, sizeof(ae_exec_state));
    if (!s) ae_trap("out of memory");
    ae_exec value = { s };
    return value;
}
static inline void ae_task_spawn(ae_exec ex, ae_task_entry_fn entry, void *ctx) {
    ae_exec_state *s = (ae_exec_state *)ex.state;
    ae_task_node *node = (ae_task_node *)calloc(1, sizeof(ae_task_node));
    if (!node) ae_trap("out of memory");
    node->entry = entry;
    node->ctx = ctx;
#ifndef _WIN32
    mtx_init(&node->mu, mtx_plain);
    cnd_init(&node->cv);
#endif
    if (s->tail) {
        s->tail->next = node;
        s->tail = node;
    } else {
        s->head = s->tail = node;
    }
}
static inline int64_t ae_exec_run(ae_exec ex) {
    ae_exec_state *s = (ae_exec_state *)ex.state;
#ifdef _WIN32
    void *prev_sched = ae_task_sched_fiber;
    int converted = 0;
    if (!ae_task_sched_fiber) {
        void *f = ConvertThreadToFiber(NULL);
        if (f) {
            ae_task_sched_fiber = f;
            converted = 1;
        }
    }
    int64_t completed = 0;
    while (s->head) {
        ae_task_node *node = s->head;
        s->head = node->next;
        if (!s->head) s->tail = NULL;
        node->next = NULL;
        if (!node->fiber) {
            node->fiber = CreateFiber(65536, ae_task_trampoline, node);
            if (!node->fiber) ae_trap("out of memory");
        }
        ae_task_in_task = 1;
        SwitchToFiber(node->fiber);
        ae_task_in_task = 0;
        if (node->done) {
            DeleteFiber(node->fiber);
            free(node->ctx);
            free(node);
            completed += 1;
        } else if (s->tail) {
            s->tail->next = node;
            s->tail = node;
        } else {
            s->head = s->tail = node;
        }
    }
    ae_task_sched_fiber = prev_sched;
    if (converted) ConvertFiberToThread();
    return completed;
#else
    int64_t completed = 0;
    while (s->head) {
        ae_task_node *node = s->head;
        s->head = node->next;
        if (!s->head) s->tail = NULL;
        node->next = NULL;
        if (!node->started) {
            node->started = 1;
            if (thrd_create(&node->thrd, ae_task_trampoline_posix, node) != thrd_success) {
                ae_trap("task thread creation failed");
            }
        }
        mtx_lock(&node->mu);
        node->turn = 1;
        cnd_signal(&node->cv);
        while (node->turn && !node->done) cnd_wait(&node->cv, &node->mu);
        mtx_unlock(&node->mu);
        if (node->done) {
            thrd_join(node->thrd, NULL);
            free(node->ctx);
            mtx_destroy(&node->mu);
            cnd_destroy(&node->cv);
            free(node);
            completed += 1;
        } else if (s->tail) {
            s->tail->next = node;
            s->tail = node;
        } else {
            s->head = s->tail = node;
        }
    }
    return completed;
#endif
}

// OS threads (spec/concurrency.md §12): spawn runs a void fn(void*) on an
// operating-system thread; the language layer packages the typed return value
// and captured slots into the context, and wait joins before reading them
// out. A child failure is recorded on its handle instead of ending the
// process; the first join that observes it reports and exits, matching an
// immediate abort at the observation point.
typedef void (*ae_thread_entry_fn)(void *);
typedef struct { int failed; const char *message; size_t line; size_t column; } ae_thread_failure;
typedef struct { ae_thread_entry_fn fn; void *ctx; ae_thread_failure *failure; } ae_thread_job;
typedef struct { void *native; void *ctx; } ae_thread_slot;
static _Thread_local ae_thread_failure *ae_thread_current_failure = NULL;
static ae_thread_failure ae_thread_latched = { 0, NULL, 0, 0 };
static int ae_thread_hook_installed = 0;
#ifdef _WIN32
typedef struct { void *native; ae_thread_failure failure; } ae_thread_handle;
struct _SECURITY_ATTRIBUTES;
extern void *CreateThread(struct _SECURITY_ATTRIBUTES *attributes, size_t stack, unsigned long (*start)(void *), void *parameter, unsigned long flags, unsigned long *id);
extern unsigned long WaitForSingleObject(void *handle, unsigned long milliseconds);
extern int CloseHandle(void *handle);
extern void ExitThread(unsigned long exit_code);
static unsigned long ae_thread_body(void *raw) {
    ae_thread_job *job = (ae_thread_job *)raw;
    ae_thread_current_failure = job->failure;
    job->fn(job->ctx);
    ae_thread_current_failure = NULL;
    free(raw);
    return 0;
}
#else
typedef struct { thrd_t native; ae_thread_failure failure; } ae_thread_handle;
static int ae_thread_body(void *raw) {
    ae_thread_job *job = (ae_thread_job *)raw;
    ae_thread_current_failure = job->failure;
    job->fn(job->ctx);
    ae_thread_current_failure = NULL;
    free(raw);
    return 0;
}
#endif
static void ae_thread_report_failure(const ae_thread_failure *failure) {
    fprintf(stderr, "ae:%zu:%zu: %s\n", failure->line, failure->column, failure->message);
    fflush(stdout);
    _Exit(1);
}
static void ae_thread_check_latch(void) {
    if (ae_thread_latched.failed) ae_thread_report_failure(&ae_thread_latched);
}
static inline ae_thread_slot *ae_thread_spawn_ctx(ae_thread_entry_fn entry, void *ctx) {
    ae_thread_handle *handle = (ae_thread_handle *)malloc(sizeof(ae_thread_handle));
    if (!handle) return NULL;
    handle->failure.failed = 0;
    handle->failure.message = NULL;
    handle->failure.line = 0;
    handle->failure.column = 0;
    ae_thread_job *job = (ae_thread_job *)malloc(sizeof(ae_thread_job));
    if (!job) {
        free(handle);
        return NULL;
    }
    job->fn = entry;
    job->ctx = ctx;
    job->failure = &handle->failure;
    if (!ae_thread_hook_installed) {
        atexit(ae_thread_check_latch);
        ae_thread_hook_installed = 1;
    }
#ifdef _WIN32
    handle->native = CreateThread(NULL, 0, ae_thread_body, job, 0, NULL);
    if (!handle->native) {
        free(job);
        free(handle);
        return NULL;
    }
#else
    if (thrd_create(&handle->native, ae_thread_body, job) != thrd_success) {
        free(job);
        free(handle);
        return NULL;
    }
#endif
    ae_thread_slot *slot = (ae_thread_slot *)malloc(sizeof(ae_thread_slot));
    if (!slot) return NULL;
    slot->native = handle;
    slot->ctx = ctx;
    return slot;
}
static inline bool ae_thread_join(void *opaque) {
    ae_thread_handle *handle = (ae_thread_handle *)opaque;
    if (!handle) return false;
#ifdef _WIN32
    if (WaitForSingleObject(handle->native, 0xFFFFFFFFu) != 0) return false;
    CloseHandle(handle->native);
#else
    if (thrd_join(handle->native, NULL) != thrd_success) return false;
#endif
    if (handle->failure.failed) {
        ae_thread_failure failure = handle->failure;
        ae_thread_latched.failed = 0;
        ae_thread_report_failure(&failure);
    }
    free(handle);
    return true;
}
static inline void ae_thread_release(ae_thread_slot **slot) {
    if (slot && *slot) {
        free(*slot);
        *slot = NULL;
    }
}
static inline ae_thread_slot *ae_thread_take(ae_thread_slot **slot) {
    ae_thread_slot *value = *slot;
    *slot = NULL;
    return value;
}
static _Noreturn void ae_fail_at(const char *msg, size_t line, size_t column) {
    if (ae_thread_current_failure) {
        ae_thread_current_failure->failed = 1;
        ae_thread_current_failure->message = msg;
        ae_thread_current_failure->line = line;
        ae_thread_current_failure->column = column;
        ae_thread_latched = *ae_thread_current_failure;
#ifdef _WIN32
        ExitThread(1);
#else
        thrd_exit(1);
#endif
    }
    fprintf(stderr, "ae:%zu:%zu: %s\n", line, column, msg);
    fflush(stdout);
    exit(1);
}

// Shared boxes: a process-lifetime allocation behind a copy-semantics handle.
// Copying the handle into another thread shares the box; nothing frees it.
typedef struct { void *box; } ae_shared;
static inline ae_shared ae_shared_new(const void *value, size_t size) {
    void *box = malloc(size > 0 ? size : 1);
    if (!box) ae_trap("out of memory");
    if (size > 0) memcpy(box, value, size);
    ae_shared handle = { box };
    return handle;
}

// Mutexes and guards: one process-lifetime OS mutex behind a copy-semantics
// handle; mutex_lock returns an owned guard whose release unlocks it.
#ifdef _WIN32
struct _SECURITY_ATTRIBUTES;
extern void *CreateMutexA(struct _SECURITY_ATTRIBUTES *attributes, int initial_owner, const char *name);
extern unsigned long WaitForSingleObject(void *handle, unsigned long milliseconds);
extern int ReleaseMutex(void *handle);
typedef struct { void *lock; } ae_mutex;
typedef struct { void *lock; } ae_guard;
static inline ae_mutex ae_mutex_new(void) {
    void *handle = CreateMutexA(NULL, 0, NULL);
    if (!handle) ae_trap("mutex_new failed");
    ae_mutex value = { handle };
    return value;
}
static inline ae_guard ae_mutex_lock(ae_mutex m) {
    WaitForSingleObject(m.lock, 0xFFFFFFFFu);
    ae_guard value = { m.lock };
    return value;
}
static inline void ae_mutex_unlock(ae_guard *g) {
    if (g->lock != NULL) {
        ReleaseMutex(g->lock);
        g->lock = NULL;
    }
}
#else
typedef struct { mtx_t *lock; } ae_mutex;
typedef struct { mtx_t *lock; } ae_guard;
static inline ae_mutex ae_mutex_new(void) {
    mtx_t *lock = (mtx_t *)malloc(sizeof(mtx_t));
    if (!lock || mtx_init(lock, mtx_plain) != thrd_success) ae_trap("mutex_new failed");
    ae_mutex value = { lock };
    return value;
}
static inline ae_guard ae_mutex_lock(ae_mutex m) {
    mtx_lock(m.lock);
    ae_guard value = { m.lock };
    return value;
}
static inline void ae_mutex_unlock(ae_guard *g) {
    if (g->lock != NULL) {
        mtx_unlock(g->lock);
        g->lock = NULL;
    }
}
#endif
static inline ae_guard ae_guard_take(ae_guard *g) {
    ae_guard value = *g;
    g->lock = NULL;
    return value;
}

// Channels (spec/concurrency.md §10): copy-semantics handle to a
// process-lifetime bounded or rendezvous (cap == 0) i64 channel. Blocking
// send/recv, draining after close, and blocked senders woken with false.
#ifdef _WIN32
struct _RTL_SRWLOCK;
struct _RTL_CONDITION_VARIABLE;
extern void InitializeSRWLock(struct _RTL_SRWLOCK *lock);
extern void AcquireSRWLockExclusive(struct _RTL_SRWLOCK *lock);
extern void ReleaseSRWLockExclusive(struct _RTL_SRWLOCK *lock);
extern void InitializeConditionVariable(struct _RTL_CONDITION_VARIABLE *cv);
extern int SleepConditionVariableSRW(struct _RTL_CONDITION_VARIABLE *cv, struct _RTL_SRWLOCK *lock, unsigned long ms, unsigned long flags);
extern void WakeConditionVariable(struct _RTL_CONDITION_VARIABLE *cv);
extern void WakeAllConditionVariable(struct _RTL_CONDITION_VARIABLE *cv);
extern void Sleep(unsigned long ms);
#else
#include <pthread.h>
#endif
typedef struct {
#ifdef _WIN32
    void *lock_storage;
    void *not_full_storage;
    void *not_empty_storage;
#else
    pthread_mutex_t lock;
    pthread_cond_t not_full;
    pthread_cond_t not_empty;
#endif
    int64_t *buf;
    int64_t cap;
    int64_t head;
    int64_t count;
    int64_t slot;
    int has_slot;
    int closed;
} ae_chan_state;
typedef struct { ae_chan_state *state; } ae_chan;
static inline ae_chan ae_chan_new(int64_t cap) {
    if (cap < 0) ae_trap("chan_new capacity must be non-negative");
    ae_chan_state *s = (ae_chan_state *)calloc(1, sizeof(ae_chan_state));
    if (!s) ae_trap("out of memory");
    s->cap = cap;
    if (cap > 0) {
        s->buf = (int64_t *)calloc((size_t)cap, sizeof(int64_t));
        if (!s->buf) ae_trap("out of memory");
    }
#ifdef _WIN32
    InitializeSRWLock((struct _RTL_SRWLOCK *)&s->lock_storage);
    InitializeConditionVariable((struct _RTL_CONDITION_VARIABLE *)&s->not_full_storage);
    InitializeConditionVariable((struct _RTL_CONDITION_VARIABLE *)&s->not_empty_storage);
#else
    pthread_mutex_init(&s->lock, NULL);
    pthread_cond_init(&s->not_full, NULL);
    pthread_cond_init(&s->not_empty, NULL);
#endif
    ae_chan value = { s };
    return value;
}
#ifdef _WIN32
#define _AE_CHAN_LOCK(s) AcquireSRWLockExclusive((struct _RTL_SRWLOCK *)&(s)->lock_storage)
#define _AE_CHAN_UNLOCK(s) ReleaseSRWLockExclusive((struct _RTL_SRWLOCK *)&(s)->lock_storage)
#define _AE_CHAN_WAIT(cv, s) SleepConditionVariableSRW((struct _RTL_CONDITION_VARIABLE *)&(s)->cv##_storage, (struct _RTL_SRWLOCK *)&(s)->lock_storage, 0xFFFFFFFFu, 0)
#define _AE_CHAN_WAKE(cv, s) WakeConditionVariable((struct _RTL_CONDITION_VARIABLE *)&(s)->cv##_storage)
#define _AE_CHAN_WAKE_ALL(cv, s) WakeAllConditionVariable((struct _RTL_CONDITION_VARIABLE *)&(s)->cv##_storage)
#else
#define _AE_CHAN_LOCK(s) pthread_mutex_lock(&(s)->lock)
#define _AE_CHAN_UNLOCK(s) pthread_mutex_unlock(&(s)->lock)
#define _AE_CHAN_WAIT(cv, s) pthread_cond_wait(&(s)->cv, &(s)->lock)
#define _AE_CHAN_WAKE(cv, s) pthread_cond_signal(&(s)->cv)
#define _AE_CHAN_WAKE_ALL(cv, s) pthread_cond_broadcast(&(s)->cv)
#endif
static inline bool ae_chan_send(ae_chan ch, int64_t value) {
    ae_chan_state *s = ch.state;
    _AE_CHAN_LOCK(s);
    bool ok = false;
    if (s->cap == 0) {
        while (!s->closed && s->has_slot) _AE_CHAN_WAIT(not_full, s);
        if (!s->closed) {
            s->slot = value;
            s->has_slot = 1;
            _AE_CHAN_WAKE(not_empty, s);
            while (!s->closed && s->has_slot) _AE_CHAN_WAIT(not_full, s);
            if (!s->has_slot) {
                ok = true;
            } else {
                s->has_slot = 0;
                ok = false;
            }
        }
    } else {
        while (!s->closed && s->count == s->cap) _AE_CHAN_WAIT(not_full, s);
        if (!s->closed) {
            const int64_t tail = (s->head + s->count) % s->cap;
            s->buf[tail] = value;
            s->count += 1;
            ok = true;
            _AE_CHAN_WAKE(not_empty, s);
        }
    }
    _AE_CHAN_UNLOCK(s);
    return ok;
}
static inline int64_t ae_chan_recv(ae_chan ch) {
    ae_chan_state *s = ch.state;
    _AE_CHAN_LOCK(s);
    int64_t value = 0;
    if (s->cap == 0) {
        while (!s->closed && !s->has_slot) _AE_CHAN_WAIT(not_empty, s);
        if (s->has_slot) {
            value = s->slot;
            s->has_slot = 0;
            _AE_CHAN_WAKE_ALL(not_full, s);
        }
    } else {
        while (!s->closed && s->count == 0) _AE_CHAN_WAIT(not_empty, s);
        if (s->count > 0) {
            value = s->buf[s->head];
            s->head = (s->head + 1) % s->cap;
            s->count -= 1;
            _AE_CHAN_WAKE(not_full, s);
        }
    }
    _AE_CHAN_UNLOCK(s);
    return value;
}
static inline void ae_chan_close(ae_chan ch) {
    ae_chan_state *s = ch.state;
    _AE_CHAN_LOCK(s);
    s->closed = 1;
    _AE_CHAN_WAKE_ALL(not_full, s);
    _AE_CHAN_WAKE_ALL(not_empty, s);
    _AE_CHAN_UNLOCK(s);
}
static inline bool ae_chan_try_send(ae_chan ch, int64_t value) {
    ae_chan_state *s = ch.state;
    _AE_CHAN_LOCK(s);
    bool ok = false;
    if (!s->closed && s->cap > 0 && s->count < s->cap) {
        const int64_t tail = (s->head + s->count) % s->cap;
        s->buf[tail] = value;
        s->count += 1;
        ok = true;
        _AE_CHAN_WAKE(not_empty, s);
    }
    _AE_CHAN_UNLOCK(s);
    return ok;
}
static inline bool ae_chan_is_ready(ae_chan ch) {
    ae_chan_state *s = ch.state;
    _AE_CHAN_LOCK(s);
    const bool ready = s->closed || (s->cap == 0 ? s->has_slot != 0 : s->count > 0);
    _AE_CHAN_UNLOCK(s);
    return ready;
}
static inline bool ae_chan_is_closed(ae_chan ch) {
    ae_chan_state *s = ch.state;
    _AE_CHAN_LOCK(s);
    const bool closed = s->closed != 0;
    _AE_CHAN_UNLOCK(s);
    return closed;
}
static inline int64_t ae_chan_select(ae_chan ch1, ae_chan ch2) {
    for (;;) {
        if (ae_chan_is_closed(ch1)) return 0;
        if (ae_chan_is_closed(ch2)) return 1;
        if (ae_chan_is_ready(ch1)) return 0;
        if (ae_chan_is_ready(ch2)) return 1;
#ifdef _WIN32
        Sleep(1);
#else
        struct timespec ts;
        ts.tv_sec = 0;
        ts.tv_nsec = 1000000;
        thrd_sleep(&ts, NULL);
#endif
    }
}

// Task scopes: an executor whose release drains every spawned child, so an
// early return still joins the work it started.
typedef struct { void *state; } ae_task_scope;
static inline ae_task_scope ae_scope_new(void) {
    ae_exec ex = ae_exec_new();
    ae_task_scope scope = { ex.state };
    return scope;
}
static inline void ae_scope_spawn(ae_task_scope sc, ae_task_entry_fn entry, void *ctx) {
    ae_exec ex = { sc.state };
    ae_task_spawn(ex, entry, ctx);
}
static inline void ae_scope_release(ae_task_scope *sc) {
    if (sc->state) {
        ae_exec ex = { sc->state };
        (void)ae_exec_run(ex);
        sc->state = NULL;
    }
}
static inline ae_task_scope ae_task_scope_take(ae_task_scope *sc) {
    ae_task_scope value = *sc;
    sc->state = NULL;
    return value;
}

// Multi-worker work-stealing pool: spawn enqueues a task, join starts the
// workers (up to 16), lets them drain and steal, then returns the completed
// count. The pool handle itself is process-lifetime.
typedef struct ae_pool_task {
    ae_task_entry_fn entry;
    void *ctx;
    struct ae_pool_task *next;
} ae_pool_task;
typedef struct {
    atomic_flag lock;
    ae_pool_task *head;
    ae_pool_task *tail;
} ae_pool_queue;
typedef struct {
    int64_t workers;
    int64_t next_q;
    _Atomic int64_t completed;
    ae_pool_queue queues[16];
} ae_pool_state;
typedef struct { ae_pool_state *state; } ae_pool;
typedef struct {
    ae_pool_state *pool;
    int64_t id;
} ae_pool_worker_arg;
static inline void ae_pool_lock(atomic_flag *flag) {
    while (atomic_flag_test_and_set_explicit(flag, memory_order_acquire)) {}
}
static inline void ae_pool_unlock(atomic_flag *flag) {
    atomic_flag_clear_explicit(flag, memory_order_release);
}
static inline ae_pool_task *ae_pool_pop_one(ae_pool_queue *q) {
    ae_pool_lock(&q->lock);
    ae_pool_task *task = q->head;
    if (task) {
        q->head = task->next;
        if (!q->head) q->tail = NULL;
        task->next = NULL;
    }
    ae_pool_unlock(&q->lock);
    return task;
}
static void ae_pool_worker_run(void *param) {
    ae_pool_worker_arg *arg = (ae_pool_worker_arg *)param;
    ae_pool_state *p = arg->pool;
    const int64_t me = arg->id;
    for (;;) {
        ae_pool_task *task = ae_pool_pop_one(&p->queues[me]);
        if (!task) {
            for (int64_t off = 1; off < p->workers; off++) {
                const int64_t victim = (me + off) % p->workers;
                task = ae_pool_pop_one(&p->queues[victim]);
                if (task) break;
            }
        }
        if (!task) break;
        task->entry(task->ctx);
        free(task->ctx);
        free(task);
        atomic_fetch_add(&p->completed, 1);
    }
}
static inline ae_pool ae_pool_new(int64_t workers) {
    if (workers < 1) workers = 1;
    if (workers > 16) workers = 16;
    ae_pool_state *p = (ae_pool_state *)calloc(1, sizeof(ae_pool_state));
    if (!p) ae_trap("out of memory");
    p->workers = workers;
    atomic_init(&p->completed, 0);
    ae_pool value = { p };
    return value;
}
static inline void ae_pool_spawn(ae_pool pool, ae_task_entry_fn entry, void *ctx) {
    ae_pool_state *p = pool.state;
    ae_pool_task *task = (ae_pool_task *)calloc(1, sizeof(ae_pool_task));
    if (!task) ae_trap("out of memory");
    task->entry = entry;
    task->ctx = ctx;
    const int64_t qid = p->next_q % p->workers;
    p->next_q += 1;
    ae_pool_queue *q = &p->queues[qid];
    ae_pool_lock(&q->lock);
    if (q->tail) {
        q->tail->next = task;
        q->tail = task;
    } else {
        q->head = q->tail = task;
    }
    ae_pool_unlock(&q->lock);
}
static inline int64_t ae_pool_join(ae_pool pool) {
    ae_pool_state *p = pool.state;
    atomic_store(&p->completed, 0);
    ae_thread_slot *handles[16];
    ae_pool_worker_arg args[16];
    for (int64_t i = 0; i < p->workers; i++) {
        args[i].pool = p;
        args[i].id = i;
        handles[i] = ae_thread_spawn_ctx(ae_pool_worker_run, &args[i]);
        if (!handles[i]) ae_trap("out of memory");
    }
    for (int64_t i = 0; i < p->workers; i++) {
        if (!ae_thread_join(handles[i]->native)) ae_trap("pool worker join failed");
        free(handles[i]);
    }
    return atomic_load(&p->completed);
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
            "static inline {c_ty} ae_add_{name}({c_ty} a, {c_ty} b, size_t line, size_t column) {{\n\
                 if ((b > 0 && a > {max_expr} - b) || (b < 0 && a < {min_expr} - b)) {{\n\
                     ae_fail_at(\"integer overflow\", line, column);\n\
                 }}\n\
                 return ({c_ty})((uint64_t)a + (uint64_t)b);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_sub_{name}({c_ty} a, {c_ty} b, size_t line, size_t column) {{\n\
                 if ((b < 0 && a > {max_expr} + b) || (b > 0 && a < {min_expr} + b)) {{\n\
                     ae_fail_at(\"integer overflow\", line, column);\n\
                 }}\n\
                 return ({c_ty})((uint64_t)a - (uint64_t)b);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_mul_{name}({c_ty} a, {c_ty} b, size_t line, size_t column) {{\n\
                 if (a > 0) {{\n\
                     if (b > 0 && a > {max_expr} / b) {{ ae_fail_at(\"integer overflow\", line, column); }}\n\
                     if (b < 0 && b < {min_expr} / a) {{ ae_fail_at(\"integer overflow\", line, column); }}\n\
                 }} else if (a < 0) {{\n\
                     if (b > 0 && a < {min_expr} / b) {{ ae_fail_at(\"integer overflow\", line, column); }}\n\
                     if (b < 0 && b < {max_expr} / a) {{ ae_fail_at(\"integer overflow\", line, column); }}\n\
                 }}\n\
                 return ({c_ty})((int64_t)a * (int64_t)b);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_div_{name}({c_ty} a, {c_ty} b, size_t line, size_t column) {{\n\
                 if (b == 0) {{ ae_fail_at(\"division by zero\", line, column); }}\n\
                 if (a == {min_expr} && b == -1) {{ ae_fail_at(\"integer overflow\", line, column); }}\n\
                 return ({c_ty})(a / b);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_rem_{name}({c_ty} a, {c_ty} b, size_t line, size_t column) {{\n\
                 if (b == 0) {{ ae_fail_at(\"division by zero\", line, column); }}\n\
                 if (a == {min_expr} && b == -1) {{ ae_fail_at(\"integer overflow\", line, column); }}\n\
                 return ({c_ty})(a % b);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_shl_{name}({c_ty} a, {c_ty} b, size_t line, size_t column) {{\n\
                 if (b < 0 || b >= {bits}) {{ ae_fail_at(\"shift count out of range\", line, column); }}\n\
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
            "static inline {c_ty} ae_shr_{name}({c_ty} a, {c_ty} b, size_t line, size_t column) {{\n\
                 if (b < 0 || b >= {bits}) {{ ae_fail_at(\"shift count out of range\", line, column); }}\n\
                 if (a >= 0) {{\n\
                     return ({c_ty})((uint64_t)a >> (unsigned)b);\n\
                 }}\n\
                 return ({c_ty})(int64_t)(~((~(uint64_t)(int64_t)a) >> (unsigned)b));\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_neg_{name}({c_ty} a, size_t line, size_t column) {{\n\
                 if (a == {min_expr}) {{ ae_fail_at(\"integer overflow\", line, column); }}\n\
                 return ({c_ty})(-(uint64_t)(int64_t)a);\n\
             }}\n"
        );
    } else {
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_add_{name}({c_ty} a, {c_ty} b, size_t line, size_t column) {{\n\
                 if (a > {max_expr} - b) {{ ae_fail_at(\"integer overflow\", line, column); }}\n\
                 return ({c_ty})(a + b);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_sub_{name}({c_ty} a, {c_ty} b, size_t line, size_t column) {{\n\
                 if (a < b) {{ ae_fail_at(\"integer overflow\", line, column); }}\n\
                 return ({c_ty})(a - b);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_mul_{name}({c_ty} a, {c_ty} b, size_t line, size_t column) {{\n\
                 if (b != 0 && a > {max_expr} / b) {{ ae_fail_at(\"integer overflow\", line, column); }}\n\
                 return ({c_ty})(a * b);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_div_{name}({c_ty} a, {c_ty} b, size_t line, size_t column) {{\n\
                 if (b == 0) {{ ae_fail_at(\"division by zero\", line, column); }}\n\
                 return ({c_ty})(a / b);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_rem_{name}({c_ty} a, {c_ty} b, size_t line, size_t column) {{\n\
                 if (b == 0) {{ ae_fail_at(\"division by zero\", line, column); }}\n\
                 return ({c_ty})(a % b);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_shl_{name}({c_ty} a, {c_ty} b, size_t line, size_t column) {{\n\
                 if (b >= {bits}) {{ ae_fail_at(\"shift count out of range\", line, column); }}\n\
                 uint64_t raw = ((uint64_t)a) << (unsigned)b;\n\
                 uint64_t mask = {mask_expr};\n\
                 return ({c_ty})(raw & mask);\n\
             }}"
        );
        let _ = writeln!(
            out,
            "static inline {c_ty} ae_shr_{name}({c_ty} a, {c_ty} b, size_t line, size_t column) {{\n\
                 if (b >= {bits}) {{ ae_fail_at(\"shift count out of range\", line, column); }}\n\
                 return ({c_ty})(a >> (unsigned)b);\n\
             }}\n"
        );
    }
}
