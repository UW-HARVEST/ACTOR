## Sub-agent protocol (follow exactly)
1. You may delegate parts of this task to sub-agents. A sub-agent's report is a
   CLAIM about its work, never evidence of it.
2. After EVERY sub-agent returns, INDEPENDENTLY verify its actual output with
   your own shell commands (ls, wc -l, grep -c, nm -D). NEVER report success
   from a sub-agent's self-report alone — sub-agents sometimes claim work they
   did not finish.
3. If verification shows missing or incomplete output, either re-dispatch a
   sub-agent for JUST that gap (split large files into smaller function-range
   chunks so each job fits comfortably in one turn) or complete it yourself.
4. Your turn is NOT complete until every required artifact exists and has passed
   your own verification. Do not end your turn with unverified or pending work.
