# `fermut score`

Emit the agent reward signal for the latest run: mutation score, delta vs
a baseline run, and the new-survivor / newly-killed mutant-id sets.

```sh
fermut score [PATH] [flags...]
```

Reads `.fermut/history.jsonl` and compares the most recent entry against an
earlier branch-comparable one. Built for an agent's post-iteration check —
*did this iteration help?* — so JSON is the default format.

| Flag                          | Default                  | Effect                                                          |
|-------------------------------|--------------------------|-----------------------------------------------------------------|
| `PATH`                        | `.`                      | Where to start the project-root walk.                            |
| `--history-path <p>`          | `.fermut/history.jsonl`  | Custom log location.                                             |
| `--baseline <N>`              | `1`                      | Compare against the entry `N` branch-comparable runs back. `1` is the immediately prior run. |
| `--branch <NAME>`             | none                     | Restrict current/baseline selection to this git branch. Pin to `main` in CI where the cache restores main-branch history into a PR build. |
| `--fail-on-regression <PTS>`  | off                      | Exit non-zero when the score dropped more than `PTS` points vs the baseline. Agent rollback / CI gate. |
| `--format json\|human`        | `json`                   | `json` emits the reward signal for machine consumers; `human` prints a short summary. |

## Branch selection

Without `--branch`, the comparable window is anchored on the latest entry's
branch: runs on other branches between the latest run and its predecessor
are skipped, so a feature-branch run can't become the baseline for a `main`
run. Entries with no recorded branch (detached HEAD, pre-git logs) count as
comparable on either side — the same best-effort rule the
`--fail-on-regression` gate on `fermut run` uses.

## JSON shape

```json
{
  "score": 85.0,
  "killed": 9,
  "survived": 1,
  "timed_out": 0,
  "timestamp": "2026-06-14T11:00:00Z",
  "baseline_score": 80.0,
  "baseline_timestamp": "2026-06-14T10:00:00Z",
  "delta": 5.0,
  "new_survivors": [],
  "newly_killed": ["src/a.py@10:x->y"],
  "regressed": false
}
```

- **`delta`** — `score - baseline_score`. Absent from the JSON when there is
  no comparable prior run, e.g. the first recorded run on a branch.
- **`new_survivors`** — mutants surviving now that the baseline killed.
  Regressions. Empty when either entry predates the survivor-id field.
- **`newly_killed`** — mutants the baseline reported as survivors that are
  now killed. The agent's progress this iteration.
- **`regressed`** — `true` when the score dropped more than 0.05 pts (the
  float-noise floor) or a new survivor appeared. The "consider reverting
  this iteration" flag. Always `false` without a baseline.
- **`survivor_ids_compared`** — present and `false` only when either entry
  lacks recorded survivor ids (e.g. a baseline that predates the field).
  Then `new_survivors` / `newly_killed` are empty regardless of reality and
  `regressed` is score-delta-only. Omitted (treat as `true`) otherwise.

`baseline_score`, `baseline_timestamp`, and `delta` are absent from the
JSON entirely when there is no comparable prior run, rather than emitted as
`null`.

`--fail-on-regression <PTS>` exits non-zero only when `regressed` is `true`
**and** the drop exceeds `PTS`, so a non-zero exit always implies
`regressed: true` — the flag and the gate never disagree. A new survivor
with no score drop sets `regressed` but won't trip a points gate (the gate
is magnitude-based).

## Why this exists

Before `score`, an agent derived the reward signal itself — subtract the
last two `history.jsonl` scores, diff the survivor-id sets, apply a
rollback rule. `score` bakes that derivation into the binary so every
scaffold gets the same definition of "did this iteration help?" See the
**[coding agents guide](../../guides/coding-agents.md)** for the inner loop
this feeds.
