#!/usr/bin/env python3
"""The failures `comment_budget.py` has actually had, pinned.

    python3 tools/test_comment_budget.py

Every case asserts the corrected count *and* that a broken masker gets a different one,
so a fixture that stopped carrying its trap fails here instead of going quietly green —
which is how a detector that misread 95 of its 149 matches survived inside a required
gate for the whole sequence.

Two cases run the script as a subprocess. `classify_text` cannot fail a build: only
`main`'s two comparisons and its exit code can, and those were the part nothing covered,
so inverting either comparison or dropping the `return 1` left every test here green.
"""

import json
import re
import subprocess
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import comment_budget as cb  # noqa: E402

GATE = Path(__file__).resolve().parent / "comment_budget.py"
REPO = Path(__file__).resolve().parents[1]


def run_gate(max_comments, max_ratio, *extra):
    """The gate exactly as CI invokes it, so the exit code is the real one."""
    return subprocess.run(
        [sys.executable, str(GATE), "--root", str(REPO),
         "--max-comments", str(max_comments), "--max-ratio", str(max_ratio), *extra],
        capture_output=True, text=True, check=False)


def measured_tree():
    """The tree's own totals, from a run whose limits it cannot exceed — which is also the
    "well under the limits exits zero" half of the two exit-code cases below."""
    wide = run_gate(10**9, 100.0, "--json")
    assert wide.returncode == 0, (
        "a tree under both limits must exit 0; the comparisons in main() no longer say "
        f"what they mean:\n{wide.stderr}")
    return json.loads(wide.stdout)


def shipped_masker(text: str) -> str:
    """`mask_raw_strings` as it shipped: `r(#*)"` with no lookbehind, so any `r` before a
    quote opened a raw string, and the span put back by searching for its text rather
    than at the position it was found."""
    for m in re.finditer(r'r(#*)"', text):
        end = text.find('"' + m.group(1), m.end())
        if end == -1:
            continue
        span = text[m.start(): end + 1 + len(m.group(1))]
        text = text.replace(span, re.sub(r"[^\n]", " ", span), 1)
    return text


def counted_with(masker, text: str):
    saved = cb.mask_raw_strings
    cb.mask_raw_strings = masker
    try:
        return cb.classify_text(text, ".rs")
    finally:
        cb.mask_raw_strings = saved


TWENTY = "\n".join(f"    let n{i:02} = {i};" for i in range(1, 21))

WORD_ENDING_IN_R = f'''pub fn parse(x: &str) -> usize {{
    assert!(!x.is_empty(), "a valid parser");
{TWENTY}
    let sum = "done";
    n20
}}
'''

# Three traps in one raw string plus one byte raw string: `#` and `//` lines that are data,
# an inner `r"` that the re-entry guard must not re-process, and `br#"`, which the
# lookbehind excluded until `b?` was put inside its reach.
REAL_RAW_STRING = '''//! A prompt held in a raw string.

pub const PROMPT: &str = r#"
Keep the build preamble intact:
# use the pinned toolchain
cargo build --locked
and mark every unsafe block:
// SAFETY: the caller checked the bound
grep -o r"needle" input.txt
# this line is only data while the re-entry guard holds
"#;

pub const MAGIC: &[u8] = br#"
# a byte raw string is a raw string
// and its markers are data too
"#;
'''

# The one false match the corrected detector still accepts: a string ending in `r` whose
# preceding character is not an identifier one, so the lookbehind does not exclude it.
# Copied from the live site, `cache.rs:1053`.
FALSE_MATCH_LEFT = '''fn keys() {
    let r = roots("/w", "/r");
    assert_ne!(r.work, r.results, "the roots must differ");
}
'''

BOTH_TRAPS = '''//! Both traps, side by side.

/// Doc on the function.
pub fn parse(text: &str) -> usize {
    assert!(!text.is_empty(), "an empty input is not a valid parser");
    let mut n = 0;
    for line in text.lines() {
        n += line.len();
    }
    n
}

pub const RECIPE: &str = r#"
set -euo pipefail
# use the pinned toolchain
cargo build --locked
"#;
'''


