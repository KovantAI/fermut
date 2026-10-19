# `fermut hom`

!!! warning "Experimental"
    A research subcommand. Flags and output format may change; it warns on
    every run.

Look for **strongly-subsuming second-order mutants (SSHOMs)**. An SSHOM
combines two killed first-order mutants `f1` and `f2`. It is killed only by
tests that kill both of them: `K(h) ≠ ∅` and `K(h) ⊆ K(f1) ∩ K(f2)`.

```sh
fermut run src/pkg --no-cache --record-kill-sets kill-sets.jsonl
fermut hom src/pkg --kill-sets kill-sets.jsonl --coverage .coverage
```

`hom` takes every flag `run` accepts for building the run config (`--tests`,
`--jobs`, `--timeout`, `--runner`, `--python`, `--coverage`, the operator and
filter flags, and the rest). The flags below are specific to `hom`:

| Flag                 | Effect                                                                                     |
|----------------------|--------------------------------------------------------------------------------------------|
| `[PATH]`             | Source root. Use the same one as the `--record-kill-sets` run. Default `.`.                |
| `--kill-sets <PATH>` | JSONL from `fermut run --record-kill-sets` (required).                                      |
| `--out <PATH>`       | Where to write the per-pair results as JSONL. Default `hom-results.jsonl`.                  |
| `--max-pairs <N>`    | Maximum number of candidate pairs to run, highest kill overlap first. Default `2000`. The summary reports how many pairs were dropped. |
| `--cross-function`   | Also pair mutants from different functions. Off by default: these pairs are mostly noise.  |

## Requirements

- Per-test coverage. `hom` uses it to pick which tests to run for each pair,
  and refuses to start without it.
- The pytest or rstest runner. `unittest` is refused.

## What it does

1. Loads the killed mutants from the kill-sets file. Survivors are dropped.
2. Pairs mutants that are in the same file (and, by default, the same
   function), have non-overlapping ranges, and share at least one killing test
   (`K(f1) ∩ K(f2) ≠ ∅`). An SSHOM can't exist without a shared killing test,
   so dropping the other pairs loses nothing.
3. Applies both edits, then runs the tests that cover either mutant, without
   `-x`.
4. Classifies each pair as `sshom`, `decoupled` (`K(h) = ∅`) or
   `non-subsuming`.

Each line of `--out` is one pair: `f1_id`, `f2_id`, `file`, `line1`, `line2`,
`class`, `k_f1`, `k_f2`, `intersection`. The stdout summary gives:

- the number of candidate pairs and how many ran;
- SSHOM density, as a percentage of the pairs that ran;
- the number of mutants that appear in at least one SSHOM. This is a loose
  upper bound on how many mutants could be removed.

See also [`subsume`](subsume.md) for first-order dominator pruning, which
needs no extra test runs.
