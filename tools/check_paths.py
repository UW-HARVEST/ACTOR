#!/usr/bin/env python3
"""Fail if a committed shell script or GitHub Actions file names one machine's home
directory. Such a path silently breaks for every other checkout and every CI runner.

Deliberately written in Python rather than as a shell one-liner in the workflow: the
patterns have to appear somewhere, and a `.sh` checker or an inline `run:` block would
be in its own scope and report itself. This file is neither, so nothing is exempted
and `.github/**` is scanned in full, including the workflow that invokes this.

`tools/src/**` is out of scope for the same reason it must be: `cache.rs` and
`workdir.rs` use absolute home paths as test fixtures, where the whole point is that
they are foreign to the machine running the test.
"""

import re
import subprocess
import sys
from pathlib import Path

PATTERNS = [
    re.compile(r"/local/home/\w"),
    re.compile(r"/home/\w"),
    re.compile(r"/Users/\w"),
]


def in_scope(rel: str) -> bool:
    return rel.endswith(".sh") or rel.startswith(".github/")


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    tracked = subprocess.run(
        ["git", "-C", str(root), "ls-files"], capture_output=True, text=True, check=True
    ).stdout.split()

    hits = []
    for rel in tracked:
        if not in_scope(rel):
            continue
        p = root / rel
        if not p.is_file():
            continue
        for n, line in enumerate(p.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
            for pat in PATTERNS:
                m = pat.search(line)
                if m:
                    hits.append((rel, n, m.group(0)))
                    break

    for rel, n, frag in hits:
        print(f"::error file={rel},line={n}::hardcoded home path {frag!r}", file=sys.stderr)
    if hits:
        print(
            f"\n{len(hits)} hardcoded home path(s) in committed *.sh / .github/**. "
            "Derive the path instead ($HOME, the script's own location, or a "
            "GitHub-provided variable).",
            file=sys.stderr,
        )
        return 1
    print("no hardcoded home paths in committed *.sh / .github/**")
    return 0


if __name__ == "__main__":
    sys.exit(main())
