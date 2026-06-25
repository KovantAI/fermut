# `fermut run`

Generate mutants, run tests against each, report killed / survived.

```sh
fermut run [PATH] [flags...]
```

| Flag                        | Default                          | Effect                                                              |
|-----------------------------|----------------------------------|---------------------------------------------------------------------|
| `PATH`                      | `.`                              | Python source root.                                                 |
| `--tests <dir>`             | `<PATH>/tests`                   | Test directory.                                                     |
| `--jobs <n>`                | num CPUs                         | Parallel workers (one mutant per worker at a time).                  |
| `--timeout <s>`             | `30`                             | Per-mutant test timeout.                                            |
| `--no-ty-filter`            | off                              | Skip the ty pre-filter.                                             |
| `--ruff-filter`             | off                              | Enable the ruff lint pre-filter.                                    |
| `--hypothesis-seed <N>`     | none                             | Pin Hypothesis seed across mutants. See [mutation testing concepts](../../concepts/mutation-testing.md#vocabulary). |
| `--pytest-arg <ARG>`        | none, repeatable                 | Forward arbitrary args to pytest.                                   |
| `--no-cache`                | cache on                         | Disable the result cache.                                           |
| `--cache-path <p>`          | `<project-root>/.fermut/cache.json`     | Custom cache file location. Project root = nearest `pyproject.toml`/`setup.cfg` ancestor. |
| `--no-history`              | history on                       | Skip the run-history log append for this run.                        |
| `--history-path <p>`        | `<project-root>/.fermut/history.jsonl`  | Custom history log location (same anchor as the cache).             |
| `--sample <r>`              | `1.0`                            | Test only this fraction of mutants (0.0–1.0), deterministic.         |
| `--sample-seed <N>`         | `0`                              | Seed for `--sample` selection.                                       |
| `--shard <i/n>`             | off                              | Distributed exec: process only the i-th of n disjoint slices.        |
| `--runner pytest\|unittest` | `pytest`                         | Pick the Python test runner.                                          |
| `--python <PATH>`           | auto-discover                    | Interpreter (path) or virtualenv (dir) to run pytest with — fermut invokes `<python> -m pytest`, no PATH reliance. Omitted → auto-discovers an active venv / nearby `.venv`, else a bare `pytest` on PATH. See [Choosing the interpreter](#python-interpreter). |
| `--isolation auto\|copy\|hardlink\|reflink` | `auto` | How worker mirrors are populated. See [Isolation modes](#isolation-modes). |
| `--annotate`                | auto in GHA                      | Emit `::error` / `::warning` annotations for CI.                     |
| `--watch`                   | off                              | Re-run on every `.py` change until Ctrl+C.                            |
| `--format human\|json`      | `human`                          | stdout output format.                                                |
| `--json <p>`                | none                             | Also write JSON report.                                              |
| `--junit <p>`               | none                             | Also write JUnit XML.                                                |
| `--html <p>`                | none                             | Also write self-contained HTML report.                                |
| `--markdown <p>`            | none                             | Also write Markdown summary (for PR comments).                        |
| `--trend`                   | off                              | Include a compact trend block at the top of the Markdown report.      |
| `--experimental`            | off                              | Include experimental operators.                                       |
| `--parity`                  | off                              | Include parity operators (cross-tool comparison only — very noisy, never for normal scoring). See [Parity operators](../operators/parity.md). |
| `--ops <list>`              | all                              | Allowlist operators.                                                  |
| `--skip-ops <list>`         | none                             | Denylist operators (wins over `--ops`).                                |
| `--diff-only [base]`        | off                              | Restrict to lines changed vs base ref (default `main`).               |
| `--since <SPEC>`            | off                              | Restrict to lines touched since commit or date. See [Incremental runs](#incremental-runs-since). |
| `--no-diff-only`            | off                              | Override `diff_only`/`since` from config — force full sweep. Mutually exclusive with `--diff-only` and `--since`. |
| `--coverage [path]`         | off                              | Per-mutant test selection from coverage contexts. See [Coverage](#coverage-integration). |
| `--no-coverage`             | off                              | Override `coverage` from config — disable coverage filtering for this run. Mutually exclusive with `--coverage`. |
| `--exclude <GLOB>`          | none, repeatable                 | Glob patterns excluding paths from mutation collection (relative to source root). Example: `--exclude 'alembic/**' --exclude 'tests/integration/**'`. When passed at least once, replaces config `exclude` (no merge). |
| `--no-equiv-detect`         | detector on                      | Skip the equivalent-mutant detector (AST patterns + CPython bytecode). Survivors are not post-processed; equivalents stay counted. |
| `--cache-scope file\|scope` | `file`                           | Cache-key granularity. `file` hashes the whole file's AST; `scope` hashes the enclosing top-level def/class only, so sibling-function edits keep cache hits. `scope` can return stale verdicts when one test indirectly exercises another function. |
| `--fail-under <SCORE>`      | none (any survivor fails)        | Pass when mutation score is at least `SCORE` (0.0–100.0). Equal to threshold passes. |
| `--no-verify-baseline`      | baseline check on                | Skip the pre-flight run of the unmutated suite. By default fermut runs your full suite once and aborts if it isn't green (a red suite would inflate the score toward 100%). Skip only when you've already confirmed green (e.g. CI ran it). |
| `--baseline-timeout <SECS>` | `300`                            | Wall-clock cap for the baseline run. Separate from `--timeout` (which bounds a single mutant) because the baseline runs the whole suite. A suite that exceeds it is killed and the run aborts. Raise for large suites. |
| `--fail-on-regression <PTS>`| off                              | Exit non-zero when score dropped more than `PTS` vs the most recent prior entry on the same git branch. Requires history. Ignored in `--watch`. |
| `--trend-branch <NAME>`     | none                             | Restrict the `--trend` markdown block's "previous run" lookup to entries recorded on this branch. Requires `--trend`. |

Exit code is non-zero when any mutant survives — wire that into CI to
gate on mutation score.

## CLI overrides vs config

CLI flags override the loaded config. Precedence is **CLI > config-file >
built-in defaults**. The common pattern: keep `fermut.toml` tuned to your
primary profile (usually `pr-gate`), then have other CI jobs override
individual knobs from YAML:

```yaml
# Same fermut.toml as the PR gate, broadened to a nightly sweep.
- run: |
    fermut run src/ \
        --tests tests/ \
        --experimental \
        --ops "" \
        --timeout 60 \
        --markdown nightly.md
```

Each list (`ops`, `skip_ops`) merges independently — a CLI `--ops`
overrides the file's `ops` but leaves the file's `skip_ops` intact.

## Coverage integration

`--coverage` accepts **either** coverage.py's native `.coverage` SQLite
database **or** a `coverage.json` export — the format is sniffed, so
both work. Prefer the database on large suites (the JSON export repeats
every node id per covered line and can reach multiple GB; the SQLite
form stores it once).

The easy way to produce it is [`fermut coverage`](coverage.md), which
also refreshes incrementally when tests change:

```sh
fermut coverage
fermut run src/ --tests tests/ --coverage .coverage
```

Or generate a JSON export manually (per-test contexts are mandatory):

```sh
pytest --cov=src --cov-context=test
coverage json -o coverage.json --show-contexts
fermut run src/ --tests tests/ --coverage coverage.json
```

Without contexts, coverage only knows which lines ran, not which tests
ran them, so per-mutant test selection isn't possible. fermut refuses
the run in that case.

When coverage is wired, the filter chain skips any mutant whose line has
zero test contexts, and the pytest runner narrows the invocation per
mutant to only the tests that exercised the mutated line.

## Incremental runs (`--since`)

`--diff-only main` is branch-relative — diffs the current branch against
its merge base with `main`. Right tool for PR-time gates.

For "what's changed since I last ran fermut?", "only test the last
week", or "run against a specific tag", use `--since`:

```sh
fermut run pkg/ --since v1.2.0
fermut run pkg/ --since HEAD~10
fermut run pkg/ --since abc123
fermut run pkg/ --since 2025-12-01
fermut run pkg/ --since '1 week ago'
```

`SPEC` is resolved in two steps:

1. **Git ref** — `git rev-parse --verify <SPEC>^{commit}`. Resolves
   SHAs, branches, tags, `HEAD~N`, reflog notation.
2. **Date** — if rev-parse fails, fall back to
   `git log -1 --before=<SPEC> --format=%H HEAD`. Anything `git log
   --before` accepts: ISO dates, `'yesterday'`, `'1 week ago'`.

Then `git diff --unified=0 --relative <commit>` produces the line set.
This diff **includes uncommitted working-tree changes** — editing a file
locally re-runs its mutants without needing to commit.

`--diff-only` and `--since` are mutually exclusive.

## Choosing the interpreter { #python-interpreter }

The pytest runner needs a Python interpreter that has `pytest` installed.
By default fermut spawns a bare `pytest` resolved from `PATH` — which
breaks in environments that won't let you put a venv's `bin/` on `PATH`
(locked-down agent sandboxes, some CI), and is ambiguous when several
interpreters are around.

`--python` removes the ambiguity. Give it either an **interpreter path**
(`--python /opt/py/bin/python3.12`) or a **virtualenv directory**
(`--python .venv` — fermut maps it to the interpreter inside). fermut then
runs `<python> -m pytest`, so the run uses *that* interpreter's pytest and
its venv's packages, with no dependence on `PATH`.

When `--python` is omitted, fermut auto-discovers an interpreter likely to
have pytest, in order:

1. the active virtualenv (`$VIRTUAL_ENV`);
2. a `.venv` found walking up from the source root (stopping at the repo
   root);
3. otherwise `None` — fall back to a bare `pytest` on `PATH` (the historical
   behavior).

Only venvs are auto-discovered: a bare system `python3` may lack pytest, so
fermut leaves that case to the PATH fallback. Set it in `fermut.toml` with
`python = ".venv"` (relative values resolve against the config's directory).
The interpreter is part of the cache key and the history config hash, so
switching interpreters never reuses another's cached verdicts or marks the
runs as directly comparable.

(`--python` applies to the pytest runner; the `unittest` runner is
unaffected.)

## Isolation modes

Each worker thread builds one mirror of the project tree, reused across
every mutant that worker processes. `--isolation` picks how the mirror
is populated. The patched file is always unlinked and rewritten (never
edited in place), so the patched file can't clobber the real source —
but tests that write to *other* files inside the tree share their writes
with the original when in `hardlink` mode.

| Mode       | What it does                                                                                              | Build cost                              | Safety                                                                                  |
|------------|-----------------------------------------------------------------------------------------------------------|-----------------------------------------|-----------------------------------------------------------------------------------------|
| `auto`     | Reflink/clonefile on supported filesystems (APFS, btrfs, xfs, zfs, ReFS); else plain copy.                | Near-zero on CoW filesystems; full copy otherwise. | Same as `copy` — mirror is independent.                                                |
| `copy`     | Plain recursive `fs::copy`.                                                                                | Slowest; baseline.                       | Fully independent.                                                                       |
| `reflink`  | Force reflink/clonefile, fall back to copy on unsupported filesystems.                                     | Near-zero on CoW filesystems.            | Fully independent.                                                                       |
| `hardlink` | Hardlink every file from source into the mirror.                                                           | Cheapest — just inode entries.           | Shares inodes; safe for the patched file (unlinked before write), unsafe if tests rewrite *other* files. |

You can also set the mode in `fermut.toml` (`isolation = "hardlink"`) or
via the `FERMUT_ISOLATION` environment variable. Precedence is the
standard CLI > env > config-file > default.

!!! note "macOS / APFS"

    Rust's `std::fs::copy` already uses APFS `fclonefileat` under the
    hood, so `copy` and `reflink` perform nearly identically on macOS;
    `hardlink` is often *slower* (extra metadata work per file). The
    mode matters most on Linux ext4 / tmpfs and other filesystems
    without CoW.
