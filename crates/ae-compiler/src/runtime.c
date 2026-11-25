#include <stdint.h>
#include <inttypes.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <limits.h>
// ---- Raw pointer access (M2.4.03) -------------------------------------------
// Every unchecked read and write the language allows is emitted through these
// two macros, so generated C has one spelling for "the caller promised this
// address is valid" and grepping it lists every such promise. The
// preconditions are the caller's: the address must lie inside a live
// allocation (or a live local), be aligned for the pointee type, hold an
// initialized value before a read, and overlap no tracked borrow.
// spec/unsafe.md 2.4 states the contract; the compiler cannot check it.
#define AE_PTR_READ(place) (*(place))
#define AE_PTR_WRITE(place, ...) (*(place) = (__VA_ARGS__))
// ---- Output (ADR-55) --------------------------------------------------------
// A console decodes the bytes it receives with its own output code page (936 on
// a Chinese Windows), so raw UTF-8 showed up there as unrelated characters.
#ifdef _WIN32
static void *ae_console(void) { /* win32-declared */
    // Declared here, not through <windows.h>: the header costs about half a
    // second of build time, and a declaration inside a function is pruned with
    // it, where top-level text is always kept. The types match the header, so
    // both coexist where a program needs the header for something else (ADR-55).
    extern void *GetStdHandle(unsigned long index);
    extern int GetConsoleMode(void *handle, unsigned long *mode);
    // GetConsoleMode fails on a pipe or a file: exactly when bytes must pass
    // through untouched. Resolved once, so a redirected run pays two calls.
    static void *handle = NULL;
    static int resolved = 0;
    if (!resolved) {
        unsigned long mode = 0;
        handle = GetStdHandle((unsigned long)-11);
        if (!handle || handle == (void *)-1 || !GetConsoleMode(handle, &mode)) handle = NULL;
        resolved = 1;
    }
    return handle;
}
// WriteConsoleW accepts fewer units than asked for; retry rather than drop.
static bool ae_write_units(void *handle, const wchar_t *text, int count) { /* win32-declared */
    extern int WriteConsoleW(void *handle, const void *text, unsigned long count, unsigned long *written, void *reserved);
    while (count > 0) {
        unsigned long written = 0;
        if (!WriteConsoleW(handle, text, (unsigned long)count, &written, NULL) || written == 0) return false;
        text += written;
        count -= (int)written;
    }
    return true;
}
static bool ae_write_console(void *handle, const char *ptr, int64_t len, int newline) { /* win32-declared */
    extern int MultiByteToWideChar(unsigned page, unsigned long flags, const char *text, int bytes, wchar_t *wide, int units);
    // Converts in 1 KiB chunks, so printing a multi-megabyte string costs a 2 KiB
    // stack frame rather than a heap buffer the size of the text. A chunk never
    // ends inside a character, an invalid sequence becomes U+FFFD (a console
    // decodes, it does not validate), and the newline joins the last chunk
    // because one console write costs microseconds: a printed line is one call.
    wchar_t buffer[1024];
    const int capacity = newline ? 1023 : 1024;
    int units = 0;
    for (;;) {
        int chunk = len > capacity ? capacity : (int)len;
        while (chunk > 1 && (int64_t)chunk < len && ((unsigned char)ptr[chunk] & 0xC0) == 0x80) chunk -= 1;
        units = chunk > 0 ? MultiByteToWideChar(65001, 0, ptr, chunk, buffer, capacity) : 0;
        if (chunk > 0 && units <= 0) return false;
        ptr += chunk;
        len -= chunk;
        if (len <= 0) break;
        if (units > 0 && !ae_write_units(handle, buffer, units)) return false;
    }
    if (newline) buffer[units++] = L'\n';
    return units == 0 || ae_write_units(handle, buffer, units);
}
#endif
static bool ae_emit(const char *ptr, int64_t len, int newline) {
#ifdef _WIN32
    void *console = ae_console();
    if (console) return ae_write_console(console, ptr, len, newline);
#endif
    if (fwrite(ptr, 1, (size_t)len, stdout) != (size_t)len) return false;
    return !newline || fputc('\n', stdout) != EOF;
}

// Deliberate termination with a status. Buffered output is flushed first, so a
// program that prints and then exits does not lose the last line to a pipe. The
// code is truncated to int: POSIX shells only see the low eight bits, and
// pretending otherwise in the language would be worse than documenting it.
_Noreturn static void ae_exit(int64_t code) {
    fflush(stdout);
    exit((int)code);
}
// Runtime messages are fixed ASCII text (an operation that quotes user data
// returns an Err the program prints itself, through the console sink), so stderr
// keeps plain fprintf and a program that never prints carries none of it.
// A thread records its own failure and ends, instead of aborting the process at
// the failure point (ADR-0217); the thread unit installs this sink. With no
// sink installed — or on a thread without a record — the failure is fatal
// immediately, exactly as before.
typedef void (*ae_fail_sink_fn)(const char *message, size_t line, size_t column);
static ae_fail_sink_fn ae_fail_sink = NULL;
_Noreturn static void ae_fail(const char *message, size_t line, size_t column) {
    if (ae_fail_sink) ae_fail_sink(message, line, column);
    fprintf(stderr, "ae:%zu:%zu: %s\n", line, column, message);
    exit(1);
}
// unit: ae_allocator_impl
// ---- Allocation interface (ADR-16) ------------------------------------------
// Every buffer the runtime owns is obtained through one of these three
// pointers, so a host (future embed API, a test harness, a tracing allocator)
// can substitute its own backend without touching generated code. Failure is
// fatal and carries the location of the operation that needed the memory:
// there is no meaningful way to continue with a half-built value, and no
// unwinding machinery to run destructors through. The whole interface is one
// prune unit: a program whose kept functions never allocate carries none of
// it (charter rule 8, M3.1.01), so a substituting host necessarily runs a
// program that allocates and keeps this unit.
// Every buffer the runtime owns is obtained through one of these three
// pointers, so a host (future embed API, a test harness, a tracing allocator)
// can substitute its own backend without touching generated code. Failure is
// fatal and carries the location of the operation that needed the memory:
// there is no meaningful way to continue with a half-built value, and no
// unwinding machinery to run destructors through.
typedef struct {
    void *(*alloc)(size_t size);
    void *(*resize)(void *block, size_t size);
    void (*release)(void *block);
} ae_allocator;
static void *ae_os_alloc(size_t size) { return malloc(size); }
static void *ae_os_resize(void *block, size_t size) { return realloc(block, size); }
static void ae_os_release(void *block) { free(block); }
static ae_allocator ae_allocator_impl = { ae_os_alloc, ae_os_resize, ae_os_release };
// `AE_ALLOC_LIMIT` caps the cumulative number of bytes handed out, which is
// how the test suite reaches the out-of-memory path on purpose. It is a
// diagnostic, not a policy knob: the default is unlimited. The counters are
// C11 atomics (ADR-0214): threads share one heap and one budget, and a data
// race on plain globals would be undefined behavior even where a torn read is
// impossible on x86-64.
static _Atomic uint64_t ae_alloc_budget = UINT64_MAX;
static _Atomic uint64_t ae_alloc_total = 0;
static _Atomic bool ae_alloc_ready = false;
// unit: -
// The lazy environment read is guarded by an exchange so concurrent first
// allocations cannot race getenv; the winner initializes, everyone else
// proceeds.
static void ae_alloc_env(void) {
    const char *limit = getenv("AE_ALLOC_LIMIT");
    if (!limit || !*limit) return;
    char *end = NULL;
    unsigned long long value = strtoull(limit, &end, 10);
    if (end == limit || *end) return;
    atomic_store(&ae_alloc_budget, (uint64_t)value);
}
// NULL-returning variant for helpers that report failure through their own
// return value (path widening, for instance) instead of terminating.
// The budget check charges first and rolls back, so two threads cannot both
// pass a check against the same stale total: the limit may trip marginally
// earlier than a serial run, but never overshot (ADR-0214).
static void *ae_alloc_try(size_t size) {
    if (!atomic_exchange(&ae_alloc_ready, true)) ae_alloc_env();
    if ((uint64_t)size > atomic_load(&ae_alloc_budget)) return NULL;
    uint64_t previous = atomic_fetch_add(&ae_alloc_total, (uint64_t)size);
    if (previous > atomic_load(&ae_alloc_budget) - (uint64_t)size) {
        atomic_fetch_sub(&ae_alloc_total, (uint64_t)size);
        return NULL;
    }
    void *block = ae_allocator_impl.alloc(size);
    if (!block) {
        atomic_fetch_sub(&ae_alloc_total, (uint64_t)size);
        return NULL;
    }
    return block;
}
static void *ae_alloc(size_t size, size_t line, size_t column) {
    void *block = ae_alloc_try(size);
    if (!block) ae_fail("out of memory", line, column);
    return block;
}
static void *ae_alloc_resize(void *block, size_t size, size_t line, size_t column) {
    if (!atomic_exchange(&ae_alloc_ready, true)) ae_alloc_env();
    // The budget is charged for the whole new size: the accounting is a coarse
    // "how much did this program ask for" measure, not a live byte count.
    if ((uint64_t)size > atomic_load(&ae_alloc_budget) ||
        atomic_fetch_add(&ae_alloc_total, (uint64_t)size) > atomic_load(&ae_alloc_budget) - (uint64_t)size) {
        ae_allocator_impl.release(block);
        ae_fail("out of memory", line, column);
    }
    void *resized = ae_allocator_impl.resize(block, size);
    if (!resized) ae_fail("out of memory", line, column);
    return resized;
}
static void ae_free(void *block) { if (block) ae_allocator_impl.release(block); }
static void ae_set_allocator(ae_allocator impl) { ae_allocator_impl = impl; }


static int64_t ae_add(int64_t a, int64_t b, size_t line, size_t column) {
    if ((b > 0 && a > INT64_MAX - b) || (b < 0 && a < INT64_MIN - b)) ae_fail("integer overflow", line, column);
    return a + b;
}
static int64_t ae_sub(int64_t a, int64_t b, size_t line, size_t column) {
    if ((b < 0 && a > INT64_MAX + b) || (b > 0 && a < INT64_MIN + b)) ae_fail("integer overflow", line, column);
    return a - b;
}
static int64_t ae_mul(int64_t a, int64_t b, size_t line, size_t column) {
    if (a > 0 ? (b > 0 ? a > INT64_MAX / b : b < INT64_MIN / a)
              : (a < 0 ? (b > 0 ? a < INT64_MIN / b : b < 0 && a < INT64_MAX / b) : 0))
        ae_fail("integer overflow", line, column);
    return a * b;
}
static int64_t ae_div(int64_t a, int64_t b, size_t line, size_t column) {
    if (b == 0) ae_fail("division by zero", line, column);
    if (a == INT64_MIN && b == -1) ae_fail("integer overflow", line, column);
    return a / b;
}
static int64_t ae_mod(int64_t a, int64_t b, size_t line, size_t column) {
    if (b == 0) ae_fail("division by zero", line, column);
    if (a == INT64_MIN && b == -1) ae_fail("integer overflow", line, column);
    return a % b;
}
static int64_t ae_neg(int64_t a, size_t line, size_t column) {
    if (a == INT64_MIN) ae_fail("integer overflow", line, column);
    return -a;
}

static int8_t ae_add_i8(int8_t a, int8_t b, size_t line, size_t column) {
#if defined(__clang__) || (defined(__GNUC__) && __GNUC__ >= 5)
    int8_t r;
    if (__builtin_add_overflow(a, b, &r)) ae_fail("integer overflow", line, column);
    return r;
#else
    int16_t r = (int16_t)a + (int16_t)b;
    if (r < INT8_MIN || r > INT8_MAX) ae_fail("integer overflow", line, column);
    return (int8_t)r;
#endif
}
static int8_t ae_sub_i8(int8_t a, int8_t b, size_t line, size_t column) {
#if defined(__clang__) || (defined(__GNUC__) && __GNUC__ >= 5)
    int8_t r;
    if (__builtin_sub_overflow(a, b, &r)) ae_fail("integer overflow", line, column);
    return r;
#else
    int16_t r = (int16_t)a - (int16_t)b;
    if (r < INT8_MIN || r > INT8_MAX) ae_fail("integer overflow", line, column);
    return (int8_t)r;
#endif
}
static int8_t ae_mul_i8(int8_t a, int8_t b, size_t line, size_t column) {
#if defined(__clang__) || (defined(__GNUC__) && __GNUC__ >= 5)
    int8_t r;
    if (__builtin_mul_overflow(a, b, &r)) ae_fail("integer overflow", line, column);
    return r;
#else
    int16_t r = (int16_t)a * (int16_t)b;
    if (r < INT8_MIN || r > INT8_MAX) ae_fail("integer overflow", line, column);
    return (int8_t)r;
#endif
}
static int8_t ae_div_i8(int8_t a, int8_t b, size_t line, size_t column) {
    if (b == 0) ae_fail("division by zero", line, column);
    if (a == INT8_MIN && b == -1) ae_fail("integer overflow", line, column);
    return (int8_t)(a / b);
}
static int8_t ae_mod_i8(int8_t a, int8_t b, size_t line, size_t column) {
    if (b == 0) ae_fail("division by zero", line, column);
    if (a == INT8_MIN && b == -1) ae_fail("integer overflow", line, column);
    return (int8_t)(a % b);
}
static int8_t ae_neg_i8(int8_t a, size_t line, size_t column) {
    if (a == INT8_MIN) ae_fail("integer overflow", line, column);
    return (int8_t)(-a);
}
static int8_t ae_shl_i8(int8_t a, int8_t b, size_t line, size_t column) {
    if (b < 0 || b >= 8) ae_fail("shift count out of range", line, column);
    uint8_t u = (uint8_t)((uint8_t)a << b);
    int8_t r;
    memcpy(&r, &u, 1);
    return r;
}
static int8_t ae_shr_i8(int8_t a, int8_t b, size_t line, size_t column) {
    if (b < 0 || b >= 8) ae_fail("shift count out of range", line, column);
    return (int8_t)(a >> b);
}

static uint8_t ae_add_u8(uint8_t a, uint8_t b, size_t line, size_t column) {
#if defined(__clang__) || (defined(__GNUC__) && __GNUC__ >= 5)
    uint8_t r;
    if (__builtin_add_overflow(a, b, &r)) ae_fail("integer overflow", line, column);
    return r;
#else
    uint16_t r = (uint16_t)a + (uint16_t)b;
    if (r > UINT8_MAX) ae_fail("integer overflow", line, column);
    return (uint8_t)r;
#endif
}
static uint8_t ae_sub_u8(uint8_t a, uint8_t b, size_t line, size_t column) {
#if defined(__clang__) || (defined(__GNUC__) && __GNUC__ >= 5)
    uint8_t r;
    if (__builtin_sub_overflow(a, b, &r)) ae_fail("integer overflow", line, column);
    return r;
#else
    if (a < b) ae_fail("integer overflow", line, column);
    return (uint8_t)(a - b);
#endif
}
static uint8_t ae_mul_u8(uint8_t a, uint8_t b, size_t line, size_t column) {
#if defined(__clang__) || (defined(__GNUC__) && __GNUC__ >= 5)
    uint8_t r;
    if (__builtin_mul_overflow(a, b, &r)) ae_fail("integer overflow", line, column);
    return r;
#else
    uint16_t r = (uint16_t)a * (uint16_t)b;
    if (r > UINT8_MAX) ae_fail("integer overflow", line, column);
    return (uint8_t)r;
#endif
}
static uint8_t ae_div_u8(uint8_t a, uint8_t b, size_t line, size_t column) {
    if (b == 0) ae_fail("division by zero", line, column);
    return (uint8_t)(a / b);
}
static uint8_t ae_mod_u8(uint8_t a, uint8_t b, size_t line, size_t column) {
    if (b == 0) ae_fail("division by zero", line, column);
    return (uint8_t)(a % b);
}
static uint8_t ae_shl_u8(uint8_t a, uint8_t b, size_t line, size_t column) {
    if (b >= 8) ae_fail("shift count out of range", line, column);
    return (uint8_t)(a << b);
}
static uint8_t ae_shr_u8(uint8_t a, uint8_t b, size_t line, size_t column) {
    if (b >= 8) ae_fail("shift count out of range", line, column);
    return (uint8_t)(a >> b);
}

