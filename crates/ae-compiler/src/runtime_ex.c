// Fallible built-ins and platform helpers that name prelude enum types.
// Included after the enum layouts, so ae_e0..ae_e3 exist here.
// Invariant (checked by the emitter): the prelude registers exactly
//   0 ReadResult{Ok(string), Err(string)}
//   1 WriteResult{Ok(i64), Err(string)}
//   2 IntResult{Ok(i64), Err(string)}
//   3 DirResult{Ok(vec[string]), Err(string)}
// with Ok at variant index 0 and Err at index 1.

#include <errno.h>
#include <math.h>

// min/max for f64 (ADR-64): implemented in the runtime, not libm, so the
// result is bit-identical on every platform. NaN is ignored: one NaN operand
// yields the other, two NaNs yield NaN. The ±0.0 tie follows the IEEE
// totalOrder (−0.0 below +0.0), so min(-0.0, 0.0) is always −0.0 and the
// printed text is portable — libm's fmin leaves that case unspecified.
// A non-static helper the extern "C" corpus test links against (M2.12
// slice 4): the declaration's prototype makes the pruner keep it, and the
// call exercises the whole declare -> unsafe call -> link -> run path on
// both platforms without depending on any libc signature.
int64_t ae_extern_probe(int64_t v) {
    return v * 2 + 1;
}
static double ae_f64_min(double a, double b) {
    if (isnan(a)) return b;
    if (isnan(b)) return a;
    if (a == 0.0 && b == 0.0) return (signbit(a) && !signbit(b)) ? a : b;
    return a < b ? a : b;
}
static double ae_f64_max(double a, double b) {
    if (isnan(a)) return b;
    if (isnan(b)) return a;
    if (a == 0.0 && b == 0.0) return (!signbit(a) && signbit(b)) ? a : b;
    return a > b ? a : b;
}
#include <time.h>
#ifndef _WIN32
#include <dirent.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>
#else
#include <io.h>
#include <fcntl.h>
#endif

// The prelude result families are `Result[Ok, string]` instantiations (P2.4).
// `WriteResult` and `IntResult` share one `Result[i64, string]` layout, so
// both macro families name ae_e1. The ids are the prelude registration order
// in prelude_enums(), kept in lockstep with c_output's order check.
#define AE_READ_RESULT ae_e0
#define AE_READ_OK AE_E0_0
#define AE_READ_ERR AE_E0_1
#define AE_WRITE_RESULT ae_e1
#define AE_WRITE_OK AE_E1_0
#define AE_WRITE_ERR AE_E1_1
#define AE_INT_RESULT ae_e1
#define AE_INT_OK AE_E1_0
#define AE_INT_ERR AE_E1_1
#define AE_DIR_RESULT ae_e2
#define AE_DIR_OK AE_E2_0
#define AE_DIR_ERR AE_E2_1
#define AE_FILE_RESULT ae_e3
#define AE_FILE_OK AE_E3_0
#define AE_FILE_ERR AE_E3_1
#define AE_STATUS_RESULT ae_e4
#define AE_STATUS_OK AE_E4_0
#define AE_STATUS_ERR AE_E4_1
#define AE_BOOL_RESULT ae_e5
#define AE_BOOL_OK AE_E5_0
#define AE_BOOL_ERR AE_E5_1
#define AE_COMMAND_RESULT ae_e6
#define AE_COMMAND_OK AE_E6_0
#define AE_COMMAND_ERR AE_E6_1
#define AE_FLOAT_RESULT ae_e7
#define AE_FLOAT_OK AE_E7_0
#define AE_FLOAT_ERR AE_E7_1

static AE_STATUS_RESULT ae_status_ok(void) {
    AE_STATUS_RESULT out = { AE_STATUS_OK, { { 0 } } };
    return out;
}
static AE_STATUS_RESULT ae_status_failed(ae_string message) {
    AE_STATUS_RESULT out = { AE_STATUS_ERR, { .v1 = { message } } };
    return out;
}

// Wall-clock time in milliseconds since the Unix epoch. File metadata reports
// whole seconds, so now_s exists to compare against it without dividing on
// every call. Both are i64: the epoch range fits comfortably, and negative
// values (before 1970) stay meaningful.
static int64_t ae_now_ms(void) { /* win32-header */
#ifdef _WIN32
    FILETIME ft;
    GetSystemTimeAsFileTime(&ft);
    const int64_t ticks = ((int64_t)ft.dwHighDateTime << 32) | (int64_t)ft.dwLowDateTime;
    // 100-nanosecond ticks since 1601 -> milliseconds since 1970.
    return ticks / 10000 - 11644473600000LL;
#else
    // timespec_get is ISO C11, so the generated file still compiles under
    // -std=c11 -pedantic-errors without defining a feature-test macro;
    // clock_gettime would need _POSIX_C_SOURCE and is hidden by strict mode.
    struct timespec ts;
    if (timespec_get(&ts, TIME_UTC) != TIME_UTC) return 0;
    return (int64_t)ts.tv_sec * 1000 + (int64_t)(ts.tv_nsec / 1000000);
#endif
}
static int64_t ae_now_s(void) {
    const int64_t ms = ae_now_ms();
    // Floor division, so a negative timestamp still rounds consistently.
    return ms / 1000 - (ms % 1000 < 0 ? 1 : 0);
}

static ae_string ae_io_message(int code, size_t line, size_t column) {
    const char *text = strerror(code == 0 ? EIO : code);
    return ae_str_from(text ? text : "I/O error", line, column);
}
// Error text carrying a platform status code where strerror does not apply.
static ae_string ae_status_message(const char *what, unsigned long code, size_t line, size_t column) {
    char buffer[160];
    const int length = snprintf(buffer, sizeof buffer, "%s (status %lu)", what, code);
    if (length < 0) return ae_str_from(what, line, column);
    return ae_str_from_len(buffer, (int64_t)(length < (int)sizeof buffer ? length : (int)sizeof buffer - 1), line, column);
}

static AE_READ_RESULT ae_try_read_file(const char *path, size_t line, size_t column) {
    errno = 0;
    FILE *f = ae_fopen(path, "rb");
    if (!f) { AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_io_message(errno, line, column) } } }; return out; }
    ae_string failure = { NULL, 0 };
    if (fseek(f, 0, SEEK_END) != 0) failure = ae_io_message(errno, line, column);
    long size = failure.ptr ? -1 : ftell(f);
    if (!failure.ptr && size < 0) failure = ae_io_message(errno, line, column);
    if (!failure.ptr) rewind(f);
    char *buffer = NULL;
    if (!failure.ptr) {
        buffer = ae_alloc_try((size_t)size + 1);
        if (!buffer) failure = ae_str_from("out of memory", line, column);
    }
    if (!failure.ptr) {
        const size_t read = fread(buffer, 1, (size_t)size, f);
        if (read != (size_t)size) { ae_free(buffer); buffer = NULL; failure = ae_io_message(errno, line, column); }
    }
    fclose(f);
    if (failure.ptr) { AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { failure } } }; return out; }
    buffer[size] = 0;
    ae_string content = { buffer, (int64_t)size };
    AE_READ_RESULT out = { AE_READ_OK, { .v0 = { content } } };
    return out;
}

static AE_WRITE_RESULT ae_try_write_file(const char *path, ae_strview content, size_t line, size_t column) {
    errno = 0;
    FILE *f = ae_fopen(path, "wb");
    if (!f) { AE_WRITE_RESULT out = { AE_WRITE_ERR, { .v1 = { ae_io_message(errno, line, column) } } }; return out; }
    const size_t written = fwrite(content.ptr, 1, (size_t)content.len, f);
    const bool closed = fclose(f) == 0;
    if (!closed || written != (size_t)content.len) {
        AE_WRITE_RESULT out = { AE_WRITE_ERR, { .v1 = { ae_io_message(errno, line, column) } } };
        return out;
    }
    AE_WRITE_RESULT out = { AE_WRITE_OK, { .v0 = { (int64_t)written } } };
    return out;
}

// Appending exists as its own operation because "read, concatenate, write" is
// not the same thing: it rewrites the whole file, loses the atomicity of a
// single append, and fails on files the process may only append to.
static AE_WRITE_RESULT ae_try_append_file(const char *path, ae_strview content, size_t line, size_t column) {
    errno = 0;
    FILE *f = ae_fopen(path, "ab");
    if (!f) { AE_WRITE_RESULT out = { AE_WRITE_ERR, { .v1 = { ae_io_message(errno, line, column) } } }; return out; }
    const size_t written = fwrite(content.ptr, 1, (size_t)content.len, f);
    const bool closed = fclose(f) == 0;
    if (!closed || written != (size_t)content.len) {
        AE_WRITE_RESULT out = { AE_WRITE_ERR, { .v1 = { ae_io_message(errno, line, column) } } };
        return out;
    }
    AE_WRITE_RESULT out = { AE_WRITE_OK, { .v0 = { (int64_t)written } } };
    return out;
}

// Environment lookup. A variable that is set but empty is Ok(""), a variable
// that does not exist is Err: collapsing the two would silently turn "PATH is
// empty" into "PATH is missing".
static AE_READ_RESULT ae_try_env(const char *name, size_t line, size_t column) { /* win32-header */
    if (name[0] == 0) {
        AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_str_from("variable name is empty", line, column) } } };
        return out;
    }
