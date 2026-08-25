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