// unit: ae_int_macros
#if defined(__clang__) || (defined(__GNUC__) && __GNUC__ >= 5)
#define AE_CHK_ADD_S(T, MIN, MAX, a, b, l, c) do { T r; if (__builtin_add_overflow((a), (b), &r)) ae_fail("integer overflow", (l), (c)); return r; } while (0)
#define AE_CHK_SUB_S(T, MIN, MAX, a, b, l, c) do { T r; if (__builtin_sub_overflow((a), (b), &r)) ae_fail("integer overflow", (l), (c)); return r; } while (0)
#define AE_CHK_MUL_S(T, MIN, MAX, a, b, l, c) do { T r; if (__builtin_mul_overflow((a), (b), &r)) ae_fail("integer overflow", (l), (c)); return r; } while (0)
#define AE_CHK_ADD_U(T, MAX, a, b, l, c) do { T r; if (__builtin_add_overflow((a), (b), &r)) ae_fail("integer overflow", (l), (c)); return r; } while (0)
#define AE_CHK_SUB_U(T, a, b, l, c) do { T r; if (__builtin_sub_overflow((a), (b), &r)) ae_fail("integer overflow", (l), (c)); return r; } while (0)
#define AE_CHK_MUL_U(T, MAX, a, b, l, c) do { T r; if (__builtin_mul_overflow((a), (b), &r)) ae_fail("integer overflow", (l), (c)); return r; } while (0)
#else
#define AE_CHK_ADD_S(T, MIN, MAX, a, b, l, c) do { if (((b) > 0 && (a) > (MAX) - (b)) || ((b) < 0 && (a) < (MIN) - (b))) ae_fail("integer overflow", (l), (c)); return (T)((a) + (b)); } while (0)
#define AE_CHK_SUB_S(T, MIN, MAX, a, b, l, c) do { if (((b) < 0 && (a) > (MAX) + (b)) || ((b) > 0 && (a) < (MIN) + (b))) ae_fail("integer overflow", (l), (c)); return (T)((a) - (b)); } while (0)
#define AE_CHK_MUL_S(T, MIN, MAX, a, b, l, c) do { if ((a) > 0 ? ((b) > 0 ? (a) > (MAX) / (b) : (b) < (MIN) / (a)) : ((a) < 0 ? ((b) > 0 ? (a) < (MIN) / (b) : (b) < 0 && (a) < (MAX) / (b)) : 0)) ae_fail("integer overflow", (l), (c)); return (T)((a) * (b)); } while (0)
#define AE_CHK_ADD_U(T, MAX, a, b, l, c) do { if ((a) > (MAX) - (b)) ae_fail("integer overflow", (l), (c)); return (T)((a) + (b)); } while (0)
#define AE_CHK_SUB_U(T, a, b, l, c) do { if ((a) < (b)) ae_fail("integer overflow", (l), (c)); return (T)((a) - (b)); } while (0)
#define AE_CHK_MUL_U(T, MAX, a, b, l, c) do { if ((b) != 0 && (a) > (MAX) / (b)) ae_fail("integer overflow", (l), (c)); return (T)((a) * (b)); } while (0)
#endif
#define AE_CHK_DIV_S(T, MIN, a, b, l, c) do { if ((b) == 0) ae_fail("division by zero", (l), (c)); if ((a) == (MIN) && (b) == -1) ae_fail("integer overflow", (l), (c)); return (T)((a) / (b)); } while (0)
#define AE_CHK_MOD_S(T, MIN, a, b, l, c) do { if ((b) == 0) ae_fail("division by zero", (l), (c)); if ((a) == (MIN) && (b) == -1) ae_fail("integer overflow", (l), (c)); return (T)((a) % (b)); } while (0)
#define AE_CHK_NEG_S(T, MIN, a, l, c) do { if ((a) == (MIN)) ae_fail("integer overflow", (l), (c)); return (T)(-(a)); } while (0)
#define AE_CHK_SHL_S(T, U, BITS, a, b, l, c) do { if ((b) < 0 || (b) >= (BITS)) ae_fail("shift count out of range", (l), (c)); U u = (U)((U)(a) << (b)); T r; memcpy(&r, &u, sizeof(r)); return r; } while (0)
#define AE_CHK_SHR_S(T, BITS, a, b, l, c) do { if ((b) < 0 || (b) >= (BITS)) ae_fail("shift count out of range", (l), (c)); return (T)((a) >> (b)); } while (0)
#define AE_CHK_DIV_U(T, a, b, l, c) do { if ((b) == 0) ae_fail("division by zero", (l), (c)); return (T)((a) / (b)); } while (0)
#define AE_CHK_MOD_U(T, a, b, l, c) do { if ((b) == 0) ae_fail("division by zero", (l), (c)); return (T)((a) % (b)); } while (0)
#define AE_CHK_SHL_U(T, BITS, a, b, l, c) do { if ((b) >= (BITS)) ae_fail("shift count out of range", (l), (c)); return (T)((a) << (b)); } while (0)
#define AE_CHK_SHR_U(T, BITS, a, b, l, c) do { if ((b) >= (BITS)) ae_fail("shift count out of range", (l), (c)); return (T)((a) >> (b)); } while (0)
#define AE_WRAP_BIN_S(T, U, op, a, b) do { U u = (U)((U)(a) op (U)(b)); T r; memcpy(&r, &u, sizeof(r)); return r; } while (0)
#define AE_WRAP_NEG_S(T, U, a) do { U u = (U)(0 - (U)(a)); T r; memcpy(&r, &u, sizeof(r)); return r; } while (0)
#if defined(__clang__) || (defined(__GNUC__) && __GNUC__ >= 5)
#define AE_SAT_ADD_S(T, MIN, MAX, a, b) do { T r; if (__builtin_add_overflow((a), (b), &r)) return (b) > 0 ? (MAX) : (MIN); return r; } while (0)
#define AE_SAT_SUB_S(T, MIN, MAX, a, b) do { T r; if (__builtin_sub_overflow((a), (b), &r)) return (b) < 0 ? (MAX) : (MIN); return r; } while (0)
#define AE_SAT_MUL_S(T, MIN, MAX, a, b) do { T r; if (__builtin_mul_overflow((a), (b), &r)) return (((a) > 0) == ((b) > 0)) ? (MAX) : (MIN); return r; } while (0)
#define AE_SAT_ADD_U(T, MAX, a, b) do { T r; if (__builtin_add_overflow((a), (b), &r)) return (MAX); return r; } while (0)
#define AE_SAT_SUB_U(T, a, b) do { T r; if (__builtin_sub_overflow((a), (b), &r)) return 0; return r; } while (0)
#define AE_SAT_MUL_U(T, MAX, a, b) do { T r; if (__builtin_mul_overflow((a), (b), &r)) return (MAX); return r; } while (0)
#define AE_OPT_ADD_S(T, MIN, MAX, a, b, out) return !__builtin_add_overflow((a), (b), (out))
#define AE_OPT_SUB_S(T, MIN, MAX, a, b, out) return !__builtin_sub_overflow((a), (b), (out))
#define AE_OPT_MUL_S(T, MIN, MAX, a, b, out) return !__builtin_mul_overflow((a), (b), (out))
#define AE_OPT_ADD_U(T, MAX, a, b, out) return !__builtin_add_overflow((a), (b), (out))
#define AE_OPT_SUB_U(T, a, b, out) return !__builtin_sub_overflow((a), (b), (out))
#define AE_OPT_MUL_U(T, MAX, a, b, out) return !__builtin_mul_overflow((a), (b), (out))
#else
#define AE_SAT_ADD_S(T, MIN, MAX, a, b) do { if ((b) > 0 && (a) > (MAX) - (b)) return (MAX); if ((b) < 0 && (a) < (MIN) - (b)) return (MIN); return (T)((a) + (b)); } while (0)
#define AE_SAT_SUB_S(T, MIN, MAX, a, b) do { if ((b) < 0 && (a) > (MAX) + (b)) return (MAX); if ((b) > 0 && (a) < (MIN) + (b)) return (MIN); return (T)((a) - (b)); } while (0)
#define AE_SAT_MUL_S(T, MIN, MAX, a, b) do { if ((a) > 0 ? ((b) > 0 ? (a) > (MAX) / (b) : (b) < (MIN) / (a)) : ((a) < 0 ? ((b) > 0 ? (a) < (MIN) / (b) : (b) < 0 && (a) < (MAX) / (b)) : 0)) return (((a) > 0) == ((b) > 0)) ? (MAX) : (MIN); return (T)((a) * (b)); } while (0)
#define AE_SAT_ADD_U(T, MAX, a, b) return ((a) > (MAX) - (b)) ? (MAX) : (T)((a) + (b))
#define AE_SAT_SUB_U(T, a, b) return ((a) < (b)) ? 0 : (T)((a) - (b))
#define AE_SAT_MUL_U(T, MAX, a, b) return ((b) != 0 && (a) > (MAX) / (b)) ? (MAX) : (T)((a) * (b))
#define AE_OPT_ADD_S(T, MIN, MAX, a, b, out) do { if (((b) > 0 && (a) > (MAX) - (b)) || ((b) < 0 && (a) < (MIN) - (b))) return false; *(out) = (T)((a) + (b)); return true; } while (0)
#define AE_OPT_SUB_S(T, MIN, MAX, a, b, out) do { if (((b) < 0 && (a) > (MAX) + (b)) || ((b) > 0 && (a) < (MIN) + (b))) return false; *(out) = (T)((a) - (b)); return true; } while (0)
#define AE_OPT_MUL_S(T, MIN, MAX, a, b, out) do { if ((a) > 0 ? ((b) > 0 ? (a) > (MAX) / (b) : (b) < (MIN) / (a)) : ((a) < 0 ? ((b) > 0 ? (a) < (MIN) / (b) : (b) < 0 && (a) < (MAX) / (b)) : 0)) return false; *(out) = (T)((a) * (b)); return true; } while (0)
#define AE_OPT_ADD_U(T, MAX, a, b, out) do { if ((a) > (MAX) - (b)) return false; *(out) = (T)((a) + (b)); return true; } while (0)
#define AE_OPT_SUB_U(T, a, b, out) do { if ((a) < (b)) return false; *(out) = (T)((a) - (b)); return true; } while (0)
#define AE_OPT_MUL_U(T, MAX, a, b, out) do { if ((b) != 0 && (a) > (MAX) / (b)) return false; *(out) = (T)((a) * (b)); return true; } while (0)
#endif
#define AE_OPT_DIV_S(T, MIN, a, b, out) do { if ((b) == 0 || ((a) == (MIN) && (b) == -1)) return false; *(out) = (T)((a) / (b)); return true; } while (0)
#define AE_OPT_MOD_S(T, MIN, a, b, out) do { if ((b) == 0 || ((a) == (MIN) && (b) == -1)) return false; *(out) = (T)((a) % (b)); return true; } while (0)
#define AE_OPT_NEG_S(T, MIN, a, out) do { if ((a) == (MIN)) return false; *(out) = (T)(-(a)); return true; } while (0)
#define AE_OPT_DIV_U(T, a, b, out) do { if ((b) == 0) return false; *(out) = (T)((a) / (b)); return true; } while (0)
#define AE_OPT_MOD_U(T, a, b, out) do { if ((b) == 0) return false; *(out) = (T)((a) % (b)); return true; } while (0)
// unit: -

static int16_t ae_add_i16(int16_t a, int16_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_ADD_S(int16_t, INT16_MIN, INT16_MAX, a, b, line, column); }
static int16_t ae_sub_i16(int16_t a, int16_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SUB_S(int16_t, INT16_MIN, INT16_MAX, a, b, line, column); }
static int16_t ae_mul_i16(int16_t a, int16_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_MUL_S(int16_t, INT16_MIN, INT16_MAX, a, b, line, column); }
static int16_t ae_div_i16(int16_t a, int16_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_DIV_S(int16_t, INT16_MIN, a, b, line, column); }
static int16_t ae_mod_i16(int16_t a, int16_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_MOD_S(int16_t, INT16_MIN, a, b, line, column); }
static int16_t ae_neg_i16(int16_t a, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_NEG_S(int16_t, INT16_MIN, a, line, column); }
static int16_t ae_shl_i16(int16_t a, int16_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SHL_S(int16_t, uint16_t, 16, a, b, line, column); }
static int16_t ae_shr_i16(int16_t a, int16_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SHR_S(int16_t, 16, a, b, line, column); }

static int32_t ae_add_i32(int32_t a, int32_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_ADD_S(int32_t, INT32_MIN, INT32_MAX, a, b, line, column); }
static int32_t ae_sub_i32(int32_t a, int32_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SUB_S(int32_t, INT32_MIN, INT32_MAX, a, b, line, column); }
static int32_t ae_mul_i32(int32_t a, int32_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_MUL_S(int32_t, INT32_MIN, INT32_MAX, a, b, line, column); }
static int32_t ae_div_i32(int32_t a, int32_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_DIV_S(int32_t, INT32_MIN, a, b, line, column); }
static int32_t ae_mod_i32(int32_t a, int32_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_MOD_S(int32_t, INT32_MIN, a, b, line, column); }
static int32_t ae_neg_i32(int32_t a, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_NEG_S(int32_t, INT32_MIN, a, line, column); }
static int32_t ae_shl_i32(int32_t a, int32_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SHL_S(int32_t, uint32_t, 32, a, b, line, column); }
static int32_t ae_shr_i32(int32_t a, int32_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SHR_S(int32_t, 32, a, b, line, column); }

static intptr_t ae_add_isize(intptr_t a, intptr_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_ADD_S(intptr_t, INTPTR_MIN, INTPTR_MAX, a, b, line, column); }
static intptr_t ae_sub_isize(intptr_t a, intptr_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SUB_S(intptr_t, INTPTR_MIN, INTPTR_MAX, a, b, line, column); }
static intptr_t ae_mul_isize(intptr_t a, intptr_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_MUL_S(intptr_t, INTPTR_MIN, INTPTR_MAX, a, b, line, column); }
static intptr_t ae_div_isize(intptr_t a, intptr_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_DIV_S(intptr_t, INTPTR_MIN, a, b, line, column); }
static intptr_t ae_mod_isize(intptr_t a, intptr_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_MOD_S(intptr_t, INTPTR_MIN, a, b, line, column); }
static intptr_t ae_neg_isize(intptr_t a, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_NEG_S(intptr_t, INTPTR_MIN, a, line, column); }
static intptr_t ae_shl_isize(intptr_t a, intptr_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SHL_S(intptr_t, uintptr_t, (intptr_t)(sizeof(intptr_t) * 8), a, b, line, column); }
static intptr_t ae_shr_isize(intptr_t a, intptr_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SHR_S(intptr_t, (intptr_t)(sizeof(intptr_t) * 8), a, b, line, column); }

static uint16_t ae_add_u16(uint16_t a, uint16_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_ADD_U(uint16_t, UINT16_MAX, a, b, line, column); }
static uint16_t ae_sub_u16(uint16_t a, uint16_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SUB_U(uint16_t, a, b, line, column); }
static uint16_t ae_mul_u16(uint16_t a, uint16_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_MUL_U(uint16_t, UINT16_MAX, a, b, line, column); }
static uint16_t ae_div_u16(uint16_t a, uint16_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_DIV_U(uint16_t, a, b, line, column); }
static uint16_t ae_mod_u16(uint16_t a, uint16_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_MOD_U(uint16_t, a, b, line, column); }
static uint16_t ae_shl_u16(uint16_t a, uint16_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SHL_U(uint16_t, 16, a, b, line, column); }
static uint16_t ae_shr_u16(uint16_t a, uint16_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SHR_U(uint16_t, 16, a, b, line, column); }

static uint32_t ae_add_u32(uint32_t a, uint32_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_ADD_U(uint32_t, UINT32_MAX, a, b, line, column); }
static uint32_t ae_sub_u32(uint32_t a, uint32_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SUB_U(uint32_t, a, b, line, column); }
static uint32_t ae_mul_u32(uint32_t a, uint32_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_MUL_U(uint32_t, UINT32_MAX, a, b, line, column); }
static uint32_t ae_div_u32(uint32_t a, uint32_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_DIV_U(uint32_t, a, b, line, column); }
static uint32_t ae_mod_u32(uint32_t a, uint32_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_MOD_U(uint32_t, a, b, line, column); }
static uint32_t ae_shl_u32(uint32_t a, uint32_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SHL_U(uint32_t, 32, a, b, line, column); }
static uint32_t ae_shr_u32(uint32_t a, uint32_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SHR_U(uint32_t, 32, a, b, line, column); }

static uint64_t ae_add_u64(uint64_t a, uint64_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_ADD_U(uint64_t, UINT64_MAX, a, b, line, column); }
static uint64_t ae_sub_u64(uint64_t a, uint64_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SUB_U(uint64_t, a, b, line, column); }
static uint64_t ae_mul_u64(uint64_t a, uint64_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_MUL_U(uint64_t, UINT64_MAX, a, b, line, column); }
static uint64_t ae_div_u64(uint64_t a, uint64_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_DIV_U(uint64_t, a, b, line, column); }
static uint64_t ae_mod_u64(uint64_t a, uint64_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_MOD_U(uint64_t, a, b, line, column); }
static uint64_t ae_shl_u64(uint64_t a, uint64_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SHL_U(uint64_t, 64, a, b, line, column); }
static uint64_t ae_shr_u64(uint64_t a, uint64_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SHR_U(uint64_t, 64, a, b, line, column); }

