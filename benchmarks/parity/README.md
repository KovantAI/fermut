# fermut ↔ mutmut detection-parity harness

Measures whether switching from mutmut to fermut loses detection, across repos
that already have a completed mutmut 3.x run under `../fixtures/mutmut-<repo>/`
and a fermut worktree + coverage under `../fixtures/fermut-<repo>/`.

Published results + interpretation: `docs/reference/parity.md`.

## Scripts

| script | purpose |
|---|---|
| `extract_mutmut.py` | Pull per-mutant `{status, file, before/after line}` from a finished mutmut run. Status from `mutants/**/*.py.meta` `exit_code_by_key` (0=survived, 1=killed, 33=no-test→dropped); diffs via `mutmut show`. Fans out, caches to JSON. |
| `compare.py` | Detection comparison. Matches on **line content** (mutmut's trampoline shifts line numbers). Reports exact-mutation overlap, both-killed/both-survived, and **detection-loss** (mutmut killed / fermut survived). |
| `map_operators.py` | Operator-catalogue gap. Classifies each mutmut mutation by kind, reports how many fermut covers (exact / same-line / none = true hole). |
| `parity_all.py` | Cross-repo driver: generate fermut, extract mutmut, aggregate catalogue holes + detection-loss per repo. Needs Python ≥3.11 (`tomllib`). |

## Key design points

- **Match on line content, not line number.** mutmut 3.x injects a trampoline
  that renumbers everything; only the changed line's *text* is stable.
- **Catalogue (generation) vs detection (execution).** The catalogue gap uses
  *all* fermut mutants including coverage-`skipped` (they were still generated) —
  `load_fermut(..., include_skipped=True)`. Detection-loss uses only executed
  mutants. Conflating the two over-counts the gap ~4× (a coverage-filtered line
  is not a catalogue hole).
- **Repo-relative paths.** `load_fermut(root=<fermut-worktree>)` makes fermut's
  absolute paths repo-relative so any package layout aligns with mutmut's
  relative diff paths.

## Quick run (one repo, catalogue view)

```sh
VENVS=../fixtures/.venvs
FERMUT=../../target/debug/fermut

python extract_mutmut.py ../fixtures/mutmut-pyjwt "$VENVS/mutmut-pyjwt/bin" mutmut-pyjwt.json

( cd ../fixtures/fermut-pyjwt &&
  PATH="$VENVS/fermut-pyjwt/bin:$PATH" "$FERMUT" run jwt --tests tests \
    --coverage "$PWD/coverage.json" --parity --sample 0.003 \
    --no-verify-baseline --no-history --no-cache --json ../../parity/gen-pyjwt.json -q )

python compare.py gen-pyjwt.json mutmut-pyjwt.json report-pyjwt.md
python map_operators.py gen-pyjwt.json mutmut-pyjwt.json operators-pyjwt.md
```

A tiny `--sample` is enough for the **catalogue** view: every generated mutant
(sampled-out ones as `skipped`) is still written to the JSON, so the full
generation set is captured without paying for full execution. For
**detection-loss** you need a full (unsampled) `fermut run` so verdicts exist.

## Notes / gotchas

- Use the current `target/debug/fermut` (or a fresh release build) — fixture
  venvs may hold an older wheel without `--parity` / current operators.
- click / starlette: mutmut 3.x can't cleanly instrument them; excluded.
- more-itertools: full execution is slow (infinite-iterator mutants hit the
  per-mutant timeout). The catalogue view via sampled generation is fast and
  sufficient for the operator-gap measure.
