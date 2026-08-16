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
matches, of which 95 are not raw strings at all.** Examples: `"…a valid parser")` in `cli.rs`,
`"…it names the results dir",` in `cache.rs`, `"model must matter");` in `cache.rs`.

For each false match, `text.find('"', m.end())` then finds some *later, unrelated* quote, and
everything between is blanked. One false match in `cli.rs` blanks **51 lines** of real code.
Blanked lines count as neither comment nor code, so they leave the denominator entirely.

What that does to the gate:

| masker | comment | counted | ratio | `--max 14` |
|---|---|---|---|---|
| **as shipped** | 2291 | 16378 | **13.9931 %** | exit 0 |
| detector requires a non-identifier char before `r` | 2383 | 16698 | **14.2711 %** | **exit 1** |

**The gate passes only because it is mismeasuring.** 320 counted lines and 92 comment lines
are being hidden from it by accident. It passes by 0.007 percentage points, which is about two
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