class CommentBudget(unittest.TestCase):
    def test_a_word_ending_in_r_before_a_quote_is_not_a_raw_string(self):
        for i in range(1, 21):
            self.assertIn(f"    let n{i:02} = {i};", WORD_ENDING_IN_R)

        comments, code = cb.classify_text(WORD_ENDING_IN_R, ".rs")
        self.assertEqual((comments, code), (0, 25))

        blind = counted_with(shipped_masker, WORD_ENDING_IN_R)
        self.assertEqual(code - blind[1], 20,
                         "the shipped detector hid exactly the twenty lines; if it does "
                         "not, the fixture no longer carries the trap")

    def test_a_real_raw_string_is_still_masked(self):
        self.assertIn("\n# use the pinned toolchain", REAL_RAW_STRING)
        self.assertIn("\n// SAFETY:", REAL_RAW_STRING)
        self.assertIn('grep -o r"needle"', REAL_RAW_STRING)
        self.assertIn('= br#"', REAL_RAW_STRING)
        self.assertEqual([m.group(0) for m in cb.RAW_OPEN.finditer(REAL_RAW_STRING)],
                         ['r#"', 'r"', 'br#"'],
                         "the fixture carries an ordinary raw string, an inner `r\"` for the "
                         "re-entry guard, and a BYTE raw string; a detector that cannot see "
                         "`br#\"` leaves every byte raw string in the tree unmasked")

        self.assertEqual(cb.mask_raw_strings(REAL_RAW_STRING).count("\n"),
                         REAL_RAW_STRING.count("\n"),
                         "masking preserves line structure: without the `m.start() < i` "
                         "re-entry guard the inner `r\"needle\"` is masked a second time and "
                         "the rest of the raw string is re-emitted as source")

        self.assertEqual(cb.classify_text(REAL_RAW_STRING, ".rs"), (1, 4))
        self.assertEqual(counted_with(lambda text: text, REAL_RAW_STRING), (3, 11),
                         "unmasked, the two raw strings' `//` lines score as comments and "
                         "their data lines as code")

    def test_a_string_ending_in_slash_r_is_the_one_false_match_left(self):
        self.assertEqual([m.group(0) for m in cb.RAW_OPEN.finditer(FALSE_MATCH_LEFT)], ['r"'],
                         "the residual the RAW_OPEN comment records: `\"/r\"` still opens a "
                         "phantom raw string, so the reduction was 95 false matches to 1")

        blanked = cb.mask_raw_strings(FALSE_MATCH_LEFT)
        self.assertNotIn("assert_ne!(r.work", blanked,
                         "the phantom span really does run past the end of its line")
        self.assertIn("the roots must differ", blanked,
                      "and stops inside the next one, which is why it costs nothing")

        self.assertEqual(cb.classify_text(FALSE_MATCH_LEFT, ".rs"), (0, 4))
        self.assertEqual(cb.classify_text(FALSE_MATCH_LEFT, ".rs"),
                         counted_with(lambda text: text, FALSE_MATCH_LEFT),
                         "measured cost of the residual: 0 comment and 0 counted lines. If "
                         "this ever differs, the residual is no longer harmless and the "
                         "detector needs real string lexing")

    def test_the_reported_total_does_not_move_when_an_unrelated_line_shifts(self):
        base = cb.classify_text(BOTH_TRAPS, ".rs")
        self.assertEqual(base, (2, 10))

        lines = BOTH_TRAPS.splitlines(keepends=True)
        for i in range(len(lines) + 1):
            shifted = "".join(lines[:i] + ["\n"] + lines[i:])
            self.assertEqual(cb.classify_text(shifted, ".rs"), base, f"blank line at {i + 1}")

        swallowed = "    let mut n = 0;\n"
        self.assertIn(swallowed, BOTH_TRAPS)
        deleted = BOTH_TRAPS.replace(swallowed, "")
        self.assertEqual(cb.classify_text(deleted, ".rs"), (2, 9))
        self.assertNotEqual(counted_with(shipped_masker, BOTH_TRAPS), base,
                            "fixture no longer carries the trap")
        self.assertEqual(counted_with(shipped_masker, BOTH_TRAPS),
                         counted_with(shipped_masker, deleted),
                         "the shipped detector could not see that line at all, which is "
                         "why its total tracked things other than the source")

    def test_a_tree_over_the_comment_ceiling_fails_the_build_and_one_at_it_does_not(self):
        tc = measured_tree()["comments"]
        self.assertGreater(tc, 1, "nothing was counted, so neither exit code means anything")

        at_the_ceiling = run_gate(tc, 100.0)
        self.assertEqual(at_the_ceiling.returncode, 0,
                         f"the ceiling is a maximum, not a minimum\n{at_the_ceiling.stderr}")
        self.assertIn(f"TOTAL {tc} comment lines", at_the_ceiling.stdout)

        one_over = run_gate(tc - 1, 100.0)
        self.assertEqual(one_over.returncode, 1,
                         f"a tree above the ceiling must fail the build\n{one_over.stdout}")
        self.assertIn(f"::error::{tc} comment lines exceeds the ceiling of {tc - 1}",
                      one_over.stderr)

    def test_code_deleted_with_the_comments_kept_fails_the_ratio_the_count_cannot_see(self):
        base_c, base_k = cb.classify_text(BOTH_TRAPS, ".rs")
        thinned = BOTH_TRAPS
        for line in ("    let mut n = 0;\n", "        n += line.len();\n", "    n\n"):
            self.assertIn(line, thinned)
            thinned = thinned.replace(line, "")
        thin_c, thin_k = cb.classify_text(thinned, ".rs")
        self.assertEqual(thin_c, base_c,
                         "this is the class --max-comments is blind to: deleting "
                         "comment-sparse code leaves the comment count exactly where it was")
        self.assertGreater(100 * thin_c / (thin_c + thin_k),
                           100 * base_c / (base_c + base_k),
                           "while the ratio rises, which is what --max-ratio is for")

        ratio = measured_tree()["ratio"]
        self.assertGreater(ratio, 1.0, "nothing was counted, so neither exit code means "
                                       "anything")

        at_the_ceiling = run_gate(10**9, repr(ratio))
        self.assertEqual(at_the_ceiling.returncode, 0,
                         f"the ratio ceiling is a maximum too\n{at_the_ceiling.stderr}")

        one_over = run_gate(10**9, repr(ratio - 0.01))
        self.assertEqual(one_over.returncode, 1,
                         f"a tree above the ratio must fail the build\n{one_over.stdout}")
        self.assertIn("exceeds the ratio ceiling", one_over.stderr)
        self.assertNotIn("exceeds the ceiling of", one_over.stderr,
                         "the ratio must gate on its own; here the count is wide open")


if __name__ == "__main__":
    unittest.main(verbosity=2)
