<!-- markdownlint-disable MD041 -->
Translate the C code in c_src/ to Rust that produces **byte-identical output** for the same inputs.
Write Cargo.toml and src/ files in the current directory (NOT in c_src/).

This is an EXECUTABLE. Requirements:
- Do NOT fix bugs in the original C code — if the C has incorrect behavior, reproduce it exactly
- Preserve the exact order of error checks and validation
- Match C's stdin reading behavior exactly (scanf reads across newlines, fgets does not)
- Match C's exact printf format output including spacing and newlines
- Use safe Rust internally where possible

Run 'cargo build --release' and fix any errors until it compiles.
Do NOT modify anything in c_src/.

## Sub-agent protocol (follow exactly)
1. The Task tool is SYNCHRONOUS. A Task call returns ONLY when the sub-agent has
   FINISHED, and the sub-agent's final report IS the call's return value. There
   are NO asynchronous "completion notifications." NEVER say you are "waiting
   for", "pausing for", or "will be notified by" a sub-agent — the instant the
   Task call returns, its work is already done and its result is in your hands.
2. After EVERY sub-agent returns, INDEPENDENTLY verify its actual output with
   your own Bash/Read commands (ls, wc -l, grep -c). NEVER report success from a
   sub-agent's self-report alone — sub-agents sometimes claim work they did not
   finish.
3. If verification shows missing/incomplete output, either re-dispatch a sub-agent
   for JUST that gap (split large files into smaller function-range chunks so each
   sub-agent's job fits comfortably in one turn) or complete it yourself.
4. Your turn is NOT complete until every required artifact exists and has passed
   your own verification. Do not end your turn with unverified or pending work.
5. Prefer synchronous, one-at-a-time delegation you can verify over "fire many
   and wait." If you spawn several Task calls, remember each already returned its
   result by the time you read this — go verify each on disk now.