#ifdef _WIN32
    wchar_t *wname = ae_widen(name);
    if (!wname) {
        AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_str_from("out of memory", line, column) } } };
        return out;
    }
    // A variable whose value is empty also reports 0, so the last error is
    // cleared first: ERROR_SUCCESS afterwards means "set but empty", and only
    // ERROR_ENVVAR_NOT_FOUND means the name is absent.
    SetLastError(ERROR_SUCCESS);
    const DWORD needed = GetEnvironmentVariableW(wname, NULL, 0);
    if (needed == 0) {
        const DWORD code = GetLastError();
        ae_free(wname);
        if (code == ERROR_SUCCESS) {
            AE_READ_RESULT empty = { AE_READ_OK, { .v0 = { ae_str_from("", line, column) } } };
            return empty;
        }
        AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { code == ERROR_ENVVAR_NOT_FOUND ? ae_str_from("variable not set", line, column) : ae_status_message("cannot read variable", code, line, column) } } };
        return out;
    }
    wchar_t *buffer = ae_alloc_try((size_t)needed * sizeof(wchar_t));
    if (!buffer) {
        ae_free(wname);
        AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_str_from("out of memory", line, column) } } };
        return out;
    }
    SetLastError(ERROR_SUCCESS);
    const DWORD length = GetEnvironmentVariableW(wname, buffer, needed);
    const DWORD read_code = GetLastError();
    ae_free(wname);
    if (length >= needed) {
        ae_free(buffer);
        AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_str_from("variable changed while reading", line, column) } } };
        return out;
    }
    if (length == 0 && read_code != ERROR_SUCCESS) {
        ae_free(buffer);
        AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_status_message("cannot read variable", read_code, line, column) } } };
        return out;
    }
    // An empty value is a successful read of zero characters: narrow() rejects
    // a zero length, so build the empty string directly.
    ae_string value = length == 0 ? ae_str_from("", line, column) : (ae_string){ NULL, 0 };
    if (length > 0) {
        char *utf8 = ae_narrow(buffer, (int)length);
        if (!utf8) {
            ae_free(buffer);
            AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_str_from("out of memory", line, column) } } };
            return out;
        }
        value = ae_str_from(utf8, line, column);
        ae_free(utf8);
    }
    ae_free(buffer);
    AE_READ_RESULT out = { AE_READ_OK, { .v0 = { value } } };
    return out;
#else
    (void)line; (void)column;
    const char *value = getenv(name);
    if (!value) {
        AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_str_from("variable not set", line, column) } } };
        return out;
    }
    AE_READ_RESULT out = { AE_READ_OK, { .v0 = { ae_str_from(value, line, column) } } };
    return out;
#endif
}

// Everything on standard input, read as bytes. Windows starts in text mode,
// which would turn CRLF into LF and make the byte count differ between
// platforms, so the descriptor is switched to binary first. The buffer grows by
// doubling; a fully read pipe returns however much arrived, which is not an
// error -- "no input" and "failed to read input" must stay distinguishable.
static AE_READ_RESULT ae_try_read_stdin(size_t line, size_t column) {
#ifdef _WIN32
    _setmode(_fileno(stdin), _O_BINARY);
#endif
    size_t capacity = 8192;
    size_t length = 0;
    char *buffer = ae_alloc_try(capacity);
    if (!buffer) {
        AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_str_from("out of memory", line, column) } } };
        return out;
    }
    errno = 0;
    for (;;) {
        if (length == capacity) {
            if (capacity > ((size_t)-1) / 2) {
                ae_free(buffer);
                AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_str_from("input too large", line, column) } } };
                return out;
            }
            char *grown = ae_alloc_try(capacity * 2);
            if (!grown) {
                ae_free(buffer);
                AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_str_from("out of memory", line, column) } } };
                return out;
            }
            memcpy(grown, buffer, length);
            ae_free(buffer);
            buffer = grown;
            capacity = capacity * 2;
        }
        const size_t got = fread(buffer + length, 1, capacity - length, stdin);
        length = length + got;
        if (got == 0) break;
    }
    if (ferror(stdin)) {
        const int code = errno;
        ae_free(buffer);
        AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_io_message(code, line, column) } } };
        return out;
    }
    ae_string text = ae_str_from_len(buffer, (int64_t)length, line, column);
    ae_free(buffer);
    AE_READ_RESULT out = { AE_READ_OK, { .v0 = { text } } };
    return out;
}

// Current working directory, in the same `/`-separated form the rest of the
// language uses for paths.
static AE_READ_RESULT ae_try_cwd(size_t line, size_t column) { /* win32-header */
#ifdef _WIN32
    const DWORD needed = GetCurrentDirectoryW(0, NULL);
    if (needed == 0) {
        AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_status_message("cannot read working directory", (unsigned long)GetLastError(), line, column) } } };
        return out;
    }
    wchar_t *buffer = ae_alloc_try((size_t)needed * sizeof(wchar_t));
    if (!buffer) {
        AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_str_from("out of memory", line, column) } } };
        return out;
    }
    const DWORD length = GetCurrentDirectoryW(needed, buffer);
    if (length == 0 || length >= needed) {
        ae_free(buffer);
        AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_status_message("cannot read working directory", (unsigned long)GetLastError(), line, column) } } };
        return out;
    }
    char *utf8 = ae_narrow(buffer, (int)length);
    ae_free(buffer);
    if (!utf8) {
        AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_str_from("out of memory", line, column) } } };
        return out;
    }
    ae_string raw = ae_str_from(utf8, line, column);
    ae_free(utf8);
    // Unify separators so path_join and friends see one form.
    for (int64_t i = 0; i < raw.len; i++) if (raw.ptr[i] == '\\') raw.ptr[i] = '/';
    AE_READ_RESULT out = { AE_READ_OK, { .v0 = { raw } } };
    return out;
#else
    size_t capacity = 256;
    char *buffer = ae_alloc_try(capacity);
    if (!buffer) {
        AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_str_from("out of memory", line, column) } } };
        return out;
    }
    while (!getcwd(buffer, capacity)) {
        if (errno != ERANGE || capacity > ((size_t)-1) / 2) {
            const int code = errno;
            ae_free(buffer);
            AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_io_message(code, line, column) } } };
            return out;
        }
        char *grown = ae_alloc_try(capacity * 2);
        if (!grown) {
            ae_free(buffer);
            AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_str_from("out of memory", line, column) } } };
            return out;
        }
        ae_free(buffer);
        buffer = grown;
        capacity = capacity * 2;
    }
    ae_string text = ae_str_from(buffer, line, column);
    ae_free(buffer);
    AE_READ_RESULT out = { AE_READ_OK, { .v0 = { text } } };
    return out;
#endif
}

// Byte offset of the first match, or an Err value when there is none. The
// views carry explicit lengths, so embedded NUL bytes are searched as well —
// strstr() would stop at the first one. memchr() is the platform's optimized
// byte scan; the success path allocates nothing, and the failure path only
// allocates the error string the result enum already owns.
static AE_INT_RESULT ae_find(ae_strview haystack, ae_strview needle, size_t line, size_t column) {
    if (needle.len <= 0) {
        AE_INT_RESULT out = { AE_INT_OK, { .v0 = { 0 } } };
        return out;
    }
    const int64_t last = haystack.len - needle.len;
    for (int64_t at = 0; at <= last; ) {
        const char *hit = memchr(haystack.ptr + at, needle.ptr[0], (size_t)(last - at + 1));
        if (!hit) break;
        at = (int64_t)(hit - haystack.ptr);
        if (needle.len == 1 || memcmp(hit, needle.ptr, (size_t)needle.len) == 0) {
            AE_INT_RESULT out = { AE_INT_OK, { .v0 = { at } } };
            return out;
        }
        at++;
    }
    AE_INT_RESULT bad = { AE_INT_ERR, { .v1 = { ae_str_from("substring not found", line, column) } } };
    return bad;
}

// Strict integer parsing: optional sign, ASCII digits only, whole string, and
// no wrap-around. Anything else is an error value, never a silent default.
static AE_INT_RESULT ae_parse_int(ae_strview text, size_t line, size_t column) {
    int64_t at = 0;
    bool negative = false;
    if (text.len > 0 && (text.ptr[0] == '-' || text.ptr[0] == '+')) { negative = text.ptr[0] == '-'; at = 1; }
    if (at >= text.len) {
        AE_INT_RESULT bad = { AE_INT_ERR, { .v1 = { ae_str_from("empty or sign-only integer", line, column) } } };
        return bad;
    }
    uint64_t magnitude = 0;
    const uint64_t limit = negative ? (uint64_t)INT64_MAX + 1u : (uint64_t)INT64_MAX;
    for (; at < text.len; at++) {
        const char c = text.ptr[at];
        if (c < '0' || c > '9') {
            AE_INT_RESULT bad = { AE_INT_ERR, { .v1 = { ae_str_from("not a base-10 integer", line, column) } } };
            return bad;
        }
        const uint64_t digit = (uint64_t)(c - '0');
        if (magnitude > (limit - digit) / 10u) {
            AE_INT_RESULT bad = { AE_INT_ERR, { .v1 = { ae_str_from("integer out of i64 range", line, column) } } };
            return bad;
        }
        magnitude = magnitude * 10u + digit;
    }
    const int64_t value = negative ? (int64_t)(0u - magnitude) : (int64_t)magnitude;
    AE_INT_RESULT out = { AE_INT_OK, { .v0 = { value } } };
    return out;
}

