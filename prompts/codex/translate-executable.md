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

## Self-verification protocol (follow exactly)
1. You work in ONE session. There is no Task tool and there are no sub-agents to
   delegate to, so do the work yourself in this turn rather than describing what a
   helper should do.
2. After EVERY step that is supposed to produce a file, INDEPENDENTLY verify the
   actual output with your own shell commands (ls, wc -l, grep -c). NEVER report
   success from your own narration alone.
3. If verification shows missing or incomplete output, finish it now. If a file is
   too large to handle in one pass, split it into smaller function-range chunks and
   work through them one at a time, verifying each on disk as you go.
4. Your turn is NOT complete until every required artifact exists and has passed
   your own verification. Do not end your turn with unverified or pending work.