static uintptr_t ae_add_usize(uintptr_t a, uintptr_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_ADD_U(uintptr_t, UINTPTR_MAX, a, b, line, column); }
static uintptr_t ae_sub_usize(uintptr_t a, uintptr_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SUB_U(uintptr_t, a, b, line, column); }
static uintptr_t ae_mul_usize(uintptr_t a, uintptr_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_MUL_U(uintptr_t, UINTPTR_MAX, a, b, line, column); }
static uintptr_t ae_div_usize(uintptr_t a, uintptr_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_DIV_U(uintptr_t, a, b, line, column); }
static uintptr_t ae_mod_usize(uintptr_t a, uintptr_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_MOD_U(uintptr_t, a, b, line, column); }
static uintptr_t ae_shl_usize(uintptr_t a, uintptr_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SHL_U(uintptr_t, (uintptr_t)(sizeof(uintptr_t) * 8), a, b, line, column); }
static uintptr_t ae_shr_usize(uintptr_t a, uintptr_t b, size_t line, size_t column) { /* ae_int_macros */ AE_CHK_SHR_U(uintptr_t, (uintptr_t)(sizeof(uintptr_t) * 8), a, b, line, column); }

static int8_t ae_wrap_add_i8(int8_t a, int8_t b) { /* ae_int_macros */ AE_WRAP_BIN_S(int8_t, uint8_t, +, a, b); }
static int8_t ae_wrap_sub_i8(int8_t a, int8_t b) { /* ae_int_macros */ AE_WRAP_BIN_S(int8_t, uint8_t, -, a, b); }
static int8_t ae_wrap_mul_i8(int8_t a, int8_t b) { /* ae_int_macros */ AE_WRAP_BIN_S(int8_t, uint8_t, *, a, b); }
static int8_t ae_wrap_neg_i8(int8_t a) { /* ae_int_macros */ AE_WRAP_NEG_S(int8_t, uint8_t, a); }
static int8_t ae_sat_add_i8(int8_t a, int8_t b) { /* ae_int_macros */ AE_SAT_ADD_S(int8_t, INT8_MIN, INT8_MAX, a, b); }
static int8_t ae_sat_sub_i8(int8_t a, int8_t b) { /* ae_int_macros */ AE_SAT_SUB_S(int8_t, INT8_MIN, INT8_MAX, a, b); }
static int8_t ae_sat_mul_i8(int8_t a, int8_t b) { /* ae_int_macros */ AE_SAT_MUL_S(int8_t, INT8_MIN, INT8_MAX, a, b); }
static bool ae_opt_add_i8(int8_t a, int8_t b, int8_t *out) { /* ae_int_macros */ AE_OPT_ADD_S(int8_t, INT8_MIN, INT8_MAX, a, b, out); }
static bool ae_opt_sub_i8(int8_t a, int8_t b, int8_t *out) { /* ae_int_macros */ AE_OPT_SUB_S(int8_t, INT8_MIN, INT8_MAX, a, b, out); }
static bool ae_opt_mul_i8(int8_t a, int8_t b, int8_t *out) { /* ae_int_macros */ AE_OPT_MUL_S(int8_t, INT8_MIN, INT8_MAX, a, b, out); }
static bool ae_opt_div_i8(int8_t a, int8_t b, int8_t *out) { /* ae_int_macros */ AE_OPT_DIV_S(int8_t, INT8_MIN, a, b, out); }
static bool ae_opt_mod_i8(int8_t a, int8_t b, int8_t *out) { /* ae_int_macros */ AE_OPT_MOD_S(int8_t, INT8_MIN, a, b, out); }
static bool ae_opt_neg_i8(int8_t a, int8_t *out) { /* ae_int_macros */ AE_OPT_NEG_S(int8_t, INT8_MIN, a, out); }

static int16_t ae_wrap_add_i16(int16_t a, int16_t b) { /* ae_int_macros */ AE_WRAP_BIN_S(int16_t, uint16_t, +, a, b); }
static int16_t ae_wrap_sub_i16(int16_t a, int16_t b) { /* ae_int_macros */ AE_WRAP_BIN_S(int16_t, uint16_t, -, a, b); }
static int16_t ae_wrap_mul_i16(int16_t a, int16_t b) { /* ae_int_macros */ AE_WRAP_BIN_S(int16_t, uint16_t, *, a, b); }
static int16_t ae_wrap_neg_i16(int16_t a) { /* ae_int_macros */ AE_WRAP_NEG_S(int16_t, uint16_t, a); }
static int16_t ae_sat_add_i16(int16_t a, int16_t b) { /* ae_int_macros */ AE_SAT_ADD_S(int16_t, INT16_MIN, INT16_MAX, a, b); }
static int16_t ae_sat_sub_i16(int16_t a, int16_t b) { /* ae_int_macros */ AE_SAT_SUB_S(int16_t, INT16_MIN, INT16_MAX, a, b); }
static int16_t ae_sat_mul_i16(int16_t a, int16_t b) { /* ae_int_macros */ AE_SAT_MUL_S(int16_t, INT16_MIN, INT16_MAX, a, b); }
static bool ae_opt_add_i16(int16_t a, int16_t b, int16_t *out) { /* ae_int_macros */ AE_OPT_ADD_S(int16_t, INT16_MIN, INT16_MAX, a, b, out); }
static bool ae_opt_sub_i16(int16_t a, int16_t b, int16_t *out) { /* ae_int_macros */ AE_OPT_SUB_S(int16_t, INT16_MIN, INT16_MAX, a, b, out); }
static bool ae_opt_mul_i16(int16_t a, int16_t b, int16_t *out) { /* ae_int_macros */ AE_OPT_MUL_S(int16_t, INT16_MIN, INT16_MAX, a, b, out); }
static bool ae_opt_div_i16(int16_t a, int16_t b, int16_t *out) { /* ae_int_macros */ AE_OPT_DIV_S(int16_t, INT16_MIN, a, b, out); }
static bool ae_opt_mod_i16(int16_t a, int16_t b, int16_t *out) { /* ae_int_macros */ AE_OPT_MOD_S(int16_t, INT16_MIN, a, b, out); }
static bool ae_opt_neg_i16(int16_t a, int16_t *out) { /* ae_int_macros */ AE_OPT_NEG_S(int16_t, INT16_MIN, a, out); }

static int32_t ae_wrap_add_i32(int32_t a, int32_t b) { /* ae_int_macros */ AE_WRAP_BIN_S(int32_t, uint32_t, +, a, b); }
static int32_t ae_wrap_sub_i32(int32_t a, int32_t b) { /* ae_int_macros */ AE_WRAP_BIN_S(int32_t, uint32_t, -, a, b); }
static int32_t ae_wrap_mul_i32(int32_t a, int32_t b) { /* ae_int_macros */ AE_WRAP_BIN_S(int32_t, uint32_t, *, a, b); }
static int32_t ae_wrap_neg_i32(int32_t a) { /* ae_int_macros */ AE_WRAP_NEG_S(int32_t, uint32_t, a); }
static int32_t ae_sat_add_i32(int32_t a, int32_t b) { /* ae_int_macros */ AE_SAT_ADD_S(int32_t, INT32_MIN, INT32_MAX, a, b); }
static int32_t ae_sat_sub_i32(int32_t a, int32_t b) { /* ae_int_macros */ AE_SAT_SUB_S(int32_t, INT32_MIN, INT32_MAX, a, b); }
static int32_t ae_sat_mul_i32(int32_t a, int32_t b) { /* ae_int_macros */ AE_SAT_MUL_S(int32_t, INT32_MIN, INT32_MAX, a, b); }
static bool ae_opt_add_i32(int32_t a, int32_t b, int32_t *out) { /* ae_int_macros */ AE_OPT_ADD_S(int32_t, INT32_MIN, INT32_MAX, a, b, out); }
static bool ae_opt_sub_i32(int32_t a, int32_t b, int32_t *out) { /* ae_int_macros */ AE_OPT_SUB_S(int32_t, INT32_MIN, INT32_MAX, a, b, out); }
static bool ae_opt_mul_i32(int32_t a, int32_t b, int32_t *out) { /* ae_int_macros */ AE_OPT_MUL_S(int32_t, INT32_MIN, INT32_MAX, a, b, out); }
static bool ae_opt_div_i32(int32_t a, int32_t b, int32_t *out) { /* ae_int_macros */ AE_OPT_DIV_S(int32_t, INT32_MIN, a, b, out); }
static bool ae_opt_mod_i32(int32_t a, int32_t b, int32_t *out) { /* ae_int_macros */ AE_OPT_MOD_S(int32_t, INT32_MIN, a, b, out); }
static bool ae_opt_neg_i32(int32_t a, int32_t *out) { /* ae_int_macros */ AE_OPT_NEG_S(int32_t, INT32_MIN, a, out); }

static int64_t ae_wrap_add_i64(int64_t a, int64_t b) { /* ae_int_macros */ AE_WRAP_BIN_S(int64_t, uint64_t, +, a, b); }
static int64_t ae_wrap_sub_i64(int64_t a, int64_t b) { /* ae_int_macros */ AE_WRAP_BIN_S(int64_t, uint64_t, -, a, b); }
static int64_t ae_wrap_mul_i64(int64_t a, int64_t b) { /* ae_int_macros */ AE_WRAP_BIN_S(int64_t, uint64_t, *, a, b); }
static int64_t ae_wrap_neg_i64(int64_t a) { /* ae_int_macros */ AE_WRAP_NEG_S(int64_t, uint64_t, a); }
static int64_t ae_sat_add_i64(int64_t a, int64_t b) { /* ae_int_macros */ AE_SAT_ADD_S(int64_t, INT64_MIN, INT64_MAX, a, b); }
static int64_t ae_sat_sub_i64(int64_t a, int64_t b) { /* ae_int_macros */ AE_SAT_SUB_S(int64_t, INT64_MIN, INT64_MAX, a, b); }
static int64_t ae_sat_mul_i64(int64_t a, int64_t b) { /* ae_int_macros */ AE_SAT_MUL_S(int64_t, INT64_MIN, INT64_MAX, a, b); }
static bool ae_opt_add_i64(int64_t a, int64_t b, int64_t *out) { /* ae_int_macros */ AE_OPT_ADD_S(int64_t, INT64_MIN, INT64_MAX, a, b, out); }
static bool ae_opt_sub_i64(int64_t a, int64_t b, int64_t *out) { /* ae_int_macros */ AE_OPT_SUB_S(int64_t, INT64_MIN, INT64_MAX, a, b, out); }
static bool ae_opt_mul_i64(int64_t a, int64_t b, int64_t *out) { /* ae_int_macros */ AE_OPT_MUL_S(int64_t, INT64_MIN, INT64_MAX, a, b, out); }
static bool ae_opt_div_i64(int64_t a, int64_t b, int64_t *out) { /* ae_int_macros */ AE_OPT_DIV_S(int64_t, INT64_MIN, a, b, out); }
static bool ae_opt_mod_i64(int64_t a, int64_t b, int64_t *out) { /* ae_int_macros */ AE_OPT_MOD_S(int64_t, INT64_MIN, a, b, out); }
static bool ae_opt_neg_i64(int64_t a, int64_t *out) { /* ae_int_macros */ AE_OPT_NEG_S(int64_t, INT64_MIN, a, out); }

static intptr_t ae_wrap_add_isize(intptr_t a, intptr_t b) { /* ae_int_macros */ AE_WRAP_BIN_S(intptr_t, uintptr_t, +, a, b); }
static intptr_t ae_wrap_sub_isize(intptr_t a, intptr_t b) { /* ae_int_macros */ AE_WRAP_BIN_S(intptr_t, uintptr_t, -, a, b); }
static intptr_t ae_wrap_mul_isize(intptr_t a, intptr_t b) { /* ae_int_macros */ AE_WRAP_BIN_S(intptr_t, uintptr_t, *, a, b); }
static intptr_t ae_wrap_neg_isize(intptr_t a) { /* ae_int_macros */ AE_WRAP_NEG_S(intptr_t, uintptr_t, a); }
static intptr_t ae_sat_add_isize(intptr_t a, intptr_t b) { /* ae_int_macros */ AE_SAT_ADD_S(intptr_t, INTPTR_MIN, INTPTR_MAX, a, b); }
static intptr_t ae_sat_sub_isize(intptr_t a, intptr_t b) { /* ae_int_macros */ AE_SAT_SUB_S(intptr_t, INTPTR_MIN, INTPTR_MAX, a, b); }
static intptr_t ae_sat_mul_isize(intptr_t a, intptr_t b) { /* ae_int_macros */ AE_SAT_MUL_S(intptr_t, INTPTR_MIN, INTPTR_MAX, a, b); }
static bool ae_opt_add_isize(intptr_t a, intptr_t b, intptr_t *out) { /* ae_int_macros */ AE_OPT_ADD_S(intptr_t, INTPTR_MIN, INTPTR_MAX, a, b, out); }
static bool ae_opt_sub_isize(intptr_t a, intptr_t b, intptr_t *out) { /* ae_int_macros */ AE_OPT_SUB_S(intptr_t, INTPTR_MIN, INTPTR_MAX, a, b, out); }
static bool ae_opt_mul_isize(intptr_t a, intptr_t b, intptr_t *out) { /* ae_int_macros */ AE_OPT_MUL_S(intptr_t, INTPTR_MIN, INTPTR_MAX, a, b, out); }
static bool ae_opt_div_isize(intptr_t a, intptr_t b, intptr_t *out) { /* ae_int_macros */ AE_OPT_DIV_S(intptr_t, INTPTR_MIN, a, b, out); }
static bool ae_opt_mod_isize(intptr_t a, intptr_t b, intptr_t *out) { /* ae_int_macros */ AE_OPT_MOD_S(intptr_t, INTPTR_MIN, a, b, out); }
static bool ae_opt_neg_isize(intptr_t a, intptr_t *out) { /* ae_int_macros */ AE_OPT_NEG_S(intptr_t, INTPTR_MIN, a, out); }

static uint8_t ae_wrap_add_u8(uint8_t a, uint8_t b) { return (uint8_t)(a + b); }
static uint8_t ae_wrap_sub_u8(uint8_t a, uint8_t b) { return (uint8_t)(a - b); }
static uint8_t ae_wrap_mul_u8(uint8_t a, uint8_t b) { return (uint8_t)(a * b); }
static uint8_t ae_wrap_neg_u8(uint8_t a) { return (uint8_t)(0 - a); }
static uint8_t ae_sat_add_u8(uint8_t a, uint8_t b) { /* ae_int_macros */ AE_SAT_ADD_U(uint8_t, UINT8_MAX, a, b); }
static uint8_t ae_sat_sub_u8(uint8_t a, uint8_t b) { /* ae_int_macros */ AE_SAT_SUB_U(uint8_t, a, b); }
static uint8_t ae_sat_mul_u8(uint8_t a, uint8_t b) { /* ae_int_macros */ AE_SAT_MUL_U(uint8_t, UINT8_MAX, a, b); }
static bool ae_opt_add_u8(uint8_t a, uint8_t b, uint8_t *out) { /* ae_int_macros */ AE_OPT_ADD_U(uint8_t, UINT8_MAX, a, b, out); }
static bool ae_opt_sub_u8(uint8_t a, uint8_t b, uint8_t *out) { /* ae_int_macros */ AE_OPT_SUB_U(uint8_t, a, b, out); }
static bool ae_opt_mul_u8(uint8_t a, uint8_t b, uint8_t *out) { /* ae_int_macros */ AE_OPT_MUL_U(uint8_t, UINT8_MAX, a, b, out); }
static bool ae_opt_div_u8(uint8_t a, uint8_t b, uint8_t *out) { /* ae_int_macros */ AE_OPT_DIV_U(uint8_t, a, b, out); }
static bool ae_opt_mod_u8(uint8_t a, uint8_t b, uint8_t *out) { /* ae_int_macros */ AE_OPT_MOD_U(uint8_t, a, b, out); }

static uint16_t ae_wrap_add_u16(uint16_t a, uint16_t b) { return (uint16_t)(a + b); }
static uint16_t ae_wrap_sub_u16(uint16_t a, uint16_t b) { return (uint16_t)(a - b); }
static uint16_t ae_wrap_mul_u16(uint16_t a, uint16_t b) { return (uint16_t)(a * b); }
static uint16_t ae_wrap_neg_u16(uint16_t a) { return (uint16_t)(0 - a); }
static uint16_t ae_sat_add_u16(uint16_t a, uint16_t b) { /* ae_int_macros */ AE_SAT_ADD_U(uint16_t, UINT16_MAX, a, b); }
static uint16_t ae_sat_sub_u16(uint16_t a, uint16_t b) { /* ae_int_macros */ AE_SAT_SUB_U(uint16_t, a, b); }
static uint16_t ae_sat_mul_u16(uint16_t a, uint16_t b) { /* ae_int_macros */ AE_SAT_MUL_U(uint16_t, UINT16_MAX, a, b); }
static bool ae_opt_add_u16(uint16_t a, uint16_t b, uint16_t *out) { /* ae_int_macros */ AE_OPT_ADD_U(uint16_t, UINT16_MAX, a, b, out); }
static bool ae_opt_sub_u16(uint16_t a, uint16_t b, uint16_t *out) { /* ae_int_macros */ AE_OPT_SUB_U(uint16_t, a, b, out); }
static bool ae_opt_mul_u16(uint16_t a, uint16_t b, uint16_t *out) { /* ae_int_macros */ AE_OPT_MUL_U(uint16_t, UINT16_MAX, a, b, out); }
static bool ae_opt_div_u16(uint16_t a, uint16_t b, uint16_t *out) { /* ae_int_macros */ AE_OPT_DIV_U(uint16_t, a, b, out); }
static bool ae_opt_mod_u16(uint16_t a, uint16_t b, uint16_t *out) { /* ae_int_macros */ AE_OPT_MOD_U(uint16_t, a, b, out); }

