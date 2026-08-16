#!/usr/bin/env python3
"""Count the comment-only lines in the tree, and fail above a ceiling.

The primary ceiling is an absolute number of comment lines, not a ratio of the tree. A
ratio cannot tell "comments were added" from "code was removed", so deleting
comment-sparse code raises it and every deletion fights the gate; an absolute count says
what the policy actually wants — do not let comments sprawl — and a deletion can only
lower it.

`--max-ratio` is a second, deliberately loose ceiling, because the two limits are blind to
different things: the count cannot see 5,000 lines of comment-sparse code deleted with
every comment kept (the count does not move), and the ratio cannot see comments and code
growing together. Both limits are supplied by `.github/workflows/type-safety.yaml`, which
records the measurement, the headroom, and what each one catches that the other cannot.

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

`tools/test_comment_budget.py` pins the failures this file has actually had, including
that both limits still exit non-zero.
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
    raw-string sweep in `mask_raw_strings`."""
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


# The opening of a raw string, byte raw strings included: an `r` or `br` that is not part of
# anything else. Without the lookbehind this matched the last letter of an ordinary word before
# a closing quote — `"a valid parser")` — and 95 of the 149 matches over `tools/src/**.rs` were
# such words. Everything to the next quote was then blanked: 38 lines of `oracle/mod.rs` at one
# match, the `r"` inside `"/usr"` on line 18, whose span ran to the next quote 42 lines later.
#
# The reduction is 95 false matches -> 1, not -> 0. RESIDUAL: a string whose last character is
# `r` preceded by a non-identifier character still opens a phantom raw string, and
# `cache.rs`'s `let r = roots("/w", "/r");` is one such site today — it costs 0 counted lines
# because its span dies inside the following line, and
# `a_string_ending_in_slash_r_is_the_one_false_match_left` pins that. Eliminating the class
# needs real Rust string lexing, which is not worth it for one harmless match.
#
# `b?` sits INSIDE the lookbehind's reach — `(?<!..)b?r` tests the character before the `b` —
# so `br"` and `br#"..."#` are masked while `abr"` is not. Without it byte raw strings were
# never masked, which is the very error the lookbehind was added to stop, in mirror image.
RAW_OPEN = re.compile(r'(?<![0-9A-Za-z_\\])b?r(#*)"')


def mask_raw_strings(text: str) -> str:
    """Blank multi-line raw strings (r"...", r#"..."#) wholesale: their contents are
    data, and in this repo they hold shell scripts and prompts whose `#` and `//` lines
    would otherwise score as comments. Line structure is preserved, so a masked line
    counts as neither comment nor code."""
    out, i = [], 0
    for m in RAW_OPEN.finditer(text):
        if m.start() < i:
            continue  # inside a raw string already masked; its body is data, not source
        close = '"' + m.group(1)
        end = text.find(close, m.end())
        if end == -1:
            continue
        stop = end + len(close)
        out.append(text[i:m.start()])
        out.append(re.sub(r"[^\n]", " ", text[m.start():stop]))
        i = stop
    out.append(text[i:])
    return "".join(out)


def classify_file(path: Path):
    return classify_text(path.read_text(encoding="utf-8", errors="replace"), path.suffix)


def classify_text(text: str, ext: str):
    """Return (comment_only_lines, code_lines). Blank lines count as neither."""
    if ext in SLASH:
        text = mask_raw_strings(text)

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
    # `allow_abbrev=False` rejects unambiguous prefixes: argparse's default accepts
    # `--max-c 2560 --max-r 20`, and the spelling `type-safety.yaml` uses should be the only
    # one that works. It is NOT what rejects the retired `--max 14`, which exits 2 either
    # way — with abbreviation on, "ambiguous option: --max could match --max-comments,
    # --max-ratio"; with it off, "the following arguments are required".
    ap = argparse.ArgumentParser(allow_abbrev=False)
    # Required, with no default: a default is a second copy of the ceiling that drifts
    # from the workflow's, and an omitted flag would report without gating anything.
    ap.add_argument("--max-comments", type=int, required=True,
                    help="primary ceiling: comment-only lines, whole tree; a deletion "
                         "cannot trip it")
    ap.add_argument("--max-ratio", type=float, required=True,
                    help="backstop ceiling: comment %% of counted lines, for the one class "
                         "--max-comments is blind to — code deleted, comments kept")
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
        rows.append((c, k, rel))

    total = 100 * tc / max(tc + tk, 1)
    rows.sort(key=lambda row: (-row[0], row[2]))

    if args.json:
        print(json.dumps({"comments": tc, "code": tk, "ratio": total,
                          "files": [{"path": rel, "comments": c, "code": k,
                                     "ratio": 100 * c / (c + k)}
                                    for c, k, rel in rows]}, indent=1))
    else:
        print(f"{'cmt':>5} {'code':>5} {'ratio':>7}  file")
        for c, k, rel in rows[:15]:
            print(f"{c:5d} {k:5d} {100 * c / (c + k):6.1f}%  {rel}")
        if len(rows) > 15:
            print(f"  ... {len(rows) - 15} more files")
        print(f"\nTOTAL {tc} comment lines (ceiling {args.max_comments}), "
              f"{tc + tk} lines counted, {total:.2f}% (ratio ceiling {args.max_ratio}%)")

    over = False
    if tc > args.max_comments:
        print(f"\n::error::{tc} comment lines exceeds the ceiling of {args.max_comments}",
              file=sys.stderr)
        over = True
    if total > args.max_ratio:
        print(f"\n::error::{total:.2f}% comment lines exceeds the ratio ceiling of "
              f"{args.max_ratio}%: comments were kept while counted lines went away",
              file=sys.stderr)
        over = True
    return 1 if over else 0


if __name__ == "__main__":
    sys.exit(main())
