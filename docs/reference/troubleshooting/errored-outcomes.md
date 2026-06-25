# Errored outcomes

**Symptom.** A large fraction of mutants come back as `errored`
(not `survived` or `killed`).

**Most common cause.** Your test suite has side-effects on the
source tree — writes to files inside `src/`, in-process patches
that don't survive the worker mirror, fixtures that mutate global
state and leak across mutants.

## Fix

1. Switch isolation mode to `copy` so each worker gets a fully
   independent tree: `--isolation copy` or `isolation = "copy"`.
2. Audit fixtures that write to disk inside the project tree. Move
   writes to `tmp_path`.
3. Check for module-level state that persists across `subprocess`
   boundaries.

## See also

- **[Isolation modes](../cli/run.md#isolation-modes)** — what each
  mode does and when to pick which.
