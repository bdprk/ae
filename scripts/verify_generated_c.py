"""Compile ae-generated C independently and exercise success and failure paths.

--sanitize uses AddressSanitizer where the toolchain provides it.
--leak adds an allocator wrapper (scripts/leakcheck.c) linked with --wrap, which
works on MinGW-W64 where libasan is unavailable: every generated program must
end with exactly the allocation count of a trivial program, i.e. only the C
runtime's own retained buffers.
"""
import argparse
import json
import os
import re
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LEAKCHECK = ROOT / "scripts" / "leakcheck.c"
TRIVIAL = "fn main() { print(1); }"

def find_executable() -> Path:
    """The binary lives in cargo's configured target directory.

    `.cargo/config.toml` points `[build] target-dir` outside the checkout, so
    the legacy in-tree `target/debug` path only exists on checkouts that
    predate it. `cargo metadata` reports the resolved directory, which keeps
    the script honest on both layouts.
    """
    name = "ae.exe" if os.name == "nt" else "ae"
    legacy = ROOT / "target" / "debug" / name
    if legacy.exists():
        return legacy
    try:
        metadata = subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            capture_output=True, check=True, cwd=ROOT,
        )
        target_directory = json.loads(metadata.stdout)["target_directory"]
        return Path(target_directory) / "debug" / name
    except (OSError, ValueError, KeyError):
        return legacy

