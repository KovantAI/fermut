# Timeouts

**Symptom.** Many mutants come back `timed_out`.

**Cause.** A mutant introduced an infinite loop (e.g. swapping `<`
to `<=` on a loop condition), or your test fixture is slow.

## Fix

- If timeouts cluster on **specific** mutants (loop conditions in
  particular files), accept it — counting them as killed is the
  right call.
- If timeouts cluster on **specific tests**, raise `--timeout`
  globally, or skip those tests with `--pytest-arg='-k not slow'`.
- If timeouts are uniform across the run, raise `--timeout`
  outright (default `30` → `60`).
- Use a [timeout budget](../cli/run.md#timeout-budgets)
  (`--timeout-factor 3`): it sizes each mutant's timeout from the baseline
  run, so hung mutants stop in seconds. It's on by default with
  [schemata](../cli/run-schemata.md), where a timeout is re-run the normal
  way before it counts. If slow-but-correct mutants start timing out on a
  busy machine, raise the factor or lower `--jobs`.
