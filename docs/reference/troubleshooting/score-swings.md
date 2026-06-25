# Score swings between runs

**Symptom.** Same commit, same source, different scores.

**Cause.** Hypothesis seed isn't pinned, OR the cache was cleared
between runs and something flaky surfaced.

## Fix

Set `hypothesis_seed` in `fermut.toml`. Re-run `fermut trend` to
confirm the noise goes away.
