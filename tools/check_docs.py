#!/usr/bin/env python3
"""Fail if the docs name a command the binary rejects, or a path that is not there.

Each `harvest-tools …` line is run with `--help` appended: clap parses the whole argv and
exits touching no agent, no store and no money.

Nothing checked this before, and the rot was not subtle. Before #141 the README documented
`--agent` (renamed `--tool` in #133), a `translate` subcommand (folded into `run`, so it now
answers `unrecognized subcommand`), six backends deleted in #138, four source files that no
longer exist, and `prompts/claude/*.md` after #130 moved that body to `prompts/shared/`. The
refresh that fixed all of it then added `--tool kiro --steps 1 run …`, which also does not
parse. Reading found none of these; executing found all of them.

Limit worth knowing: this proves arguments PARSE, not that they mean what the prose says.
"""

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Either profile: only argument PARSING is exercised, and that is identical in both. The
# `tests` job compiles no standalone binary at all (`cargo test --bin` leaves none behind),
# so it builds a debug one for this; a sweep box already has the release one.
BINARY = next(
    (
        p
        for p in (
            ROOT / "tools/target/release/harvest-tools",
            ROOT / "tools/target/debug/harvest-tools",
        )
        if p.is_file()
    ),
    ROOT / "tools/target/release/harvest-tools",
)

DOCS = ["README.md", "docs/DEVELOPER.md", "CLAUDE.md"]

# A placeholder is the author telling the reader to substitute something, so it cannot be
# executed and its absence is not rot.
PLACEHOLDER = re.compile(r"[<>]")

COMMAND = re.compile(r"\bharvest-tools ([^\n`|#]*)")

# Paths the reader is told to CREATE, and build outputs, which are never tracked and whose
# absence means only that nobody has built yet.
NOT_OURS = re.compile(r"mycases/|^results/\.\.\.|^tools/target/")
PATH = re.compile(r"\b(?:tools|prompts|docs|tables|results|test-corpus|harvest-bench)/[A-Za-z0-9_./*-]*")

# `https://rust-lang.org/tools/install/` contains something shaped exactly like a repo path.
# Judged by CONTEXT rather than by the matched text, because the text alone cannot tell them
# apart -- a real `tools/install/` would be indistinguishable.
IN_URL = re.compile(r"https?://\S*$")


def commands():
    for doc in DOCS:
        text = (ROOT / doc).read_text()
        for m in COMMAND.finditer(text):
            args = m.group(1).strip().rstrip("\\").strip()
            # "harvest-tools CLI (Rust)" is prose naming the binary, not an invocation.
            if not args or args[0] not in "-abcdefghijklmnopqrstuvwxyz":
                continue
            yield doc, args


def paths():
    for doc in DOCS:
        text = (ROOT / doc).read_text()
        for m in PATH.finditer(text):
            p = m.group(0).rstrip(".,)`")
            if "*" in p or NOT_OURS.search(p):
                continue
            if IN_URL.search(text[max(0, m.start() - 60) : m.start()]):
                continue
            yield doc, p


def enum_values(flag: str) -> set[str]:
    """What `--help` says a flag accepts. `--help` short-circuits clap BEFORE it validates
    values, so `--tool opencode --help` exits 0 for a tool that was deleted -- the check
    below has to compare against this set rather than rely on the parse."""
    out = subprocess.run([str(BINARY), "--help"], capture_output=True, text=True, cwd=ROOT)
    body = out.stdout.split(f"{flag} <", 1)[-1]
    values = set()
    for line in body.splitlines():
        m = re.match(r"\s+- ([a-z0-9][a-z0-9._-]*):", line)
        if m:
            values.add(m.group(1))
        elif values and line.strip() and not line.startswith(" " * 10):
            break
    return values