static ae_string ae_to_string(int64_t value, size_t line, size_t column) {
    char buffer[24];
    const int length = snprintf(buffer, sizeof buffer, "%" PRId64, value);
    if (length < 0) ae_fail("cannot format integer", line, column);
    return ae_str_from_len(buffer, (int64_t)length, line, column);
}
static ae_string ae_to_string_u64(uint64_t value, size_t line, size_t column) {
    char buffer[24];
    const int length = snprintf(buffer, sizeof buffer, "%" PRIu64, value);
    if (length < 0) ae_fail("cannot format integer", line, column);
    return ae_str_from_len(buffer, (int64_t)length, line, column);
}
static ae_string ae_to_string_char(uint32_t value, size_t line, size_t column) {
    char buf[4];
    const size_t len = ae_utf8_encode(value, buf);
    return ae_str_from_len(buf, (int64_t)len, line, column);
}

// ---- f64 text (ADR-37) ------------------------------------------------------
// One spelling on every platform. `%g` alone is not enough: UCRT writes a NaN
// as "-nan(ind)" while glibc writes "-nan", and exponent widths have differed
// between CRTs. So specials are written here, and finite values are rendered by
// the shortest precision that round-trips, choosing the shorter of fixed and
// scientific notation (ties go to fixed). This is a canonical form, not the
// shortest possible decimal: 0.1 prints as 0.1, 1.0 as 1.0, 1/3 with the 17
// digits needed to get the same bits back.
static size_t ae_f64_fixed(double value, char *out, size_t cap) {
    for (int decimals = 0; decimals <= 17; decimals++) {
        const int written = snprintf(out, cap, "%.*f", decimals, value);
        if (written < 0 || (size_t)written >= cap) return 0;
        if (strtod(out, NULL) != value) continue;
        size_t length = (size_t)written;
        char *dot = memchr(out, '.', length);
        if (dot) {
            char *end = out + length;
            // Keep one fractional digit: "1.0" reads as a float, "1." does not.
            while (end > dot + 2 && end[-1] == '0') end -= 1;
            length = (size_t)(end - out);
        }
        return length;
    }
    return 0;
}

static size_t ae_f64_scientific(double value, char *out, size_t cap) {
    for (int digits = 1; digits <= 17; digits++) {
        const int written = snprintf(out, cap, "%.*e", digits - 1, value);
        if (written < 0 || (size_t)written >= cap) return 0;
        if (strtod(out, NULL) == value) return (size_t)written;
    }
    return 0;
}

