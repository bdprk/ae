"""Sync the Windows working-tree state to the WSL2 verification clone and
run the Linux gate suite there (the hybrid strategy: Windows checkout is the
source of truth, the WSL2 clone is the Linux verification leg — see the
platform-matrix 2026-11-21 entry).

Steps:
1. export the uncommitted state (tracked diff + untracked files via
   `git ls-files --others --exclude-standard`, so ignored build output never
   travels);
2. apply it in the WSL clone (a `git checkout -- .` first so CRLF noise from
   the Windows checkout cannot masquerade as changes);
3. run the Linux gates: `cargo build` + `cargo test --workspace --locked`,
   `verify_generated_c.py --sanitize --leak`, the differential fuzzer,
   std layering, and the script unit tests;
4. store the raw outputs under `bench/results/<timestamp>-wsl2/`.

Usage: python scripts/wsl_sync.py [--distro Ubuntu] [--clone ~/ae] [--skip-fuzz]
"""

import argparse
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RESULTS = ROOT / "bench" / "results"


def to_wsl_path(windows_path: Path) -> str:
    """`C:\\dir\\file` -> `/mnt/c/dir/file` (the WSL mount of the drive)."""
    resolved = windows_path.resolve()
    drive_letter = resolved.drive[0].lower()
    return f"/mnt/{drive_letter}{resolved.as_posix()[2:]}"


def run(args: list[str], cwd: Path = ROOT, check: bool = True) -> str:
    result = subprocess.run(args, capture_output=True, text=True, errors="replace", check=check, cwd=cwd)
    return result.stdout


def wsl_run(distro: str, command: str, check: bool = True) -> str:
    result = subprocess.run(
        ["wsl.exe", "-d", distro, "--", "bash", "-c", command],
        capture_output=True, text=True, errors="replace", check=check,
    )
    # wsl.exe prints localized notices (the localhost-proxy warning) on
    # stdout before the command's own output; the gate comparisons below
    # use exact text, so those lines are dropped.
    return "\n".join(
        line for line in result.stdout.splitlines() if not line.startswith("wsl: ")
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--distro", default="Ubuntu")
    parser.add_argument("--clone", default="~/ae")
    parser.add_argument("--skip-fuzz", action="store_true")
    options = parser.parse_args()

    timestamp = time.strftime("%Y-%m-%d-%H%M%S")
    out_dir = RESULTS / f"{timestamp}-wsl2"
    out_dir.mkdir(parents=True, exist_ok=True)
    log_lines: list[str] = []

    def log(message: str) -> None:
        print(message)
        log_lines.append(message)

    # 1. Export the uncommitted Windows state.
    #    The clone's origin is the Windows checkout itself, so first move it
    #    to the Windows HEAD: a clone left on an older commit makes every
    #    patch generated against the current HEAD fail to apply.
    wsl_run(options.distro, f"cd {options.clone} && git checkout -- . && (git clean -fd >/dev/null 2>&1 || true)")
    wsl_run(options.distro, f"cd {options.clone} && git fetch --quiet origin && git reset --hard origin/main")
    head = wsl_run(options.distro, f"cd {options.clone} && git rev-parse --short HEAD").strip()
    log(f"clone at {head}")
    patch = run(["git", "diff"])
    untracked = [name for name in run(
        ["git", "ls-files", "--others", "--exclude-standard"],
    ).split("\n") if name.strip()]
    log(f"exported: tracked diff {len(patch.splitlines())} lines, {len(untracked)} untracked files")

    temp_dir = Path(tempfile.mkdtemp(prefix="ae-wsl-sync-"))
    patch_file = temp_dir / "changes.patch"
    patch_file.write_text(patch, encoding="utf-8", newline="\n")
    tar_file = temp_dir / "untracked.tgz"
    if untracked:
        subprocess.run(
            ["tar", "czf", str(tar_file), *untracked],
            capture_output=True, text=True, check=True, cwd=ROOT,
        )

    patch_wsl = to_wsl_path(patch_file)
    tar_wsl = to_wsl_path(tar_file)

    # 2. Apply in WSL: reset EOL noise, apply the patch, extract untracked.
    wsl_run(options.distro, f"cd {options.clone} && git checkout -- . && (git clean -fd >/dev/null 2>&1 || true)")
    wsl_run(options.distro, f"cd {options.clone} && git apply '{patch_wsl}'")
    if untracked:
        wsl_run(options.distro, f"cd {options.clone} && tar xzf '{tar_wsl}'")
    applied = wsl_run(options.distro, f"cd {options.clone} && git status --porcelain | wc -l").strip()
    log(f"applied: {applied} changed paths in the WSL clone")

    # 3. The Linux gates.
    cargo_env = "source $HOME/.cargo/env && "
    build = wsl_run(options.distro, f"{cargo_env}cd {options.clone} && cargo build --workspace --locked 2>&1 | tail -1")
    gates: list[tuple[str, str]] = [("cargo build", build.strip())]
    test = wsl_run(options.distro, f"{cargo_env}cd {options.clone} && cargo test --workspace --locked 2>&1 | grep -E '^test result'")
    totals = re.findall(r"(\d+) passed; (\d+) failed", test)
    passed_total = sum(int(passed) for passed, _ in totals)
    failed_total = sum(int(failed) for _, failed in totals)
    gates.append(("cargo test", f"{passed_total} passed, {failed_total} failed across {len(totals)} binaries"))
    sanitize = wsl_run(options.distro, f"cd {options.clone} && python3 scripts/verify_generated_c.py --sanitize --leak 2>&1 | tail -1")
    gates.append(("sanitize+leak", sanitize.strip()))
    if not options.skip_fuzz:
        fuzz = wsl_run(options.distro, f"cd {options.clone} && python3 scripts/diff_fuzz.py --seeds 40 2>&1 | tail -1")
        gates.append(("fuzz 40 seeds", fuzz.strip()))
    layering = wsl_run(options.distro, f"cd {options.clone} && python3 scripts/std_layering.py")
    gates.append(("std layering", layering.strip()))
    ids = wsl_run(options.distro, f"cd {options.clone} && python3 scripts/check_commit_ids.py")
    gates.append(("commit ids", ids.strip()))

    failures = []
    for name, output in gates:
        last = output.strip().splitlines()[-1] if output.strip() else "empty"
        log(f"{name}: {last}")
        if name == "cargo test" and failed_total:
            failures.append("cargo test reported failures")
        if name == "sanitize+leak" and "PASS" not in output and output.strip() != "empty":
            failures.append("sanitize+leak did not pass cleanly")
        if name == "std layering" and output.strip() != "std layering holds":
            failures.append("std layering violated")
        if name == "commit ids" and output.strip() != "commit titles and ledger records agree":
            failures.append("commit ids disagreed")

    # 4. Archive the raw outputs.
    (out_dir / "wsl-gates.log").write_text("\n".join(log_lines) + "\n", encoding="utf-8", newline="\n")
    (out_dir / "wsl-cargo-test.log").write_text(test, encoding="utf-8", newline="\n")
    log(f"raw outputs: {out_dir}")

    if failures:
        print(f"{len(failures)} gate failures:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
