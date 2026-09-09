# Migrate from mutmut

For projects already running [mutmut](https://github.com/boxed/mutmut)
that want to move to fermut. Covers config, ignore markers, CLI
flags, operator names, and what doesn't translate.

If you're still deciding whether to switch, start with the
[landscape](../concepts/landscape.md) page — it's the honest
side-by-side.

## TL;DR

The one-line migration:

```sh
fermut migrate mutmut
```

This reads `[tool.mutmut]` from `pyproject.toml` (or `[mutmut]` from
`setup.cfg`), writes a starter `fermut.toml`, rewrites every
`# pragma: no mutate` to `# fermut: ignore`, and prints a "manual
review" report for anything it couldn't translate. Add `--dry-run`
to preview, `--pyproject` to write into `pyproject.toml` instead.
See **[`fermut migrate` reference](../reference/cli/migrate.md)**.

The rest of this page explains the mapping so you can read the
output (and the manual-review lines) with context.

## mutmut 1.x/2.x vs 3.x

mutmut 3.0 (released August 2024) rewrote the configuration surface.
The single `fermut migrate mutmut` subcommand recognizes both eras
transparently — it does **not** require you to declare a version, and
it accepts a mixed block if a repo was partially migrated. The
mapping is symmetrical: every renamed knob has both spellings wired
to the same fermut output.

| What it means                | mutmut 1.x/2.x key       | mutmut 3.x key                          | fermut output                |
|------------------------------|--------------------------|-----------------------------------------|------------------------------|
| Source root(s) to mutate     | `paths_to_mutate`        | `source_paths`                          | `source_root`                |
| Test directory               | `tests_dir`              | `pytest_add_cli_args_test_selection`    | `tests`                      |
| Extra pytest flags           | (folded into `runner`)   | `pytest_add_cli_args`                   | `pytest_args`                |
| Restrict to covered lines    | `use_coverage = true`    | `mutate_only_covered_lines = true`      | `coverage = ".coverage"`     |
| Type-check filter            | `--mypy` CLI flag        | `type_check_command = [...]`            | built-in `ty` filter         |

### Shape differences worth knowing

- **mutmut 1.x/2.x accepts a bare string or comma list.** Examples:
  `paths_to_mutate = "src/"`, `paths_to_mutate = "src/, lib/"`. The
  migrator takes the first entry and emits a manual-review note for
  the rest.
- **mutmut 3.x always uses TOML lists.** `source_paths = ["src/"]`
  and `pytest_add_cli_args = ["-x", "-q"]` are the canonical forms.
  A scalar value here is a config error in mutmut itself; the
  migrator treats a non-list `pytest_add_cli_args` as malformed and
  surfaces it under manual review rather than guessing how to split
  it.
- **`pytest_add_cli_args_test_selection` is a *list* of test paths.**
  fermut still expects a single `tests` directory; if the list has
  more than one entry, the extras are surfaced as a note. Either
  pick the most important entry or fold the rest into `pytest_args`.
- **`runner` disappears in 3.x.** mutmut 3 always shells out to
  `pytest` and exposes its CLI via `pytest_add_cli_args*`. The
  migrator emits `runner = "pytest"` by default for any 3.x config.
- **`type_check_command` is configurable in 3.x** (mypy, pyright,
  any process that prints diagnostics). fermut has no slot for an
  external type checker — it embeds `ty` directly — so the key lands
  in manual review with a pointer at `--no-ty-filter`.

If your project was on 1.x/2.x and the team has been gradually
porting keys to 3.x in the same `[tool.mutmut]` block, the migrator
reads both halves and merges them. Keys are processed in sorted
order, so when both spellings of the same knob are set to different
values, the alphabetically later key wins — that is the 3.x spelling
in every renamed pair (`source_paths` > `paths_to_mutate`,
`pytest_add_cli_args_test_selection` > `tests_dir`,
`mutate_only_covered_lines` > `use_coverage`, `type_check_command` >
`mypy`). `pytest_add_cli_args` and `runner` are additive — both
contribute to the final `pytest_args` list. Conflicts are **not**
flagged today, so prefer to delete the stale spelling before running
migrate. (Tracking issue welcome if this hits you in practice.)

## Ignore markers

mutmut and fermut both support inline skip markers, but the spelling
differs:

| mutmut                  | fermut                                       |
|-------------------------|----------------------------------------------|
| `# pragma: no mutate`   | `# fermut: ignore`                           |
| (line-level only)       | `# fermut: ignore[arith-op-swap]` (op-scoped) |
| (no module-level form)  | `# fermut: ignore-file` at top of file       |

fermut's marker is also operator-scopable — `# fermut: ignore[arith-op-swap]`
silences only that operator on the line, where mutmut's pragma is
all-or-nothing. See **[Inline ignore](../reference/operators/inline-ignore.md)**.

A one-liner that covers most repos:

```sh
rg -l '# pragma: no mutate' \
    | xargs sd '# pragma: no mutate' '# fermut: ignore'
```

(Or `sed -i` — your call. Review the diff before committing; mutmut
pragmas occasionally land on lines where the equivalent fermut marker
wants a narrower operator scope.)

## Config

mutmut reads from `setup.cfg` `[mutmut]` or `pyproject.toml`
`[tool.mutmut]`. fermut reads from `fermut.toml` or
`pyproject.toml` `[tool.fermut]`. Run `fermut init` to detect your
layout and write a starter — it does **not** read your mutmut block,
so you don't have to clean up first.

Common-key mapping:

| mutmut key                          | fermut equivalent                                                |
|-------------------------------------|------------------------------------------------------------------|
| `paths_to_mutate` (1.x/2.x)         | `source_root` (or positional path on `fermut run`)               |
| `source_paths` (3.x)                | `source_root`                                                    |
| `tests_dir` (1.x/2.x)               | `tests`                                                          |
| `pytest_add_cli_args_test_selection` (3.x) | `tests` (first path; extras surface as a note)            |
| `runner` (1.x/2.x)                  | `runner` (`"pytest"` / `"unittest"`) + `pytest_args`             |
| `pytest_add_cli_args` (3.x)         | `pytest_args`                                                    |
| `backup`                            | n/a — fermut uses per-worker mirrors (see `isolation`)           |
| `dict_synonyms`                     | n/a — see *What doesn't translate*                                |
| `also_copy`                         | n/a — worker mirror copies the source tree by default            |
| `pre_mutation`                      | n/a — see *What doesn't translate*                                |
| `post_mutation`                     | n/a — see *What doesn't translate*                                |
| `use_coverage` (1.x/2.x) / `--use-coverage` | `coverage = ".coverage"` + `--coverage` (or `coverage.json`) |
| `mutate_only_covered_lines` (3.x)   | `coverage = ".coverage"` (or a manual `coverage.json` export)   |
| `simple_output`                     | `--format json` (machine) or default human                       |
| `--mypy` (1.x/2.x) / `type_check_command` (3.x) | replaced by built-in `ty` filter (on by default)     |

A side-by-side example:

=== "mutmut 1.x/2.x"

    ```toml
    # pyproject.toml
    [tool.mutmut]
    paths_to_mutate = "src/"
    tests_dir = "tests/"
    runner = "python -m pytest -x -q"
    use_coverage = true
    ```

=== "mutmut 3.x"

    ```toml
    # pyproject.toml
    [tool.mutmut]
    source_paths = ["src/"]
    pytest_add_cli_args_test_selection = ["tests/"]
    pytest_add_cli_args = ["-x", "-q"]
    mutate_only_covered_lines = true
    ```

=== "fermut"

    ```toml
    # pyproject.toml
    [tool.fermut]
    source_root = "src"
    tests = "tests"
    runner = "pytest"
    pytest_args = ["-x", "-q"]
    coverage = ".coverage"   # or "coverage.json" for a manual JSON export
    ```

## CLI

| mutmut                              | fermut                                          |
|-------------------------------------|-------------------------------------------------|
| `mutmut run`                        | `fermut run src/ --tests tests/`                |
| `mutmut run --paths-to-mutate src/` | `fermut run src/`                               |
| `mutmut run --use-coverage`         | `fermut run --coverage .coverage` (or a manual `--coverage coverage.json`) |
| `mutmut run --mypy`                 | (default: `ty` pre-filter on; `--no-ty-filter` to disable) |
| `mutmut results`                    | `fermut run --json report.json` then `fermut show report.json` |
| `mutmut show <id>`                  | `fermut show report.json <id>`                  |
| `mutmut html`                       | `fermut run --html out.html`                    |
| `mutmut junitxml`                   | `fermut run --junit out.xml`                    |
| `mutmut stats`                      | `fermut trend` (per-run history, not aggregate) |
| (no equivalent)                     | `fermut run --diff-only main`                   |
| (no equivalent)                     | `fermut run --shard 1/4` (CI matrix parallelism) |
| (no equivalent)                     | `fermut pr-comment` (sticky PR comment)         |

## Operators

mutmut's operators are positional (`mutmut run --operators 1,3,5`).
fermut's are kebab-case names that match what shows up in reports
and inline markers. Mapping for the operators with a direct analogue:

| mutmut concept                  | fermut operator      |
|---------------------------------|----------------------|
| arithmetic operator swap        | `arith-op-swap`      |
| comparison operator swap        | `compare-op-swap`    |
| boundary `< / <=` swap          | `boundary-shift`     |
| boolean `and / or` swap         | `bool-op-swap`       |
| unary operator swap             | `unary-op-swap`      |
| augmented-assignment swap       | `aug-assign-swap`    |
| constant replacement            | `constant-replace`   |
| number increment/decrement      | `number-shift` (direct mutmut analogue: `N` → `N±1`) |
| number → 0                      | `number-to-zero` (fermut-only; not produced by mutmut. Often noisy; consider `--skip-ops number-to-zero` after migration if it dominates survivors) |
| string → empty                  | `string-to-empty`    |
| `return x` → `return None`      | `return-value-to-none` |
| `break` ↔ `continue`            | `break-continue-swap` |
| decorator drop                  | `remove-decorator`   |

The full fermut catalogue (30 stable, more experimental) lives in
**[Operators → Stable](../reference/operators/stable.md)**. Use
`--ops` / `--skip-ops` with names, e.g.
`fermut run --skip-ops string-to-empty,number-to-zero`.

## What doesn't translate

mutmut features without a fermut equivalent today:

- **`pre_mutation` / `post_mutation` hooks.** fermut has no
  per-mutant hook surface. If you used these to reset a fixture or
  poke a sidecar, fold the logic into your test setup or move it to
  a `--pytest-arg` plugin invocation. Open an issue if this blocks
  you — it's been requested before.
- **`dict_synonyms`.** mutmut rewrites variable references
  using a configured alias table. fermut doesn't — names are matched
  syntactically.
- **`mutmut stats` aggregate output.** fermut tracks per-run history
  (score deltas, sparkline) via `fermut trend`, not the
  long-running roll-up mutmut shows. Most teams find the trend view
  enough, but if you depended on stats for offline analysis, the
  per-run JSON reports give you the raw material to roll your own.
- **In-place file edits + backup.** mutmut edits source files and
  restores from a backup. fermut runs each worker against its own
  mirror of the source tree (reflink/clonefile when supported, copy
  otherwise) — so there's nothing to back up and tests can't
  accidentally see a mutated tree from a peer worker. See `isolation`
  in **[Configuration](../reference/configuration.md)**.

## Worked example

A small project currently running mutmut in CI:

```toml
# pyproject.toml — before
[tool.mutmut]
paths_to_mutate = "src/"
tests_dir = "tests/"
runner = "python -m pytest -x -q"
use_coverage = true
```

```yaml
# .github/workflows/mutation.yml — before
- run: pip install mutmut
- run: coverage run -m pytest
- run: mutmut run
- run: mutmut results
```

After:

```toml
# pyproject.toml — after  (drop the [tool.mutmut] block; `fermut migrate
# mutmut --pyproject` writes the [tool.fermut] block for you)
[tool.fermut]
source_root = "src"
tests = "tests"
runner = "pytest"
pytest_args = ["-x", "-q"]
coverage = ".coverage"
```

```yaml
# .github/workflows/fermut.yml — after
- run: uv tool install fermut
# `fermut coverage` writes the .coverage database fermut reads directly.
# Or, manually: pytest --cov=src --cov-context=test && coverage json -o
# coverage.json --show-contexts  (then set coverage = "coverage.json").
- run: fermut coverage
- run: fermut run src/ --tests tests/ \
        --diff-only origin/${{ github.base_ref }} \
        --markdown fermut-report.md
- if: always() && hashFiles('fermut-report.md') != ''
  env:
    GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
  run: fermut pr-comment --markdown fermut-report.md
```

Two things came along for free: `--diff-only` (no mutmut equivalent)
and the sticky PR comment. The full workflow template is in
**[Integrations → GitHub Actions](integrations.md#github-actions)**.

## Where next

- **[Working on projects](projects.md)** — the gradual-rollout
  playbook applies cleanly after migration.
- **[Filters](filters.md)** — `--diff-only`, `--coverage`,
  `--sample`, and the inline ignore markers in one place.
- **[Landscape](../concepts/landscape.md)** — the honest
  feature-by-feature comparison.
