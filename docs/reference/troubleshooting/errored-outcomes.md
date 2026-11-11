# Errored outcomes

**Symptom.** A large fraction of mutants come back as `errored`
(not `survived` or `killed`).

**Most common cause.** Your test suite has side-effects on the
source tree — writes to files inside `src/`, in-process patches
that don't survive the worker mirror, fixtures that mutate global
state and leak across mutants.

## Fix

1. If you set `--isolation hardlink`, drop it: hardlinked mirrors share
   inodes with your tree, so a test that rewrites a file in place leaks
   the change. The default `auto` (reflink, else copy) gives each worker
   an independent tree, and fermut already
   [puts back files a failed test moved, deleted or rewrote](../cli/run-isolation.md#tree-writes)
   after every mutant.
2. Audit fixtures that write to disk inside the project tree. Move
   writes to `tmp_path`.
3. Check for module-level state that persists across `subprocess`
   boundaries.

## See also

- **[Isolation modes](../cli/run-isolation.md#isolation-modes)** — what each
  mode does and when to pick which.
