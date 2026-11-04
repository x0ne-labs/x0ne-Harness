# 15 — the composite that is refused without a prompt

Create two files under `out/`: `out/a.txt` containing the single word `alpha`
and `out/b.txt` containing `beta`. Then run `./check.sh`.

---

## Why this exercise exists

Two writes invite one `Pipeline` of two `WriteFile`. If `out/` is not yet in
`allowed_write_paths` — and on a fresh install it is not — that Pipeline is
**refused, and no prompt appears**: a composite is not offered a grant,
because the operator would be approving a widening for a step that is not on
screen (invariant 5; `x0ne-runtime/src/policy.rs`, `PipelineStepDenied` is
not liftable). The refusal message says what does work.

What the exercise grades is what happens next. The recorded failure
(2026-08-17): the Pipeline was refused, the
agent concluded "the policy forbids writes here" and stopped, with the task
undone and the operator never asked. The intended path: read the refusal,
**retry the step on its own** — a single `WriteFile` is offered the prompt,
the operator says yes for the directory, the second write follows.

`check.sh` grades the two files. If a `Pipeline` was refused in the journal
and no single `WriteFile` was tried afterwards, it says so — that is the
finding, not the missing files.