def run():
    parser = argparse.ArgumentParser()
    parser.add_argument("--sanitize", action="store_true")
    parser.add_argument("--leak", action="store_true")
    options = parser.parse_args()
    executable = find_executable()
    cc = os.environ.get("AE_CC", "gcc")
    cases = [
        ("vectors.ae", b"5\n55\n154\n0", True),
        ("arrays.ae", b"60", True),
        ("borrow.ae", b"60", True),
        ("functions.ae", b"720", True),
        ("methods.ae", b"(4, 0)\n3\n8", True),
        ("struct_borrow.ae", b"60\ntrue\nfalse\n34\nplot: 60 sq units\n200\nfield: 200 sq units", True),
        ("struct_mut_borrow.ae", b"1\n6\n6\n3\n12", True),
        ("enum_methods.ae", b"12\ncircle r=2\n20\nrect 4x5\n1\n3", True),
        ("generic_impl.ae", b"30\n(30)\n2.5\n(2.5)", True),
        ("closures.ae", b"42\n101\n102\nhello closures\nship w=42\n43\nmemo!\n26", True),
        ("function_types.ae", b"42\n7\n169", True),
        ("control-flow.ae", b"45", True),
        ("strings.ae", b"hello\nhello!\nhi there\n8\n\xe4\xb8\xad\xe6\x96\x87 \xf0\x9f\x98\x80", True),
        ("enums.ae", b"27\n20\n0\ncircle r=3\nrect 4x5\nempty\ntext:hi\nnumber:7\nflag:true\n12\nnumber:9", True),
        ("concat.ae", b"field/tail\n[bcd]\nbcd\nfield\na!c\nb", True),
        # Runs inside the temporary directory: the example creates and then
        # removes its own sandbox, so the working tree stays clean.
        ("file_tidy.ae", b"a_small.txt 5 small\nb_medium.txt 60 small\nc_large.txt 300 large\norganized 3 into small/large\ncleared 6 files\ndone", True),
        # `?` propagation: the error path must release everything the arm before
        # it bound, which the allocation baseline checks.
        ("copy_notes.ae", b"copied text notes\n11\n2\ntrue\ncleared the sandbox\ndone", True),
        # Hash map: keys() returns a sorted copy, so the report order is stable
        # across platforms; the map is released when the program exits.
        ("word_count.ae", b"distinct 8\nbrown 1\ndog 1\nfox 2\njumps 1\nlazy 1\nover 1\nquick 1\nthe 3", True),
        # map_inc: the counting compound inserts on first sight and updates in
        # place afterwards; sorted keys() keeps the report stable.
        ("map_count.ae", b"a=3\nb=2\nc=3\nd=1", True),
        # A thousand missing keys: the fixed Err text is shared read-only data,
        # so none of those results may retain or double-release an allocation.
        ("map_missing.ae", b"key not found\n7\n0\ndone", True),
        # Traits: dispatch through a trait bound equals the hand-written call,
        # and owned structs move into the impl method like any parameter.
        ("traits.ae", b"area=49\narea=12\n12", True),
        # Fused split/lines arena: popping, overwriting and pushing past the
        # exact capacity must not double-free arena bytes or leak replacements.
        ("split_arena.ae", b"4\nALPHA\nALPHA|beta|gamma|tail\n3\nthree\n1\ngrown", True),
        # A view into a struct's string field: the brackets are byte offsets and
        # the base stays readable because views are read-only.
        ("field_view.ae", b"Grace\nHopper\n12", True),
        # Lazy token cursor: same boundaries as split, no per-token allocation.
        ("tokens.ae", b"4\n16\n5\n14\n14\n3\none,two,three\n1", True),
        # Character-safe truncation: every cut lands on a UTF-8 boundary, so the
        # ellipsis is never preceded by half a character.
        # Filesystem + clock + ordering in one program: creates its own fixture
        # so the listing and sizes are known, and every step matches on a result.
        ("dir_report.ae", (
            "ready\n"
            "a.txt 6 bytes\ntrue\n"
            "b.txt 4 bytes\ntrue\n"
            "c.txt 8 bytes\ntrue\n"
            "largest 8\n"
            "entries 3"
        ).encode("utf-8"), True),
        # JSON path queries: a member, a nested member, an array element, an
        # escaped \u sequence, a null, a missing name and a type mismatch all
        # come back as values, and the document is read from disk.
        # Generic structs: each argument list is a distinct layout, and the
        # nested string fields must be released exactly once.
        ("pairs.ae", b"id=7\n1\none\n2\nin\nner", True),
        # Generic functions: one C function per argument list, and the owned
        # parameter that is not returned must be released exactly once.
        ("generic_fn.ae", b"41\nada\nleft\ntrue\nwrapped\nk\n2", True),
        # Constraints: one body is emitted for several argument lists, so each
        # instantiation must release its own string parameters exactly once.
        ("constraints.ae", b"5\nae\n9\ntrue\n7\nfalse", True),
        # ADR-40: declarations audited at the front end still emit the same
        # per-instantiation bodies; mixed i64/f64/string/bool arguments must
        # release owned strings exactly once.
        ("generic_constraints.ae", b"5\nae\n4.0\n10\n3.5\nfalse\ntrue", True),
        # Option propagation: a None returns early, a discarded Some payload
        # (string and vec) is released exactly once on the successful path.
        ("option_propagate.ae", b"found!\nnone\nalways\nnone\n1\nnone\n3\nnone", True),
        # Generic borrow parameters: no owned value is moved into the callee,
        # so the caller's strings and vectors must still be intact afterwards.
        ("generic_borrow.ae", b"2\n3\n3\n60\n3\nae\n10\nbeta", True),
        # find returns an IntResult: the Err branch allocates an error string
        # that the enum release must give back exactly once.
        ("find_text.ae", b"4\n9\nsubstring not found\n0\n7", True),
        # f64: one canonical text form on every platform, explicit conversions,
        # and a checked division that reports x/0.0 instead of returning inf.
        ("float_math.ae", b"count 8\nmean 5.0\nvariance 4.0\nshare 0.5\nthird 0.3333333333333333\nexact 5\nexact failed: f64 value is not an exact integer", True),
        # Math builtins (ADR-60): exact libm results print identically on every
        # platform, sqrt(-1) reads back as "nan", and the transcendental checks
        # pass with a tolerance instead of betting on identical ulp.
        ("math_basics.ae", (
            "sqrt(4) 2.0\n"
            "abs(-2.5) 2.5\n"
            "floor(2.7) 2.0\n"
            "ceil(2.1) 3.0\n"
            "round(2.5) 3.0\n"
            "round(-2.5) -3.0\n"
            "pow(2,10) 1024.0\n"
            "exp(0) 1.0\n"
            "log(1) 0.0\n"
            "sin(0) 0.0\n"
            "cos(0) 1.0\n"
            "sqrt(-1) nan\n"
            "sqrt(2) ok\n"
            "sin(1) ok\n"
            "cos(1) ok\n"
            "atan2(1,1) ok\n"
            "log(exp(1)) ok\n"
            "pow(2,0.5) ok\n"
            "min 2.0\n"
            "max 3.0\n"
            "min nan 5.0\n"
            "min -0 -0.0\n"
            "max -0 0.0"
        ).encode("utf-8"), True),
        # vec[f64] (ADR-61): statistics over an owned float vector with the
        # math builtins; the move into `total` happens last and the accumulator
        # grows by push. All printed values are exactly representable, so the
        # expected bytes are identical on both platforms.
        ("float_stats.ae", (
            "count 8\n"
            "mean 5.0\n"
            "variance 4.0\n"
            "stddev 2.0\n"
            "peak 9.0\n"
            "zscore 2.0\n"
            "doubled 8.0\n"
            "doubled last 18.0\n"
            "total 40.0"
        ).encode("utf-8"), True),
        # map[string, f64] (ADR-65): latest-value table + history vector +
        # math builtins in one program. Map reads go through map_get's
        # FloatResult; keys() output is sorted, so the report order is stable.
        ("sensor_stats.ae", (
            "sensors 3\n"
            "temp 21.5\n"
            "mean 21.3\n"
            "min 20.0\n"
            "max 22.5\n"
            "false\n"
            "22.0\n"
            "first humidity\n"
            "count 3"
        ).encode("utf-8"), True),
        ("json_report.ae", (
            "ready\n"
            "wrote 175 bytes\n"
            "name gateway\n"
            "workers 4\n"
            "debug true\n"
            "limits.rps 120\n"
            "routes[1] /api/v1\n"
            "routes: 'routes' is an array, not a number\n"
            "routes[9]: path segment [9] is past the end of the array\n"
            "owner ops\u00e9quipe\n"
            "timeout: no member named 'timeout'\n"
            "name: 'name' is a string, not a number\n"
            "limits { \"rps\": 120, \"burst\": 40 }\n"
            "counted 3 routes\n"
            "true\n"
            "cleaned\n"
            "done"
        ).encode("utf-8"), True),
        # Generic enums: Option[string] and Option[i64] are separate layouts,
        # and a moved-in payload is released with the value.
        ("maybe.ae", b"longest line: three\nlongest line: (none)\nfirst number: 42\nfirst number: (none)", True),
        # Two modules and a private helper: only `pub` items are reachable, and
        # the import graph is resolved relative to the entry file.
        ("modules/main.ae", b"a=3\nb=2\nc=1\n3 entries, 6 words", True),
        ("text_preview.ae", (
            "这是一行很长的中文文本，用于演示按字符而…\n"
            "short line\n"
            "😀 emoji 也按一个字符计算，不会被…\n"
            "lines 3\nchars 68\nbytes 167"
        ).encode("utf-8"), True),
        # P4.1 heap shapes: the allocator bridge, the container wrappers,
        # owned string slots, heap views and the recursive tree — every one
        # must free exactly once under the leak wrapper. rc_shared stays out:
        # its count cell is a process-lifetime atomic by design, which the
        # strict block-count equality reads as a leak (the sanitizer sweep
        # covers it with the established exemption).
        ("tests/run/box_alloc.ae", b"42\n42\n99", True),
        ("tests/run/box_wrapper.ae", b"42\n42", True),
        ("tests/run/box_string.ae", b"written\n0\nfreed\nslot view\n9", True),
        ("tests/run/ptr_string_view.ae", b"heap view\ncompare ok\n0\ndone", True),
        ("tests/run/tree_nodes.ae", b"15\n5\ndone", True),
        # ADR-0245 stage 3: a bound heap view releases nothing; the owned
        # string still drops in place exactly once before the block frees.
        ("tests/run/ptr_string_view_bind.ae", b"bound heap view\n15\n0\ndone", True),
        # ADR-0245 stage 4: an unbound deref chain indexes and measures —
        # the borrow-pointer materialization releases nothing.
        ("tests/run/heap_vec_index.ae", b"11\n33\n3\n0\ndone", True),
    ]
    with tempfile.TemporaryDirectory(prefix="ae-c-verify-") as tmp:
        tmp = Path(tmp)
        bad = tmp / "bounds.ae"
        bad.write_text("fn main() { let a = [1, 2]; print(a[2]); }", encoding="utf-8")
        trivial = tmp / "trivial.ae"
        trivial.write_text(TRIVIAL, encoding="utf-8")
        # Baseline: allocations the C runtime itself keeps at exit.
        baseline = 0
        if options.leak:
            baseline = leak_counts(executable, cc, tmp, trivial, options)[0]
            print(f"BASELINE leak blocks={baseline}")
        for name, expected, ok in cases + [(str(bad), b"array index out of bounds", False)]:
            # A `tests/` prefix resolves against the repo root (fixtures that
            # import the standard library through their in-tree relative
            # paths); every other name stays an example.
            source = Path(name) if name == str(bad) else ROOT / (name if name.startswith("tests/") else Path("examples") / name)
            generated = subprocess.run([executable, "emit-c", source], check=True, capture_output=True).stdout
            cfile = tmp / "test.c"
            binary = tmp / ("test.exe" if os.name == "nt" else "test")
            cfile.write_bytes(generated)
            flags = ["-std=c11", "-pedantic-errors", "-Werror=return-type", "-O1"]
            if options.sanitize:
                flags += ["-fsanitize=address,undefined", "-fno-omit-frame-pointer", "-fno-sanitize-recover=all"]
            extra = []
            if options.leak:
                extra = [str(LEAKCHECK), "-Wl,--wrap=malloc", "-Wl,--wrap=calloc", "-Wl,--wrap=free", "-Wl,--wrap=realloc"]
            if os.name != "nt":
                # Math builtins (ADR-60) call libm directly; MinGW serves the
                # symbols from the C runtime, glibc wants an explicit -lm.
                extra += ["-lm"]
            subprocess.run([cc, *flags, cfile, *extra, "-o", binary], check=True)
            result = subprocess.run([binary], capture_output=True, cwd=tmp)
            if ok:
                stdout = result.stdout.replace(b"\r\n", b"\n").strip()
                assert result.returncode == 0 and stdout == expected, (name, result.returncode, result.stdout, result.stderr)
            else:
                assert result.returncode != 0 and expected in result.stderr, (name, result.returncode, result.stderr)
            assert b"runtime error:" not in result.stderr and b"ERROR: AddressSanitizer" not in result.stderr, (name, result.stderr)
            if options.leak:
                blocks, leaked = parse_leak(result.stderr)
                # The C runtime keeps a couple of buffers alive at exit; a real
                # leak adds a block on top of that baseline.
                assert blocks == baseline, (name, "allocation blocks retained", blocks, baseline)
                print(f"  leak blocks={blocks} bytes={leaked}")
            print(f"PASS {source.name}")

def leak_counts(executable, cc, tmp, source, options):
    generated = subprocess.run([executable, "emit-c", source], check=True, capture_output=True).stdout
    cfile = tmp / "leak-probe.c"
    binary = tmp / ("leak-probe.exe" if os.name == "nt" else "leak-probe")
    cfile.write_bytes(generated)
    libs = [] if os.name == "nt" else ["-lm"]
    subprocess.run([cc, "-std=c11", "-pedantic-errors", "-O1", cfile, str(LEAKCHECK),
                    "-Wl,--wrap=malloc", "-Wl,--wrap=calloc", "-Wl,--wrap=free", "-Wl,--wrap=realloc", *libs, "-o", binary], check=True)
    return parse_leak(subprocess.run([binary], capture_output=True, cwd=tmp).stderr)

def parse_leak(stdout):
    match = re.search(rb"\[leak\] blocks=(-?\d+) bytes=(-?\d+)", stdout)
    assert match, ("missing leak report", stdout)
    return int(match.group(1)), int(match.group(2))

if __name__ == "__main__":
    run()
