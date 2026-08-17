# PR 13 — The comment budget is green on a false measurement

## The finding, measured

`tools/comment_budget.py` is a required CI gate (`.github/workflows/type-safety.yaml`,
"Comment budget", `--max 14`). It masks raw strings before counting, because in this repo they
hold shell scripts whose `#` lines would score as comments. The detector is:

```python
for m in re.finditer(r'r(#*)"', text):
    end = text.find('"' + m.group(1), m.end())
```

`r(#*)"` matches **any `r` immediately followed by a quote** — including the final `r` of an
ordinary word at the end of an ordinary string. Measured over `tools/src/**.rs`: **149
matches, of which 95 are not raw strings at all.** Examples: `"/usr"` in `oracle/mod.rs`,
`"…it names the results dir",` in `cache.rs`, `"model must matter");` in `cache.rs`.

For each false match, `text.find('"', m.end())` then finds some *later, unrelated* quote, and
everything between is blanked. The worst one in the *counted* tree is `oracle/mod.rs:18`, the
`r"` inside `"/usr"`, whose span runs to the next quote 42 lines later and blanks **38 counted
lines** (17 comment, 21 code). Blanked lines count as neither comment nor code, so they leave
the denominator entirely. (An earlier draft of this spec cited "51 lines of `cli.rs` at one
match": `cli.rs` is in `SKIP_FILES`, so nothing it hides ever reaches the budget.)

**As landed, the reduction is 95 false matches → 1, not → 0.** The lookbehind excludes only
`[0-9A-Za-z_\\]`, so a string whose last character is `r` preceded by anything else still
opens a phantom raw string: `let r = roots("/w", "/r");` at `cache.rs:1053` is the one such
site in the tree today. Its span dies inside the following line, so its measured cost is 0
comment and 0 counted lines — no number above moves — and
`a_string_ending_in_slash_r_is_the_one_false_match_left` pins that rather than leaving it a
surprise. Eliminating the class needs real Rust string lexing, which is not worth it for one
harmless match. The fix also has to accept the optional `b` of a byte raw string, with the
lookbehind applied before it: `b` is inside the excluded class, so `br"…"` and `br#"…"#` went
from masked to never masked, the mirror image of the bug. Measured `br(#*)"` sites in the
counted tree: **0**, so no number moves for that either.

What that does to the gate:

| masker | comment | counted | ratio | `--max 14` |
|---|---|---|---|---|
| **as shipped** | 2291 | 16378 | **13.9883 %** | exit 0 |
| detector requires a non-identifier char before `r` | 2383 | 16698 | **14.2711 %** | **exit 1** |

**The gate passes only because it is mismeasuring.** 320 counted lines and 92 comment lines
are being hidden from it by accident. It passes by 0.012 percentage points, which is about two
comment lines of headroom, so any change to code that happens to contain a word ending in `r`
before a quote moves the total and can flip the gate for reasons unrelated to comments. That
flakiness is already observed: shortening one `ensure!` message by a line moved the measured
tree by ~10 lines.

A second, smaller defect in the same loop: `text.replace(span, …, 1)` replaces the **first
textual occurrence** of that span anywhere in the file rather than the span at `m.start()`.
Harmless today only because the replacement preserves length and duplicate spans mask each
other by coincidence. Replace by slice, not by search.

## Why this is an escalation, not a routine fix

Correcting the detector turns a required gate red. Per `CLAUDE.md` that is exactly the third
move — "it is genuinely miscalibrated, which is an escalation and not your decision" — so this
PR exists to make the decision deliberately, in one place, with the numbers above recorded
beside it. **Do not fix the detector and quietly raise the number in the same breath without
saying so**; and do not leave the detector broken to keep the gate green.

There is precedent for how this is handled here: PR 0 raised the flag 13 → 14 with the
reasoning recorded next to it in `type-safety.yaml`, and that was an operator call, not the
implementing agent's.

## The operator decision, already made — do not escalate this one

**Replace the ratio with an absolute comment-line ceiling.** This is the choice; implement it.
It was recommended by a PR 0 reviewer, is recorded twice in `docs/architecture-plan.md` as
unlanded work, and the alternative — leaving a required gate mismeasuring by 320 lines — is
worse than any threshold argument.

Set the ceiling from the **corrected** measurement with stated headroom, and record beside the
flag in `type-safety.yaml`: the old metric, the old and new measured totals, and why an
absolute ceiling is the right shape. Do not adjust comments anywhere in the tree to make a
number work.

**As landed, the absolute ceiling is primary and a loose ratio is kept as a backstop**
(`--max-comments 3100 --max-ratio 20`). Deleting the ratio outright leaves one class ungated,
and it is the class only the ratio can see: delete thousands of lines of comment-sparse code,
keep every comment, and the absolute count does not move — exit 0 — while density climbs. At
20% the backstop carries 5.6 points of headroom over the measured 14.42%: it fires only once
the counted tree shrinks by 4,665 lines (28%) with not one comment removed, so it cannot
recreate the 0.01-point fragility `--max 14` had. What each flag catches that the other cannot
is recorded beside both in `type-safety.yaml`.

## Why an absolute ceiling

`docs/architecture-plan.md` already carries a reviewer's recommendation that has never landed,
and the same reasoning applies with more force now:

> an absolute comment-line ceiling cannot be tripped by a deletion; a ratio can

A whole-tree *ratio* is the wrong metric for a refactor made of deletions. Removing
comment-sparse code **raises** the ratio, so every deletion PR in this sequence fights the
gate, and PR 0 hit exactly that. What the policy actually wants to prevent is comment sprawl —
an absolute ceiling on comment lines says that directly, cannot be tripped by deleting code,
and needs no headroom argument.

So this PR should:

1. **Fix the detector** — require a non-identifier character before the `r`, and replace the
   span by slice rather than by search. Report the corrected totals.
2. **Replace the ratio with an absolute comment-line ceiling**, set from the corrected
   measurement with stated headroom, and record in `type-safety.yaml` beside the flag both why
   the metric changed and what the old ratio was.
3. Keep the per-file report, which is genuinely useful.

If the operator prefers to keep the ratio, then the threshold must be re-derived from the
corrected measurement and the reason recorded — but say which was chosen and why.

## Required tests

The script has no tests, which is why a detector this wrong survived in a required gate.

1. **`a_word_ending_in_r_before_a_quote_is_not_a_raw_string`** — a fixture containing
   `assert!(x, "a valid parser");` followed by twenty ordinary code lines must count all
   twenty. Show it failing against the current detector.
2. **`a_real_raw_string_is_still_masked`** — a fixture with `r#"…#…"#` holding `#` lines must
   not score them as comments. This is the behaviour the masker exists for; the fix must not
   trade one error for the other.
3. **`the_reported_total_does_not_move_when_an_unrelated_line_shifts`** — the observed flake,
   pinned: insert a blank line in one file and assert the totals are identical.

Three more, added because all of the above call `classify_text` only — nothing reached the
comparisons and the exit code, which are the part that actually gates:

4. **`a_tree_over_the_comment_ceiling_fails_the_build_and_one_at_it_does_not`** — runs the
   script as a subprocess against the real tree at the measured total (exit 0) and one below
   it (exit 1, naming the count). Inverting, `>=`-ing or removing the comparison, or dropping
   the `return 1`, all turn it red.
5. **`code_deleted_with_the_comments_kept_fails_the_ratio_the_count_cannot_see`** — one test
   per limit: the class the absolute count is blind to, then the ratio's own exit code at the
   measured ratio and 0.01 below it, with `--max-comments` wide open so only the ratio can
   fire.
6. **`a_string_ending_in_slash_r_is_the_one_false_match_left`** — the known residual recorded
   as behaviour, asserting its cost is 0 comment and 0 counted lines.

`a_real_raw_string_is_still_masked` also carries `br#"…"#` and an inner `r"` inside the raw
string, so dropping the `b?` or the `m.start() < i` re-entry guard turns it red.

Named after the failure, per `CLAUDE.md`.

## Constraints

- Python only. No Rust change, no dependency, no new file outside `tools/`.
- The gate must still be a gate: it exits non-zero when the tree exceeds the limit. Prove it
  by planting a violation and showing red.
- Do not touch any other gate, and do not adjust comments across the tree to make a number
  work — that is gaming the measurement rather than fixing it.
- Never write to `/tmp`; scratch under `/local/home/scheschb/scratch/<yours>`.
- Answer: **after my change, what input still makes this check fail?** Name it.

## Acceptance criteria

The ten gates (see `docs/HANDOFF.md`), plus:

- the corrected totals reported for the whole tree, both metrics;
- the chosen limit stated with its headroom and its rationale in `type-safety.yaml`;
- all three tests passing, with test 1 shown failing against the old detector.

## Commit message

That the gate was green on a false measurement, with both measured totals side by side and the
count of false matches; the two detector defects; which metric was chosen and why an absolute
ceiling cannot be tripped by a deletion; and the three tests with the evidence each can fail.
