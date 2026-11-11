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
  migrator only writes a `runner` key when it translates a 1.x/2.x
  `runner` command; a pure 3.x config gets none, and fermut's default
  runner (pytest) applies.
- **`type_check_command` is configurable in 3.x** (mypy, pyright,
  pyrefly or ty, run once over mutmut's `mutants/` tree). fermut has no
  slot for an external type checker — it embeds `ty` and checks each
  mutant on its own, on by default — so the key lands in manual review
  with a pointer at `--no-ty-filter`.

If your project was on 1.x/2.x and the team has been gradually
porting keys to 3.x in the same `[tool.mutmut]` block, the migrator
reads both halves and merges them. Keys are processed in sorted
order, so when both spellings of the same knob are set to different
values, the alphabetically later key wins. That is the 3.x spelling
for `source_paths` (beats `paths_to_mutate`), but the **1.x** spelling
for the tests path: `tests_dir` sorts after
`pytest_add_cli_args_test_selection`, so `tests_dir` wins. The coverage
pair (`use_coverage` / `mutate_only_covered_lines`) isn't a conflict:
either one set to `true` turns coverage on. `mypy` and
`type_check_command` both go to manual review. `pytest_add_cli_args` and `runner` are additive — both
contribute to the final `pytest_args` list. Conflicts are **not**
flagged today, so prefer to delete the stale spelling before running
migrate. (Tracking issue welcome if this hits you in practice.)

## Ignore markers

mutmut and fermut both support inline skip markers, but the spelling
differs:

| mutmut                         | fermut                                        |
|--------------------------------|-----------------------------------------------|
| `# pragma: no mutate`          | `# fermut: ignore`                            |
| `# pragma: no mutate start` / `end` (3.x) | `# fermut: ignore-start` / `ignore-end` |
| `# pragma: no mutate block` (3.x) | `# fermut: ignore-block`                   |
| `do_not_mutate_patterns` (3.x) | `ignore_patterns` (same regexes)              |
| `do_not_mutate` (3.x)          | `exclude` (globs re-rooted under `source_root`) |
| (no operator scope)            | `# fermut: ignore[arith-op-swap]` (op-scoped) |

fermut's marker is also operator-scopable — `# fermut: ignore[arith-op-swap]`
silences only that operator on the line, where mutmut's pragma is
all-or-nothing. See **[Inline ignore](../reference/operators/inline-ignore.md)**.

`fermut migrate mutmut` does all of the above: it rewrites the pragmas
in place (region forms included) and writes `exclude` /
`ignore_patterns` into the generated config. Review the diff before
committing; mutmut pragmas occasionally land on lines where the fermut
marker wants a narrower operator scope.

### Your known-equivalent mutants

mutmut has no "equivalent" status and no list of them: a team marks a
mutant it has judged equivalent with `# pragma: no mutate` on the line,
a `do_not_mutate_patterns` regex, or a `do_not_mutate` file glob.
The migrator carries all three over, so the mutants your team has
already triaged stay suppressed under fermut, with no re-triage.

If you instead kept the list outside mutmut (a spreadsheet of
`mutmut show` ids, say), it doesn't port. A mutmut id
(`module.x_function__mutmut_3`) is a per-function counter. A fermut id
(`file@offset:op:orig->repl`) is a byte offset. Neither one maps onto
the other. Re-check
the survivors from fermut's first run instead. fermut's equivalence
detector already settles many of them (see
[Equivalent mutants](equivalent-mutants.md)), and for the rest, mark
the line with `# fermut: ignore[op]`.

## Config

mutmut reads from `setup.cfg` `[mutmut]` or `pyproject.toml`
`[tool.mutmut]`. fermut reads from `fermut.toml` or
`pyproject.toml` `[tool.fermut]`.

**Use `fermut migrate mutmut`, not `fermut init`.** `migrate` reads your
mutmut block and translates it (the mapping below); `init` ignores it and
detects the layout from scratch, for projects with no config to carry
over. Run one, not both: each refuses to overwrite an existing fermut
config without `--force`. Once `migrate` has written the fermut config,
delete the mutmut block.

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
| `do_not_mutate` (3.x)               | `exclude` (patterns re-rooted under `source_root`)               |
| `do_not_mutate_patterns` (3.x)      | `ignore_patterns`                                                |
| `only_mutate` (3.x)                 | n/a — narrow `source_root` or list the rest in `exclude`         |
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
    source_root = "src/"
    tests = "tests/"
    runner = "pytest"        # 1.x only: translated from `runner`; 3.x emits no runner key
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
| `mutmut results`                    | `fermut results` (reads `.fermut/last.json`, written by every `run`) |
| `mutmut show <id>`                  | `fermut show <id>`                              |
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
| comparison operator swap        | `compare-op-swap` (`==`, `is`, `in`); ordering compares get `boundary-shift`, `ror-equality`, `ror-const` under the default `minimal` [profile](../reference/operators/profiles.md), `compare-op-swap` under `--operators classic` |
| boundary `< / <=` swap          | `boundary-shift`     |
| boolean `and / or` swap         | `bool-op-swap` (an `and` / `or` used as a value); in a condition, `bool-operand-drop` + `bool-const` under `minimal` |
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
source_root = "src/"
tests = "tests/"
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
- run: |
    fermut run src/ --tests tests/ \
      --diff-only origin/${{ github.base_ref }} \
      --markdown fermut-report.md
- if: always() && hashFiles('fermut-report.md') != ''
  env:
    GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
  run: fermut pr-comment --markdown fermut-report.md
```

Two things came along for free: `--diff-only` (no mutmut equivalent)
and the sticky PR comment. The full workflow template is in
**[CI recipes → GitHub Actions](integrations.md#github-actions)**.

### Replacing a mutmut nightly

A nightly `mutmut run` over the whole tree maps to a full fermut sweep.
Three things decide how long it takes:

- **Keep the verdict cache between nights.** mutmut keeps its results in
  `mutants/`; fermut keeps them in `.fermut/cache.json`. CI runners start
  empty, so persist it, or every night runs cold. The packaged
  [`sweep` + `trend` actions](github-action.md#sharded-full-sweep) cache
  verdicts, coverage and history for you; for a hand-written job, see the
  [nightly recipe](integrations.md#pr-gate-nightly).
- **Shard it.** `--shard i/n` splits mutants across a CI matrix with no
  coordination, and `fermut merge` combines the reports and refuses a
  missing shard. mutmut has no equivalent.
- **Start with the default operators.** With `--parity` (the opt-in
  `parity:*` operators on top of the defaults) fermut covers ~99% of the
  lines mutmut mutates ([parity](../reference/parity.md)); the defaults
  alone cover somewhat less. `--experimental` adds operators mutmut
  doesn't have, so it's more work, not a like-for-like replacement. Add
  it once the default run is green.

Scores aren't comparable across tools (different operators, and fermut
skips uncovered lines instead of counting them as survivors), so reset
your score thresholds from fermut's first full run rather than carrying
mutmut's over.

## Where next

- **[Rolling out to a team](projects.md)** — the gradual-rollout
  playbook applies cleanly after migration.
- **[Filters](filters.md)** — `--diff-only`, `--coverage`,
  `--sample`, and the inline ignore markers in one place.
- **[Landscape](../concepts/landscape.md)** — the honest
  feature-by-feature comparison.
