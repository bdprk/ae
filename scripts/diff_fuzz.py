#!/usr/bin/env python3
"""Differential fuzzing for the ae compiler (plan M2.10, C-backend slice).

Generates random *well-typed* programs from a grammar of arithmetic,
comparisons, control flow, strings (including one a branch moves into a
by-value call, the mixed drop shape M2.4 keeps unproven), vectors with
their mutation surface (push, indexed store, sort) and string vectors
(push, element store releasing the old element, join), owned struct
vectors with Drop glue (push/overwrite/pop/scope release, drop prints in
the compared output), string vector literals, vector fields and nested
field chains (`box.cells`, `outer.box.cells`), indexed place borrows
(`&v[i]`), typed local bindings (P2.4/P2.5), and `&[T]` slices built at
call sites from vectors, arrays and forwarded parameters, and sub-slice
windows (`&v[0..0]`, `&v[3..3]`, `&arr[2..3]`, string/owned/nested element
windows) (ADR-0236 rungs 1/3), structs, enums,
matches with payload bindings, counted loops with `continue`, and the
borrow surface (M2.5): a Doc struct with a `&mut self` method and a free
`&mut` function, field views born and read inside one statement group,
the L2 field-granularity write across a live view, and the ordered
read-then-release shape left-to-right evaluation keeps legal; compiles each source
with every backend and optimization level — the MIR block-graph backend (the
-O0 default, M2.3) and the structured tree emitter (`AE_CODEGEN=tree`),
both at -O0, plus the optimized build; runs every binary; and asserts that
all runs produce identical output. A runtime trap is agreement too (the
generator can produce overflowing arithmetic; seed 1762): every mode must
exit with the same code, print the same stdout up to the trap, and trap at
the same position with the same message. The parser's own move/borrow/type
rules gate generation (a generated program the compiler rejects is a
generator bug and fails the run), so each seed doubles as a regression test
of compile-or-die determinism.

Usage: python scripts/diff_fuzz.py [--seeds N] [--start SEED] [--jobs N]
                                   [--save-corpus DIR]

Seeds run through a worker pool (--jobs, default min(cpu, 8)); each worker
owns its files and its own content cache, so parallel seeds never share
mutable state. Results print as they complete and the summary repeats the
failed seeds in order. --save-corpus writes every generated program as
DIR/seed_N.ae — the regression corpus for future compiler changes.
"""

import argparse
import json
import os
import re
import time
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


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


EXE = find_executable()