static uint32_t ae_wrap_add_u32(uint32_t a, uint32_t b) { return (uint32_t)(a + b); }
static uint32_t ae_wrap_sub_u32(uint32_t a, uint32_t b) { return (uint32_t)(a - b); }
static uint32_t ae_wrap_mul_u32(uint32_t a, uint32_t b) { return (uint32_t)(a * b); }
static uint32_t ae_wrap_neg_u32(uint32_t a) { return (uint32_t)(0 - a); }
static uint32_t ae_sat_add_u32(uint32_t a, uint32_t b) { /* ae_int_macros */ AE_SAT_ADD_U(uint32_t, UINT32_MAX, a, b); }
static uint32_t ae_sat_sub_u32(uint32_t a, uint32_t b) { /* ae_int_macros */ AE_SAT_SUB_U(uint32_t, a, b); }
static uint32_t ae_sat_mul_u32(uint32_t a, uint32_t b) { /* ae_int_macros */ AE_SAT_MUL_U(uint32_t, UINT32_MAX, a, b); }
static bool ae_opt_add_u32(uint32_t a, uint32_t b, uint32_t *out) { /* ae_int_macros */ AE_OPT_ADD_U(uint32_t, UINT32_MAX, a, b, out); }
static bool ae_opt_sub_u32(uint32_t a, uint32_t b, uint32_t *out) { /* ae_int_macros */ AE_OPT_SUB_U(uint32_t, a, b, out); }
static bool ae_opt_mul_u32(uint32_t a, uint32_t b, uint32_t *out) { /* ae_int_macros */ AE_OPT_MUL_U(uint32_t, UINT32_MAX, a, b, out); }
static bool ae_opt_div_u32(uint32_t a, uint32_t b, uint32_t *out) { /* ae_int_macros */ AE_OPT_DIV_U(uint32_t, a, b, out); }
static bool ae_opt_mod_u32(uint32_t a, uint32_t b, uint32_t *out) { /* ae_int_macros */ AE_OPT_MOD_U(uint32_t, a, b, out); }

static uint64_t ae_wrap_add_u64(uint64_t a, uint64_t b) { return (uint64_t)(a + b); }
static uint64_t ae_wrap_sub_u64(uint64_t a, uint64_t b) { return (uint64_t)(a - b); }
static uint64_t ae_wrap_mul_u64(uint64_t a, uint64_t b) { return (uint64_t)(a * b); }
static uint64_t ae_wrap_neg_u64(uint64_t a) { return (uint64_t)(0 - a); }
static uint64_t ae_sat_add_u64(uint64_t a, uint64_t b) { /* ae_int_macros */ AE_SAT_ADD_U(uint64_t, UINT64_MAX, a, b); }
static uint64_t ae_sat_sub_u64(uint64_t a, uint64_t b) { /* ae_int_macros */ AE_SAT_SUB_U(uint64_t, a, b); }
static uint64_t ae_sat_mul_u64(uint64_t a, uint64_t b) { /* ae_int_macros */ AE_SAT_MUL_U(uint64_t, UINT64_MAX, a, b); }
static bool ae_opt_add_u64(uint64_t a, uint64_t b, uint64_t *out) { /* ae_int_macros */ AE_OPT_ADD_U(uint64_t, UINT64_MAX, a, b, out); }
static bool ae_opt_sub_u64(uint64_t a, uint64_t b, uint64_t *out) { /* ae_int_macros */ AE_OPT_SUB_U(uint64_t, a, b, out); }
static bool ae_opt_mul_u64(uint64_t a, uint64_t b, uint64_t *out) { /* ae_int_macros */ AE_OPT_MUL_U(uint64_t, UINT64_MAX, a, b, out); }
static bool ae_opt_div_u64(uint64_t a, uint64_t b, uint64_t *out) { /* ae_int_macros */ AE_OPT_DIV_U(uint64_t, a, b, out); }
static bool ae_opt_mod_u64(uint64_t a, uint64_t b, uint64_t *out) { /* ae_int_macros */ AE_OPT_MOD_U(uint64_t, a, b, out); }

static uintptr_t ae_wrap_add_usize(uintptr_t a, uintptr_t b) { return (uintptr_t)(a + b); }
static uintptr_t ae_wrap_sub_usize(uintptr_t a, uintptr_t b) { return (uintptr_t)(a - b); }
static uintptr_t ae_wrap_mul_usize(uintptr_t a, uintptr_t b) { return (uintptr_t)(a * b); }
static uintptr_t ae_wrap_neg_usize(uintptr_t a) { return (uintptr_t)(0 - a); }
static uintptr_t ae_sat_add_usize(uintptr_t a, uintptr_t b) { /* ae_int_macros */ AE_SAT_ADD_U(uintptr_t, UINTPTR_MAX, a, b); }
static uintptr_t ae_sat_sub_usize(uintptr_t a, uintptr_t b) { /* ae_int_macros */ AE_SAT_SUB_U(uintptr_t, a, b); }
static uintptr_t ae_sat_mul_usize(uintptr_t a, uintptr_t b) { /* ae_int_macros */ AE_SAT_MUL_U(uintptr_t, UINTPTR_MAX, a, b); }
static bool ae_opt_add_usize(uintptr_t a, uintptr_t b, uintptr_t *out) { /* ae_int_macros */ AE_OPT_ADD_U(uintptr_t, UINTPTR_MAX, a, b, out); }
static bool ae_opt_sub_usize(uintptr_t a, uintptr_t b, uintptr_t *out) { /* ae_int_macros */ AE_OPT_SUB_U(uintptr_t, a, b, out); }
static bool ae_opt_mul_usize(uintptr_t a, uintptr_t b, uintptr_t *out) { /* ae_int_macros */ AE_OPT_MUL_U(uintptr_t, UINTPTR_MAX, a, b, out); }
static bool ae_opt_div_usize(uintptr_t a, uintptr_t b, uintptr_t *out) { /* ae_int_macros */ AE_OPT_DIV_U(uintptr_t, a, b, out); }
static bool ae_opt_mod_usize(uintptr_t a, uintptr_t b, uintptr_t *out) { /* ae_int_macros */ AE_OPT_MOD_U(uintptr_t, a, b, out); }

static size_t ae_index(int64_t index, size_t len, size_t line, size_t column) {
    if (index < 0 || (uint64_t)index >= len) ae_fail("array index out of bounds", line, column);
    return (size_t)index;
}

// Owned length-carrying strings (ADR-8): len/slice are O(1); buffers stay
// NUL-terminated for C interop. Concatenation consumes both operands; moves
// copy the struct and forfeit the slot; releases are idempotent so
// overlapping release paths can never double-free.
typedef struct { char *ptr; int64_t len; } ae_string;
static const char ae_missing_key_text[] = "key not found";
static void ae_str_release(ae_string *slot) {
    // The miss text is read-only data, so release skips it by identity instead
    // of freeing it. Equality is defined for unrelated pointers; a range test
    // with < or > would not be.
    if (slot->ptr && slot->ptr != ae_missing_key_text) ae_free(slot->ptr);
    slot->ptr = NULL;
    slot->len = 0;
}
// Borrowed string view (ADR-8): zero-copy, never released. Views point into
// an owning buffer; the compile-time borrow tracker keeps the base alive.
typedef struct { const char *ptr; int64_t len; } ae_strview;
#define AE_VIEW(s) ((ae_strview){(s).ptr, (s).len})

