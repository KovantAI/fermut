# Migrate from cosmic-ray

For projects already running [cosmic-ray](https://github.com/sixty-north/cosmic-ray)
that want to move to fermut. Covers config, session model, CLI flags,
operator names, and what doesn't translate.

cosmic-ray and fermut take different shapes — cosmic-ray is built
around a long-lived session database and (optionally) a celery
worker pool, fermut around a single short run with a content-hashed
cache. Most of the migration work is *unlearning* sessions, not
translating flags. Read **[landscape](../concepts/landscape.md)**
first if you haven't.

## TL;DR

The one-line migration:

```sh
fermut migrate cosmic-ray
```

This reads `cosmic-ray.toml`, writes a starter `fermut.toml`, and
prints a "manual review" report for anything it couldn't translate
(celery, interceptors, …). `excluded-modules` is auto-translated to
the `exclude` key; patterns outside `source_root` surface as a note.
Add `--dry-run` to preview, `--pyproject` to write into
`pyproject.toml` instead. See
**[`fermut migrate` reference](../reference/cli/migrate.md)**.

The rest of this page explains the mapping so you can read the
output (and the manual-review lines) with context.

## The session model

cosmic-ray's flow is two-phase:

```text
cosmic-ray init  config.toml session.sqlite   # enumerate mutants
cosmic-ray exec  config.toml session.sqlite   # execute
cosmic-ray dump  session.sqlite               # extract results
```

fermut collapses this:

```text
fermut run src/ --tests tests/ --json report.json
```

There is no persistent session. State that cosmic-ray puts in the
sqlite DB lives in two places for fermut:

- **The JSON report** (`--json`) — one file per run, content of one
  session's results.
- **The result cache** (`.fermut/cache.json`) — keyed by
  `(mutant.id, ast_hash(file), scope)`. Identical mutants across
  runs are skipped automatically; you don't have to "resume" a
  session. AST-hash means reformat passes don't invalidate.

Practical consequence: if your cosmic-ray workflow involved resuming
a partial session after a crash, the fermut analogue is "rerun the
same command — the cache absorbs the already-tested mutants."

## Config

cosmic-ray reads from a TOML file (commonly `cosmic-ray.toml`)
passed positionally to every subcommand. fermut reads `fermut.toml`
or `pyproject.toml` `[tool.fermut]`.

Common-key mapping:

| cosmic-ray key                            | fermut equivalent                                  |
|-------------------------------------------|----------------------------------------------------|
| `module-path`                             | `source_root` (or positional path on `fermut run`) |
| `python-version`                          | n/a — fermut autodetects via active interpreter    |
| `timeout` (float seconds)                 | `timeout = N` (rounded up to whole seconds)        |
| `excluded-modules`                        | `exclude = ["pattern/**", …]` — glob patterns relative to `source_root`. `fermut migrate cosmic-ray` auto-translates: the `module-path` prefix is stripped from each pattern. Patterns that don't sit under `source_root` are kept verbatim with a note. Repeatable on CLI as `--exclude`. |
| `test-command`                            | `runner` + `pytest_args`                           |
| `[cosmic-ray.distributor]` (`local` / `http`) | n/a — use `--shard` in CI matrix               |
| `[cosmic-ray.execution-engine] name = "celery4"` | n/a — see *What doesn't translate*          |
| `[cosmic-ray.cloning] method = "copy"`    | `isolation = "copy"` (or `auto` / `reflink` / `hardlink`) |
| `[cosmic-ray.interceptors] enabled = [...]` | n/a — see *What doesn't translate*               |
| `[cosmic-ray.badge]`                       | n/a — use `fermut trend` + the Markdown report    |

Side-by-side:

=== "cosmic-ray"

    ```toml
    # cosmic-ray.toml
    [cosmic-ray]
    module-path = "src/"
    timeout = 60.0
    excluded-modules = ["src/migrations/*"]
    test-command = "pytest -x -q"

    [cosmic-ray.distributor]
    name = "local"

    [cosmic-ray.cloning]
    method = "copy"
    ```

=== "fermut"

    ```toml
    # pyproject.toml
    [tool.fermut]
    source_root = "src"
    timeout = 60
    runner = "pytest"
    pytest_args = ["-x", "-q"]
    isolation = "copy"
    exclude = ["migrations/**"]
    ```

## CLI

| cosmic-ray                                | fermut                                              |
|-------------------------------------------|-----------------------------------------------------|
| `cosmic-ray init cfg.toml session.sqlite` | n/a — no init phase                                 |
| `cosmic-ray exec cfg.toml session.sqlite` | `fermut run src/ --tests tests/`                    |
| `cosmic-ray dump session.sqlite`          | `fermut run --json report.json`                     |
| `cosmic-ray report session.sqlite`        | `fermut show report.json`                           |
| `cr-html session.sqlite > out.html`       | `fermut run --html out.html`                        |
| `cr-rate session.sqlite`                  | survivors count + score is in the JSON report; gate via process exit (`fermut run` exits 1 on survivors) |
| `cosmic-ray --verbose exec …`             | `fermut -v run …` (`-vv` for trace)                 |
| `cosmic-ray exec --keep-going`            | n/a — fermut continues by default; non-zero exit is the gate |
| (no equivalent)                           | `fermut run --diff-only main`                       |
| (no equivalent)                           | `fermut run --shard 1/4`                            |
| (no equivalent)                           | `fermut pr-comment --markdown report.md`            |

## Operators

cosmic-ray names operators with a path-like form
(`core/ReplaceBinaryOperator_Add_Sub`, `core/ReplaceComparisonOperator_Lt_Gt`).
fermut uses kebab-case names that match what reports and inline
markers print. Rough mapping for the operators with a direct analogue:

| cosmic-ray operator family                              | fermut operator        |
|---------------------------------------------------------|------------------------|
| `core/ReplaceBinaryOperator_*`                          | `arith-op-swap`        |
| `core/ReplaceComparisonOperator_*`                      | `compare-op-swap` + `boundary-shift` |
| `core/ReplaceBooleanOperator_*`                         | `bool-op-swap`         |
| `core/ReplaceUnaryOperator_*`                           | `unary-op-swap`        |
| `core/ReplaceTrueWithFalse` / `ReplaceFalseWithTrue`    | `constant-replace`     |
| `core/NumberReplacer`                                   | `number-shift` + `number-to-zero` |
| `core/ReplaceBreakWithContinue` / inverse               | `break-continue-swap`  |
| `core/RemoveDecorator`                                  | `remove-decorator`     |
| `core/ReplaceMethodCallWithNone` (partial)              | `return-value-to-none` |

cosmic-ray's "interceptors" (e.g. `spor`, exception-class filters)
don't have direct fermut equivalents. Pre-filtering in fermut is the
`ty` type filter (on by default) and `--coverage` test-narrowing.

Full fermut catalogue: **[Operators → Stable](../reference/operators/stable.md)**.
Use `--ops` / `--skip-ops` with names, e.g.
`fermut run --skip-ops number-to-zero,string-to-empty`.

## What doesn't translate

cosmic-ray features without a fermut equivalent today:

- **Celery / SQL worker pool.** cosmic-ray's `celery4` execution
  engine and the http-distributor pattern have no analogue. The
  fermut equivalent for "spread the run across N machines" is
  `--shard i/n` in a CI matrix — zero-coordination, deterministic
  per `mutant.id`. See
  **[Integrations → Sharded full sweep](integrations.md#sharded-full-sweep)**.
- **Persistent session DB.** Resumability comes from the result
  cache, not from a session. You can't "open" an old session and
  re-query it — you keep the JSON reports.
- **`spor` / interceptor framework.** cosmic-ray exposes a Python
  hook surface for filtering mutants at enumeration time. fermut's
  pre-filters (`ty`, `--coverage`, `--ops` / `--skip-ops`, inline
  ignores) cover most of the common uses but don't accept arbitrary
  Python callbacks. If you used `spor` to mark equivalent mutants,
  the closest fermut surface is the inline `# fermut: ignore` marker.
- **Badges / `cr-rate`.** No standalone badge generator. The
  Markdown report (`--markdown`) embeds the score and the trend
  block (`--trend`) is what most teams pin in their README.
- **Multiple test-command variants.** cosmic-ray supports calling
  any shell command as the test runner. fermut supports `pytest`
  and `unittest` (extensible at the trait level in Rust) but not an
  arbitrary shell command. Most cosmic-ray `test-command` values
  decompose into `pytest_args`.

## Worked example

A project running cosmic-ray with the local distributor:

```toml
# cosmic-ray.toml — before
[cosmic-ray]
module-path = "src/"
timeout = 60.0
test-command = "pytest -x -q"

[cosmic-ray.distributor]
name = "local"

[cosmic-ray.cloning]
method = "copy"
```

```sh
# before
cosmic-ray init cosmic-ray.toml session.sqlite
cosmic-ray exec cosmic-ray.toml session.sqlite
cr-report session.sqlite
cr-html session.sqlite > report.html
```

After:

```toml
# pyproject.toml — after  (or write a standalone fermut.toml via
# `fermut migrate cosmic-ray`; pass --pyproject for inline)
[tool.fermut]
source_root = "src"
tests = "tests"
timeout = 60
runner = "pytest"
pytest_args = ["-x", "-q"]
isolation = "copy"
```

```sh
# after
fermut run src/ --tests tests/ --json report.json --html report.html
fermut show report.json
```

Three things came along for free: `--diff-only` for PR gates,
`--shard` for parallelism without celery, and the sticky PR
comment. The CI variant is in
**[Integrations → GitHub Actions](integrations.md#github-actions)**.

## Where next

- **[Working on projects](projects.md)** — gradual-rollout playbook
  applies cleanly after migration.
- **[Integrations → Sharded full sweep](integrations.md#sharded-full-sweep)** —
  the celery replacement.
- **[Landscape](../concepts/landscape.md)** — the honest
  feature-by-feature comparison.
