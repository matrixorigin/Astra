#!/usr/bin/env python3
"""Validate capability-matrix system-test references without compiling.

The Rust matrix deliberately stores test names as strings so it can remain a
small product inventory. Rust cannot type-check a string against a test in a
different integration-test crate, so this validator closes that otherwise
silent drift: every ``system_test`` entry must resolve to a Rust function in
the workspace's ``crates`` tree.

This is an offline, dependency-free check intended for local runs and CI:

    scripts/e2e/validate_capability_matrix.py
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path


SYSTEM_TEST_RE = re.compile(r'\bsystem_test:\s*"([A-Za-z_][A-Za-z0-9_]*)"')
FUNCTION_RE = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(")


def collect_system_tests(matrix: Path) -> list[str]:
    return list(dict.fromkeys(SYSTEM_TEST_RE.findall(matrix.read_text())))


def collect_rust_functions(crate_root: Path) -> set[str]:
    functions: set[str] = set()
    for path in crate_root.rglob("*.rs"):
        if any(part in {"target", ".git"} for part in path.parts):
            continue
        functions.update(FUNCTION_RE.findall(path.read_text()))
    return functions


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        type=Path,
        default=Path(__file__).resolve().parents[2],
        help="repository root (defaults to the checkout containing this script)",
    )
    args = parser.parse_args()
    root = args.root.resolve()
    matrix = root / "crates/astra-harness/src/capability_matrix.rs"
    if not matrix.is_file():
        print(f"error: capability matrix not found: {matrix}", file=sys.stderr)
        return 2

    names = collect_system_tests(matrix)
    functions = collect_rust_functions(root / "crates")
    missing = [name for name in names if name not in functions]
    if missing:
        print("missing capability system-test functions:", file=sys.stderr)
        for name in missing:
            print(f"  {name}", file=sys.stderr)
        return 1

    print(f"capability matrix: {len(names)} system-test references resolved")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