class Gen:
    """A deterministic random program generator."""

    def __init__(self, seed: int):
        self.state = (seed & 0xFFFFFFFF) or 1
        self.lines: list[str] = []
        # Bindings are collision-free: every declaration draws its name from
        # one per-program counter instead of a random suffix, so two
        # statements can never declare the same variable twice.
        self.name_seq = 0

    def rnd(self) -> int:
        self.state ^= (self.state << 13) & 0xFFFFFFFF
        self.state ^= self.state >> 7
        self.state ^= (self.state << 17) & 0xFFFFFFFF
        return self.state & 0xFFFFFFFF

    def pick(self, choices):
        return choices[self.rnd() % len(choices)]

    def fresh(self, prefix: str) -> str:
        self.name_seq += 1
        return f"{prefix}{self.name_seq}"

    def int_expr(self, vars: list[str], depth: int = 0) -> str:
        # Depth 2, not 3: each level wraps division operands in a guard that
        # roughly triples the node count, and the compiler rejects any
        # expression over 128 nodes — a vec literal with two deep elements
        # blew straight through it (cluster seeds 240072/242098/245815/251900,
        # 2026-11-15). Depth 2 keeps every generated shape well inside the
        # limit while the guard-wrapping still exercises the checked paths.
        if depth >= 2 or self.rnd() % 3 == 0:
            if vars and self.rnd() % 3 != 0:
                return self.pick(vars)
            return str(self.rnd() % 1000 - 500)
        op = self.pick(["+", "-", "*", "/", "%"])
        a = self.int_expr(vars, depth + 1)
        b = self.int_expr(vars, depth + 1)
        # Division and modulo wrap their divisor in a provably nonzero guard:
        # `(b * b) + 1` is positive for every integer b, so the program is
        # well-defined at runtime whichever way the operands evaluate.
        if op in ("/", "%"):
            return f"({a} {op} (({b} * {b}) + 1))"
        return f"({a} {op} {b})"

    def bool_expr(self, vars: list[str], depth: int = 0) -> str:
        if depth >= 2:
            return self.pick(["true", "false"])
        kind = self.rnd() % 3
        if kind == 0:
            return f"({self.int_expr(vars, depth + 1)} {self.pick(['<', '<=', '>', '>=', '==', '!='])} {self.int_expr(vars, depth + 1)})"
        if kind == 1:
            return f"({self.bool_expr(vars, depth + 1)} {self.pick(['&&', '||'])} {self.bool_expr(vars, depth + 1)})"
        return f"!{self.bool_expr(vars, depth + 1)}"

    def statement(self, vars: list[str], depth: int, docs: list[str], pointers: list[tuple[str, str, bool]]) -> list[str]:
        """One statement, possibly introducing new variables. Returns lines.

        `docs` and `pointers` carry the borrow and raw-pointer surfaces
        (M2.5, M2.12), threading through the same subtree discipline `vars`
        follows: a binding created inside a branch stays in that branch's
        subtree, so no generated statement references a variable the parser
        would reject as undefined on some path. Pointer targets are scalars,
        which never move, so no generated shape can dangle.
        """
        if depth >= 3:
            return [f"print({self.int_expr(vars)});"]
        kind = self.rnd() % 30
        # With pointers in scope, a third of the draws land on the pointer
        # kinds (19/20): the fresh surface needs the pressure, and the shapes
        # are cheap to generate.
        if pointers and self.rnd() % 3 == 0:
            kind = self.pick([19, 20])
        if 14 <= kind <= 17 and not docs:
            kind = 14
        if kind == 18 and not vars:
            kind = 3
        if kind in (19, 20) and (not vars or not pointers):
            kind = 3
        if kind == 23:
            # ADR-0215 (M4.1.02): a spawned closure whose borrow capture is
            # provably joined — the wait lands on every path before the
            # borrowed variable's region ends. The joined read sees exactly
            # the storage the caller froze; both backends agree.
            name = self.fresh("sj")
            sname = self.fresh("s")
            a = '"j' + str(self.rnd() % 1000) + '"'
            return [
                f"var {sname} = {a};",
                f"let {name} = spawn(|| -> i64 {{ return len({sname}) * 2; }});",
                f"print(wait({name}));",
                f"print(len({sname}));",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 22:
            # spec/concurrency.md §8 A1/A2: one shared cell, two threads each
            # adding 10 five times. Each thread sums its own atomic_add old
            # values locally; the two sums together are the pre-sequence
            # {0,10,...,90} = 450 regardless of interleaving, and the final
            # load is 100 — both deterministic under any schedule.
            name = self.fresh("at")
            first = self.fresh("t")
            second = self.fresh("t")
            return [
                f"let {name} = cell_new(0);",
                f"let {first} = spawn(|| -> i64 {{",
                f"    var i = 0;",
                f"    var seen = 0;",
                f"    while i < 5 {{ seen = seen + cell_add({name}, 10); i = i + 1; }}",
                f"    return seen;",
                f"}});",
                f"let {second} = spawn(|| -> i64 {{",
                f"    var i = 0;",
                f"    var seen = 0;",
                f"    while i < 5 {{ seen = seen + cell_add({name}, 10); i = i + 1; }}",
                f"    return seen;",
                f"}});",
                f"print(wait({first}) + wait({second}));",
                f"print(cell_load({name}));",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 21:
            # ADR-0214: a spawned closure moves its owned captures into the
            # thread and wait hands the result back. Scalars copy (they stay
            # usable outside), and a string capture is consumed by the body's
            # return — the shape that forces a value capture, since a borrow
            # capture cannot cross. The capture variables are retired after
            # the spawn, so no later statement reads a moved name.
            name = self.fresh("th")
            if self.rnd() % 2 == 0:
                return [
                    f"let {name} = spawn(|| -> i64 {{ return ({self.int_expr(vars)}) + 1; }});",
                    f"print(wait({name}));",
                    *self.statement(vars, depth + 1, docs, pointers),
                ]
            sname = self.fresh("s")
            a = '"f' + str(self.rnd() % 1000) + '"'
            # The body's plain return fails the borrow probe (a view cannot
            # satisfy the owned string return), so the capture classifies as
            # a value and moves — a concat would stay a legal borrow and the
            # spawn would reject it.
            return [
                f"var {sname} = {a};",
                f"let {name} = spawn(|| -> string {{ return {sname}; }});",
                f"print(wait({name}));",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 13:
            # A string vector with the full mutation surface: pushes, an
            # indexed store (releasing the old element), and reads. The
            # differential exercises the string-ownership paths — the push
            # consumes, the store releases — against all four modes.
            name = self.fresh("sv")
            return [
                f"var {name} = strvec_new();",
                f'push({name}, "w{self.rnd() % 100}");',
                f'push({name}, "x{self.rnd() % 100}");',
                f'{name}[0] = "y{self.rnd() % 100}";',
                f"print(len({name}));",
                f'print({name}[0]);',
                f'print(join({name}, "-"));',
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 12:
            # A vector with the full mutation surface: push, indexed store,
            # sort, and reads. The MIR renders all of these as named
            # intrinsics now, so the four-way differential exercises their
            # lowering (index checks, sort determinism) on every seed.
            name = self.fresh("vv")
            return [
                f"var {name} = vec[{self.int_expr(vars)}, {self.int_expr(vars)}];",
                f"push({name}, {self.int_expr(vars)});",
                f"{name}[0] = {self.int_expr(vars)};",
                f"sort({name});",
                f"print(len({name}));",
                f"print({name}[0]);",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 11:
            # A string one branch consumes: the taken path moves it into a
            # by-value call, the untaken path leaves it live, so its scope-end
            # release is a mixed drop site the MIR backend must keep without
            # proof (M2.4). The name is retired after the branch — it is never
            # read again, which is what keeps the program accepted.
            name = self.fresh("str")
            a = '"m' + str(self.rnd() % 1000) + '"'
            return [
                f"var {name} = {a};",
                f"print(len({name}) > 0);",
                f"if {self.bool_expr(vars)} {{ print(eat({name})); }}",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 9:
            # A match over a generated enum value: every variant is covered,
            # arms carry payload bindings, and each arm prints one line.
            name = self.fresh("op")
            value = self.int_expr(vars)
            shape = self.rnd() % 3
            if shape == 0:
                init = f"Op::Wrap({value})"
            elif shape == 1:
                init = f"Op::Pair({value}, {self.int_expr(vars)})"
            else:
                init = "Op::Flat"
            return [
                f"var {name} = {init};",
                f"match {name} {{",
                f"    Op::Wrap(w) => {{ print(w); }}",
                f"    Op::Pair(a, b) => {{ print(a + b); }}",
                f"    Op::Flat => {{ print(42); }}",
                f"}}",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 10:
            # A counted loop with a continue landing on the hidden increment.
            # The bound is a constant, so the iteration count is fixed and the
            # output is deterministic whichever backend emits the loop.
            bound = (self.rnd() % 4) + 1
            skip = self.rnd() % bound
            name = self.fresh("i")
            return [
                f"for {name} in 0..{bound} {{",
                f"    if {name} == {skip} {{ continue; }}",
                f"    print({name});",
                f"}}",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 6:
            # A string built from two literals; reads are borrows.
            name = self.fresh("str")
            a = '"t' + str(self.rnd() % 1000) + '"'
            b = '"u' + str(self.rnd() % 1000) + '"'
            return [
                f"var {name} = {a} + {b};",
                f"print(len({name}));",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 7:
            # A vector with pushes and a length read.
            name = self.fresh("vec")
            return [
                f"var {name} = vec[{self.int_expr(vars)}, {self.int_expr(vars)}];",
                f"push({name}, {self.int_expr(vars)});",
                f"print(len({name}));",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 8:
            # A struct with two scalar fields, read field-wise.
            name = self.fresh("pt")
            return [
                f"var {name} = Pt {{ a: {self.int_expr(vars)}, b: {self.int_expr(vars)} }};",
                f"print({name}.a + {name}.b);",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 0:
            name = self.fresh("x")
            return [f"var {name} = {self.int_expr(vars)};"] + self.statement(vars + [name], depth + 1, docs, pointers)
        if kind == 1:
            return [
                f"if {self.bool_expr(vars)} {{",
                *self.statement(vars, depth + 1, docs, pointers),
                "} else {",
                *self.statement(vars, depth + 1, docs, pointers),
                "}",
            ]
        if kind == 2:
            name = self.fresh("n")
            bound = self.int_expr(vars)
            return [
                f"var {name} = {bound};",
                f"var {self.fresh('i')} = 0;",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 3:
            return [f"print({self.int_expr(vars)});"]
        if kind == 4:
            return [
                f"if {self.bool_expr(vars)} {{",
                f"    print({self.int_expr(vars)});",
                "}",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 14:
            # A Doc with two string fields opens the borrow surface: the
            # `&mut self` method, the free `&mut` function, the field view,
            # and the L2 field-granularity write. Views are born and die
            # inside one statement group, so no borrow state outlives a
            # statement and every emitted shape is one the parser accepts.
            name = self.fresh("d")
            return [
                f'var {name} = Doc {{ name: "alpha{self.rnd() % 100}", note: "keep" }};',
                *self.statement(vars, depth + 1, docs + [name], pointers),
            ]
        if kind == 15:
            # The L2 field-granularity face (ADR-0205): a view of `name`
            # stays readable across a write to the *other* field, then dies
            # at its read. The four modes must agree on both printed lines.
            d = self.pick(docs)
            h = self.fresh("h")
            return [
                f"let {h} = slice_view({d}.name, 0, 2);",
                f'{d}.note = "m{self.rnd() % 100}";',
                f"print({h});",
                f"print({d}.note);",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 16:
            # A `&mut` lending call (M2.5 slice 3): no views exist anywhere,
            # so the call's whole-variable release site is uncovered and it
            # parses; the modes must agree on the write landing, the return
            # value, and the renamed field.
            d = self.pick(docs)
            n = self.fresh("n")
            if self.rnd() % 2 == 0:
                call = f'{d}.rename("r{self.rnd() % 100}")'
            else:
                call = f'doc_bump(&mut {d}, "r{self.rnd() % 100}")'
            return [
                f"let {n} = {call};",
                f"print({n});",
                f"print({d}.name);",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 17:
            # The ordered face (M2.5 slice 3): the view is read by the first
            # argument — its last use — and the mut call releases after it;
            # left-to-right evaluation is what keeps this accepted.
            d = self.pick(docs)
            h = self.fresh("h")
            n = self.fresh("n")
            return [
                f"let {h} = slice_view({d}.name, 0, 2);",
                f'let {n} = doc_combine(len({h}), {d}.rename("r{self.rnd() % 100}"));',
                f"print({n});",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 18:
            # A raw pointer to a scalar (M2.12): the target never moves, so
            # the pointer stays valid for its whole scope. Every generated
            # scalar is a `var`, so the mutable form is always legal.
            target = self.pick(vars)
            name = self.fresh("ptr")
            if self.rnd() % 2 == 0:
                return [
                    f"let {name} = &{target} as *const i64;",
                    *self.statement(vars, depth + 1, docs, pointers + [(name, target, False)]),
                ]
            return [
                f"let {name} = &mut {target} as *mut i64;",
                *self.statement(vars, depth + 1, docs, pointers + [(name, target, True)]),
            ]
        if kind == 19:
            # A deref read inside unsafe (M2.12 slice 2): the pointer and the
            # variable agree on every backend.
            name, target, _ = self.pick(pointers)
            return [
                f"unsafe {{ print({name}.deref()); }}",
                f"print({target});",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 20:
            # A write through a mutable pointer (M2.12 slice 3): the variable,
            # the pointer, and any later pointer all observe the write.
            name, target, mutable = self.pick(pointers)
            if not mutable:
                name, target, mutable = next(((p, t, m) for (p, t, m) in pointers if m), (None, None, False))
            if name is None:
                return [f"print({self.int_expr(vars)});"]
            return [
                f"unsafe {{ *{name} = {self.int_expr(vars)}; }}",
                f"print({target});",
                f"unsafe {{ print({name}.deref()); }}",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 24:
            # Owned struct vector with Drop glue (P2.4 slices 4-5): element
            # construction, push, indexed overwrite, pop and scope release all
            # run the destructor exactly once, in the same order on every
            # backend (the drop prints are part of the differential output).
            name = self.fresh("tg")

            def tag(prefix: str) -> str:
                return f'FuzzTag {{ name: "{prefix}{self.rnd() % 1000}", n: {self.int_expr(vars)} }}'

            return [
                f"var {name} = vec[{tag('a')}, {tag('b')}];",
                f"push({name}, {tag('c')});",
                f"print(len({name}));",
                f"print({name}[0].n);",
                f"{name}[0] = {tag('d')};",
                f"print({name}[0].n);",
                f"print(fuzz_tag_n(&{name}[1]));",
                f"pop({name});",
                f"print(len({name}));",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 25:
            # String vector literals and the full mutation surface (P2.4
            # slice 6): text elements are owned, the store releases the old
            # element, and join reads the container.
            name = self.fresh("sw")

            def lit(prefix: str) -> str:
                return f'"{prefix}{self.rnd() % 1000}"'

            return [
                f"var {name} = vec[{lit('la')}, {lit('lb')}];",
                f"push({name}, {lit('lc')});",
                f"{name}[0] = {lit('ld')};",
                f"print(len({name}));",
                f'print(join({name}, "-"));',
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 26:
            # Vector fields and their nested receivers (P2.4 slices 9-11):
            # a struct's i64 vector field and an owned-struct vector field
            # mutate through `box.cells`, an assignment through the field
            # releases the old element, and a nested `outer.box.cells` chain
            # writes and reads in place.
            box = self.fresh("fb")
            outer = self.fresh("fo")

            def tag(prefix: str) -> str:
                return f'FuzzTag {{ name: "{prefix}{self.rnd() % 1000}", n: {self.int_expr(vars)} }}'

            value = lambda: self.int_expr(vars)
            return [
                f"var {box} = FuzzBox {{ cells: vec[{value()}, {value()}], tags: vec[{tag('f')}] }};",
                f"push({box}.cells, {value()});",
                f"{box}.cells[0] = {value()};",
                f"print(len({box}.cells));",
                f"print({box}.cells[0]);",
                f"push({box}.tags, {tag('g')});",
                f"print({box}.tags[0].n);",
                f"print(len({box}.tags));",
                f"var {outer} = FuzzOuter {{ box: FuzzBox {{ cells: vec[{value()}], tags: vec[{tag('h')}] }} }};",
                f"push({outer}.box.cells, {value()});",
                f"{outer}.box.cells[0] = {value()};",
                f"print(len({outer}.box.cells));",
                f"print(fuzz_tag_n(&{outer}.box.tags[0]));",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 27:
            # Local type annotations (P2.5.1): an empty `vec` takes its
            # element type from the annotation, a scalar binding declares its
            # shape, and a string literal binds with an annotation.
            name = self.fresh("tv")
            sname = self.fresh("ts")
            return [
                f"var {name}: vec = vec[];",
                f"push({name}, {self.int_expr(vars)});",
                f"print(len({name}));",
                f"print({name}[0]);",
                f'let {sname}: string = "t{self.rnd() % 1000}";',
                f"print(len({sname}));",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 28:
            # ADR-0236: a `&[i64]` built from a vector and from an array, plus
            # a forwarded slice parameter. The callee's loop and index reads
            # see the caller's elements and length through the fat pointer.
            name = self.fresh("sl")
            arr = self.fresh("sa")
            values = [self.int_expr(vars) for _ in range(3)]
            return [
                f"var {name} = vec[{values[0]}, {values[1]}, {values[2]}];",
                f"print(fuzz_slice_sum(&{name}));",
                f"print(fuzz_slice_forward(&{name}));",
                f"print(fuzz_slice_first(&{name}));",
                f"print(fuzz_slice_sum(&{name}[1..3]));",
                f"print(fuzz_slice_sum(&{name}[0..0]));",
                f"print(fuzz_slice_sum(&{name}[3..3]));",
                f"let {arr} = [{values[0]}, {values[1]}, {values[2]}];",
                f"print(fuzz_slice_sum(&{arr}));",
                f"print(fuzz_slice_first(&{arr}[0..2]));",
                f"print(fuzz_slice_sum(&{arr}[2..3]));",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        if kind == 29:
            # ADR-0236 element kinds: `&[string]` reads views, `&[FuzzTag]`
            # projects an owned element's field, and `&[vec]` iterates nested
            # containers — none of it moves the owner.
            words = self.fresh("ss")
            tags = self.fresh("st")
            rows = self.fresh("sr")

            def tag(prefix: str) -> str:
                return f'FuzzTag {{ name: "{prefix}{self.rnd() % 1000}", n: {self.int_expr(vars)} }}'

            value = lambda: self.int_expr(vars)
            return [
                f'var {words} = vec["w{self.rnd() % 1000}", "w{self.rnd() % 1000}"];',
                f"print(fuzz_slice_words(&{words}));",
                f"print(fuzz_slice_word_len(&{words}));",
                f"print(fuzz_slice_words(&{words}[0..1]));",
                f"print(fuzz_slice_word_len(&{words}[1..2]));",
                f"var {tags} = vec[{tag('s')}, {tag('t')}];",
                f"print(fuzz_slice_tags(&{tags}));",
                f"print(fuzz_slice_tag0(&{tags}));",
                f"print(fuzz_slice_tags(&{tags}[0..1]));",
                f"print(fuzz_slice_tag0(&{tags}[1..2]));",
                f"print({tags}[0].n);",
                f"var {rows} = vec[vec[{value()}, {value()}], vec[{value()}]];",
                f"print(fuzz_slice_rows(&{rows}));",
                f"print(fuzz_slice_rows(&{rows}[1..2]));",
                *self.statement(vars, depth + 1, docs, pointers),
            ]
        name = self.fresh("s")
        return [
            f"var {name} = {self.int_expr(vars)};",
            f"print({name} > 0 || {name} <= 0);",
            *self.statement(vars, depth + 1, docs, pointers),
        ]

    def program(self) -> str:
        # `main` cannot `return` in this language; its exit is implicit. The
        # struct Pt and its two scalar fields are fixed: field reads borrow.
        # The Op enum exercises match dispatch and payload bindings.
        lines = [
            "struct Pt { a: i64, b: i64 }",
            "struct Doc { name: string, note: string }",
            "enum Op { Wrap(i64), Pair(i64, i64), Flat }",
            "struct FuzzTag { name: string, n: i64 }",
            "impl Drop for FuzzTag {",
            '    fn drop(&mut self) { print("d" + self.name); }',
            "}",
            "struct FuzzBox { cells: vec, tags: vec[FuzzTag] }",
            "struct FuzzOuter { box: FuzzBox }",
            "fn fuzz_tag_n(t: &FuzzTag) -> i64 { return t.n; }",
            "fn fuzz_slice_sum(v: &[i64]) -> i64 { var t = 0; for x in v { t += x; } return t; }",
            "fn fuzz_slice_forward(v: &[i64]) -> i64 { return fuzz_slice_sum(v); }",
            "fn fuzz_slice_first(v: &[i64]) -> i64 { return v[0]; }",
            "fn fuzz_slice_words(v: &[string]) -> i64 { var t = 0; for w in v { t += len(w); } return t; }",
            "fn fuzz_slice_word_len(v: &[string]) -> i64 { return len(v[0]); }",
            "fn fuzz_slice_tags(v: &[FuzzTag]) -> i64 { var t = 0; for x in v { t += x.n; } return t; }",
            "fn fuzz_slice_tag0(v: &[FuzzTag]) -> i64 { return v[0].n; }",
            "fn fuzz_slice_rows(v: &[vec]) -> i64 { var t = 0; for r in v { t += len(r); } return t; }",
            "fn eat(t: string) -> i64 { return len(t); }",
            "impl Doc {",
            "    fn rename(&mut self, to: string) -> i64 {",
            "        self.name = to;",
            "        return len(self.name);",
            "    }",
            "}",
            "fn doc_bump(d: &mut Doc, to: string) -> i64 {",
            "    d.name = to;",
            "    return len(d.name);",
            "}",
            "fn doc_combine(a: i64, b: i64) -> i64 {",
            "    return a + b;",
            "}",
            "fn main() {",
        ]
        vars = ["seed0"]
        lines.append("    var seed0 = 7;")
        body = self.statement(vars, 0, [], [])
        lines.extend("    " + line for line in body)
        lines.append("}")
        return "\n".join(lines)


def compile_at(source: str, workdir: Path, tag: str, opt: bool, tree: bool = False,
               cache_dir: str = "", timeout: int = 120) -> Path:
    src = workdir / f"prog_{tag}.ae"
    src.write_text(source, encoding="utf-8")
    # `build` optimizes by default; --debug compiles the same source at -O0.
    # AE_CODEGEN=tree switches that one compile to the structured tree
    # emitter, so the MIR backend's C is differentiated against it (M2.3).
    # AE_CACHE points the worker at its own content cache, so parallel
    # workers never share mutable cache state with each other or with the
    # user's cache.
    env = {**os.environ, "AE_CODEGEN": "tree"} if tree else None
    if cache_dir:
        env = {**(env or os.environ), "AE_CACHE": cache_dir}
    result = subprocess.run(
        [str(EXE), "build", str(src), "-o", str(workdir / f"prog_{tag}")] + ([] if opt else ["--debug"]),
        capture_output=True,
        text=True,
        env=env,
        timeout=timeout,
    )
    if result.returncode != 0:
        raise RuntimeError(f"compile ({tag}) failed: {result.stderr}")
    out = workdir / f"prog_{tag}"
    if os.name == "nt" and not out.suffix:
        # Windows executability comes from the `.exe` suffix, which `-o` does
        # not add when the user names the file explicitly. A stale file from
        # an earlier seed with the same tag is replaced, not merged.
        renamed = out.with_suffix(".exe")
        renamed.unlink(missing_ok=True)
        out.rename(renamed)
        return renamed
    return out


def _run_dump(runs: list[tuple[str, subprocess.CompletedProcess]]) -> str:
    """Every mode's exit code, stdout, and stderr, for a mismatch report."""
    blocks = []
    for name, run in runs:
        blocks.append(f"--- {name} (exit {run.returncode}) ---{NL}{run.stdout}{NL}[stderr]{NL}{run.stderr}")
    return NL.join(blocks)


CACHE_BYTE_BUDGET = 512 * 1024 * 1024


def cache_is_over_budget(cache_dir: Path) -> bool:
    total = 0
    try:
        for entry in cache_dir.rglob("*"):
            if entry.is_file():
                total += entry.stat().st_size
                if total > CACHE_BYTE_BUDGET:
                    return True
    except OSError:
        return False
    return total > CACHE_BYTE_BUDGET


def maybe_trim_cache(cache_dir: Path, state: dict) -> None:
    # At most one size walk per 120s: the cache is keyed by distinct program
    # shapes, so a duration run grows it without bound; dropping it only
    # costs recompiles.
    now = time.perf_counter()
    if now - state["last"] < 120.0:
        return
    state["last"] = now
    if cache_is_over_budget(cache_dir):
        print("cache over 512M budget: dropping", flush=True)
        shutil.rmtree(cache_dir, ignore_errors=True)
        cache_dir.mkdir(parents=True, exist_ok=True)


def run_seed(seed: int, workdir: Path, cache_dir: Path, compile_timeout: int) -> dict:
    """One seed's four-way differential, fully isolated.

    The seed's files live in their own directory and the compiles run with
    their own content cache, so parallel workers never share mutable state
    with each other or with the user's cache. Returns a result dict; never
    raises (every failure becomes a result).

    An environmental failure (compile timeout under load, ENOSPC, tmpdir
    race) is the host losing the race, not the language diverging, so the
    seed retries once; a still-environmental result is reported as ENV and
    is not counted as a differential failure (M2.13 counts real divergences).
    """
    result = run_seed_once(seed, workdir, cache_dir, compile_timeout)
    if result["ok"] is False and result.get("env"):
        retry = run_seed_once(seed, workdir, cache_dir, compile_timeout)
        if retry["ok"] or not retry.get("env"):
            return retry
        result["detail"] = "ENV after retry: " + retry["detail"] + " (first attempt: " + result["detail"] + ")"
    return result


def run_seed_once(seed: int, workdir: Path, cache_dir: Path, compile_timeout: int) -> dict:
    started = time.perf_counter()
    result = {"seed": seed, "ok": None, "detail": "", "lines": 0, "source": ""}
    gen = Gen(seed)
    source = gen.program()
    result["source"] = source
    seed_dir = workdir / f"s{seed}"
    seed_dir.mkdir(parents=True, exist_ok=True)
    cache = str(cache_dir)
    try:
        debug_bin = compile_at(source, seed_dir, "d", opt=False, cache_dir=cache, timeout=compile_timeout)
        tree_bin = compile_at(source, seed_dir, "t", opt=False, tree=True, cache_dir=cache, timeout=compile_timeout)
        release_bin = compile_at(source, seed_dir, "r", opt=True, cache_dir=cache, timeout=compile_timeout)
        runs = []
        for bin_path in (debug_bin, tree_bin, release_bin):
            runs.append((bin_path.name, subprocess.run([str(bin_path)], capture_output=True, text=True, timeout=30)))
        interp_run = subprocess.run(
            [str(EXE), "run", str(seed_dir / "prog_d.ae"), "--interp"],
            capture_output=True,
            text=True,
            timeout=30,
        )
        runs.append(("interp", interp_run))
        # A runtime trap is part of the language's observable behavior, so
        # agreement includes traps (seed 1762's overflow): every mode must
        # exit with the same code, print the same stdout up to the trap, and
        # trap at the same position with the same message. ADR-55 exempts
        # diagnostic wording for compile and file errors, not runtime traps;
        # the interp's `ae: ` prefix spacing is normalized away.
        codes = {run.returncode for _, run in runs}
        outs = [run.stdout for _, run in runs]
        traps = [re.match(r"ae:\s*(\d+:\d+): (.+)", run.stderr.strip()) for _, run in runs] if codes != {0} else []
        if codes != {0} and (None in traps or len(set(t.groups() for t in traps if t)) != 1):
            result["ok"] = False
            result["detail"] = "TRAP MISMATCH" + NL + _run_dump(runs)
        elif len(codes) != 1 or len(set(outs)) != 1:
            result["ok"] = False
            result["detail"] = "OUTPUT MISMATCH" + NL + _run_dump(runs)
        else:
            result["ok"] = True
            result["lines"] = len(outs[0].splitlines())
    except subprocess.TimeoutExpired as error:
        # A compile or run timeout is the host losing the race under load, so
        # the result is flagged environmental and `run_seed` retries it once
        # instead of charging the M2.13 gate with a false divergence.
        result["ok"] = False
        result["env"] = True
        result["detail"] = f"environmental timeout: {error}"
    except OSError as error:
        # OSError covers the environment failing under the seed (ENOSPC on a
        # long duration run, tmpdir races): that seed fails, the run survives
        # — a harness that dies on ENOSPC loses its whole segment summary.
        result["ok"] = False
        result["env"] = True
        result["detail"] = str(error)
    except RuntimeError as error:
        # The compiler rejected the generated program: a real
        # generator/compiler disagreement, not host noise, so this stays a
        # differential FAILURE (no env flag, no retry).
        result["ok"] = False
        result["detail"] = str(error)
    # The seed's directory (source, generated C, binaries) is dead weight
    # once the verdict is decided; a duration run accumulates ~14.6k seeds
    # per hour and a 16G root disk fills in exactly one segment without
    # this cleanup (evaluate-2, 2026-11-15). The shared content cache in
    # cache_dir stays: bounded by distinct program shapes, it is what
    # keeps the compiles fast.
    shutil.rmtree(seed_dir, ignore_errors=True)
    result["seconds"] = time.perf_counter() - started
    return result


NL = chr(10)

def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--seeds", type=int, default=40)
    parser.add_argument("--start", type=int, default=0)
    parser.add_argument("--stop-before", type=int,
                        help="exclusive seed limit, also honored in duration mode")
    parser.add_argument("--jobs", type=int, default=min(os.cpu_count() or 2, 8),
                        help="parallel workers; each gets its own files and content cache")
    parser.add_argument("--compile-timeout", type=int, default=120,
                        help="seconds before one compile is abandoned (and fails that seed)")
    parser.add_argument("--save-corpus", metavar="DIR",
                        help="write every generated program as DIR/seed_N.ae")
    parser.add_argument("--duration", type=float, default=0,
                        help="run for this many hours instead of --seeds seeds "
                             "(the M2.13 24h acceptance mode); the seed stream "
                             "continues past any count and the summary reports "
                             "the seeds actually run")
    args = parser.parse_args()
    if args.stop_before is not None and args.stop_before <= args.start:
        parser.error("--stop-before must exceed --start")

    if not EXE.exists():
        print(f"ae binary missing: {EXE}\nbuild it first: cargo build --workspace", file=sys.stderr)
        return 2

    jobs = max(1, args.jobs)
    started = time.perf_counter()
    results: dict[int, dict] = {}
    cache_state = {"last": 0.0}

    with tempfile.TemporaryDirectory() as tmp:
        workdir = Path(tmp)
        cache_dir = workdir / "cache"
        deadline = started + args.duration * 3600.0
        seed = args.start

        def stream():
            # The optional ceiling keeps adjacent campaign ranges disjoint.
            while args.stop_before is None or seed < args.stop_before:
                if args.duration > 0:
                    if time.perf_counter() >= deadline:
                        return
                elif seed - args.start >= args.seeds:
                    return
                yield seed

        def run_one(current_seed):
            return run_seed(current_seed, workdir, cache_dir, args.compile_timeout)

        from concurrent.futures import ThreadPoolExecutor, FIRST_COMPLETED, wait
        with ThreadPoolExecutor(max_workers=jobs) as pool:
            futures = set()
            for current_seed in stream():
                futures.add(pool.submit(run_one, current_seed))
                seed = current_seed + 1
                # Bounded in-flight window: block until a worker finishes, so
                # the print stream stays live and the deadline check governs
                # scheduling rather than a huge backlog.
                while len(futures) >= jobs * 2:
                    done, _ = wait(futures, return_when=FIRST_COMPLETED)
                    for future in done:
                        result = future.result()
                        futures.discard(future)
                        results[result["seed"]] = result
                        mark = "ok" if result["ok"] else ("ENV" if result.get("env") else "FAILURE")
                        print(f"seed {result['seed']}: {mark} ({result['lines']} lines)", flush=True)
                maybe_trim_cache(cache_dir, cache_state)
            while futures:
                done, _ = wait(futures, return_when=FIRST_COMPLETED)
                for future in done:
                    result = future.result()
                    futures.discard(future)
                    results[result["seed"]] = result
                    mark = "ok" if result["ok"] else ("ENV" if result.get("env") else "FAILURE")
                    print(f"seed {result['seed']}: {mark} ({result['lines']} lines)", flush=True)
                maybe_trim_cache(cache_dir, cache_state)

        if args.save_corpus:
            corpus_dir = Path(args.save_corpus)
            corpus_dir.mkdir(parents=True, exist_ok=True)
            for seed in sorted(results):
                (corpus_dir / f"seed_{seed}.ae").write_text(results[seed]["source"], encoding="utf-8")
            print(f"corpus saved: {corpus_dir} ({len(results)} programs)")

    elapsed = time.perf_counter() - started
    order = sorted(results)
    failed_seeds = [seed for seed in order if not results[seed]["ok"] and not results[seed].get("env")]
    env_seeds = [seed for seed in order if not results[seed]["ok"] and results[seed].get("env")]
    for seed in failed_seeds:
        print("")
        print(f"seed {seed}: FAILURE")
        print(results[seed]["source"])
        print(results[seed]["detail"])
    for seed in env_seeds:
        print("")
        print(f"seed {seed}: ENVIRONMENTAL (host lost the race, not a language divergence)")
        print(results[seed]["source"])
        print(results[seed]["detail"])
    rate = len(results) / elapsed if elapsed > 0 else 0.0
    suffix = f", {len(env_seeds)} environmental" if env_seeds else ""
    print(f"\n{len(results)} seeds, {len(failed_seeds)} failures{suffix}, {elapsed:.1f}s ({rate:.2f} seeds/s, {jobs} jobs)")
    if failed_seeds:
        print(f"failed seeds: {failed_seeds}")
    if env_seeds:
        print(f"environmental seeds: {env_seeds}")
    return 1 if failed_seeds else 0


if __name__ == "__main__":
    sys.exit(main())
