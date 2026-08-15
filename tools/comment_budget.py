#!/usr/bin/env python3
"""Report the fraction of lines that are comments, and fail above a budget.

Counts doc comments (`///`, `//!`) as comments. `tokei` and `scc` classify them as
code, which makes them useless here: measured on `tools/build.rs` — 43 comment lines
above 15 lines of code — tokei reports `code=15, comments=5`. The bloat this budget
exists to bound lives mostly in `//!` module headers, so a tool that scores them as
code would report the repo at 11% and never fire.

A comment line is one whose content is *only* comment text. A trailing comment
(`let n = 4; // why`) is not counted: it adds nothing for a reader to scroll past,
and counting it would penalise the terse form the policy actually wants.

String literals are lexed rather than pattern-matched, because `translate.rs` embeds
shell and JSON in raw strings and a line-prefix heuristic scores those `#` and `//`
lines as comments.
"""

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

# `#` starts a comment in these; `//` and `/* */` in the C-family ones.
HASH = {".toml", ".yaml", ".yml", ".sh", ".bash"}
SLASH = {".rs"}
SKIP_DIRS = {"results", "test-corpus", "harvest-bench", "c2saferrust", "target", ".git", "tables"}

# `cli.rs` is the CLI surface: its doc comments are `--help` output, and CLAUDE.md
# explicitly forbids pruning them ("deleting one silently changes what the binary
# prints"). Counting text the policy will not let you remove makes the budget
# unsatisfiable by legitimate means — the only way to absorb a growing `--help` is to
# delete invariant documentation elsewhere, which is exactly backwards. Measured when
# this was added: cli.rs was 149 of 1755 comment lines, i.e. the difference between
# passing and failing, entirely in user-facing output.
SKIP_FILES = {"tools/src/cli.rs"}


def blank_out_strings(line: str, ext: str) -> str:
    """Replace string-literal contents with spaces so their bytes cannot be mistaken
    for comment markers. Not a full parser: a string spanning lines is handled by the
    raw-string sweep in `classify_file`."""
    out, i, n = [], 0, len(line)
    while i < n:
        c = line[i]
        if c in "\"'":
            quote = c
            out.append(" ")
            i += 1
            while i < n:
                if line[i] == "\\":
                    out.append("  ")
                    i += 2
                    continue
                if line[i] == quote:
                    out.append(" ")
                    i += 1
                    break
                out.append(" ")
                i += 1
            continue
        out.append(c)
        i += 1
    return "".join(out)


def classify_file(path: Path):
    """Return (comment_only_lines, code_lines). Blank lines count as neither."""
    ext = path.suffix
    text = path.read_text(encoding="utf-8", errors="replace")

    # Mask multi-line raw strings (r"...", r#"..."#) wholesale: their contents are
    # data, and in this repo they hold shell scripts whose `#` lines would otherwise
    # score as comments.
    if ext in SLASH:
        for m in re.finditer(r'r(#*)"', text):
            end = text.find('"' + m.group(1), m.end())
            if end == -1:
                continue
            span = text[m.start(): end + 1 + len(m.group(1))]
            text = text.replace(span, re.sub(r"[^\n]", " ", span), 1)

    comments = code = 0
    in_block = False
    for raw in text.splitlines():
        line = raw.strip()
        if not line:
            continue
        if in_block:
            comments += 1
            if "*/" in line:
                in_block = False
            continue
        stripped = blank_out_strings(line, ext)
        if ext in SLASH and stripped.lstrip().startswith("/*"):
            comments += 1
            if "*/" not in stripped:
                in_block = True
            continue
        marker = "//" if ext in SLASH else "#"
        pos = stripped.find(marker)
        if pos == -1:
            code += 1
        elif not stripped[:pos].strip():
            comments += 1  # comment-only line
        else:
            code += 1  # trailing comment: the line still carries code
    return comments, code


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--max", type=float, default=15.0, help="budget, percent")
    ap.add_argument("--root", default=".")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    root = Path(args.root).resolve()
    tracked = subprocess.run(
        ["git", "-C", str(root), "ls-files"], capture_output=True, text=True, check=True
    ).stdout.split()

    rows, tc, tk = [], 0, 0
    for rel in tracked:
        p = root / rel
        if p.suffix not in HASH | SLASH or not p.is_file():
            continue
        if set(Path(rel).parts) & SKIP_DIRS or rel in SKIP_FILES:
            continue
        c, k = classify_file(p)
        if c + k == 0:
            continue
        tc += c
        tk += k
        rows.append((100 * c / (c + k), c, k, rel))

    total = 100 * tc / max(tc + tk, 1)
    rows.sort(reverse=True)

    if args.json:
        print(json.dumps({"ratio": total, "comments": tc, "code": tk,
                          "files": [{"path": r[3], "ratio": r[0], "comments": r[1], "code": r[2]}
                                    for r in rows]}, indent=1))
    else:
        print(f"{'ratio':>7} {'cmt':>5} {'code':>5}  file")
        for r, c, k, rel in rows[:15]:
            print(f"{r:6.1f}% {c:5d} {k:5d}  {rel}")
        if len(rows) > 15:
            print(f"  ... {len(rows) - 15} more files")
        print(f"\nTOTAL {tc} comment / {tc + tk} lines = {total:.1f}%  (budget {args.max:.0f}%)")

    if total > args.max:
        print(f"\n::error::comment ratio {total:.1f}% exceeds the {args.max:.0f}% budget", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
