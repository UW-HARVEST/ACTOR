# CLAUDE.md

## Code comments

Default to no comments.

Only add a comment when it explains WHY something non-obvious is necessary:

- a hidden constraint
- a subtle invariant
- a workaround for a specific bug
- surprising behavior

Never:

- restate what the code does
- narrate implementation steps
- explain obvious control flow
- copy information from the conversation into comments
- add comments that are redundant with names/types

Before finishing, remove any comment whose meaning is obvious from the code
directly below it.