// Rewrites "1.5e-005" as "1.5e-5": lowercase marker, sign always present, and
// no leading zeros in the exponent. Both CRTs already agree on this for the
// values tested, but the language must not depend on that agreement.
static size_t ae_f64_exponent_form(char *text, size_t length) {
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

static size_t ae_f64_text(double value, char *out, size_t cap) {
    if (isnan(value)) { memcpy(out, "nan", 3); return 3; }
    if (isinf(value)) {
        const char *text = value < 0.0 ? "-inf" : "inf";
        const size_t length = strlen(text);
        memcpy(out, text, length);
        return length;
    }
    char fixed[64];
    char scientific[64];
    // Beyond 1e17 fixed notation needs more than 64 bytes; skip it there
    // instead of truncating a value that would then fail to round-trip.
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

static ae_string ae_f64_to_string(double value, size_t line, size_t column) {
    char buffer[96];
    const size_t length = ae_f64_text(value, buffer, sizeof buffer);
    return ae_str_from_len(buffer, (int64_t)length, line, column);
}

static void ae_f64_print(double value) {
    char buffer[96];
    const size_t length = ae_f64_text(value, buffer, sizeof buffer);
    if (!ae_emit(buffer, (int64_t)length, 1)) ae_fail("output error", 0, 0);
}

static double ae_f64_div(double a, double b, size_t line, size_t column) {
    // Same rule as integer division: zero is a reported failure, never a
    // silent infinity. Non-finite results from overflow are still IEEE values.
    if (b == 0.0) ae_fail("f64 division by zero", line, column);
    return a / b;
}
static float ae_f32_div(float a, float b, size_t line, size_t column) {
    if (b == 0.0f) ae_fail("f32 division by zero", line, column);
    return a / b;
}

static double ae_f64_from_i64(int64_t value, size_t line, size_t column) {
    // Every integer up to 2^53 is exact; beyond that the conversion would
    // round silently, so it is refused instead of approximated.
    if (value > INT64_C(9007199254740992) || value < -INT64_C(9007199254740992)) {
        ae_fail("i64 to f64 conversion would lose precision", line, column);
    }
    return (double)value;
}

static int64_t ae_f64_to_i64_trunc(double value, size_t line, size_t column) {
    // `as` truncates toward zero; non-finite and out-of-range values trap.
    if (!isfinite(value) || value < -9223372036854775808.0 || value >= 9223372036854775808.0) {
        ae_fail("float-to-integer cast out of range", line, column);
    }
    return (int64_t)value;
}
static int8_t ae_f64_to_i8_trunc(double value, size_t line, size_t column) {
    if (!isfinite(value) || value <= -129.0 || value >= 128.0) {
        ae_fail("float-to-integer cast out of range", line, column);
    }
    return (int8_t)value;
}
static uint8_t ae_f64_to_u8_trunc(double value, size_t line, size_t column) {
    if (!isfinite(value) || value <= -1.0 || value >= 256.0) {
        ae_fail("float-to-integer cast out of range", line, column);
    }
    return (uint8_t)(int16_t)value;
}
static int16_t ae_f64_to_i16_trunc(double value, size_t line, size_t column) {
    if (!isfinite(value) || value <= -32769.0 || value >= 32768.0) {
        ae_fail("float-to-integer cast out of range", line, column);
    }
    return (int16_t)value;
}
static uint16_t ae_f64_to_u16_trunc(double value, size_t line, size_t column) {
    if (!isfinite(value) || value <= -1.0 || value >= 65536.0) {
        ae_fail("float-to-integer cast out of range", line, column);
    }
    return (uint16_t)(int32_t)value;
}
static int32_t ae_f64_to_i32_trunc(double value, size_t line, size_t column) {
    if (!isfinite(value) || value <= -2147483649.0 || value >= 2147483648.0) {
        ae_fail("float-to-integer cast out of range", line, column);
    }
    return (int32_t)value;
}
static uint32_t ae_f64_to_u32_trunc(double value, size_t line, size_t column) {
    if (!isfinite(value) || value <= -1.0 || value >= 4294967296.0) {
        ae_fail("float-to-integer cast out of range", line, column);
    }
    return (uint32_t)(int64_t)value;
}
static intptr_t ae_f64_to_isize_trunc(double value, size_t line, size_t column) {
    if (!isfinite(value) || value < -9223372036854775808.0 || value >= 9223372036854775808.0) {
        ae_fail("float-to-integer cast out of range", line, column);
    }
    return (intptr_t)value;
}
static uint64_t ae_f64_to_u64_trunc(double value, size_t line, size_t column) {
    if (!isfinite(value) || value <= -1.0 || value >= 18446744073709551616.0) {
        ae_fail("float-to-integer cast out of range", line, column);
    }
    if (value < 0.0) return 0;
    return (uint64_t)value;
}
static uintptr_t ae_f64_to_usize_trunc(double value, size_t line, size_t column) {
    if (!isfinite(value) || value <= -1.0 || value >= 18446744073709551616.0) {
        ae_fail("float-to-integer cast out of range", line, column);
    }
    if (value < 0.0) return 0;
    return (uintptr_t)value;
}
static int8_t ae_f32_to_i8_trunc(float value, size_t line, size_t column) {
    if (!isfinite(value) || value <= -129.0f || value >= 128.0f) ae_fail("float-to-integer cast out of range", line, column);
    return (int8_t)value;
}
static uint8_t ae_f32_to_u8_trunc(float value, size_t line, size_t column) {
    if (!isfinite(value) || value <= -1.0f || value >= 256.0f) ae_fail("float-to-integer cast out of range", line, column);
    return (uint8_t)(int16_t)value;
}
static int16_t ae_f32_to_i16_trunc(float value, size_t line, size_t column) {
    if (!isfinite(value) || value <= -32769.0f || value >= 32768.0f) ae_fail("float-to-integer cast out of range", line, column);
    return (int16_t)value;
}
static uint16_t ae_f32_to_u16_trunc(float value, size_t line, size_t column) {
    if (!isfinite(value) || value <= -1.0f || value >= 65536.0f) ae_fail("float-to-integer cast out of range", line, column);
    return (uint16_t)(int32_t)value;
}
static int32_t ae_f32_to_i32_trunc(float value, size_t line, size_t column) {
    if (!isfinite(value) || value < -2147483648.0f || value >= 2147483648.0f) ae_fail("float-to-integer cast out of range", line, column);
    return (int32_t)value;
}
static uint32_t ae_f32_to_u32_trunc(float value, size_t line, size_t column) {
    if (!isfinite(value) || value <= -1.0f || value >= 4294967296.0f) ae_fail("float-to-integer cast out of range", line, column);
    return (uint32_t)(int64_t)value;
}
static int64_t ae_f32_to_i64_trunc(float value, size_t line, size_t column) {
    if (!isfinite(value) || value < -9223372036854775808.0f || value >= 9223372036854775808.0f) ae_fail("float-to-integer cast out of range", line, column);
    return (int64_t)value;
}
static intptr_t ae_f32_to_isize_trunc(float value, size_t line, size_t column) {
    if (!isfinite(value) || value < -9223372036854775808.0f || value >= 9223372036854775808.0f) ae_fail("float-to-integer cast out of range", line, column);
    return (intptr_t)value;
}
static uint64_t ae_f32_to_u64_trunc(float value, size_t line, size_t column) {
    if (!isfinite(value) || value <= -1.0f || value >= 18446744073709551616.0f) ae_fail("float-to-integer cast out of range", line, column);
    if (value < 0.0f) return 0;
    return (uint64_t)value;
}
static uintptr_t ae_f32_to_usize_trunc(float value, size_t line, size_t column) {
    if (!isfinite(value) || value <= -1.0f || value >= 18446744073709551616.0f) ae_fail("float-to-integer cast out of range", line, column);
    if (value < 0.0f) return 0;
    return (uintptr_t)value;
}

static AE_INT_RESULT ae_f64_to_i64(double value, size_t line, size_t column) {
    if (!isfinite(value)) {
        AE_INT_RESULT bad = { AE_INT_ERR, { .v1 = { ae_str_from("f64 value is not finite", line, column) } } };
        return bad;
    }
    if (value < -9223372036854775808.0 || value >= 9223372036854775808.0) {
        AE_INT_RESULT bad = { AE_INT_ERR, { .v1 = { ae_str_from("f64 value is outside the i64 range", line, column) } } };
        return bad;
    }
    const int64_t converted = (int64_t)value;
    if ((double)converted != value) {
        AE_INT_RESULT bad = { AE_INT_ERR, { .v1 = { ae_str_from("f64 value is not an exact integer", line, column) } } };
        return bad;
    }
    AE_INT_RESULT out = { AE_INT_OK, { .v0 = { converted } } };
    return out;
}

static bool ae_path_exists(const char *path, size_t line, size_t column) { /* win32-header */
    (void)line; (void)column;
#ifdef _WIN32
    wchar_t *wide = ae_widen(path);
    if (!wide) return false;
    const DWORD attributes = GetFileAttributesW(wide);
    ae_free(wide);
    return attributes != INVALID_FILE_ATTRIBUTES;
#else
    struct stat info;
    return stat(path, &info) == 0;
#endif
}

static bool ae_path_is_dir(const char *path, size_t line, size_t column) { /* win32-header */
    (void)line; (void)column;
#ifdef _WIN32
    wchar_t *wide = ae_widen(path);
    if (!wide) return false;
    const DWORD attributes = GetFileAttributesW(wide);
    ae_free(wide);
    return attributes != INVALID_FILE_ATTRIBUTES && (attributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
#else
    struct stat info;
    return stat(path, &info) == 0 && S_ISDIR(info.st_mode);
#endif
}

// Directory listing: entry names only, never "." or "..", sorted by bytes so
// repeated runs and the two platforms agree on order.
static int ae_name_compare(const void *left, const void *right) {
    const ae_string *a = left;
    const ae_string *b = right;
    const int64_t shared = a->len < b->len ? a->len : b->len;
    const int order = shared > 0 ? memcmp(a->ptr, b->ptr, (size_t)shared) : 0;
    if (order != 0) return order;
    return a->len < b->len ? -1 : (a->len > b->len ? 1 : 0);
}
// In-place orderings for owned string vectors, byte-order like keys() and
// directory listings so every sorted output is comparable across platforms.
static void ae_strvec_sort(ae_strvec *v) {
    if (v->len > 1) qsort(v->data, (size_t)v->len, sizeof(ae_string), ae_name_compare);
}
static void ae_strvec_reverse(ae_strvec *v) {
    for (int64_t i = 0, j = v->len - 1; i < j; i = i + 1, j = j - 1) {
        const ae_string swap = v->data[i];
        v->data[i] = v->data[j];
        v->data[j] = swap;
    }
}
static AE_DIR_RESULT ae_try_list_dir(const char *path, size_t line, size_t column) { /* win32-header */
    ae_strvec names = ae_strvec_new(line, column);
#ifdef _WIN32
    const size_t length = strlen(path);
    const bool separated = length > 0 && (path[length - 1] == '\\' || path[length - 1] == '/');
    char *pattern = ae_alloc_try(length + (separated ? 2 : 3));
    if (!pattern) {
        ae_strvec_release(&names);
        AE_DIR_RESULT out = { AE_DIR_ERR, { .v1 = { ae_str_from("out of memory", line, column) } } };
        return out;
    }
    memcpy(pattern, path, length);
    if (!separated) pattern[length] = '\\';
    pattern[length + (separated ? 0 : 1)] = '*';
    pattern[length + (separated ? 1 : 2)] = 0;
    wchar_t *wide = ae_widen(pattern);
    ae_free(pattern);
    if (!wide) {
        ae_strvec_release(&names);
        AE_DIR_RESULT out = { AE_DIR_ERR, { .v1 = { ae_str_from("out of memory", line, column) } } };
        return out;
    }
    WIN32_FIND_DATAW data;
    HANDLE walk = FindFirstFileW(wide, &data);
    ae_free(wide);
    if (walk == INVALID_HANDLE_VALUE) {
        ae_string message = ae_status_message("cannot list directory", (unsigned long)GetLastError(), line, column);
        ae_strvec_release(&names);
        AE_DIR_RESULT out = { AE_DIR_ERR, { .v1 = { message } } };
        return out;
    }
    do {
        if (wcscmp(data.cFileName, L".") == 0 || wcscmp(data.cFileName, L"..") == 0) continue;
        char *utf8 = ae_narrow(data.cFileName, (int)wcslen(data.cFileName));
        if (!utf8) {
            FindClose(walk);
            ae_strvec_release(&names);
            AE_DIR_RESULT out = { AE_DIR_ERR, { .v1 = { ae_str_from("out of memory", line, column) } } };
            return out;
        }
        ae_strvec_push(&names, ae_str_from(utf8, line, column), line, column);
        ae_free(utf8);
    } while (FindNextFileW(walk, &data));
    FindClose(walk);
#else
    DIR *walk = opendir(path);
    if (!walk) {
        ae_string message = ae_io_message(errno, line, column);
        ae_strvec_release(&names);
        AE_DIR_RESULT out = { AE_DIR_ERR, { .v1 = { message } } };
        return out;
    }
    struct dirent *entry;
    while ((entry = readdir(walk)) != NULL) {
        if (strcmp(entry->d_name, ".") == 0 || strcmp(entry->d_name, "..") == 0) continue;
        ae_strvec_push(&names, ae_str_from(entry->d_name, line, column), line, column);
    }
    closedir(walk);
#endif
    if (names.len > 1) qsort(names.data, (size_t)names.len, sizeof(ae_string), ae_name_compare);
    AE_DIR_RESULT out = { AE_DIR_OK, { .v0 = { names } } };
    return out;
}

// ---- Path algebra -----------------------------------------------------------
// Separators: both '/' and '\' are accepted on input, but '/'-joined output is
// what this layer emits, because Windows accepts '/' and identical output on
// both platforms keeps tests and recorded results comparable.

static bool ae_is_separator(char c) { return c == '/' || c == '\\'; }

// Concatenate `dir` and `name`, inserting a separator only when needed.
static ae_string ae_path_concat(ae_strview dir, ae_strview name, bool separator, size_t line, size_t column) {
    const int64_t total = dir.len + name.len + (separator ? 1 : 0);
    char *buffer = ae_alloc((size_t)total + 1, line, column);
    memcpy(buffer, dir.ptr, (size_t)dir.len);
    int64_t at = dir.len;
    if (separator) buffer[at++] = '/';
    memcpy(buffer + at, name.ptr, (size_t)name.len);
    buffer[total] = 0;
    ae_string out = { buffer, total };
    return out;
}
// An absolute name (leading separator, or a drive letter) replaces the
// directory instead of being appended to it.
static bool ae_path_is_absolute(ae_strview name) {
    if (name.len == 0) return false;
    if (ae_is_separator(name.ptr[0])) return true;
    return name.len >= 2 && name.ptr[1] == ':';
}
static ae_string ae_path_join(ae_strview dir, ae_strview name, size_t line, size_t column) {
    if (name.len == 0) return ae_str_copy_view(dir, line, column);
    if (dir.len == 0) return ae_str_copy_view(name, line, column);
    if (ae_path_is_absolute(name)) return ae_str_copy_view(name, line, column);
    const bool separated = ae_is_separator(dir.ptr[dir.len - 1]);
    return ae_path_concat(dir, name, !separated, line, column);
}
// Final component of a path. Trailing separators are dropped first so
// "dir/name/" yields "name"; a path that is only separators keeps one so the
// root stays recognisable.
static ae_string ae_path_name(ae_strview path, size_t line, size_t column) {
    int64_t end = path.len;
    while (end > 1 && ae_is_separator(path.ptr[end - 1])) end--;
    int64_t start = end;
    while (start > 0 && !ae_is_separator(path.ptr[start - 1])) start--;
    if (start == end) {
        // All separators: the name of the root is the root marker itself.
        ae_strview root = { path.ptr, end };
        return ae_str_copy_view(root, line, column);
    }
    ae_strview tail = { path.ptr + start, end - start };
    return ae_str_copy_view(tail, line, column);
}
// Everything before the final component. A path without a separator has no
// parent, which reports as the empty string rather than a fabricated "." .
static ae_string ae_path_parent(ae_strview path, size_t line, size_t column) {
    int64_t end = path.len;
    while (end > 1 && ae_is_separator(path.ptr[end - 1])) end--;
    int64_t at = end;
    while (at > 0 && !ae_is_separator(path.ptr[at - 1])) at--;
    if (at == 0) return ae_str_from_len("", 0, line, column);
    int64_t stop = at;
    while (stop > 1 && ae_is_separator(path.ptr[stop - 1])) stop--;
#ifdef _WIN32
    // Keep the separator after a drive prefix: "C:/dir" has parent "C:/".
    if (stop == 2 && path.ptr[1] == ':' && at > stop) stop = at;
#endif
    if (stop == 1 && ae_is_separator(path.ptr[0])) stop = 1;
    return ae_str_from_len(path.ptr, stop, line, column);
}

// ---- Metadata ---------------------------------------------------------------
// Size in bytes, modification time in seconds since the Unix epoch, and whether
// the target is a directory. Failure is a value, never a silent zero size.
static AE_FILE_RESULT ae_try_file_info(const char *path, size_t line, size_t column) { /* win32-header */
#ifdef _WIN32
    wchar_t *wide = ae_widen(path);
    if (!wide) {
        AE_FILE_RESULT out = { AE_FILE_ERR, { .v1 = { ae_str_from("out of memory", line, column) } } };
        return out;
    }
    WIN32_FILE_ATTRIBUTE_DATA data;
    if (!GetFileAttributesExW(wide, GetFileExInfoStandard, &data)) {
        const unsigned long status = (unsigned long)GetLastError();
        ae_free(wide);
        AE_FILE_RESULT out = { AE_FILE_ERR, { .v1 = { ae_status_message("cannot read file metadata", status, line, column) } } };
        return out;
    }
    ae_free(wide);
    const int64_t size = (int64_t)(((uint64_t)data.nFileSizeHigh << 32) | (uint64_t)data.nFileSizeLow);
    // FILETIME counts 100 ns ticks from 1601-01-01; this offset lands on the
    // Unix epoch, so the number matches st_mtime on POSIX.
    const int64_t ticks = (int64_t)(((uint64_t)data.ftLastWriteTime.dwHighDateTime << 32) | (uint64_t)data.ftLastWriteTime.dwLowDateTime);
    const int64_t modified = ticks / 10000000 - 11644473600LL;
    const bool is_dir = (data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
    AE_FILE_RESULT out = { AE_FILE_OK, { .v0 = { { size, modified, is_dir } } } };
    return out;
#else
    errno = 0;
    struct stat info;
    if (stat(path, &info) != 0) {
        AE_FILE_RESULT out = { AE_FILE_ERR, { .v1 = { ae_io_message(errno, line, column) } } };
        return out;
    }
    AE_FILE_RESULT out = { AE_FILE_OK, { .v0 = { { (int64_t)info.st_size, (int64_t)info.st_mtime, S_ISDIR(info.st_mode) } } } };
    return out;
#endif
}

// ---- Mutation --------------------------------------------------------------
static AE_STATUS_RESULT ae_try_copy_file(const char *from, const char *to, size_t line, size_t column) {
    errno = 0;
    FILE *source = ae_fopen(from, "rb");
    if (!source) return ae_status_failed(ae_io_message(errno, line, column));
    FILE *target = ae_fopen(to, "wb");
    if (!target) {
        fclose(source);
        return ae_status_failed(ae_io_message(errno, line, column));
    }
    ae_string failure = { NULL, 0 };
    char buffer[65536];
    size_t read;
    while ((read = fread(buffer, 1, sizeof buffer, source)) > 0) {
        if (fwrite(buffer, 1, read, target) != read) { failure = ae_io_message(errno, line, column); break; }
    }
    if (!failure.ptr && ferror(source)) failure = ae_io_message(errno, line, column);
    if (fclose(source) != 0 && !failure.ptr) failure = ae_io_message(errno, line, column);
    if (fclose(target) != 0 && !failure.ptr) failure = ae_io_message(errno, line, column);
    if (failure.ptr) return ae_status_failed(failure);
    return ae_status_ok();
}

static AE_STATUS_RESULT ae_try_rename_path(const char *from, const char *to, size_t line, size_t column) { /* win32-header */
#ifdef _WIN32
    errno = 0;
    wchar_t *source = ae_widen(from);
    wchar_t *target = ae_widen(to);
    if (!source || !target) {
        ae_free(source); ae_free(target);
        return ae_status_failed(ae_str_from("out of memory", line, column));
    }
    const bool moved = MoveFileExW(source, target, MOVEFILE_REPLACE_EXISTING | MOVEFILE_COPY_ALLOWED) != 0;
    const unsigned long status = (unsigned long)GetLastError();
    ae_free(source); ae_free(target);
    if (!moved) return ae_status_failed(ae_status_message("cannot rename path", status, line, column));
    return ae_status_ok();
#else
    errno = 0;
    if (rename(from, to) != 0) return ae_status_failed(ae_io_message(errno, line, column));
    return ae_status_ok();
#endif
}

// Removes a file. Directories are refused on both platforms: POSIX remove()
// would also delete an empty directory, and reporting the same outcome on two
// platforms matters more than the extra convenience.
static AE_STATUS_RESULT ae_try_remove_file(const char *path, size_t line, size_t column) { /* win32-header */
    if (ae_path_is_dir(path, line, column)) {
        return ae_status_failed(ae_str_from("cannot remove a directory with try_remove_file", line, column));
    }
#ifdef _WIN32
    errno = 0;
    wchar_t *wide = ae_widen(path);
    if (!wide) return ae_status_failed(ae_str_from("out of memory", line, column));
    const bool removed = DeleteFileW(wide) != 0;
    const unsigned long status = (unsigned long)GetLastError();
    ae_free(wide);
    if (!removed) return ae_status_failed(ae_status_message("cannot remove file", status, line, column));
    return ae_status_ok();
#else
    errno = 0;
    if (remove(path) != 0) return ae_status_failed(ae_io_message(errno, line, column));
    return ae_status_ok();
#endif
}

// Returns an empty string on success, or the failure text. Keeping the message
// as the return value lets the caller decide whether a race is acceptable.
static ae_string ae_make_one_dir(const char *path, size_t line, size_t column) { /* win32-header */
    ae_string none = { NULL, 0 };
#ifdef _WIN32
    wchar_t *wide = ae_widen(path);
    if (!wide) return ae_str_from("out of memory", line, column);
    if (CreateDirectoryW(wide, NULL)) { ae_free(wide); return none; }
    const unsigned long status = (unsigned long)GetLastError();
    ae_free(wide);
    if (status == ERROR_ALREADY_EXISTS) return none;
    return ae_status_message("cannot create directory", status, line, column);
#else
    errno = 0;
    if (mkdir(path, 0777) == 0) return none;
    if (errno == EEXIST) return none;
    return ae_io_message(errno, line, column);
#endif
}
// Creates the directory and every missing parent, so a caller can write
// "out/2026/march/" in one step. Existing directories are not an error.
static AE_STATUS_RESULT ae_try_create_dir(const char *path, size_t line, size_t column) {
    const int64_t length = (int64_t)strlen(path);
    if (length == 0) return ae_status_failed(ae_str_from("empty path", line, column));
    char *buffer = ae_alloc_try((size_t)length + 1);
    if (!buffer) return ae_status_failed(ae_str_from("out of memory", line, column));
    memcpy(buffer, path, (size_t)length + 1);
    ae_string failure = { NULL, 0 };
    for (int64_t at = 1; at <= length; at++) {
        if (at < length && !ae_is_separator(buffer[at])) continue;
        const char saved = buffer[at];
        buffer[at] = 0;
        bool skip = false;
#ifdef _WIN32
        if (at == 2 && buffer[1] == ':') skip = true; // drive prefix
#endif
        if (!skip && !ae_path_is_dir(buffer, line, column)) {
            failure = ae_make_one_dir(buffer, line, column);
        }
        buffer[at] = saved;
        if (failure.ptr) break;
    }
    ae_free(buffer);
    if (failure.ptr) return ae_status_failed(failure);
    return ae_status_ok();
}

// Changes the process working directory. This is a global mutation: every
// later relative path, including a child process's, resolves against the new
// directory. It is a result value for the same reason the rest are - the
// target may be gone or unreadable - and no undo is provided, so callers keep
// the old cwd() if they intend to come back.
static AE_STATUS_RESULT ae_set_cwd(const char *path, size_t line, size_t column) { /* win32-header */
#ifdef _WIN32
    wchar_t *wide = ae_widen(path);
    if (!wide) return ae_status_failed(ae_str_from("out of memory", line, column));
    const BOOL ok = SetCurrentDirectoryW(wide);
    const unsigned long status = ok ? 0UL : (unsigned long)GetLastError();
    ae_free(wide);
    if (!ok) return ae_status_failed(ae_status_message("cannot change the working directory", status, line, column));
    return ae_status_ok();
#else
    errno = 0;
    if (chdir(path) != 0) return ae_status_failed(ae_io_message(errno, line, column));
    return ae_status_ok();
#endif
}

// Removes an empty directory. Recursive removal is deliberately absent: a
// helper that silently deletes a tree is the easiest way to lose data.
static AE_STATUS_RESULT ae_try_remove_dir(const char *path, size_t line, size_t column) { /* win32-header */
#ifdef _WIN32
    wchar_t *wide = ae_widen(path);
    if (!wide) return ae_status_failed(ae_str_from("out of memory", line, column));
    const bool removed = RemoveDirectoryW(wide) != 0;
    const unsigned long status = (unsigned long)GetLastError();
    ae_free(wide);
    if (!removed) return ae_status_failed(ae_status_message("cannot remove directory", status, line, column));
    return ae_status_ok();
#else
    errno = 0;
    if (rmdir(path) != 0) return ae_status_failed(ae_io_message(errno, line, column));
    return ae_status_ok();
#endif
}

// ---- Map helpers that need the prelude result types -------------------------
// A missing key is a value, not a zero: `0` is a perfectly normal count.
static AE_INT_RESULT ae_map_get(const ae_map *m, ae_strview key, size_t line, size_t column) {
    // A miss allocates nothing; the location stays for a uniform signature.
    (void)line; (void)column;
    int64_t value = 0;
    if (!ae_map_lookup(m, key, &value)) {
        // Still an owned string; release recognises this one address.
        ae_string reason = { (char *)ae_missing_key_text, (int64_t)(sizeof ae_missing_key_text - 1) };
        AE_INT_RESULT out = { AE_INT_ERR, { .v1 = { reason } } };
        return out;
    }
    AE_INT_RESULT out = { AE_INT_OK, { .v0 = { value } } };
    return out;
}
// Keys are copied and sorted by bytes, so iteration is reproducible and the
// caller owns a plain string vector.
static ae_strvec ae_map_keys(const ae_map *m, size_t line, size_t column) {
    ae_strvec keys;
    keys.len = 0;
    keys.cap = m->len > 4 ? m->len : 4;
    keys.data = ae_alloc((size_t)keys.cap * sizeof(ae_string), line, column);
    keys.arena = NULL;
    keys.arena_len = 0;
    for (int64_t i = 0; i < m->cap; i++) {
        if (m->states[i] != AE_MAP_FULL) continue;
        keys.data[keys.len++] = ae_str_copy_view(AE_VIEW(m->slots[i].key), line, column);
    }
    if (keys.len > 1) qsort(keys.data, (size_t)keys.len, sizeof(ae_string), ae_name_compare);
    return keys;
}

// ---- Hash map: string keys -> f64 values (ADR-65) ---------------------------
// Same open-addressing design as the i64 map: 24-byte slots (key string plus
// one double), parallel state bytes, half-load rehash. Hashing and slot
// math are shared with the i64 map; only the value type differs.
typedef struct { ae_string key; double value; } ae_mapf_slot;
typedef struct { ae_mapf_slot *slots; unsigned char *states; int64_t cap; int64_t len; int64_t used; } ae_mapf;

static ae_mapf ae_mapf_new(size_t line, size_t column) {
    (void)line; (void)column;
    ae_mapf m;
    m.slots = NULL;
    m.states = NULL;
    m.cap = 0;
    m.len = 0;
    m.used = 0;
    return m;
}
static int64_t ae_mapf_find(const ae_mapf *m, ae_strview key) {
    if (m->cap == 0) return -1;
    int64_t at = ae_map_slot_of(key, m->cap);
    for (int64_t probes = 0; probes < m->cap; probes++) {
        const unsigned char state = m->states[at];
        if (state == AE_MAP_EMPTY) return -1;
        if (state == AE_MAP_FULL && ae_str_eq(AE_VIEW(m->slots[at].key), key)) return at;
        at = (at + 1) & (m->cap - 1);
    }
    return -1;
}
static void ae_mapf_grow(ae_mapf *m, size_t line, size_t column) {
    int64_t cap = m->cap == 0 ? 8 : m->cap;
    while ((m->len + 1) * 2 >= cap) {
        if (cap > INT64_MAX / 4) ae_fail("map too large", line, column);
        cap = cap * 2;
    }
    if (cap == m->cap && m->used == m->len) return;
    ae_mapf_slot *slots = ae_alloc((size_t)cap * sizeof(ae_mapf_slot), line, column);
    unsigned char *states = ae_alloc((size_t)cap, line, column);
    memset(states, AE_MAP_EMPTY, (size_t)cap);
    for (int64_t i = 0; i < m->cap; i++) {
        if (m->states[i] != AE_MAP_FULL) continue;
        int64_t at = ae_map_slot_of(AE_VIEW(m->slots[i].key), cap);
        while (states[at] == AE_MAP_FULL) at = (at + 1) & (cap - 1);
        slots[at] = m->slots[i];
        states[at] = AE_MAP_FULL;
    }
    ae_free(m->slots);
    ae_free(m->states);
    m->slots = slots;
    m->states = states;
    m->cap = cap;
    m->used = m->len;
}
static void ae_mapf_put(ae_mapf *m, ae_strview key, double value, size_t line, size_t column) {
    ae_mapf_grow(m, line, column);
    int64_t at = ae_map_slot_of(key, m->cap);
    int64_t reuse = -1;
    for (;;) {
        const unsigned char state = m->states[at];
        if (state == AE_MAP_EMPTY) break;
        if (state == AE_MAP_DELETED) {
            if (reuse < 0) reuse = at;
        } else if (ae_str_eq(AE_VIEW(m->slots[at].key), key)) {
            m->slots[at].value = value;
            return;
        }
        at = (at + 1) & (m->cap - 1);
    }
    if (reuse >= 0) at = reuse; else m->used = m->used + 1;
    m->slots[at].key = ae_str_from_len(key.ptr, key.len, line, column);
    m->slots[at].value = value;
    m->states[at] = AE_MAP_FULL;
    m->len = m->len + 1;
}
static AE_FLOAT_RESULT ae_mapf_get(const ae_mapf *m, ae_strview key, size_t line, size_t column) {
    (void)line; (void)column;
    int64_t at = ae_mapf_find(m, key);
    if (at < 0) {
        AE_FLOAT_RESULT missing = { AE_FLOAT_ERR, { .v1 = { { (char *)ae_missing_key_text, (int64_t)(sizeof ae_missing_key_text - 1) } } } };
        return missing;
    }
    AE_FLOAT_RESULT out = { AE_FLOAT_OK, { .v0 = { m->slots[at].value } } };
    return out;
}
static bool ae_mapf_has(const ae_mapf *m, ae_strview key, size_t line, size_t column) {
    (void)line; (void)column;
    return ae_mapf_find(m, key) >= 0;
}
static bool ae_mapf_remove(ae_mapf *m, ae_strview key, size_t line, size_t column) {
    (void)line; (void)column;
    int64_t at = ae_mapf_find(m, key);
    if (at < 0) return false;
    ae_str_release(&m->slots[at].key);
    m->states[at] = AE_MAP_DELETED;
    m->len = m->len - 1;
    return true;
}
static double ae_mapf_inc(ae_mapf *m, ae_strview key, double delta, size_t line, size_t column) {
    ae_mapf_grow(m, line, column);
    int64_t at = ae_map_slot_of(key, m->cap);
    int64_t reuse = -1;
    for (;;) {
        const unsigned char state = m->states[at];
        if (state == AE_MAP_EMPTY) break;
        if (state == AE_MAP_DELETED) {
            if (reuse < 0) reuse = at;
        } else if (ae_str_eq(AE_VIEW(m->slots[at].key), key)) {
            m->slots[at].value = m->slots[at].value + delta;
            return m->slots[at].value;
        }
        at = (at + 1) & (m->cap - 1);
    }
    if (reuse >= 0) at = reuse; else m->used = m->used + 1;
    m->slots[at].key = ae_str_from_len(key.ptr, key.len, line, column);
    m->slots[at].value = delta;
    m->states[at] = AE_MAP_FULL;
    m->len = m->len + 1;
    return delta;
}
static int64_t ae_mapf_len(const ae_mapf *m, size_t line, size_t column) {
    (void)line; (void)column;
    return m->len;
}
static void ae_mapf_release(ae_mapf *m) {
    if (m->slots) {
        for (int64_t i = 0; i < m->cap; i++) {
            if (m->states[i] == AE_MAP_FULL) ae_str_release(&m->slots[i].key);
        }
    }
    ae_free(m->slots);
    ae_free(m->states);
    m->slots = NULL;
    m->states = NULL;
    m->cap = 0;
    m->len = 0;
    m->used = 0;
}
static void ae_mapf_forfeit(ae_mapf *m) {
    m->slots = NULL;
    m->states = NULL;
    m->cap = 0;
    m->len = 0;
    m->used = 0;
}
static ae_strvec ae_mapf_keys(const ae_mapf *m, size_t line, size_t column) {
    ae_strvec keys;
    keys.len = 0;
    keys.cap = m->len > 4 ? m->len : 4;
    keys.data = ae_alloc((size_t)keys.cap * sizeof(ae_string), line, column);
    keys.arena = NULL;
    keys.arena_len = 0;
    for (int64_t i = 0; i < m->cap; i++) {
        if (m->states[i] != AE_MAP_FULL) continue;
        keys.data[keys.len++] = ae_str_copy_view(AE_VIEW(m->slots[i].key), line, column);
    }
    if (keys.len > 1) qsort(keys.data, (size_t)keys.len, sizeof(ae_string), ae_name_compare);
    return keys;
}

// ---- JSON reader (ADR-26) ---------------------------------------------------
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

static void ae_json_ws(const char *t, int64_t n, int64_t *i) {
    while (*i < n) {
        const char c = t[*i];
        if (c != ' ' && c != '\t' && c != '\n' && c != '\r') break;
        *i += 1;
    }
}
static int ae_json_hex(char c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}
static int ae_json_utf8(char *out, unsigned cp) {
    if (cp < 0x80u) { out[0] = (char)cp; return 1; }
    if (cp < 0x800u) { out[0] = (char)(0xC0u | (cp >> 6)); out[1] = (char)(0x80u | (cp & 0x3Fu)); return 2; }
    if (cp < 0x10000u) { out[0] = (char)(0xE0u | (cp >> 12)); out[1] = (char)(0x80u | ((cp >> 6) & 0x3Fu)); out[2] = (char)(0x80u | (cp & 0x3Fu)); return 3; }
    out[0] = (char)(0xF0u | (cp >> 18)); out[1] = (char)(0x80u | ((cp >> 12) & 0x3Fu)); out[2] = (char)(0x80u | ((cp >> 6) & 0x3Fu)); out[3] = (char)(0x80u | (cp & 0x3Fu)); return 4;
}
static int ae_json_skip_string(const char *t, int64_t n, int64_t *i) {
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
static int ae_json_skip_value(const char *t, int64_t n, int64_t *i, int depth) {
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
static int ae_json_decode(const char *t, int64_t n, int64_t *i, ae_string *out, size_t line, size_t column) {
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
    char *buffer = ae_alloc((size_t)raw + 1, line, column);
    int64_t s = *i + 1;
    int64_t k = 0;
    while (s < j) {
        const unsigned char c = (unsigned char)t[s];
        if (c != '\\') { buffer[k] = (char)c; k += 1; s += 1; continue; }
        s += 1;
        if (s >= j) { ae_free(buffer); return 0; }
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
                if (s + 4 > j) { ae_free(buffer); return 0; }
                for (int d = 0; d < 4; d++) {
                    const int v = ae_json_hex(t[s + d]);
                    if (v < 0) { ae_free(buffer); return 0; }
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
                if (cp == 0) { ae_free(buffer); return 0; }
                k += ae_json_utf8(buffer + k, cp);
                break;
            }
            default: ae_free(buffer); return 0;
        }
    }
    buffer[k] = 0;
    out->ptr = buffer;
    out->len = k;
    *i = j + 1;
    return 1;
}
static int ae_json_kind(const char *t, int64_t n, int64_t start) {
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
static int ae_json_member(const char *t, int64_t n, int64_t *pos, const char *key, int64_t key_len, int64_t *vs, int64_t *ve, int *found, size_t line, size_t column) {
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
            ae_string name;
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
static int ae_json_element(const char *t, int64_t n, int64_t *pos, int64_t index, int64_t *vs, int64_t *ve, int *found) {
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
static int ae_json_path_check(const char *path, int64_t plen, char *why, size_t why_cap) {
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
static int ae_json_locate(const char *t, int64_t n, const char *path, int64_t plen, int64_t *vs, int64_t *ve, int *kind, char *why, size_t why_cap, size_t line, size_t column) {
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
static int ae_json_count(const char *t, int64_t n, int64_t start, int kind) {
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
static AE_READ_RESULT ae_json_get(const char *doc_text, int64_t doc_size, const char *path_text, int64_t path_size, size_t line, size_t column) {
    const ae_strview doc = { doc_text, doc_size };
    const ae_strview path = { path_text, path_size };

    int64_t vs = 0;
    int64_t ve = 0;
    int kind = 0;
    char why[192];
    if (ae_json_locate(doc.ptr, doc.len, path.ptr, path.len, &vs, &ve, &kind, why, sizeof why, line, column) != AE_JSON_OK) {
        AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_str_from(why, line, column) } } };
        return out;
    }
    ae_string value;
    if (kind == AE_JSON_STRING) {
        int64_t at = vs;
        if (!ae_json_decode(doc.ptr, doc.len, &at, &value, line, column)) {
            AE_READ_RESULT out = { AE_READ_ERR, { .v1 = { ae_str_from("JSON string could not be decoded", line, column) } } };
            return out;
        }
    } else {
        value = ae_str_from_len(doc.ptr + vs, ve - vs, line, column);
    }
    AE_READ_RESULT out = { AE_READ_OK, { .v0 = { value } } };
    return out;
}
static AE_INT_RESULT ae_json_int(const char *doc_text, int64_t doc_size, const char *path_text, int64_t path_size, size_t line, size_t column) {
    const ae_strview doc = { doc_text, doc_size };
    const ae_strview path = { path_text, path_size };

    int64_t vs = 0;
    int64_t ve = 0;
    int kind = 0;
    char why[192];
    if (ae_json_locate(doc.ptr, doc.len, path.ptr, path.len, &vs, &ve, &kind, why, sizeof why, line, column) != AE_JSON_OK) {
        AE_INT_RESULT out = { AE_INT_ERR, { .v1 = { ae_str_from(why, line, column) } } };
        return out;
    }
    if (kind != AE_JSON_NUMBER) {
        snprintf(why, sizeof why, "'%.*s' is %s, not a number", (int)path.len, path.ptr, ae_json_kind_name(kind));
        AE_INT_RESULT out = { AE_INT_ERR, { .v1 = { ae_str_from(why, line, column) } } };
        return out;
    }
    int64_t i = vs;
    int negative = 0;
    if (i < ve && doc.ptr[i] == '-') { negative = 1; i += 1; }
    if (i >= ve) {
        snprintf(why, sizeof why, "'%.*s' is not an integer", (int)path.len, path.ptr);
        AE_INT_RESULT out = { AE_INT_ERR, { .v1 = { ae_str_from(why, line, column) } } };
        return out;
    }
    int64_t value = 0;
    for (; i < ve; i++) {
        const char d = doc.ptr[i];
        if (d < '0' || d > '9') {
            // A fraction or exponent is a real number; this reader has no float type.
            snprintf(why, sizeof why, "'%.*s' is not an integer", (int)path.len, path.ptr);
            AE_INT_RESULT out = { AE_INT_ERR, { .v1 = { ae_str_from(why, line, column) } } };
            return out;
        }
        const int digit = d - '0';
        if (value > (INT64_MAX - digit) / 10) {
            snprintf(why, sizeof why, "'%.*s' does not fit in i64", (int)path.len, path.ptr);
            AE_INT_RESULT out = { AE_INT_ERR, { .v1 = { ae_str_from(why, line, column) } } };
            return out;
        }
        value = value * 10 + digit;
    }
    AE_INT_RESULT out = { AE_INT_OK, { .v0 = { negative ? -value : value } } };
    return out;
}
static AE_BOOL_RESULT ae_json_bool(const char *doc_text, int64_t doc_size, const char *path_text, int64_t path_size, size_t line, size_t column) {
    const ae_strview doc = { doc_text, doc_size };
    const ae_strview path = { path_text, path_size };

    int64_t vs = 0;
    int64_t ve = 0;
    int kind = 0;
    char why[192];
    if (ae_json_locate(doc.ptr, doc.len, path.ptr, path.len, &vs, &ve, &kind, why, sizeof why, line, column) != AE_JSON_OK) {
        AE_BOOL_RESULT out = { AE_BOOL_ERR, { .v1 = { ae_str_from(why, line, column) } } };
        return out;
    }
    const int64_t width = ve - vs;
    bool value = false;
    if (kind == AE_JSON_BOOL && width == 4 && memcmp(doc.ptr + vs, "true", 4) == 0) value = true;
    else if (!(kind == AE_JSON_BOOL && width == 5 && memcmp(doc.ptr + vs, "false", 5) == 0)) {
        snprintf(why, sizeof why, "'%.*s' is %s, not a boolean", (int)path.len, path.ptr, ae_json_kind_name(kind));
        AE_BOOL_RESULT out = { AE_BOOL_ERR, { .v1 = { ae_str_from(why, line, column) } } };
        return out;
    }
    AE_BOOL_RESULT out = { AE_BOOL_OK, { .v0 = { value } } };
    return out;
}
static AE_INT_RESULT ae_json_len(const char *doc_text, int64_t doc_size, const char *path_text, int64_t path_size, size_t line, size_t column) {
    const ae_strview doc = { doc_text, doc_size };
    const ae_strview path = { path_text, path_size };

    int64_t vs = 0;
    int64_t ve = 0;
    int kind = 0;
    char why[192];
    if (ae_json_locate(doc.ptr, doc.len, path.ptr, path.len, &vs, &ve, &kind, why, sizeof why, line, column) != AE_JSON_OK) {
        AE_INT_RESULT out = { AE_INT_ERR, { .v1 = { ae_str_from(why, line, column) } } };
        return out;
    }
    if (kind != AE_JSON_ARRAY && kind != AE_JSON_OBJECT) {
        snprintf(why, sizeof why, "'%.*s' is %s; only arrays and objects have a member count", (int)path.len, path.ptr, ae_json_kind_name(kind));
        AE_INT_RESULT out = { AE_INT_ERR, { .v1 = { ae_str_from(why, line, column) } } };
        return out;
    }
    const int64_t count = ae_json_count(doc.ptr, doc.len, vs, kind);
    if (count < 0) {
        AE_INT_RESULT out = { AE_INT_ERR, { .v1 = { ae_str_from("JSON syntax error while counting members", line, column) } } };
        return out;
    }
    AE_INT_RESULT out = { AE_INT_OK, { .v0 = { count } } };
    return out;
}
static bool ae_json_has(const char *doc_text, int64_t doc_size, const char *path_text, int64_t path_size, size_t line, size_t column) {
    const ae_strview doc = { doc_text, doc_size };
    const ae_strview path = { path_text, path_size };

    int64_t vs = 0;
    int64_t ve = 0;
    int kind = 0;
    char why[192];
    return ae_json_locate(doc.ptr, doc.len, path.ptr, path.len, &vs, &ve, &kind, why, sizeof why, line, column) == AE_JSON_OK;
}

// ---- Child processes (ADR-29) ----------------------------------------------
// The child's stdout and stderr share one pipe: a program that reports its
// failure on stderr still comes back in the order it was written, and the
// parent never has to drain two pipes at once (which deadlocks as soon as one
// of them fills). A non-zero exit status is a result, not an error.
static AE_COMMAND_RESULT ae_command_failed(ae_string message) {
    AE_COMMAND_RESULT out = { AE_COMMAND_ERR, { .v1 = { message } } };
    return out;
}
#ifndef _WIN32
static AE_COMMAND_RESULT ae_run_command(const char *program, const ae_strvec *args, size_t line, size_t column) {
    int fds[2];
    if (pipe(fds) != 0) return ae_command_failed(ae_io_message(errno, line, column));
    // A second pipe carries the exec failure back: closing on exec (CLOEXEC)
    // turns "exec worked" into EOF, so the parent can tell "could not start the
    // program" apart from "the program chose to exit 127".
    int errors[2];
    if (pipe(errors) != 0) {
        const int code = errno;
        close(fds[0]);
        close(fds[1]);
        return ae_command_failed(ae_io_message(code, line, column));
    }
    (void)fcntl(errors[1], F_SETFD, FD_CLOEXEC);
    const pid_t pid = fork();
    if (pid < 0) {
        const int code = errno;
        close(fds[0]);
        close(fds[1]);
        close(errors[0]);
        close(errors[1]);
        return ae_command_failed(ae_io_message(code, line, column));
    }
    if (pid == 0) {
        // Child: point both streams at the pipe, then become the program.
        // _exit skips the parent's stdio buffers and atexit work.
        if (dup2(fds[1], STDOUT_FILENO) < 0 || dup2(fds[1], STDERR_FILENO) < 0) _exit(127);
        close(fds[0]);
        close(fds[1]);
        close(errors[0]);
        char **argv = ae_alloc_try(sizeof(char *) * (size_t)(args->len + 2));
        if (!argv) _exit(127);
        argv[0] = (char *)program;
        for (int64_t i = 0; i < args->len; i++) argv[i + 1] = args->data[i].ptr;
        argv[args->len + 1] = NULL;
        execvp(program, argv);
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
        close(fds[0]);
        int ignored = 0;
        while (waitpid(pid, &ignored, 0) < 0 && errno == EINTR) {}
        return ae_command_failed(ae_status_message("cannot start the program", (unsigned long)exec_error, line, column));
    }
    int64_t capacity = 4096;
    int64_t length = 0;
    char *buffer = ae_alloc((size_t)capacity + 1, line, column);
    for (;;) {
        if (length == capacity) {
            char *grown = ae_alloc((size_t)capacity * 2 + 1, line, column);
            memcpy(grown, buffer, (size_t)length);
            ae_free(buffer);
            buffer = grown;
            capacity = capacity * 2;
        }
        const ssize_t got = read(fds[0], buffer + length, (size_t)(capacity - length));
        if (got < 0) {
            if (errno == EINTR) continue;
            const int code = errno;
            ae_free(buffer);
            close(fds[0]);
            return ae_command_failed(ae_io_message(code, line, column));
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
            const int code = errno;
            ae_free(buffer);
            return ae_command_failed(ae_io_message(code, line, column));
        }
    }
    // A signal is reported the way a shell reports it (128 + number), so the
    // caller sees one integer either way.
    const int64_t code = WIFEXITED(status) ? (int64_t)WEXITSTATUS(status)
                                           : (int64_t)(128 + (WIFSIGNALED(status) ? WTERMSIG(status) : 0));
    ae_string output = { buffer, length };
    AE_COMMAND_RESULT out = { AE_COMMAND_OK, { .v0 = { { code, output } } } };
    return out;
}
#else
static AE_COMMAND_RESULT ae_run_command(const char *program, const ae_strvec *args, size_t line, size_t column) { /* win32-header */
    // Quotes and trailing backslashes are refused instead of escaped: guessing
    // CreateProcess's quoting rules wrong silently changes what the child runs.
    for (int64_t i = 0; i < args->len; i++) {
        const ae_string arg = args->data[i];
        for (int64_t k = 0; k < arg.len; k++) {
            if (arg.ptr[k] == '"') return ae_command_failed(ae_str_from("command arguments must not contain a double quote", line, column));
        }
        if (arg.len > 0 && arg.ptr[arg.len - 1] == '\\') return ae_command_failed(ae_str_from("command arguments must not end with a backslash", line, column));
    }
    for (const char *c = program; *c; c++) {
        if (*c == '"') return ae_command_failed(ae_str_from("the program name must not contain a double quote", line, column));
    }
    // Build the command line in UTF-8 first: the quoting characters are ASCII,
    // so one conversion at the end is enough.
    int64_t width = (int64_t)strlen(program) + 3;
    for (int64_t i = 0; i < args->len; i++) width += args->data[i].len + 3;
    char *command = ae_alloc((size_t)width, line, column);
    int64_t at = 0;
    command[at++] = '"';
    for (const char *c = program; *c; c++) command[at++] = *c;
    command[at++] = '"';
    for (int64_t i = 0; i < args->len; i++) {
        const ae_string arg = args->data[i];
        int spaced = 0;
        for (int64_t k = 0; k < arg.len; k++) {
            if (arg.ptr[k] == ' ' || arg.ptr[k] == '\t') { spaced = 1; break; }
        }
        // Quote only what has to be quoted. A shell that re-parses its own
        // command line treats a quoted flag as part of the command name, so
        // quoting every argument breaks the commonest helper program there is.
        command[at++] = ' ';
        if (spaced) command[at++] = '"';
        memcpy(command + at, arg.ptr, (size_t)arg.len);
        at += arg.len;
        if (spaced) command[at++] = '"';
    }
    command[at] = 0;

    SECURITY_ATTRIBUTES sa;
    ZeroMemory(&sa, sizeof sa);
    sa.nLength = sizeof sa;
    sa.bInheritHandle = TRUE;
    HANDLE read_end = NULL;
    HANDLE write_end = NULL;
    if (!CreatePipe(&read_end, &write_end, &sa, 0)) {
        ae_free(command);
        return ae_command_failed(ae_status_message("cannot create a pipe", (unsigned long)GetLastError(), line, column));
    }
    SetHandleInformation(read_end, HANDLE_FLAG_INHERIT, 0);
    STARTUPINFOW si;
    ZeroMemory(&si, sizeof si);
    si.cb = sizeof si;
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdOutput = write_end;
    si.hStdError = write_end;
    si.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
    PROCESS_INFORMATION pi;
    ZeroMemory(&pi, sizeof pi);
    wchar_t *wide = ae_widen(command);
    ae_free(command);
    if (!wide) {
        CloseHandle(read_end);
        CloseHandle(write_end);
        return ae_command_failed(ae_str_from("out of memory", line, column));
    }
    const BOOL started = CreateProcessW(NULL, wide, NULL, NULL, TRUE, CREATE_NO_WINDOW, NULL, NULL, &si, &pi);
    const DWORD failure = started ? 0 : GetLastError();
    ae_free(wide);
    CloseHandle(write_end);
    if (!started) {
        CloseHandle(read_end);
        return ae_command_failed(ae_status_message("cannot start the program", (unsigned long)failure, line, column));
    }
    CloseHandle(pi.hThread);
    int64_t capacity = 4096;
    int64_t length = 0;
    char *buffer = ae_alloc((size_t)capacity + 1, line, column);
    for (;;) {
        if (length == capacity) {
            char *grown = ae_alloc((size_t)capacity * 2 + 1, line, column);
            memcpy(grown, buffer, (size_t)length);
            ae_free(buffer);
            buffer = grown;
            capacity = capacity * 2;
        }
        DWORD got = 0;
        const int64_t room = capacity - length;
        if (!ReadFile(read_end, buffer + length, (DWORD)(room > 0x7FFFFFFF ? 0x7FFFFFFF : room), &got, NULL)) {
            const DWORD code = GetLastError();
            if (code == ERROR_BROKEN_PIPE) break;
            ae_free(buffer);
            CloseHandle(read_end);
            CloseHandle(pi.hProcess);
            return ae_command_failed(ae_status_message("cannot read the program output", (unsigned long)code, line, column));
        }
        if (got == 0) break;
        length += (int64_t)got;
    }
    CloseHandle(read_end);
    buffer[length] = 0;
    WaitForSingleObject(pi.hProcess, INFINITE);
    DWORD status = 0;
    GetExitCodeProcess(pi.hProcess, &status);
    CloseHandle(pi.hProcess);
    ae_string output = { buffer, length };
    AE_COMMAND_RESULT out = { AE_COMMAND_OK, { .v0 = { { (int64_t)status, output } } } };
    return out;
}
#endif
// ---- M2.12 U7 exposure trampoline ------------------------------------------
// The corpus test exposes `test_add` with #[no_mangle]; this trampoline
// calls it the way a real C caller would — through a foreign declaration
// of a symbol defined later in the same translation unit. Scalars only:
// a struct-parameter trampoline needs the user struct's type, which the
// runtime text cannot name (it precedes the user section).
// Static: the pruner keeps it only when a program actually calls it, so
// the `test_add` reference ships with the corpus test alone.
static int64_t ae_exposure_trampoline(int64_t a, int64_t b) {
    extern int64_t test_add(int64_t a, int64_t b);
    return test_add(a, b);
}

// ---- ADR-0208 slice 1: ae-side allocator bridge -------------------------
// The self-hosted containers declare the allocator extern "C" from ae
// source (std/vec.ae and friends). The runtime's own allocator family uses
// size_t and internal linkage, so an ae declaration's emitted prototype
// (int64_t everywhere) cannot match it directly; these wrappers carry
// int64_t signatures and follow the U7 trampoline precedent — static, same
// translation unit, retained by the pruner whenever a declaration
// references the symbol.
static int64_t *ae_std_alloc(int64_t size, int64_t line, int64_t column) {
    return ae_alloc((size_t)size, (size_t)line, (size_t)column);
}
static int64_t *ae_std_free(int64_t *block) {
    ae_free(block);
    return block;
}