// `print`'s shapes, all ending in a newline and all through one sink, so output
// keeps the order the program wrote it in.
static void ae_print_view(ae_strview text) {
    if (!ae_emit(text.ptr, text.len, 1)) ae_fail("output error", 0, 0);
}
static void ae_print_i64(int64_t value) {
    char buffer[32];
    const int length = snprintf(buffer, sizeof buffer, "%" PRId64, value);
    if (length <= 0 || !ae_emit(buffer, length, 1)) ae_fail("output error", 0, 0);
}
static void ae_print_u64(uint64_t value) {
    char buffer[32];
    const int length = snprintf(buffer, sizeof buffer, "%" PRIu64, value);
    if (length <= 0 || !ae_emit(buffer, length, 1)) ae_fail("output error", 0, 0);
}
static void ae_print_cstr(const char *text) {
    ae_print_view((ae_strview){ text, (int64_t)strlen(text) });
}
static void ae_print_bool(bool value) {
    if (!ae_emit(value ? "true" : "false", value ? 4 : 5, 1)) ae_fail("output error", 0, 0);
}
static size_t ae_utf8_encode(uint32_t cp, char out[4]) {
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
static void ae_print_char(uint32_t value) {
    char buf[4];
    const size_t len = ae_utf8_encode(value, buf);
    if (!ae_emit(buf, (int64_t)len, 1)) ae_fail("output error", 0, 0);
}
static uint32_t ae_u32_to_char(uint32_t value, size_t line, size_t column) {
    if (value > 0x10FFFFu || (value >= 0xD800u && value <= 0xDFFFu)) {
        ae_fail("invalid Unicode scalar value", line, column);
    }
    return value;
}
static ae_string ae_str_from(const char *text, size_t line, size_t column) {
    size_t n = strlen(text);
    char *copy = ae_alloc(n + 1, line, column);
    memcpy(copy, text, n + 1);
    ae_string s = { copy, (int64_t)n };
    return s;
}
static ae_string ae_str_cat(ae_string a, ae_string b, size_t line, size_t column) {
    if (a.len > INT64_MAX - b.len) { ae_str_release(&a); ae_str_release(&b); ae_fail("string too long", line, column); }
    const int64_t total = a.len + b.len;
    char *out = ae_alloc((size_t)total + 1, line, column);
    memcpy(out, a.ptr, (size_t)a.len);
    memcpy(out + a.len, b.ptr, (size_t)b.len + 1);
    ae_str_release(&a); ae_str_release(&b);
    ae_string s = { out, total };
    return s;
}
// Concatenations with a literal keep the program text out of the allocator:
// a literal is not an owned value, so it is neither copied into the heap
// first nor freed afterwards; the result owns one new buffer. strlen on a
// literal folds to a constant, so the length costs nothing.
// Formats an i64 into a caller-owned buffer (always >= 24 bytes) and returns
// the length. Same formatter as ae_to_string, minus the heap round-trip:
// a temp that dies inside one expression never needs to own memory.
static int64_t ae_int_to_buf(int64_t value, char *buffer, size_t line, size_t column) {
    const int length = snprintf(buffer, 24, "%" PRId64, value);
    if (length < 0 || length >= 24) ae_fail("cannot format integer", line, column);
    return length;
}
static int64_t ae_u64_to_buf(uint64_t value, char *buffer, size_t line, size_t column) {
    const int length = snprintf(buffer, 24, "%" PRIu64, value);
    if (length < 0 || length >= 24) ae_fail("cannot format integer", line, column);
    return length;
}
// `"lit" + to_string(n)` with the digits formatted on the stack: the result
// owns one buffer, and neither the literal nor the digits touch the allocator.
static ae_string ae_str_cat_lit_buf(const char *lit, const char *digits, int64_t len, size_t line, size_t column) {
    const size_t n = strlen(lit);
    if (len > INT64_MAX - (int64_t)n) ae_fail("string too long", line, column);
    char *out = ae_alloc(n + (size_t)len + 1, line, column);
    memcpy(out, lit, n);
    memcpy(out + n, digits, (size_t)len);
    out[n + len] = 0;
    ae_string s = { out, (int64_t)n + len };
    return s;
}
// `to_string(n) + "lit"`: the digits go in front, same zero-alloc rule.
static ae_string ae_str_cat_buf_lit(const char *digits, int64_t len, const char *lit, size_t line, size_t column) {
    const size_t n = strlen(lit);
    if (len > INT64_MAX - (int64_t)n) ae_fail("string too long", line, column);
    char *out = ae_alloc((size_t)len + n + 1, line, column);
    memcpy(out, digits, (size_t)len);
    memcpy(out + len, lit, n + 1);
    ae_string s = { out, len + (int64_t)n };
    return s;
}
static ae_string ae_str_cat_lit_left(const char *lit, ae_string b, size_t line, size_t column) {
    const size_t n = strlen(lit);
    if (b.len > (int64_t)(INT64_MAX - (int64_t)n)) { ae_str_release(&b); ae_fail("string too long", line, column); }
    const int64_t total = (int64_t)n + b.len;
    char *out = ae_alloc((size_t)total + 1, line, column);
    memcpy(out, lit, n);
    memcpy(out + n, b.ptr, (size_t)b.len + 1);
    ae_str_release(&b);
    ae_string s = { out, total };
    return s;
}
static ae_string ae_str_cat_lit_right(ae_string a, const char *lit, size_t line, size_t column) {
    const size_t n = strlen(lit);
    if ((int64_t)n > INT64_MAX - a.len) { ae_str_release(&a); ae_fail("string too long", line, column); }
    const int64_t total = a.len + (int64_t)n;
    char *out = ae_alloc((size_t)total + 1, line, column);
    memcpy(out, a.ptr, (size_t)a.len);
    memcpy(out + a.len, lit, n + 1);
    ae_str_release(&a);
    ae_string s = { out, total };
    return s;
}
static void ae_str_forfeit(ae_string *slot) { slot->ptr = NULL; slot->len = 0; }
static bool ae_str_eq(ae_strview a, ae_strview b) {
    if (a.len != b.len) return false;
    // Map keys are short ("k1234"): a byte loop inlines and beats a memcmp call
    // whose fixed cost is several times the comparison itself (ADR-56).
    if (a.len <= 8) {
        for (int64_t i = 0; i < a.len; i++) {
            if (a.ptr[i] != b.ptr[i]) return false;
        }
        return true;
    }
    return memcmp(a.ptr, b.ptr, (size_t)a.len) == 0;
}
static int64_t ae_str_len(ae_strview s, size_t line, size_t column) {
    (void)line; (void)column;
    return s.len;
}
// UTF-8 (ADR-17): len/slice count bytes, but a slice may not end in the middle
// of a character. Every byte of a multi-byte sequence except the first has the
// pattern 10xxxxxx, so checking that one byte is O(1) and cannot be fooled by
// the slice length. Strings that already contain malformed bytes are left
// alone: the rule forbids introducing broken sequences, it does not validate
// input (read_file and argv are byte streams, and forcing a validation pass
// would make every load O(n) for a guarantee callers rarely want).
static bool ae_utf8_continue(unsigned char byte) { return (byte & 0xC0) == 0x80; }
static void ae_utf8_check(ae_strview s, int64_t start, int64_t end, size_t line, size_t column) {
    if ((start > 0 && start < s.len && ae_utf8_continue((unsigned char)s.ptr[start]))
        || (end > 0 && end < s.len && ae_utf8_continue((unsigned char)s.ptr[end]))) {
        ae_fail("string index splits a UTF-8 character", line, column);
    }
}
static ae_strview ae_slice_view(ae_strview s, int64_t start, int64_t end, size_t line, size_t column) {
    if (start < 0 || end < start || end > s.len) ae_fail("string index out of bounds", line, column);
    ae_utf8_check(s, start, end, line, column);
    ae_strview r = { s.ptr + start, end - start };
    return r;
}
static ae_string ae_slice(ae_strview s, int64_t start, int64_t end, size_t line, size_t column) {
    if (start < 0 || end < start || end > s.len) ae_fail("string index out of bounds", line, column);
    ae_utf8_check(s, start, end, line, column);
    char *out = ae_alloc((size_t)(end - start) + 1, line, column);
    memcpy(out, s.ptr + start, (size_t)(end - start));
    out[end - start] = 0;
    ae_string r = { out, end - start };
    return r;
}
// ASCII whitespace, matching what a text file usually contains. Trimming is
// deliberately byte-oriented: it never removes part of a multi-byte character
// (no UTF-8 continuation byte is whitespace).
static bool ae_is_space(unsigned char byte) {
    return byte == ' ' || byte == '\t' || byte == '\n' || byte == '\r' || byte == '\v' || byte == '\f';
}
// Prefix/suffix tests compare bytes, so a suffix check like ends_with(name,
// ".txt") cannot be fooled by a name such as "a.txt.bak" the way contains() can.
static bool ae_starts_with(ae_strview s, ae_strview prefix) {
    return prefix.len <= s.len && (prefix.len == 0 || memcmp(s.ptr, prefix.ptr, (size_t)prefix.len) == 0);
}
static bool ae_ends_with(ae_strview s, ae_strview suffix) {
    return suffix.len <= s.len && (suffix.len == 0 || memcmp(s.ptr + (s.len - suffix.len), suffix.ptr, (size_t)suffix.len) == 0);
}
static bool ae_contains(ae_strview haystack, ae_strview needle, size_t line, size_t column) {
    // strstr carries the platform's optimized substring search; buffers are
    // NUL-terminated so embedded-NUL truncation matches len/bytes semantics.
    (void)line; (void)column;
    if (needle.len == 0) return true;
    if (haystack.len < needle.len) return false;
    return strstr(haystack.ptr, needle.ptr) != NULL;
}

static ae_string ae_str_from_len(const char *p, int64_t n, size_t line, size_t column) {
    char *copy = ae_alloc((size_t)n + 1, line, column);
    memcpy(copy, p, (size_t)n);
    copy[n] = 0;
    ae_string out = { copy, n };
    return out;
}
// Concatenation may not consume a borrowed view, so views are copied into the
// new buffer first. The view itself stays valid and untouched.
static ae_string ae_str_copy_view(ae_strview s, size_t line, size_t column) {
    return ae_str_from_len(s.ptr, s.len, line, column);
}
// Number of characters, i.e. number of bytes that are not continuations. For
// well-formed text that is the codepoint count; for stray continuation bytes
// the count is still monotone and never exceeds the byte length.
static int64_t ae_utf8_len(ae_strview s, size_t line, size_t column) {
    (void)line; (void)column;
    int64_t count = 0;
    for (int64_t i = 0; i < s.len; i++) {
        if (!ae_utf8_continue((unsigned char)s.ptr[i])) count = count + 1;
    }
    return count;
}
// First `n` characters as an owned string: the character-safe way to truncate
// text without computing byte offsets by hand. `n` beyond the end copies
// everything, so a caller can clamp with utf8_len or just ask for a big number.
static ae_string ae_utf8_prefix(ae_strview s, int64_t n, size_t line, size_t column) {
    if (n < 0) ae_fail("character index out of bounds", line, column);
    int64_t at = 0;
    int64_t seen = 0;
    while (at < s.len && seen < n) {
        at = at + 1;
        while (at < s.len && ae_utf8_continue((unsigned char)s.ptr[at])) at = at + 1;
        seen = seen + 1;
    }
    return ae_str_from_len(s.ptr, at, line, column);
}
// Leading and trailing whitespace removed, as a fresh owned string. The result
// is always a valid slice of the input, so it cannot split a character.
static ae_string ae_trim(ae_strview s, size_t line, size_t column) {
    int64_t start = 0;
    int64_t end = s.len;
    while (start < end && ae_is_space((unsigned char)s.ptr[start])) start = start + 1;
    while (end > start && ae_is_space((unsigned char)s.ptr[end - 1])) end = end - 1;
    return ae_str_from_len(s.ptr + start, end - start, line, column);
}

// Owned vector of owned strings. Elements are borrow-only reads.
//
// `arena` is NULL for vectors built element by element (`push`, `args`, `keys`):
// every element owns its own buffer and is released individually. `split` and
// `lines` instead copy all tokens into one arena and point every element into
// it, so N tokens cost two allocations instead of N (ADR-49). Elements whose
// pointer lies inside the arena are cleared without freeing; the arena is
// released with the vector.
typedef struct { ae_string *data; int64_t len; int64_t cap; char *arena; int64_t arena_len; } ae_strvec;
static int ae_strvec_borrowed(const ae_strvec *v, const ae_string *s) {
    // Compare as integers: two pointers into unrelated objects have no defined
    // ordering in ISO C, but their uintptr_t values are comparable here.
    return v->arena != NULL
        && s->ptr != NULL
        && (uintptr_t)s->ptr >= (uintptr_t)v->arena
        && (uintptr_t)s->ptr < (uintptr_t)v->arena + (uintptr_t)v->arena_len;
}
static ae_strvec ae_strvec_new(size_t line, size_t column) {
    ae_strvec v;
    v.len = 0;
    v.cap = 4;
    v.data = ae_alloc((size_t)v.cap * sizeof(ae_string), line, column);
    v.arena = NULL;
    v.arena_len = 0;
    return v;
}
static void ae_strvec_push(ae_strvec *v, ae_string s, size_t line, size_t column) {
    if (v->len == v->cap) {
        // A fused `lines("")` vector starts at capacity 0, so growth must have a
        // floor; doubling from zero would ask for zero bytes and then write
        // past it.
        if (v->cap > INT64_MAX / 2) ae_fail("vector too large", line, column);
        int64_t ncap = v->cap < 4 ? 4 : v->cap * 2;
        ae_string *nd = ae_alloc_resize(v->data, (size_t)ncap * sizeof(ae_string), line, column);
        v->data = nd;
        v->cap = ncap;
    }
    v->data[v->len] = s;
    v->len = v->len + 1;
}
static void ae_strvec_pop(ae_strvec *v, size_t line, size_t column) {
    if (v->len == 0) ae_fail("pop from empty vector", line, column);
    v->len = v->len - 1;
    if (ae_strvec_borrowed(v, &v->data[v->len])) {
        v->data[v->len].ptr = NULL;
        v->data[v->len].len = 0;
    } else {
        ae_str_release(&v->data[v->len]);
    }
}
static int64_t ae_strvec_len(ae_strvec v, size_t line, size_t column) {
    (void)line; (void)column;
    return v.len;
}
static int64_t ae_strvec_index(int64_t index, int64_t len, size_t line, size_t column) {
    if (index < 0 || index >= len) ae_fail("array index out of bounds", line, column);
    return index;
}
static ae_string *ae_strvec_get(ae_strvec *v, int64_t index, size_t line, size_t column) {
    ae_strvec_index(index, v->len, line, column);
    return &v->data[index];
}
// Borrowed string vectors (&vec[string]) read elements through this accessor.
static const ae_string *ae_strvec_get_const(const ae_strvec *v, int64_t index, size_t line, size_t column) {
    ae_strvec_index(index, v->len, line, column);
    return &v->data[index];
}
static void ae_strvec_release(ae_strvec *v) {
    for (int64_t i = 0; i < v->len; i++) {
        if (ae_strvec_borrowed(v, &v->data[i])) {
            v->data[i].ptr = NULL;
            v->data[i].len = 0;
        } else {
            ae_str_release(&v->data[i]);
        }
    }
    ae_free(v->data);
    ae_free(v->arena);
    v->data = NULL;
    v->arena = NULL;
    v->arena_len = 0;
    v->len = 0;
    v->cap = 0;
}
// Overwriting an element releases the old value first; an element inside the
// vector's own arena is cleared without freeing (ADR-49).
static void ae_strvec_clear_slot(ae_strvec *v, int64_t index, size_t line, size_t column) {
    ae_strvec_index(index, v->len, line, column);
    if (ae_strvec_borrowed(v, &v->data[index])) {
        v->data[index].ptr = NULL;
        v->data[index].len = 0;
    } else {
        ae_str_release(&v->data[index]);
    }
}
static void ae_strvec_forfeit(ae_strvec *v) {
    v->data = NULL;
    v->arena = NULL;
    v->arena_len = 0;
    v->len = 0;
    v->cap = 0;
}
// Next separator position at or after `from`, or -1 when none is left. A
// one-byte separator (the common `" "` and `"\n"` shapes) goes through memchr,
// which is SIMD-optimised in both libc implementations, instead of comparing
// byte by byte (ADR-49).
static int64_t ae_find_sep(const char *base, int64_t from, int64_t len, ae_strview sep) {
    if (from >= len || sep.len > len - from) return -1;
    if (sep.len == 1) {
        const char *hit = memchr(base + from, sep.ptr[0], (size_t)(len - from));
        return hit == NULL ? -1 : (int64_t)(hit - base);
    }
    const char first = sep.ptr[0];
    for (int64_t i = from; i + sep.len <= len; ) {
        const char *hit = memchr(base + i, first, (size_t)(len - sep.len - i + 1));
        if (!hit) return -1;
        i = (int64_t)(hit - base);
        if (memcmp(hit, sep.ptr, (size_t)sep.len) == 0) return i;
        i += 1;
    }
    return -1;
}

// Views only: splitting never takes ownership of its operands, so a borrowed
// `&string` argument works exactly like an owned one.
//
// Two passes (ADR-49): the first counts tokens and the bytes they carry, so the
// pointer array and the byte arena are allocated once at their exact size.
// Each token keeps its terminator, so `ptr` stays usable with C string APIs.
static ae_strvec ae_split(ae_strview s, ae_strview sep, size_t line, size_t column) {
    if (sep.len == 0) ae_fail("empty separator", line, column);
    int64_t count = 1;
    for (int64_t at = ae_find_sep(s.ptr, 0, s.len, sep); at >= 0; at = ae_find_sep(s.ptr, at + sep.len, s.len, sep)) {
        count += 1;
    }
    if (count > INT64_MAX / (int64_t)sizeof(ae_string)) ae_fail("vector too large", line, column);
    // Input bytes minus the separators, plus one terminator per token.
    int64_t bytes = (s.len - (count - 1) * sep.len) + count;
    ae_strvec out;
    out.data = ae_alloc((size_t)count * sizeof(ae_string), line, column);
    out.len = 0;
    out.cap = count;
    out.arena = ae_alloc((size_t)bytes, line, column);
    out.arena_len = bytes;
    char *cursor = out.arena;
    int64_t start = 0;
    for (int64_t at = ae_find_sep(s.ptr, 0, s.len, sep); at >= 0; at = ae_find_sep(s.ptr, at + sep.len, s.len, sep)) {
        int64_t n = at - start;
        if (n > 0) memcpy(cursor, s.ptr + start, (size_t)n);
        cursor[n] = 0;
        out.data[out.len].ptr = cursor;
        out.data[out.len].len = n;
        out.len += 1;
        cursor += n + 1;
        start = at + sep.len;
    }
    int64_t n = s.len - start;
    if (n > 0) memcpy(cursor, s.ptr + start, (size_t)n);
    cursor[n] = 0;
    out.data[out.len].ptr = cursor;
    out.data[out.len].len = n;
    out.len += 1;
    return out;
}

static ae_string ae_join(const ae_strvec *v, ae_strview sep, size_t line, size_t column) {
    if (v->len == 0) {
        char *empty = ae_alloc(1, line, column);
        empty[0] = 0;
        ae_string out = { empty, 0 };
        return out;
    }
    if (sep.len != 0 && v->len - 1 > INT64_MAX / sep.len) ae_fail("string too long", line, column);
    int64_t total = sep.len * (v->len - 1);
    for (int64_t i = 0; i < v->len; i++) {
        if (total > INT64_MAX - v->data[i].len) ae_fail("string too long", line, column);
        total += v->data[i].len;
    }
    char *out = ae_alloc((size_t)total + 1, line, column);
    int64_t at = 0;
    for (int64_t i = 0; i < v->len; i++) {
        if (i > 0) { memcpy(out + at, sep.ptr, (size_t)sep.len); at += sep.len; }
        memcpy(out + at, v->data[i].ptr, (size_t)v->data[i].len);
        at += v->data[i].len;
    }
    out[at] = 0;
    ae_string result = { out, total };
    return result;
}

#define AE_TOKENS_SEP_MAX 16
typedef struct { ae_strview text; char sep[AE_TOKENS_SEP_MAX]; int64_t sep_len; int64_t pos; int64_t done; } ae_tokens;
// Lazy token cursor (ADR-53): the same token stream as `split`, but produced
// one token at a time. It holds a view of the base string and an inline copy of
// the separator, and allocates nothing, so iterating a 2.6 MB file costs no
// per-token memory. The token boundaries match `split` exactly, including the
// trailing empty token after a final separator and the single empty token for
// empty input. The separator is copied into the cursor rather than pointed at:
// the argument is usually a temporary that is released at the end of the
// statement, and the cursor must stay valid for the whole loop. Sixteen bytes
// cover every realistic delimiter, so the cursor owns no memory and a longer
// one is rejected instead of allocated.
static ae_tokens ae_tokens_new(ae_strview text, ae_strview sep, size_t line, size_t column) {
    if (sep.len == 0) ae_fail("empty separator", line, column);
    if (sep.len > AE_TOKENS_SEP_MAX) ae_fail("token separator longer than 16 bytes", line, column);
    ae_tokens cursor;
    cursor.text = text;
    memcpy(cursor.sep, sep.ptr, (size_t)sep.len);
    cursor.sep_len = sep.len;
    cursor.pos = 0;
    cursor.done = 0;
    return cursor;
}
// Rewinds to the first token, so a second loop over the same binding sees the
// same stream instead of an empty one.
static void ae_tokens_reset(ae_tokens *cursor) { cursor->pos = 0; cursor->done = 0; }
// Returns false once the stream is exhausted; `out` points into the base text.
static bool ae_tokens_next(ae_tokens *cursor, ae_strview *out) {
    if (cursor->done) return false;
    int64_t start = cursor->pos;
    int64_t at = -1;
    if (cursor->sep_len == 1) {
        const char *hit = memchr(cursor->text.ptr + start, cursor->sep[0], (size_t)(cursor->text.len - start));
        if (hit) at = (int64_t)(hit - cursor->text.ptr);
    } else {
        const char first = cursor->sep[0];
        for (int64_t i = start; i + cursor->sep_len <= cursor->text.len; ) {
            const char *hit = memchr(cursor->text.ptr + i, first, (size_t)(cursor->text.len - cursor->sep_len - i + 1));
            if (!hit) break;
            i = (int64_t)(hit - cursor->text.ptr);
            if (memcmp(hit, cursor->sep, (size_t)cursor->sep_len) == 0) { at = i; break; }
            i += 1;
        }
    }
    if (at < 0) {
        out->ptr = cursor->text.ptr + start;
        out->len = cursor->text.len - start;
        cursor->pos = cursor->text.len;
        cursor->done = 1;
    } else {
        out->ptr = cursor->text.ptr + start;
        out->len = at - start;
        cursor->pos = at + cursor->sep_len;
    }
    return true;
}

// lines(): split on newlines. Trailing newlines do not produce a final empty
// line, matching common stdlib lines(). Two passes with memchr (ADR-49): count
// lines first, then fill one exact-size pointer array and one byte arena.
static ae_strvec ae_lines(ae_strview s, size_t line, size_t column) {
    int64_t newlines = 0;
    const char *p = s.ptr;
    int64_t rem = s.len;
    while (rem > 0) {
        const char *hit = memchr(p, '\n', (size_t)rem);
        if (hit == NULL) break;
        newlines += 1;
        rem -= (int64_t)(hit - p) + 1;
        p = hit + 1;
    }
    int64_t count = newlines + ((s.len > 0 && s.ptr[s.len - 1] != '\n') ? 1 : 0);
    if (count > INT64_MAX / (int64_t)sizeof(ae_string)) ae_fail("vector too large", line, column);
    int64_t bytes = (s.len - newlines) + count;
    ae_strvec out;
    out.data = ae_alloc((size_t)(count > 0 ? count : 1) * sizeof(ae_string), line, column);
    out.len = 0;
    out.cap = count;
    out.arena = ae_alloc((size_t)(bytes > 0 ? bytes : 1), line, column);
    out.arena_len = bytes > 0 ? bytes : 1;
    char *cursor = out.arena;
    p = s.ptr;
    rem = s.len;
    while (rem > 0) {
        const char *hit = memchr(p, '\n', (size_t)rem);
        if (hit == NULL) break;
        int64_t n = (int64_t)(hit - p);
        if (n > 0) memcpy(cursor, p, (size_t)n);
        cursor[n] = 0;
        out.data[out.len].ptr = cursor;
        out.data[out.len].len = n;
        out.len += 1;
        cursor += n + 1;
        rem -= n + 1;
        p = hit + 1;
    }
    if (rem > 0) {
        if (rem > 0) memcpy(cursor, p, (size_t)rem);
        cursor[rem] = 0;
        out.data[out.len].ptr = cursor;
        out.data[out.len].len = rem;
        out.len += 1;
    }
    return out;
}

// count_lines_with(text, pattern): newline-delimited line count where the
// line contains the pattern. Single pass, zero allocations; line search
// uses memchr for first-byte skipping.
static int ae_line_contains(const char *h, int64_t hl, ae_strview pat) {
    if (pat.len == 0) return 1;
    if (pat.len > hl) return 0;
    const char first = pat.ptr[0];
    int64_t i = 0;
    while (i + pat.len <= hl) {
        const char *hit = memchr(h + i, first, (size_t)(hl - i - pat.len + 1));
        if (!hit) return 0;
        i = (int64_t)(hit - h);
        if (memcmp(h + i, pat.ptr, (size_t)pat.len) == 0) return 1;
        i = i + 1;
    }
    return 0;
}
static int64_t ae_count_lines_with(ae_strview s, ae_strview pat, size_t line, size_t column) {
    if (s.len == 0) return 0;
    int64_t count = 0;
    const char *p = s.ptr;
    int64_t remain = s.len;
    while (remain > 0) {
        const char *nl = memchr(p, 0x0a, (size_t)remain);
        int64_t llen = nl ? (int64_t)(nl - p) : remain;
        if (ae_line_contains(p, llen, pat)) count = count + 1;
        if (!nl) break;
        remain -= llen + 1;
        p += llen + 1;
    }
    return count;
}

// Program arguments bridge: the generated main records argc/argv here.
static int64_t ae_argc_g = 0;
static char **ae_argv_g = NULL;

// Owned growable i64 vector. Moves copy the struct and forfeit the source
// slot (zeroed without freeing); releases are idempotent, so overlapping
// release paths can never double-free.
typedef struct { int64_t *data; int64_t len; int64_t cap; } ae_vec;
static ae_vec ae_vec_new(size_t line, size_t column) {
    ae_vec v;
    v.len = 0;
    v.cap = 4;
    v.data = ae_alloc((size_t)v.cap * sizeof(int64_t), line, column);
    return v;
}
static void ae_vec_push(ae_vec *v, int64_t x, size_t line, size_t column) {
    if (v->len == v->cap) {
        if (v->cap > INT64_MAX / 2) ae_fail("vector too large", line, column);
        int64_t ncap = v->cap * 2;
        int64_t *nd = ae_alloc_resize(v->data, (size_t)ncap * sizeof(int64_t), line, column);
        v->data = nd;
        v->cap = ncap;
    }
    v->data[v->len] = x;
    v->len = v->len + 1;
}
static void ae_vec_pop(ae_vec *v, size_t line, size_t column) {
    if (v->len == 0) ae_fail("pop from empty vector", line, column);
    v->len = v->len - 1;
}
static int64_t ae_vec_len(ae_vec v, size_t line, size_t column) {
    (void)line; (void)column;
    return v.len;
}
static int64_t ae_vec_index(int64_t index, int64_t len, size_t line, size_t column) {
    if (index < 0 || index >= len) ae_fail("array index out of bounds", line, column);
    return index;
}
static int64_t ae_vec_get(ae_vec v, int64_t index, size_t line, size_t column) {
    ae_vec_index(index, v.len, line, column);
    return v.data[index];
}
static void ae_vec_release(ae_vec *v) {
    ae_free(v->data);
    v->data = NULL;
    v->len = 0;
    v->cap = 0;
}
// Zero a slot whose contents were moved elsewhere: no free.
static void ae_vec_forfeit(ae_vec *v) {
    v->data = NULL;
    v->len = 0;
    v->cap = 0;
}

typedef struct { double *data; int64_t len; int64_t cap; } ae_f64vec; // ADR-61
static ae_f64vec ae_f64vec_new(size_t line, size_t column) {
    // Same shape as ae_vec with double elements (ADR-61). The element
    // type is a plain value — no ownership, no borrowed views, no release
    // per element — so this family has no borrow tracking at all.
    ae_f64vec v;
    v.len = 0;
    v.cap = 4;
    v.data = ae_alloc((size_t)v.cap * sizeof(double), line, column);
    return v;
}
static void ae_f64vec_push(ae_f64vec *v, double x, size_t line, size_t column) {
    if (v->len == v->cap) {
        if (v->cap > INT64_MAX / 2) ae_fail("vector too large", line, column);
        int64_t ncap = v->cap * 2;
        double *nd = ae_alloc_resize(v->data, (size_t)ncap * sizeof(double), line, column);
        v->data = nd;
        v->cap = ncap;
    }
    v->data[v->len] = x;
    v->len = v->len + 1;
}
static void ae_f64vec_pop(ae_f64vec *v, size_t line, size_t column) {
    if (v->len == 0) ae_fail("pop from empty vector", line, column);
    v->len = v->len - 1;
}
static int64_t ae_f64vec_len(ae_f64vec v, size_t line, size_t column) {
    (void)line; (void)column;
    return v.len;
}
static double ae_f64vec_get(ae_f64vec v, int64_t index, size_t line, size_t column) {
    ae_vec_index(index, v.len, line, column);
    return v.data[index];
}
static void ae_f64vec_release(ae_f64vec *v) {
    ae_free(v->data);
    v->data = NULL;
    v->len = 0;
    v->cap = 0;
}
static void ae_f64vec_forfeit(ae_f64vec *v) {
    v->data = NULL;
    v->len = 0;
    v->cap = 0;
}

typedef struct { void *data; int64_t len; int64_t cap; int64_t elem_size; } ae_gvec; // ADR-0235
static ae_gvec ae_gvec_new(int64_t elem_size, size_t line, size_t column) {
    ae_gvec v;
    v.len = 0;
    v.cap = 4;
    v.elem_size = elem_size > 0 ? elem_size : 1;
    if ((size_t)v.cap > SIZE_MAX / (size_t)v.elem_size) ae_fail("vector too large", line, column);
    v.data = ae_alloc((size_t)v.cap * (size_t)v.elem_size, line, column);
    return v;
}
static void ae_gvec_push(ae_gvec *v, const void *elem, size_t line, size_t column) {
    if (v->len == v->cap) {
        if (v->cap > INT64_MAX / 2) ae_fail("vector too large", line, column);
        int64_t ncap = v->cap * 2;
        if ((size_t)ncap > SIZE_MAX / (size_t)v->elem_size) ae_fail("vector too large", line, column);
        void *nd = ae_alloc_resize(v->data, (size_t)ncap * (size_t)v->elem_size, line, column);
        v->data = nd;
        v->cap = ncap;
    }
    memcpy((char *)v->data + (size_t)v->len * (size_t)v->elem_size, elem, (size_t)v->elem_size);
    v->len = v->len + 1;
}
static void ae_gvec_pop(ae_gvec *v, size_t line, size_t column) {
    if (v->len == 0) ae_fail("pop from empty vector", line, column);
    v->len = v->len - 1;
}
static int64_t ae_gvec_len(ae_gvec v, size_t line, size_t column) {
    (void)line; (void)column;
    return v.len;
}
static const void *ae_gvec_at(const ae_gvec *v, int64_t index, size_t line, size_t column) {
    ae_vec_index(index, v->len, line, column);
    return (const char *)v->data + (size_t)index * (size_t)v->elem_size;
}
static void ae_gvec_set(ae_gvec *v, int64_t index, const void *elem, size_t line, size_t column) {
    ae_vec_index(index, v->len, line, column);
    memcpy((char *)v->data + (size_t)index * (size_t)v->elem_size, elem, (size_t)v->elem_size);
}
static void ae_gvec_release(ae_gvec *v) {
    ae_free(v->data);
    v->data = NULL;
    v->len = 0;
    v->cap = 0;
    v->elem_size = 0;
}
static void ae_gvec_forfeit(ae_gvec *v) {
    v->data = NULL;
    v->len = 0;
    v->cap = 0;
    v->elem_size = 0;
}

// In-place orderings for i64 vectors. qsort is the C library's introsort and
// the comparator is a plain function, so no allocation and no closure state is
// involved; vectors are at most INT64_MAX/8 elements, which qsort accepts as a
// size_t count on both platforms.
static int ae_cmp_i64(const void *left, const void *right) {
    const int64_t a = *(const int64_t *)left;
    const int64_t b = *(const int64_t *)right;
    return (a > b) - (a < b);
}
static void ae_vec_sort(ae_vec *v) {
    if (v->len > 1) qsort(v->data, (size_t)v->len, sizeof(int64_t), ae_cmp_i64);
}
static void ae_vec_reverse(ae_vec *v) {
    for (int64_t i = 0, j = v->len - 1; i < j; i = i + 1, j = j - 1) {
        const int64_t swap = v->data[i];
        v->data[i] = v->data[j];
        v->data[j] = swap;
    }
}

// File I/O. Paths are UTF-8; on Windows they are widened for _wfopen so
// non-ASCII paths behave like the rest of the platform layer.
#ifdef _WIN32
// The full windows.h pulls in winsock, RPC, GDI and shell declarations that
// this runtime never uses. Measured on WinLibs GCC 16.2: parsing the plain
// header costs ~690 ms, with this define ~520 ms. The runtime pruner drops
// this include entirely when no retained function needs a Win32 declaration,
// so pure-computation programs do not pay for it at build time.
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>
// UTF-8 to UTF-16 for the wide-character Win32 APIs; callers free the result.
static wchar_t *ae_widen(const char *text) { /* win32-header */
    const int len = MultiByteToWideChar(CP_UTF8, 0, text, -1, NULL, 0);
    if (len <= 0) return NULL;
    wchar_t *wide = ae_alloc_try((size_t)len * sizeof(wchar_t));
    if (!wide) return NULL;
    MultiByteToWideChar(CP_UTF8, 0, text, -1, wide, len);
    return wide;
}
// UTF-16 to a fresh UTF-8 buffer of `len` units; returns NULL on failure.
static char *ae_narrow(const wchar_t *text, int len) { /* win32-header */
    if (len <= 0) return NULL;
    const int size = WideCharToMultiByte(CP_UTF8, 0, text, len, NULL, 0, NULL, NULL);
    if (size <= 0) return NULL;
    char *narrow = ae_alloc_try((size_t)size + 1);
    if (!narrow) return NULL;
    WideCharToMultiByte(CP_UTF8, 0, text, len, narrow, size, NULL, NULL);
    narrow[size] = 0;
    return narrow;
}
static FILE *ae_fopen(const char *path, const char *mode) { /* win32-header */
    wchar_t *wpath = ae_widen(path);
    wchar_t *wmode = ae_widen(mode);
    if (!wpath || !wmode) { ae_free(wpath); ae_free(wmode); return NULL; }
    FILE *f = _wfopen(wpath, wmode);
    ae_free(wpath);
    ae_free(wmode);
    return f;
}
#else
static FILE *ae_fopen(const char *path, const char *mode) { return fopen(path, mode); }
#endif

#ifdef _WIN32
// Windows hands `main` the command line in the ANSI code page, so a path typed
// on a Chinese system arrives as GBK bytes while every file API expects UTF-8 --
// the program could not open its own arguments (ADR-54). Convert each argument
// through UTF-16 once, here, so `args()` matches the rest of the language.
// This keeps <windows.h> in programs that read their arguments; programs that
// do not still prune it (ADR-39).
static char *ae_utf8_from_acp(const char *text) { /* win32-header */
    const int wide_len = MultiByteToWideChar(CP_ACP, 0, text, -1, NULL, 0);
    if (wide_len <= 0) return NULL;
    wchar_t *wide = ae_alloc_try((size_t)wide_len * sizeof(wchar_t));
    if (!wide) return NULL;
    char *utf8 = NULL;
    if (MultiByteToWideChar(CP_ACP, 0, text, -1, wide, wide_len) > 0) {
        const int size = WideCharToMultiByte(CP_UTF8, 0, wide, wide_len - 1, NULL, 0, NULL, NULL);
        if (size > 0) {
            utf8 = ae_alloc_try((size_t)size + 1);
            if (utf8) {
                WideCharToMultiByte(CP_UTF8, 0, wide, wide_len - 1, utf8, size, NULL, NULL);
                utf8[size] = 0;
            }
        }
    }
    ae_free(wide);
    return utf8;
}
#endif
// The CRT hands `main` the command line decoded into the ANSI code page, so on
// a machine whose code page cannot represent the text (437 on a CI runner,
// say) a path typed as UTF-16 arrives with '?' placeholders and every later
// widen() sees the corruption. The command line itself is always UTF-16, so
// the arguments are re-derived from it once, in UTF-8, and the process uses
// those instead — the intent of ADR-54 without the ACP round trip. The
// replacement entries live for the program's whole run, like argv itself.
// Command-line arguments are converted only when the program actually reads
// them, by the string-vector accessor further below, so a program that never
// allocates carries none of the conversion's allocator (M3.1.01). The
// generated main still calls this and stores argc/argv in the globals below.
// Comments here deliberately avoid naming runtime symbols: the pruner walks
// identifiers in unit bodies, comments included, and this function is kept
// for every program.
static void ae_args_init(int argc, char **argv) {
    (void)argc;
    (void)argv;
}
// The converted argv is cached for the process lifetime, but exit returns the
// blocks it owns so a leak check reports only the allocator's retained state.
static char **ae_args_cached = NULL;
static char *ae_args_text = NULL;
static void ae_args_release_cache(void) {
    ae_free(ae_args_text);
    ae_free(ae_args_cached);
    ae_args_text = NULL;
    ae_args_cached = NULL;
}
static ae_strvec ae_args(size_t line, size_t column) { /* win32-declared */
    ae_strvec out = ae_strvec_new(line, column);
#ifdef _WIN32
    // Each declaration matches <windows.h> exactly, so a program that keeps
    // the header for something else (a file API) still compiles.
    extern wchar_t *GetCommandLineW(void);
    extern wchar_t **CommandLineToArgvW(const wchar_t *text, int *count);
    extern void *LocalFree(void *mem);
    extern int lstrlenW(const wchar_t *text);
    // CP_UTF8 is 65001; the literal avoids pulling <windows.h> for the macro.
    extern int WideCharToMultiByte(unsigned page, unsigned long flags, const wchar_t *wide, int units, char *narrow, int bytes, const char *default_char, int *used_default);
    // The UTF-16 command line is converted here, on first read: a program
    // that never reads arguments allocates nothing for them (M3.1.01).
    // Until this runs, ae_argv_g holds the narrow argv main received,
    // which stays the fallback when conversion is impossible.
    static int converted = 0;
    if (!converted) {
        converted = 1;
        int count = 0;
        wchar_t **tokens = CommandLineToArgvW(GetCommandLineW(), &count);
        if (tokens && count == (int)ae_argc_g) {
            // One block holds every converted string, so the cache is two
            // allocations that exit can return (nulls stay the fallback).
            size_t total = 0;
            for (int i = 0; i < count; i++) {
                const int units = lstrlenW(tokens[i]);
                const int bytes = WideCharToMultiByte(65001, 0, tokens[i], units, NULL, 0, NULL, NULL);
                total += (size_t)(bytes > 0 ? bytes : 0) + 1;
            }
            char **utf8 = ae_alloc((size_t)count * sizeof(char *), line, column);
            char *text_block = ae_alloc(total, line, column);
            char *cursor = text_block;
            for (int i = 0; i < count; i++) {
                const int units = lstrlenW(tokens[i]);
                const int bytes = WideCharToMultiByte(65001, 0, tokens[i], units, NULL, 0, NULL, NULL);
                if (bytes <= 0) { utf8[i] = ae_argv_g[i]; continue; }
                WideCharToMultiByte(65001, 0, tokens[i], units, cursor, bytes, NULL, NULL);
                cursor[bytes] = 0;
                utf8[i] = cursor;
                cursor += (size_t)bytes + 1;
            }
            ae_argv_g = utf8;
            ae_args_cached = utf8;
            ae_args_text = text_block;
            atexit(ae_args_release_cache);
        }
        if (tokens) LocalFree(tokens);
    }
#endif
    // argv is UTF-8 on every platform by the time this runs: POSIX main
    // receives UTF-8 directly and the Windows branch above converts the
    // UTF-16 command line once, on first read.
    for (int64_t i = 0; i < ae_argc_g; i++) {
        ae_strvec_push(&out, ae_str_from(ae_argv_g[i], line, column), line, column);
    }
    return out;
}

static ae_string ae_read_file(const char *path, size_t line, size_t column) {
    FILE *f = ae_fopen(path, "rb");
    if (!f) ae_fail("cannot read file", line, column);
    if (fseek(f, 0, SEEK_END) != 0) { fclose(f); ae_fail("cannot read file", line, column); }
    long size = ftell(f);
    if (size < 0) { fclose(f); ae_fail("cannot read file", line, column); }
    rewind(f);
    size_t wanted = (size_t)size + 1;
    char *buffer = ae_alloc(wanted, line, column);
    size_t read = fread(buffer, 1, (size_t)size, f);
    fclose(f);
    if (read != (size_t)size) { ae_free(buffer); ae_fail("cannot read file", line, column); }
    buffer[size] = 0;
    ae_string out = { buffer, (int64_t)size };
    return out;
}
static int64_t ae_write_file(ae_string path, ae_strview content, size_t line, size_t column) {
    FILE *f = ae_fopen(path.ptr, "wb");
    if (!f) ae_fail("cannot write file", line, column);
    size_t written = fwrite(content.ptr, 1, (size_t)content.len, f);
    if (fclose(f) != 0 || written != (size_t)content.len) ae_fail("cannot write file", line, column);
    return (int64_t)written;
}
static ae_vec ae_bytes(ae_strview s, size_t line, size_t column) {
    if (s.len > INT64_MAX / (int64_t)sizeof(int64_t)) ae_fail("vector too large", line, column);
    ae_vec v;
    v.len = s.len;
    v.cap = s.len > 4 ? s.len : 4;
    v.data = ae_alloc((size_t)v.cap * sizeof(int64_t), line, column);
    for (int64_t i = 0; i < s.len; i++) {
        v.data[i] = (int64_t)(unsigned char)s.ptr[i];
    }
    return v;
}

// ---- Hash map: string keys -> i64 values ------------------------------------
// Open addressing with linear probing. A slot holds the key and the value; the
// empty (0), full (1) or deleted (2) byte lives in a parallel array (ADR-57), so
// slots stay 24 bytes and a probe usually settles on packed state bytes without
// touching one. A deleted slot keeps probing alive until the next rehash, so
// removing a key never breaks a chain.
typedef struct { ae_string key; int64_t value; } ae_map_slot;
typedef struct { ae_map_slot *slots; unsigned char *states; int64_t cap; int64_t len; int64_t used; } ae_map;

#define AE_MAP_EMPTY 0
#define AE_MAP_FULL 1
#define AE_MAP_DELETED 2

static ae_map ae_map_new(size_t line, size_t column) {
    (void)line; (void)column;
    ae_map m;
    m.slots = NULL;
    m.states = NULL;
    m.cap = 0;
    m.len = 0;
    m.used = 0;
    return m;
}
// FNV-1a: fixed and platform independent, so iteration order is reproducible.
static uint64_t ae_map_hash(ae_strview key) {
    uint64_t hash = UINT64_C(1469598103934665603);
    for (int64_t i = 0; i < key.len; i++) {
        hash ^= (uint64_t)(unsigned char)key.ptr[i];
        hash *= UINT64_C(1099511628211);
    }
    return hash;
}
static int64_t ae_map_slot_of(ae_strview key, int64_t cap) {
    return (int64_t)(ae_map_hash(key) & (uint64_t)(cap - 1));
}
// Index of the matching full slot, or -1. Stops only at an empty slot.
static int64_t ae_map_find(const ae_map *m, ae_strview key) {
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
static void ae_map_grow(ae_map *m, size_t line, size_t column) {
    int64_t cap = m->cap == 0 ? 8 : m->cap;
    // Rehash into a table twice as large once live entries fill half of it,
    // which also drops every tombstone. Half rather than three quarters: linear
    // probing walks one slot at a time, so the average probe count is what the
    // load factor really buys (ADR-56: measured 1.17x on the map workload for
    // one extra table, against 1.03x at five eighths).
    while ((m->len + 1) * 2 >= cap) {
        if (cap > INT64_MAX / 4) ae_fail("map too large", line, column);
        cap = cap * 2;
    }
    if (cap == m->cap && m->used == m->len) return;
    // Both arrays are allocated and cleared by hand rather than with a
    // calloc-style interface: every state byte must start EMPTY, and keeping one
    // allocation primitive makes the allocator interface a single
    // implementation instead of two.
    ae_map_slot *slots = ae_alloc((size_t)cap * sizeof(ae_map_slot), line, column);
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
// Inserts or overwrites. The key is copied; the caller keeps its own string.
static void ae_map_put(ae_map *m, ae_strview key, int64_t value, size_t line, size_t column) {
    ae_map_grow(m, line, column);
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
    if (reuse >= 0) at = reuse;
    else m->used = m->used + 1;
    m->slots[at].key = ae_str_from_len(key.ptr, key.len, line, column);
    m->slots[at].value = value;
    m->states[at] = AE_MAP_FULL;
    m->len = m->len + 1;
}
// `map_inc(m, key, delta)`: the canonical counting operation. One hash
// locates the slot, the delta is added in place, and the new value comes
// back; a missing key starts from zero, which is what every histogram wants
// (Go's `m[k] += d`, Rust's `entry(k).or_insert(0) += d`). Unlike
// ae_map_get there is no error value: absence is part of the semantics.
// An existing key is never copied; only a first sighting allocates.
static int64_t ae_map_inc(ae_map *m, ae_strview key, int64_t delta, size_t line, size_t column) {
    ae_map_grow(m, line, column);
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
    if (reuse >= 0) at = reuse;
    else m->used = m->used + 1;
    m->slots[at].key = ae_str_from_len(key.ptr, key.len, line, column);
    m->slots[at].value = delta;
    m->states[at] = AE_MAP_FULL;
    m->len = m->len + 1;
    return delta;
}
static bool ae_map_lookup(const ae_map *m, ae_strview key, int64_t *out) {
    const int64_t at = ae_map_find(m, key);
    if (at < 0) return false;
    if (out) *out = m->slots[at].value;
    return true;
}
static bool ae_map_has(const ae_map *m, ae_strview key, size_t line, size_t column) {
    (void)line; (void)column;
    return ae_map_find(m, key) >= 0;
}
static bool ae_map_remove(ae_map *m, ae_strview key, size_t line, size_t column) {
    (void)line; (void)column;
    const int64_t at = ae_map_find(m, key);
    if (at < 0) return false;
    ae_str_release(&m->slots[at].key);
    m->slots[at].value = 0;
    m->states[at] = AE_MAP_DELETED;
    m->len = m->len - 1;
    return true;
}
static int64_t ae_map_len(const ae_map *m, size_t line, size_t column) {
    (void)line; (void)column;
    return m->len;
}
static void ae_map_release(ae_map *m) {
    for (int64_t i = 0; i < m->cap; i++) {
        if (m->states[i] == AE_MAP_FULL) ae_str_release(&m->slots[i].key);
    }
    ae_free(m->slots);
    ae_free(m->states);
    m->slots = NULL;
    m->states = NULL;
    m->cap = 0;
    m->len = 0;
    m->used = 0;
}
static void ae_map_forfeit(ae_map *m) {
    m->slots = NULL;
    m->states = NULL;
    m->cap = 0;
    m->len = 0;
    m->used = 0;
}

// unit: ae_thread_entry_fn
// ---- Threads (ADR-0214) ------------------------------------------------------
// One spelling per primitive: spawn runs a void fn(void *ctx) on an operating
// system thread and returns an opaque handle; join waits for it to finish and
// releases the handle. Return values ride a heap slot the language layer
// packages into ctx, so this layer stays type-free. The unit is pruned with
// the rest: a program that never spawns carries none of it (charter rule 8).
// Handles are heap blocks the caller must join exactly once; a leaked handle
// shows up in a leak check as a retained block, and joining twice is a
// caller-visible contract violation safe code cannot spell because the
// language surface never hands the handle out twice.
// Platform-neutral shapes: the entry signature and the job block that pairs
// it with its context. A typed member keeps the function pointer out of an
// object pointer, which ISO C forbids converting.
typedef void (*ae_thread_entry_fn)(void *ctx);
// One thread's failure record, written by that thread's own ae_fail and
// read by the joiner after the join completes (ADR-0217).
typedef struct {
    int failed;
    const char *message;
    size_t line;
    size_t column;
} ae_thread_failure;
typedef struct { ae_thread_entry_fn fn; void *ctx; ae_thread_failure *failure; } ae_thread_job;
// Set for the duration of a spawned body; NULL on the main thread, where a
// failure has no joiner to report to and stays immediately fatal.
static _Thread_local ae_thread_failure *ae_thread_current_failure = NULL;
// The most recent failure, so a handle that is never waited still surfaces at
// process exit instead of being swallowed.
static ae_thread_failure ae_thread_latched = { 0, NULL, 0, 0 };
static int ae_thread_hook_installed = 0;
#ifdef _WIN32
// Declared with the real spellings, not void: the argument-conversion part
// of this file includes <windows.h> on Windows, and a `void *` first
// parameter would conflict with processthreadsapi.h's LPSECURITY_ATTRIBUTES
// when that header arrives. The struct tag matches the header's own, so the
// two declarations merge; this unit sits after that include for exactly this
// reason, and a program that prunes the header declares them alone.
// The tag completes when the argument-conversion part pulls <windows.h>; the
// forward declaration keeps the standalone spelling warning-free.
struct _SECURITY_ATTRIBUTES;
extern void *CreateThread(struct _SECURITY_ATTRIBUTES *attributes, size_t stack, unsigned long (*start)(void *), void *parameter, unsigned long flags, unsigned long *id); /* win32-declared */
extern unsigned long WaitForSingleObject(void *handle, unsigned long milliseconds); /* win32-declared */
extern int CloseHandle(void *handle); /* win32-declared */
extern void ExitThread(unsigned long exit_code); /* win32-declared */
typedef struct { void *native; ae_thread_failure failure; } ae_thread_handle;
static unsigned long ae_thread_body(void *raw) {
    ae_thread_job *job = raw;
    ae_thread_current_failure = job->failure;
    job->fn(job->ctx);
    ae_thread_current_failure = NULL;
    ae_free(raw);
    return 0;
}
#else
#include <threads.h>
typedef struct { thrd_t native; ae_thread_failure failure; } ae_thread_handle;
static int ae_thread_body(void *raw) {
    ae_thread_job *job = raw;
    ae_thread_current_failure = job->failure;
    job->fn(job->ctx);
    ae_thread_current_failure = NULL;
    ae_free(raw);
    return 0;
}
#endif
// Report a recorded failure the way a main-thread failure reports itself, then
// end the process: the first observation point decides (a wait or process
// exit), and the exit status and stderr line match an immediate abort.
static _Noreturn void ae_thread_report_failure(const ae_thread_failure *failure) {
    fprintf(stderr, "ae:%zu:%zu: %s\n", failure->line, failure->column, failure->message);
    fflush(stdout);
    _Exit(1);
}
// The sink ae_fail calls once threads exist: inside a thread it records the
// failure on that thread's handle and ends only the thread; anywhere else it
// reports and ends the process.
static _Noreturn void ae_thread_fail(const char *message, size_t line, size_t column) { /* win32-declared */
    if (ae_thread_current_failure) {
        ae_thread_current_failure->failed = 1;
        ae_thread_current_failure->message = message;
        ae_thread_current_failure->line = line;
        ae_thread_current_failure->column = column;
        ae_thread_latched = *ae_thread_current_failure;
#ifdef _WIN32
        ExitThread(1);
#else
        thrd_exit(1);
#endif
    }
    ae_thread_failure failure = { 1, message, line, column };
    ae_thread_report_failure(&failure);
}
// A failure nobody waited for is still reported: the process ends nonzero at
// exit rather than pretending the program succeeded.
static void ae_thread_check_latch(void) {
    if (ae_thread_latched.failed) ae_thread_report_failure(&ae_thread_latched);
}
static void *ae_thread_spawn(ae_thread_entry_fn entry, void *ctx) { /* win32-declared */
    // Two slots: the entry function and its context. The block is owned by
    // the thread body, which frees it after the call; the handle is a
    // separate allocation so a leaked join stays one leak, not two.
    // The handle carries the thread's failure record, so the joiner can report
    // it once the join completed.
    ae_thread_handle *handle = ae_alloc_try(sizeof(ae_thread_handle));
    if (!handle) return NULL;
    handle->failure.failed = 0;
    handle->failure.message = NULL;
    handle->failure.line = 0;
    handle->failure.column = 0;
    ae_thread_job *job = ae_alloc_try(sizeof(ae_thread_job));
    if (!job) { ae_free(handle); return NULL; }
    job->fn = entry;
    job->ctx = ctx;
    job->failure = &handle->failure;
    if (!ae_thread_hook_installed) {
        ae_fail_sink = ae_thread_fail;
        atexit(ae_thread_check_latch);
        ae_thread_hook_installed = 1;
    }
#ifdef _WIN32
    handle->native = CreateThread(NULL, 0, ae_thread_body, job, 0, NULL);
    if (!handle->native) { ae_free(job); ae_free(handle); return NULL; }
#else
    if (thrd_create(&handle->native, ae_thread_body, job) != thrd_success) {
        ae_free(job);
        ae_free(handle);
        return NULL;
    }
#endif
    return handle;
}
static bool ae_thread_join(void *opaque) { /* win32-declared */
    ae_thread_handle *handle = opaque;
    if (!handle) return false;
#ifdef _WIN32
    if (WaitForSingleObject(handle->native, 0xFFFFFFFFu) != 0) return false;
    CloseHandle(handle->native);
#else
    if (thrd_join(handle->native, NULL) != thrd_success) return false;
#endif
    // The join is complete, so any sibling joined before this point already ran
    // to completion; this thread's failure now ends the process.
    if (handle->failure.failed) {
        ae_thread_failure failure = handle->failure;
        ae_thread_latched.failed = 0;
        ae_thread_report_failure(&failure);
    }
    ae_free(handle);
    return true;
}
// unit: -

// unit: ae_atomic
// ---- Atomics (ADR-0214, spec/concurrency.md §8) ------------------------------
// One process-lifetime cell behind a copy-semantics handle: copying the
// struct shares the cell, nothing frees it, and every operation is C11
// seq_cst. The store is the release half of the handoff a load in another
// thread observes after join provides the happens-before edge.
typedef struct { _Atomic int64_t *cell; } ae_atomic;
static ae_atomic ae_cell_new(int64_t seed) {
    // The cell is process-lifetime, so the seed's allocation carries the
    // main-call position (0, 0) — the operation itself cannot fail to
    // report anything else.
    _Atomic int64_t *cell = ae_alloc(sizeof(_Atomic int64_t), 0, 0);
    atomic_store_explicit(cell, seed, memory_order_seq_cst);
    return (ae_atomic){ cell };
}
static int64_t ae_cell_load(ae_atomic a) { return atomic_load_explicit(a.cell, memory_order_seq_cst); }
static void ae_cell_store(ae_atomic a, int64_t value) { atomic_store_explicit(a.cell, value, memory_order_seq_cst); }
static int64_t ae_cell_add(ae_atomic a, int64_t value) { return atomic_fetch_add_explicit(a.cell, value, memory_order_seq_cst); }
static bool ae_cell_cas(ae_atomic a, int64_t expected, int64_t replacement) {
    int64_t observed = expected;
    return atomic_compare_exchange_strong_explicit(a.cell, &observed, replacement, memory_order_seq_cst, memory_order_seq_cst);
}
// unit: -

// unit: ae_shared
// ---- Shared boxes (ADR-0216, spec/concurrency.md §9) -------------------------
// One process-lifetime box behind a copy-semantics handle: copying the
// struct shares the box, nothing frees it. The box holds one value of the
// element type T; shared_new copies the initial value in, and shared_get
// reads a copy out.
typedef struct { void *box; } ae_shared;
static ae_shared ae_shared_alloc(size_t size) {
    return (ae_shared){ ae_alloc(size, 0, 0) };
}
// unit: -

// unit: ae_mutex
// ---- Mutexes and guards (ADR-0216, spec/concurrency.md §9) -------------------
// One process-lifetime OS mutex behind a copy-semantics handle. mutex_lock
// returns an owned guard whose scope-exit release calls ae_mutex_unlock.
#ifdef _WIN32
struct _SECURITY_ATTRIBUTES;
extern void *CreateMutexA(struct _SECURITY_ATTRIBUTES *attributes, int initial_owner, const char *name); /* win32-declared */
extern unsigned long WaitForSingleObject(void *handle, unsigned long milliseconds); /* win32-declared */
extern int ReleaseMutex(void *handle); /* win32-declared */
typedef struct { void *lock; } ae_mutex;
typedef struct { void *lock; } ae_guard;
static ae_mutex ae_mutex_new(void) { /* win32-declared */
    void *h = CreateMutexA(NULL, 0, NULL);
    if (!h) { fprintf(stderr, "ae: mutex_new failed\n"); exit(1); }
    return (ae_mutex){ h };
}
static ae_guard ae_mutex_lock(ae_mutex m) { /* win32-declared */
    WaitForSingleObject(m.lock, 0xFFFFFFFFu);
    return (ae_guard){ m.lock };
}
static void ae_mutex_unlock(ae_guard *g) { /* win32-declared */
    if (g->lock != NULL) {
        ReleaseMutex(g->lock);
        g->lock = NULL;
    }
}
#else
#include <pthread.h>
typedef struct { pthread_mutex_t *lock; } ae_mutex;
typedef struct { pthread_mutex_t *lock; } ae_guard;
static ae_mutex ae_mutex_new(void) {
    pthread_mutex_t *m = ae_alloc(sizeof(pthread_mutex_t), 0, 0);
    if (pthread_mutex_init(m, NULL) != 0) { fprintf(stderr, "ae: mutex_new failed\n"); exit(1); }
    return (ae_mutex){ m };
}
static ae_guard ae_mutex_lock(ae_mutex m) {
    pthread_mutex_lock(m.lock);
    return (ae_guard){ m.lock };
}
static void ae_mutex_unlock(ae_guard *g) {
    if (g->lock != NULL) {
        pthread_mutex_unlock(g->lock);
        g->lock = NULL;
    }
}
#endif
// unit: -

// unit: ae_chan
// ---- Channels (M4.3.01–M4.3.04, spec/concurrency.md §10) ---------------------
// Copy-semantics handle to a process-lifetime bounded or rendezvous (cap == 0)
// i64 channel. Supports blocking send/recv, draining after close, and waking
// blocked senders with false on close.
#ifdef _WIN32
struct _RTL_SRWLOCK;
struct _RTL_CONDITION_VARIABLE;
extern void InitializeSRWLock(struct _RTL_SRWLOCK *lock); /* win32-declared */
extern void AcquireSRWLockExclusive(struct _RTL_SRWLOCK *lock); /* win32-declared */
extern void ReleaseSRWLockExclusive(struct _RTL_SRWLOCK *lock); /* win32-declared */
extern void InitializeConditionVariable(struct _RTL_CONDITION_VARIABLE *cv); /* win32-declared */
extern int SleepConditionVariableSRW(struct _RTL_CONDITION_VARIABLE *cv, struct _RTL_SRWLOCK *lock, unsigned long ms, unsigned long flags); /* win32-declared */
extern void WakeConditionVariable(struct _RTL_CONDITION_VARIABLE *cv); /* win32-declared */
extern void WakeAllConditionVariable(struct _RTL_CONDITION_VARIABLE *cv); /* win32-declared */
extern void Sleep(unsigned long ms); /* win32-declared */
#else
#include <pthread.h>
#include <threads.h>
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
static ae_chan ae_chan_new(int64_t cap) { /* win32-declared */
    if (cap < 0) { fprintf(stderr, "ae: chan_new capacity must be non-negative\n"); exit(1); }
    ae_chan_state *s = ae_alloc(sizeof(ae_chan_state), 0, 0);
    memset(s, 0, sizeof(ae_chan_state));
    s->cap = cap;
    if (cap > 0) {
        s->buf = ae_alloc((size_t)cap * sizeof(int64_t), 0, 0);
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
    return (ae_chan){ s };
}
static bool ae_chan_send(ae_chan ch, int64_t value) { /* win32-declared */
    ae_chan_state *s = ch.state;
#ifdef _WIN32
    AcquireSRWLockExclusive((struct _RTL_SRWLOCK *)&s->lock_storage);
#else
    pthread_mutex_lock(&s->lock);
#endif
    bool ok = false;
    if (s->cap == 0) {
        while (!s->closed && s->has_slot) {
#ifdef _WIN32
            SleepConditionVariableSRW((struct _RTL_CONDITION_VARIABLE *)&s->not_full_storage, (struct _RTL_SRWLOCK *)&s->lock_storage, 0xFFFFFFFFu, 0);
#else
            pthread_cond_wait(&s->not_full, &s->lock);
#endif
        }
        if (!s->closed) {
            s->slot = value;
            s->has_slot = 1;
#ifdef _WIN32
            WakeConditionVariable((struct _RTL_CONDITION_VARIABLE *)&s->not_empty_storage);
#else
            pthread_cond_signal(&s->not_empty);
#endif
            while (!s->closed && s->has_slot) {
#ifdef _WIN32
                SleepConditionVariableSRW((struct _RTL_CONDITION_VARIABLE *)&s->not_full_storage, (struct _RTL_SRWLOCK *)&s->lock_storage, 0xFFFFFFFFu, 0);
#else
                pthread_cond_wait(&s->not_full, &s->lock);
#endif
            }
            if (!s->has_slot) {
                ok = true;
            } else {
                s->has_slot = 0;
                ok = false;
            }
        }
    } else {
        while (!s->closed && s->count == s->cap) {
#ifdef _WIN32
            SleepConditionVariableSRW((struct _RTL_CONDITION_VARIABLE *)&s->not_full_storage, (struct _RTL_SRWLOCK *)&s->lock_storage, 0xFFFFFFFFu, 0);
#else
            pthread_cond_wait(&s->not_full, &s->lock);
#endif
        }
        if (!s->closed) {
            int64_t tail = (s->head + s->count) % s->cap;
            s->buf[tail] = value;
            s->count += 1;
            ok = true;
#ifdef _WIN32
            WakeConditionVariable((struct _RTL_CONDITION_VARIABLE *)&s->not_empty_storage);
#else
            pthread_cond_signal(&s->not_empty);
#endif
        }
    }
#ifdef _WIN32
    ReleaseSRWLockExclusive((struct _RTL_SRWLOCK *)&s->lock_storage);
#else
    pthread_mutex_unlock(&s->lock);
#endif
    return ok;
}
static int64_t ae_chan_recv(ae_chan ch) { /* win32-declared */
    ae_chan_state *s = ch.state;
#ifdef _WIN32
    AcquireSRWLockExclusive((struct _RTL_SRWLOCK *)&s->lock_storage);
#else
    pthread_mutex_lock(&s->lock);
#endif
    int64_t value = 0;
    if (s->cap == 0) {
        while (!s->closed && !s->has_slot) {
#ifdef _WIN32
            SleepConditionVariableSRW((struct _RTL_CONDITION_VARIABLE *)&s->not_empty_storage, (struct _RTL_SRWLOCK *)&s->lock_storage, 0xFFFFFFFFu, 0);
#else
            pthread_cond_wait(&s->not_empty, &s->lock);
#endif
        }
        if (s->has_slot) {
            value = s->slot;
            s->has_slot = 0;
#ifdef _WIN32
            WakeAllConditionVariable((struct _RTL_CONDITION_VARIABLE *)&s->not_full_storage);
#else
            pthread_cond_broadcast(&s->not_full);
#endif
        }
    } else {
        while (!s->closed && s->count == 0) {
#ifdef _WIN32
            SleepConditionVariableSRW((struct _RTL_CONDITION_VARIABLE *)&s->not_empty_storage, (struct _RTL_SRWLOCK *)&s->lock_storage, 0xFFFFFFFFu, 0);
#else
            pthread_cond_wait(&s->not_empty, &s->lock);
#endif
        }
        if (s->count > 0) {
            value = s->buf[s->head];
            s->head = (s->head + 1) % s->cap;
            s->count -= 1;
#ifdef _WIN32
            WakeConditionVariable((struct _RTL_CONDITION_VARIABLE *)&s->not_full_storage);
#else
            pthread_cond_signal(&s->not_full);
#endif
        }
    }
#ifdef _WIN32
    ReleaseSRWLockExclusive((struct _RTL_SRWLOCK *)&s->lock_storage);
#else
    pthread_mutex_unlock(&s->lock);
#endif
    return value;
}
static void ae_chan_close(ae_chan ch) { /* win32-declared */
    ae_chan_state *s = ch.state;
#ifdef _WIN32
    AcquireSRWLockExclusive((struct _RTL_SRWLOCK *)&s->lock_storage);
    s->closed = 1;
    WakeAllConditionVariable((struct _RTL_CONDITION_VARIABLE *)&s->not_full_storage);
    WakeAllConditionVariable((struct _RTL_CONDITION_VARIABLE *)&s->not_empty_storage);
    ReleaseSRWLockExclusive((struct _RTL_SRWLOCK *)&s->lock_storage);
#else
    pthread_mutex_lock(&s->lock);
    s->closed = 1;
    pthread_cond_broadcast(&s->not_full);
    pthread_cond_broadcast(&s->not_empty);
    pthread_mutex_unlock(&s->lock);
#endif
}
static bool ae_chan_try_send(ae_chan ch, int64_t value) { /* win32-declared */
    ae_chan_state *s = ch.state;
#ifdef _WIN32
    AcquireSRWLockExclusive((struct _RTL_SRWLOCK *)&s->lock_storage);
#else
    pthread_mutex_lock(&s->lock);
#endif
    bool ok = false;
    if (!s->closed && s->cap > 0 && s->count < s->cap) {
        int64_t tail = (s->head + s->count) % s->cap;
        s->buf[tail] = value;
        s->count += 1;
        ok = true;
#ifdef _WIN32
        WakeConditionVariable((struct _RTL_CONDITION_VARIABLE *)&s->not_empty_storage);
#else
        pthread_cond_signal(&s->not_empty);
#endif
    }
#ifdef _WIN32
    ReleaseSRWLockExclusive((struct _RTL_SRWLOCK *)&s->lock_storage);
#else
    pthread_mutex_unlock(&s->lock);
#endif
    return ok;
}
static bool ae_chan_is_ready(ae_chan ch) { /* win32-declared */
    ae_chan_state *s = ch.state;
#ifdef _WIN32
    AcquireSRWLockExclusive((struct _RTL_SRWLOCK *)&s->lock_storage);
#else
    pthread_mutex_lock(&s->lock);
#endif
    bool ready = s->closed || (s->cap == 0 ? s->has_slot != 0 : s->count > 0);
#ifdef _WIN32
    ReleaseSRWLockExclusive((struct _RTL_SRWLOCK *)&s->lock_storage);
#else
    pthread_mutex_unlock(&s->lock);
#endif
    return ready;
}
static bool ae_chan_is_closed(ae_chan ch) { /* win32-declared */
    ae_chan_state *s = ch.state;
#ifdef _WIN32
    AcquireSRWLockExclusive((struct _RTL_SRWLOCK *)&s->lock_storage);
#else
    pthread_mutex_lock(&s->lock);
#endif
    bool closed = s->closed != 0;
#ifdef _WIN32
    ReleaseSRWLockExclusive((struct _RTL_SRWLOCK *)&s->lock_storage);
#else
    pthread_mutex_unlock(&s->lock);
#endif
    return closed;
}
// A closed arm wins over an arm that merely holds a value (M4.3.05): a
// cancellation channel only ever closes, so the cancelled receiver returns
// without consuming and the pending value stays for the next receiver.
static int64_t ae_chan_select(ae_chan ch1, ae_chan ch2) { /* win32-declared */
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
// unit: -

// unit: ae_exec
// ---- Tasks (M4.2, ADR-0218) -------------------------------------------------
#ifdef _WIN32
extern void *ConvertThreadToFiber(void *param); /* win32-declared */
extern void *CreateFiber(size_t stack, void (*start)(void *), void *param); /* win32-declared */
extern void SwitchToFiber(void *fiber); /* win32-declared */
extern void DeleteFiber(void *fiber); /* win32-declared */
extern int ConvertFiberToThread(void); /* win32-declared */
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
static void ae_task_trampoline(void *param) { /* win32-declared */
#ifdef _WIN32
    ae_task_node *node = (ae_task_node *)param;
    node->entry(node->ctx);
    node->done = 1;
    SwitchToFiber(ae_task_sched_fiber);
#else
    (void)param;
#endif
}
static int ae_task_trampoline_posix(void *param) {
#ifndef _WIN32
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
#else
    (void)param;
#endif
    return 0;
}
static void ae_task_yield(void) { /* win32-declared */
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
static ae_exec ae_exec_new(void) {
    ae_exec_state *s = (ae_exec_state *)calloc(1, sizeof(ae_exec_state));
    if (!s) ae_fail("out of memory", 0, 0);
    return (ae_exec){ s };
}
static void ae_task_spawn(ae_exec ex, ae_task_entry_fn entry, void *ctx) {
    ae_exec_state *s = (ae_exec_state *)ex.state;
    ae_task_node *node = (ae_task_node *)calloc(1, sizeof(ae_task_node));
    if (!node) ae_fail("out of memory", 0, 0);
    node->entry = entry;
    node->ctx = ctx;
#ifndef _WIN32
    mtx_init(&node->mu, mtx_plain);
    cnd_init(&node->cv);
#endif
    if (s->tail) { s->tail->next = node; s->tail = node; }
    else { s->head = s->tail = node; }
}
static int64_t ae_exec_run(ae_exec ex) { /* win32-declared */
    ae_exec_state *s = (ae_exec_state *)ex.state;
#ifdef _WIN32
    void *prev_sched = ae_task_sched_fiber;
    int converted = 0;
    if (!ae_task_sched_fiber) {
        void *f = ConvertThreadToFiber(NULL);
        if (f) { ae_task_sched_fiber = f; converted = 1; }
    }
    int64_t completed = 0;
    while (s->head) {
        ae_task_node *node = s->head;
        s->head = node->next;
        if (!s->head) s->tail = NULL;
        node->next = NULL;
        if (!node->fiber) {
            node->fiber = CreateFiber(65536, ae_task_trampoline, node);
            if (!node->fiber) ae_fail("out of memory", 0, 0);
        }
        ae_task_in_task = 1;
        SwitchToFiber(node->fiber);
        ae_task_in_task = 0;
        if (node->done) {
            DeleteFiber(node->fiber);
            ae_free(node->ctx);
            free(node);
            completed += 1;
        } else {
            if (s->tail) { s->tail->next = node; s->tail = node; }
            else { s->head = s->tail = node; }
        }
    }
    if (converted) {
        ConvertFiberToThread();
        ae_task_sched_fiber = prev_sched;
    }
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
                ae_fail("out of memory", 0, 0);
            }
        }
        mtx_lock(&node->mu);
        node->turn = 1;
        cnd_signal(&node->cv);
        while (node->turn && !node->done) cnd_wait(&node->cv, &node->mu);
        int is_done = node->done;
        mtx_unlock(&node->mu);
        if (is_done) {
            thrd_join(node->thrd, NULL);
            cnd_destroy(&node->cv);
            mtx_destroy(&node->mu);
            ae_free(node->ctx);
            free(node);
            completed += 1;
        } else {
            if (s->tail) { s->tail->next = node; s->tail = node; }
            else { s->head = s->tail = node; }
        }
    }
    return completed;
#endif
}
// unit: -

// unit: ae_task_scope
// ---- Structured Task Scope (M4.2.04, ADR-0220) ------------------------------
typedef struct { void *state; } ae_task_scope;
static ae_task_scope ae_scope_new(void) {
    ae_exec ex = ae_exec_new();
    return (ae_task_scope){ ex.state };
}
static void ae_scope_spawn(ae_task_scope sc, ae_task_entry_fn entry, void *ctx) {
    ae_exec ex = { sc.state };
    ae_task_spawn(ex, entry, ctx);
}
static void ae_scope_release(ae_task_scope *sc) {
    if (!sc || !sc->state) return;
    ae_exec ex = { sc->state };
    (void)ae_exec_run(ex);
    free(sc->state);
    sc->state = NULL;
}
// unit: -

// unit: ae_pool
// ---- Multiworker Work-Stealing Pool (M4.2.07, ADR-0220) ---------------------
typedef struct ae_pool_task {
    void (*entry)(void *);
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
static void ae_pool_lock(atomic_flag *f) {
    while (atomic_flag_test_and_set_explicit(f, memory_order_acquire)) {}
}
static void ae_pool_unlock(atomic_flag *f) {
    atomic_flag_clear_explicit(f, memory_order_release);
}
static ae_pool_task *ae_pool_pop_one(ae_pool_queue *q) {
    ae_pool_lock(&q->lock);
    ae_pool_task *t = q->head;
    if (t) {
        q->head = t->next;
        if (!q->head) q->tail = NULL;
        t->next = NULL;
    }
    ae_pool_unlock(&q->lock);
    return t;
}
static void ae_pool_worker_run(void *param) {
    ae_pool_worker_arg *arg = (ae_pool_worker_arg *)param;
    ae_pool_state *p = arg->pool;
    int64_t me = arg->id;
    for (;;) {
        ae_pool_task *t = ae_pool_pop_one(&p->queues[me]);
        if (!t) {
            for (int64_t off = 1; off < p->workers; off++) {
                int64_t victim = (me + off) % p->workers;
                t = ae_pool_pop_one(&p->queues[victim]);
                if (t) break;
            }
        }
        if (!t) break;
        t->entry(t->ctx);
        ae_free(t->ctx);
        free(t);
        atomic_fetch_add(&p->completed, 1);
    }
}
static ae_pool ae_pool_new(int64_t workers) {
    if (workers < 1) workers = 1;
    if (workers > 16) workers = 16;
    ae_pool_state *p = (ae_pool_state *)calloc(1, sizeof(ae_pool_state));
    if (!p) ae_fail("out of memory", 0, 0);
    p->workers = workers;
    atomic_init(&p->completed, 0);
    return (ae_pool){ p };
}
static void ae_pool_spawn(ae_pool pool, void (*entry)(void *), void *ctx) {
    ae_pool_state *p = pool.state;
    ae_pool_task *t = (ae_pool_task *)calloc(1, sizeof(ae_pool_task));
    if (!t) ae_fail("out of memory", 0, 0);
    t->entry = entry;
    t->ctx = ctx;
    int64_t qid = p->next_q % p->workers;
    p->next_q += 1;
    ae_pool_queue *q = &p->queues[qid];
    ae_pool_lock(&q->lock);
    if (q->tail) { q->tail->next = t; q->tail = t; }
    else { q->head = q->tail = t; }
    ae_pool_unlock(&q->lock);
}
static int64_t ae_pool_join(ae_pool pool) {
    ae_pool_state *p = pool.state;
    atomic_store(&p->completed, 0);
    void *handles[16];
    ae_pool_worker_arg args[16];
    for (int64_t i = 0; i < p->workers; i++) {
        args[i].pool = p;
        args[i].id = i;
        handles[i] = ae_thread_spawn(ae_pool_worker_run, &args[i]);
        if (!handles[i]) ae_fail("out of memory", 0, 0);
    }
    for (int64_t i = 0; i < p->workers; i++) {
        (void)ae_thread_join(handles[i]);
    }
    return atomic_load(&p->completed);
}
// unit: -

// unit: -