def submodules() -> set[str]:
    """The submodule paths, from `.gitmodules`.

    A path INSIDE one cannot be judged from this repo: the index holds only the gitlink, and
    the `tests` job checks no submodule out because it needs none. So the verdict must not
    depend on whether they happen to be populated -- that is the environment leaking into a
    gate, and it is why this passed locally and failed in CI on `test-corpus/Public-Tests/`.
    """
    out = subprocess.run(
        ["git", "config", "-f", ".gitmodules", "--get-regexp", r"^submodule\..*\.path$"],
        capture_output=True,
        text=True,
        cwd=ROOT,
    )
    return {line.split()[-1] for line in out.stdout.splitlines() if line.strip()}


def tracked() -> set[str]:
    """Paths git knows about, plus the directories containing them.

    Deliberately NOT `Path.exists()`. A checkout carries untracked debris -- deleting
    `translate.rs` leaves the file behind for anyone who had it -- so an on-disk check
    passes locally on a reference that is broken for every fresh clone. Measured while
    writing this gate: `tools/src/{translate,verify,workdir}.rs` were all sitting in the
    working tree, months after being deleted, and the filesystem check called them fine.
    """
    out = subprocess.run(
        ["git", "ls-files"], capture_output=True, text=True, cwd=ROOT, check=True
    )
    files = set(out.stdout.split())
    dirs = {str(Path(f).parent) for f in files}
    while dirs:
        files |= dirs
        dirs = {str(Path(d).parent) for d in dirs if d not in (".", "/")} - files
    # Submodule contents are not in this repo's index; the gitlink itself is.
    return files


def main() -> int:
    if not BINARY.is_file():
        print(f"::error::{BINARY} is not built; build it before this gate", file=sys.stderr)
        return 1

    tools = enum_values("--tool")
    if len(tools) < 2:
        print("::error::could not read --tool's accepted values from --help", file=sys.stderr)
        return 1

    bad, ran, skipped = [], 0, 0
    for doc, args in commands():
        if PLACEHOLDER.search(args):
            skipped += 1
            continue
        ran += 1
        argv = args.split()
        out = subprocess.run(
            [str(BINARY), *argv, "--help"], capture_output=True, text=True, cwd=ROOT
        )
        if out.returncode != 0:
            first = (out.stderr or out.stdout).strip().splitlines()
            bad.append(f"{doc}: harvest-tools {args}\n    {first[0] if first else '(no output)'}")
            continue
        # Comma-separated, because `--tool claude,codex,kiro` is how a multi-tool run is spelled.
        for i, a in enumerate(argv):
            if a == "--tool" and i + 1 < len(argv):
                unknown = set(argv[i + 1].split(",")) - tools
                if unknown:
                    bad.append(
                        f"{doc}: harvest-tools {args}\n    --tool does not accept "
                        f"{sorted(unknown)}; it accepts {sorted(tools)}"
                    )

    known, subs = tracked(), submodules()
    missing, checked, deferred = [], 0, 0
    for doc, p in paths():
        head = p.split("/", 1)[0]
        if head in subs:
            # The gitlink itself is ours to check; what is under it is not.
            if head not in known:
                missing.append(f"{doc}: {p} (submodule {head} is not registered)")
            else:
                deferred += 1
            continue
        checked += 1
        if p not in known and not (ROOT / p).is_dir():
            missing.append(f"{doc}: {p}")

    # A gate that inspected nothing must not report success -- the docs could have been
    # emptied, or these patterns could have stopped matching, and every assertion above
    # would hold vacuously.
    if ran < 5:
        print(f"::error::only {ran} runnable command(s) found; the extractor is broken", file=sys.stderr)
        return 1
    if checked < 10:
        print(f"::error::only {checked} path(s) found; the extractor is broken", file=sys.stderr)
        return 1

    for b in bad:
        print(f"::error::{b}", file=sys.stderr)
    for m in missing:
        print(f"::error::documented path does not exist: {m}", file=sys.stderr)
    if bad or missing:
        print(
            f"\n{len(bad)} command(s) the binary rejects, {len(missing)} path(s) absent. "
            "A documented command that does not parse is one a reader cannot run.",
            file=sys.stderr,
        )
        return 1

    print(
        f"docs: {ran} command(s) parse, {checked} path(s) exist "
        f"({skipped} placeholder(s), {deferred} inside submodules)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
