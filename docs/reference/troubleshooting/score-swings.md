# Score swings between runs

**Symptom.** Same commit, same source, different scores.

**Cause.** Hypothesis seed isn't pinned, OR the cache was cleared
between runs and something flaky surfaced. Set and dict order (string hash
randomization) isn't a cause: fermut pins `PYTHONHASHSEED` for every process
it starts, so each run iterates in the same order. If you export
`PYTHONHASHSEED` yourself, fermut uses yours (`random` brings the swings
back). Tests that only pass in one iteration order are for your test tool to
find, not mutation testing.

## Fix

Set `hypothesis_seed` in `fermut.toml`. Re-run `fermut trend` to
confirm the noise goes away. To rule schemata out, compare with
`--no-schemata`; a verdict that only differs there, run after run, is a bug
worth reporting.
