#!/usr/bin/env python3
"""Enforce the 500-physical-line limit for first-party Rust source files."""

from pathlib import Path
import sys

MAX_LINES = 500
BYPASS_PREFIX = "// BYPASS_LINECOUNT:"


def check_file(path: Path) -> str | None:
    lines = path.read_text(encoding="utf-8").splitlines()
    if len(lines) <= MAX_LINES:
        return None
    for line in lines:
        stripped = line.strip()
        if stripped.startswith(BYPASS_PREFIX) and len(stripped) > len(BYPASS_PREFIX) + 5:
            return None
    return f"{path}: {len(lines)} lines (limit {MAX_LINES})"


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    violations: list[str] = []
    for folder in ("src", "tests"):
        base = root / folder
        if not base.exists():
            continue
        for rs_file in sorted(base.rglob("*.rs")):
            err = check_file(rs_file)
            if err:
                violations.append(err)
    if violations:
        print("Line count policy violations:", file=sys.stderr)
        for v in violations:
            print(f"  - {v}", file=sys.stderr)
        return 1
    print("All first-party Rust source files are within the 500-line limit.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
