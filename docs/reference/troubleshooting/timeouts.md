# Timeouts

**Symptom.** Many mutants come back `timed_out`.

**Cause.** A mutant introduced an infinite loop (e.g. swapping `<`
to `<=` on a loop condition), or your test fixture is slow.

## Fix

- If timeouts cluster on **specific** mutants (loop conditions in
  particular files), accept it — counting them as killed is the
  right call.
- If timeouts cluster on **specific tests**, raise whichever limit is
  lower (see below), or skip those tests with `--pytest-arg='-k not slow'`.
- If timeouts are uniform across the run, raise the lower of the two
  limits. Each mutant's limit is `min(--timeout, factor × baseline + 5 s)`
  (`--timeout` default `30`, `--timeout-factor` default `3`): raise
  `--timeout-factor` when the budget is lower, `--timeout` when the flat
  cap is lower (baselines over ~8 s at the defaults). Raising only the
  other one changes nothing. `--timeout-factor 0` drops the budget and
  leaves `--timeout` alone.
- The [timeout budget](../cli/run.md#timeout-budgets) is on by default
  (`--timeout-factor 3`): it sizes each mutant's timeout from the baseline
  run, so hung mutants stop in seconds. With
  [schemata](../cli/run-schemata.md), a timeout is also confirmed in a
  fork with no mutant active before it counts. If slow-but-correct mutants start timing out on a
  busy machine, raise the factor or lower `--jobs`; `--timeout-factor 0`
  turns the budget off (flat `--timeout`).
