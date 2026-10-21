# Jupyter notebooks

Mutation-testing logic that lives in a `.ipynb` notebook — without
fermut needing to understand notebooks at all.

## TL;DR

fermut does **not** mutate `.ipynb` files directly. Pair the notebook
to a plain `.py` with [jupytext](https://jupytext.readthedocs.io/) — a
tool that keeps a `.py` mirror of a notebook in sync — then test the
`.py` and point fermut at the `.py`. Everything — coverage selection,
operators, caching — works unchanged.

```sh
jupytext --set-formats ipynb,py:percent analysis.ipynb   # pair once
fermut coverage --source analysis --tests tests/          # writes .coverage
fermut run analysis.py --tests tests/ --coverage .coverage
```

## Why not mutate `.ipynb` directly

It isn't a fermut limitation — it's upstream in **coverage.py**.
Coverage decides a file's measurable lines by parsing the on-disk file
as Python. A `.ipynb` on disk is JSON, so coverage finds one bogus
statement and discards every line your tests actually executed:

```
Name             Stmts   Miss  Cover
------------------------------------
analysis.ipynb       1      0   100%   ← wrong; the real code is invisible
```

No per-line data means no per-test contexts, and fermut's
[coverage filter](coverage.md) — the thing that makes mutation runs
fast enough for a PR loop — has nothing to select on. Notebook
importers (`import_ipynb`, custom loaders) don't help: coverage
re-parses the JSON file at report time regardless of how the cells
were compiled.

So the working path is to give coverage a real `.py` file.

## Pairing with jupytext

`jupytext` keeps a `.py` mirror of your notebook in sync. The
`py:percent` format round-trips cleanly — cells become `# %%`
blocks, edits in either file propagate.

```sh
pip install jupytext

# Pair the notebook to a percent-format .py (writes the metadata link)
jupytext --set-formats ipynb,py:percent analysis.ipynb

# Thereafter, regenerate the .py whenever the notebook changes:
jupytext --sync analysis.ipynb
```

This produces `analysis.py`. Functions defined in notebook code cells
become ordinary top-level functions you can import.

## Worked example

**`analysis.ipynb`** — two code cells:

```python
# %% cell 1
def add(a, b):
    total = a + b
    if total > 0:
        return total
    return 0

# %% cell 2
def scale(x):
    return x * 2
```

**`tests/analysis_test.py`** — import the paired `.py`, not the notebook:

```python
import analysis

def test_add_positive():
    assert analysis.add(2, 3) == 5

def test_add_zero():
    assert analysis.add(-1, -1) == 0

def test_scale():
    assert analysis.scale(4) == 8
```

**Run it:**

```sh
jupytext --sync analysis.ipynb                       # analysis.ipynb -> analysis.py
fermut coverage --source analysis --tests tests/     # writes .coverage
fermut run analysis.py --tests tests/ --coverage .coverage
```

`fermut coverage` runs the suite and writes a `.coverage` database
fermut reads directly (no `coverage json` export step). Or, manually:

```sh
pytest --cov=analysis --cov-context=test
coverage json -o coverage.json --show-contexts
fermut run analysis.py --tests tests/ --coverage coverage.json
```

Either way the coverage data carries the per-test contexts fermut
needs — each line tagged with the pytest nodeID that hit it (a
`coverage.json` export shows them as):

```json
"contexts": {
  "3": ["tests/analysis_test.py::test_add_positive|run",
        "tests/analysis_test.py::test_add_zero|run"],
  "5": ["tests/analysis_test.py::test_add_positive|run"]
}
```

fermut mutates `analysis.py` and selects only the touching tests per
mutant, exactly as it would for any hand-written module.

## CI

Regenerate the `.py` and `.coverage` at the start of the run so
neither goes stale against the notebook:

```sh
jupytext --sync analysis.ipynb && \
  fermut coverage --source analysis --tests tests/ && \
  fermut run analysis.py --tests tests/ --coverage .coverage
```

(Or the manual `pytest --cov … && coverage json …` export, passed as
`--coverage coverage.json`.)

Commit the paired `.py` (or regenerate in CI) so the mutation target
is always present.

## Pitfalls

- **Pointing fermut at the `.ipynb`.** It won't be mutated usefully —
  always target the paired `.py`.
- **Stale `.py`.** Edited the notebook but didn't `jupytext --sync`?
  fermut mutates the old code. Sync before every run.
- **Notebook *output* in tests.** This workflow tests the **functions
  defined** in cells, not cell execution / outputs. For asserting on
  cell outputs end-to-end (e.g. `nbval`), mutation testing doesn't
  apply — there's no importable unit to mutate.

## Where next

- **[Coverage](coverage.md)** — how per-test selection works and why
  it's required.
- **[Filters](filters.md)** — narrow mutants with `--diff-only`,
  `--ops`, inline ignores.
