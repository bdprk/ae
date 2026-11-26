# Security Policy

AE is a prototype. It compiles `.ae` sources to C11 and hands that C to the
system toolchain, so the trust boundary is the same as running a C compiler on
attacker-supplied input: a hostile program is a hostile C program.

## Supported versions

Only the tip of `main` is supported. Releases before `v0.2.0` are snapshots and
do not receive fixes.

## Reporting a vulnerability

Report privately through
[GitHub Security Advisories](https://github.com/bdprk/ae/security/advisories/new)
rather than a public issue. Please include:

- the smallest `.ae` source that reproduces the problem, and the exact command
  that ran it (`check`, `emit-c`, `run`, `build`, or `test`);
- `ae version` output, the platform, and the C toolchain (`gcc --version` or
  the `AE_CC` value in use);
- what you expected, and what happened instead (diagnostic text, wrong value,
  panic, sanitizer report, or a generated-C snippet).

Reports about the following are in scope:

- a well-formed source that makes the compiler panic, instead of producing a
  diagnostic;
- a source the compiler accepts that produces generated C with undefined
  behavior, or that breaks a documented guarantee (checked arithmetic, bounds
  checks, use-after-move rejection, deterministic deallocation);
- a diagnostic that misattributes a source location in a way that makes the
  error unactionable.

Out of scope: the safety of programs the user deliberately writes to be unsafe
at the C level, the absence of features listed as not yet implemented in
`docs/current-language.md`, and any finding that requires patching the
repository's own build scripts.

## What to expect

This is a single-maintainer project with no bounty program. Acknowledgement
usually happens within a week; a fix lands as a normal commit with a regression
test, and the advisory is published once the fix is on `main`. Credit is given
in the advisory unless you ask otherwise.
