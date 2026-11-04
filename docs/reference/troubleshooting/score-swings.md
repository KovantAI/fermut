# Score swings between runs

**Symptom.** Same commit, same source, different scores.

**Cause.** Hypothesis seed isn't pinned, OR the cache was cleared
between runs and something flaky surfaced. A test that depends on set or
dict order (string hash randomization) flips too: under
[schemata](../cli/run.md#schemata) every mutant's fork shares one hash seed
per run, so such a test flips per run rather than per mutant.

## Fix

Set `hypothesis_seed` in `fermut.toml`. Re-run `fermut trend` to
confirm the noise goes away. To rule schemata out, compare with
`--no-schemata`; a verdict that only differs there, run after run, is a bug
worth reporting.
