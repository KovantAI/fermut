# Claude Code playbook

Battle-tested workflow for driving fermut from a [Claude Code](https://claude.com/claude-code) session. The same pattern works for any LLM-driven coding agent, but the gotchas were found while running against real Python backends and the timing data is from actual sessions.

Distilled from one session that took a ~30kLOC backend from a 58% raw / 70% (skip-ops) baseline to **80.2%** mutation score in **38 minutes of fermut wall time** plus ~3 hours of test-authoring time.

If you're looking for the general agent loop, start with [Coding agents](coding-agents.md). This page is the operational checklist.

## Install the skill

This playbook ships as an [Agent Skill](https://agentskills.io),
`fermut-mutation-testing`, that lets a coding agent (Claude Code, Codex, and
others) drive the loop below for you. A skill only loads from a place the
agent looks, so installing the `fermut` package doesn't give you the skill by
itself. Pick one of the two ways below.

### Option 1: from the fermut binary (any agent)

```bash
fermut install-skills
```

This writes the skill bundled with your installed fermut into
`.claude/skills/` in the current directory. Commit that directory and everyone
on the team gets the skill. The skill always matches the binary it came from,
so the flags and subcommands it tells the agent to run exist in your version.
Run the command again after upgrading fermut to refresh it.

| Flag | Writes to |
|------|-----------|
| (none) | `./.claude/skills/` (Claude Code, this project) |
| `--user` | `~/.claude/skills/` (Claude Code, every project) |
| `--agents` | `./.agents/skills/` (Codex and other Agent Skills readers) |
| `--user --agents` | `~/.agents/skills/` |
| `--dir DIR` | `DIR/<skill>/` |

If the skill is already installed and its files differ from the bundled copy
(you edited it, or it came from another fermut version), it is left alone and
the command exits `1`. Add `--force` to overwrite it. `--force` replaces only
the files fermut ships and keeps any files you added to the skill directory.

```console
$ fermut install-skills
  fermut-mutation-testing: installed
fermut 0.4.1 skills in /path/to/project/.claude/skills. Claude Code picks up project and user skills live; if they don't show up, start a new session.
```

### Option 2: the Claude Code plugin

The fermut repository is also a Claude Code plugin marketplace. Inside Claude
Code:

```text
/plugin marketplace add KovantAI/fermut
/plugin install fermut@fermut
```

Or from a shell:

```bash
claude plugin marketplace add KovantAI/fermut
claude plugin install fermut@fermut
```

The plugin makes the skill available in every project. It also registers the
[`fermut mcp`](../reference/cli/mcp.md) server, so the agent can call
`fermut_doctor`, `fermut_run`, `fermut_next`, `fermut_explain` and the other
tools natively instead of shelling out. The server runs the `fermut` on your
`PATH` from the directory you start Claude Code in, so install fermut first
(`pip install fermut`); without it, `/mcp` lists the `fermut` server as failed
and the skill still works through the CLI.

Its version follows fermut releases, so plugin updates bring the skill for the
latest fermut rather than the version you have installed. If you pin an older
fermut, use `fermut install-skills` instead, and register the server yourself
with `claude mcp add fermut -- fermut mcp`.

To suggest the plugin to everyone who opens your repository in Claude Code,
add this to the project's `.claude/settings.json`:

```json
{
  "extraKnownMarketplaces": {
    "fermut": {
      "source": { "source": "github", "repo": "KovantAI/fermut" }
    }
  },
  "enabledPlugins": {
    "fermut@fermut": true
  }
}
```

### Using the skill

The agent picks the skill when your request matches it: "are my tests any
good", "get the mutation score above 80%", "kill the survivors in
`billing.py`". To run it explicitly, type its slash command:

| Installed with | Command |
|----------------|---------|
| `fermut install-skills` | `/fermut-mutation-testing` |
| the plugin | `/fermut:fermut-mutation-testing` |

### If the skill doesn't show up

- **Check that it's loaded.** In Claude Code, `/skills` lists every skill it
  found. The plugin's skill appears under the `fermut` plugin, and `/mcp`
  shows the plugin's `fermut` server.
- **Reload after a plugin install.** If `/plugin install` says to run
  `/reload-plugins`, do that, or start a new session.
- **Install where the agent looks.** `fermut install-skills` writes to the
  current directory. Run it from the project root, the directory you start the
  agent in.
- **Match the agent to the directory.** Claude Code reads `.claude/skills/`.
  Codex and other Agent Skills readers use `.agents/skills/`, so install with
  `--agents` for them.

## When to use this playbook

- You want a target mutation score (60%, 70%, 80%) and need a concrete plan to get there
- You're driving fermut from Claude Code (or similar) and want to skip the trial-and-error
- Your tests pass and coverage is high, but mutation testing reveals weak assertions

## Pre-flight checklist

Before the first `fermut run`, verify all five:

### 1. Binary

Prefer the debug build over release — `target/release/fermut` may be stale during fermut development. Confirm subcommands are present:

```bash
<fermut-checkout>/target/debug/fermut --help | grep -E "explain|suggest|run"
```

### 2. Optional dependencies installed

Many Python projects gate test modules behind `[project.optional-dependencies]`. Without them, `pytest --collect-only` errors on missing imports and `.coverage` comes back empty.

=== "uv"

    ```bash
    uv sync --all-extras --group dev
    ```

=== "pip"

    ```bash
    pip install -e ".[all]"
    ```

!!! warning "Symptom if you skip this step"
    `pytest --collect-only -q` reports `N errors during collection` with `ModuleNotFoundError` for things like `msgraph`, `azure.ai.projects`, etc. `.coverage` will not have the contexts fermut needs.

### 3. Tooling on PATH

- `ty` — fermut's default pre-filter. Without it: pass `--no-ty-filter`.
- `pytest-cov` — install explicitly; it transitively pulls in `coverage`:
  ```bash
  uv add --dev pytest-cov         # or: pip install pytest-cov
  ```
- Confirm `coverage >= 7` is reachable from the venv.

### 4. Project venv

Locate `.venv/bin/pytest`. fermut spawns `pytest` directly, so the venv must be on PATH when fermut runs.

### 5. `--fail-under` — skip it in this loop

`--fail-under <SCORE>` exists (CLI flag and `fail_under` config key, 0.0–100.0): the run fails only when the score is below the threshold instead of the default exit-non-zero-on-any-survivor. Useful as a CI score gate, but irrelevant when this playbook drives the loop and reads outcomes from JSON — don't pass it.

## Coverage setup

fermut reads coverage.py's native `.coverage` SQLite database directly to know which tests touch which lines and re-runs only the relevant ones per mutant. Running `pytest --cov=src --cov-context=test` writes `.coverage`; fermut auto-discovers it at the project root (or pass `--coverage .coverage`). Get this wrong and either every mutant is skipped (no coverage filter signal) or fermut can't pass the right test selection back to pytest.

### `.coveragerc`

```ini
[run]
source = src
relative_files = True
branch = False

[report]
exclude_lines =
    pragma: no cover
    if __name__ == .__main__.:
```

!!! danger "Do NOT set `dynamic_context = test_function`"
    That setting produces context strings like `module.test_func`, which fermut cannot pass to pytest. Pytest expects `tests/file_test.py::test_func`. Use `pytest --cov-context=test` instead (next step).

### Generate coverage

The one command — `fermut coverage` — runs the suite and writes a
`.coverage` SQLite database fermut reads directly (no `coverage json`
export step). It's incremental: after the first run it re-measures only
the test files whose content changed.

```bash
fermut coverage --source <src-dir> --tests "$PWD/tests"
```

The `run`/`explain` commands below all pass `--coverage "$PWD/.coverage"`
(explicit and absolute because these are agent scripts where cwd is
unstable — auto-discovery is the convenience for interactive use, not
these scripts).

Or, manually — a `coverage json --show-contexts` export. fermut sniffs
the file format, so a JSON export still works as a legacy alternative
(the `.coveragerc` and context-format notes below apply to this path).
If you take this path, pass `--coverage "$PWD/coverage.json"` in place of
`--coverage "$PWD/.coverage"` on every `run`/`explain` below:

```bash
uv run pytest --cov=<src-dir> --cov-context=test --cov-report= -q
uv run coverage json --rcfile=.coveragerc --show-contexts -o coverage.json
```

`--show-contexts` is **mandatory** on this manual JSON path. Without it:

```
coverage.json at coverage.json has no per-test contexts.
```

### Verify the context format

```bash
python -c "
import json
d = json.load(open('coverage.json'))
for f, info in d['files'].items():
    ctx = info.get('contexts', {})
    for line, tests in ctx.items():
        if tests and any(t for t in tests):
            print(f, line, tests[:2]); raise SystemExit
"
```

Expected:

```
src/foo.py 42 ['tests/foo_test.py::test_bar|run']
```

If you see `['foo_test.test_bar']` (no `tests/` prefix, no `::` separator), `dynamic_context` is still set somewhere — remove it.

## Paths and project root

!!! note "Relative paths are auto-absolutized"
    fermut absolutizes a relative source-root and `--tests` before resolving the project root, so relative paths no longer break the mirror (the old `IO error ... (os error 2)` footgun is fixed). The `$PWD/...` form below is still fine and unambiguous — use it if you like, but it's optional.

```bash
PATH="$PWD/.venv/bin:$PATH" <fermut> run "$PWD" \
    --tests "$PWD/tests" \
    --coverage "$PWD/.coverage" \
    --json .fermut/last.json --no-history -q
```

!!! warning "Pass project root, not the source subdirectory"
    Prefer the project root over `src/` so source-root, tests, and coverage keys stay consistent. fermut auto-detects the coverage cwd by probing where the coverage keys resolve on disk (it walks `source_root` and its ancestors), so this is about consistency, not a path-join bug. If keys resolve nowhere under the root, fermut logs `coverage file keys do not resolve under source_root ... all mutants may appear uncovered` and every mutant looks uncovered — check the logs if mutant count is unexpectedly zero.

## Workflow

### Stage 1 — Sanity sample

```bash
PATH="$PWD/.venv/bin:$PATH" <fermut> run "$PWD" \
    --tests "$PWD/tests" --coverage "$PWD/.coverage" \
    --sample 0.005 --json .fermut/sample.json --no-history -q
```

Should complete in &lt;30s. Verify:

- Some mutants are tested (not all skipped)
- No `IO error` messages
- Score is a real number, not 100% with 0 killed

### Stage 2 — Full baseline

```bash
PATH="$PWD/.venv/bin:$PATH" <fermut> run "$PWD" \
    --tests "$PWD/tests" --coverage "$PWD/.coverage" \
    --json .fermut/last.json --no-history -q
```

Wall time scales with `mutant_count × per-pytest-invocation latency`. Real data points:

| Repo | Mutants | Tests | Baseline wall |
|---|---:|---:|---:|
| ~30kLOC backend | 7,616 | 542 | 19m17s |
| ~60kLOC backend | ~12k | ~2000 | ~78 min |

Run in the background; let the harness notify on completion rather than polling.

### Stage 3 — Operator triage

```bash
jq '[.outcomes[] | select(.status=="killed" or .status=="survived") | {op: .mutant.operator, st: .status}] | group_by(.op) | map({op: .[0].op, killed: ([.[]|select(.st=="killed")]|length), survived: ([.[]|select(.st=="survived")]|length), total: length}) | map(. + {kill_rate: ((.killed*100)/.total | floor)}) | sort_by(.kill_rate)' .fermut/last.json
```

Typical kill-rate buckets:

| Op | Kill rate | Value |
|---|---:|---|
| `lambda-body-to-none` | 100% | high |
| `return-value-to-none` | 96% | high |
| `unary-op-swap` | 91% | high |
| `not-insertion` | 85% | high |
| `compare-op-swap` | 84% | high |
| `assign-value-to-none` | 74% | high |
| `string-to-empty` | 69% | mixed |
| `bool-op-swap` | 60% | high |
| `string-sentinel` | 58% | mixed |
| `number-shift` | 54% | medium |
| `boundary-shift` | 41% | medium |
| `keyword-arg-drop` | 35% | mostly noise |
| `constant-replace` | 24% | mostly noise |

### Stage 4 — Skip noise (with math)

**Compute, don't copy.** The right skip set depends on the kill rates in *your* report. Skipping a high-kill-rate op **lowers** overall score because you drop more killed than survived mutants. `break-continue-swap` and `remove-decorator` are common 100%-kill-rate offenders that *must not* go into the skip set.

Use this jq scenario calculator to predict the score under candidate skip sets before committing:

```bash
jq '
def stat(skip):
  [.outcomes[] | select((.status=="killed" or .status=="survived" or .status=="timeout") and (.mutant.operator as $o | (skip | index($o) | not)))] as $kept
  | ($kept | map(select(.status=="killed")) | length) as $k
  | ($kept | map(select(.status=="survived")) | length) as $s
  | ($kept | map(select(.status=="timeout")) | length) as $t
  | {kept: ($k+$s+$t), killed: $k, survived: $s, score: (($k*1000/($k+$s+$t)|floor)/10)};
{
  baseline: stat([]),
  noise_min: stat(["keyword-arg-drop","constant-replace"]),
  noise_med: stat(["keyword-arg-drop","constant-replace","number-to-neg","number-to-zero","arith-op-swap","dict-item-drop"]),
  noise_max: stat(["keyword-arg-drop","constant-replace","number-to-neg","number-to-zero","arith-op-swap","dict-item-drop","boundary-shift","string-sentinel"])
}' .fermut/last.json
```

**Rule:** only skip ops where `survived ≥ killed` in your report. Verify each candidate before adding.

### Stage 5 — Survivor triage by file

```bash
jq '[.outcomes[] | select(.status=="survived") | .mutant.file] | group_by(.) | map({file: .[0], n: length}) | sort_by(-.n) | .[0:25]' .fermut/last.json
```

A survivor past the coverage filter is on a line some test executes — a legitimate kill target. Walk top-down.

**Deprioritize when**:

- **Keyword-arg-only object assign-none.** `RequestConfiguration(query_parameters=p)` → `None` is often equivalent because passing `None` for an optional kwarg is observationally identical to a default-only config. Look for `RequestConfiguration`, `ConfigDict`, `*Settings`, other passthrough containers.
- **Import-time side-effect coverage.** A test that imports `main.app` runs decorators but doesn't assert anything. Survivors on FastAPI `@router.get(...)` lines are like this — killable by asserting routes exist, just lower ROI than logic mutants.
- **Generated code** (`clients/python/*` from openapi-python-client and similar): regeneration overwrites any test tweaks. Coverage filter usually drops these anyway.

See [Equivalent mutants](equivalent-mutants.md) for more.

### Stage 6 — Inner loop: kill survivors

For each survivor in a high-value file:

```bash
<fermut> explain .fermut/last.json <idx-or-id> \
    --tests "$PWD/tests" --coverage "$PWD/.coverage" \
    --format json
```

!!! tip "Use plain `explain --format json`, not `explain --llm` or `suggest`"
    Plain `explain` returns a heuristic ExplainReport (operator hint, source context, nearby symbols, sample tests). Cheap, zero API tokens.

    `explain --llm` and the `suggest` subcommand both call the Anthropic API to draft a test (`suggest` always; `explain` only with `--llm`). **Wasteful when Claude is the driver** — Claude has repo context loaded and produces better, repo-idiomatic tests than a context-free SDK call.

**ExplainReport schema** (the JSON output):

```json
{
  "mutant": {"id": "...", "file": "...", "line": 67, "operator": "compare-op-swap", "original": "<=", "replacement": ">="},
  "status": "survived",
  "source_context": {"start_line": 62, "mutant_line": 67, "lines": ["...", "if 0 <= key < len(messages):", "..."]},
  "enclosing": {"kind": "def", "name": "async_set"},
  "hint": "comparison flipped. Tests likely assert truthiness, not the specific ordering...",
  "coverage": {
    "covered": true,
    "tests": ["tests/foo_test.py::test_bar"],
    "note": "tests executed this line but did not distinguish the mutation; strengthen their assertions"
  },
  "test_matches": {"symbol": "async_set", "matches": [{"file": "...", "line": 80}]},
  "skeleton": {"name": "test_async_set_compare", "code": "def test_async_set_compare():\n    ..."}
}
```

The highest-signal field is `coverage.tests` — these are tests that *executed* the line but failed to assert hard enough to kill the mutant. Strengthening one of them usually wins. Use `--context N` to widen the source window when the enclosing function is large.

After Claude writes the killing test:

1. Run it locally: `uv run pytest tests/new_test.py -q` — confirm green
2. Regenerate coverage — `fermut coverage` re-measures only the changed
   test file and updates `.coverage`:
   ```bash
   fermut coverage --source src --tests "$PWD/tests"
   ```
   Or, manually, refresh `coverage.json`:
   ```bash
   uv run pytest --cov=src --cov-context=test --cov-report= -q
   uv run coverage json --rcfile=.coveragerc --show-contexts -o coverage.json
   ```
3. Re-run fermut with same flags (match `--coverage` to whichever file
   you wrote)

### Stage 7 — Cache behavior

`.fermut/cache.json` is keyed on `(mutant_id, ast_hash(file), scope)` — an **AST-structural hash** of the file, not a raw byte sha256, so pure formatting/comment edits (e.g. `ruff format`) don't invalidate it; only structural changes do. `scope` includes the per-mutant test selection set, so when new tests change which contexts cover a line, the affected mutants miss cache and re-run while everything else hits cache.

**Typical iter rerun: &lt;60 seconds** when tests live in a file isolated from broad source coverage.

!!! warning "Cache-reset footgun"
    When you add boundary-condition tests to a directory whose tests already cover many adjacent source files (e.g. `tests/teams_storage/` covers all of `src/.../teams_storage/*.py`), per-test coverage signatures shift broadly → many mutants invalidate → iter rerun can take 10+ minutes. **One-time cost; the next iter is back to fast.**

    Mitigation: isolate the new test into its own file rather than appending to a shared one. Or accept the one-time miss.

!!! danger "Do not delete `.fermut/cache.json` to 'be safe'"
    Wastes hours; there is no correctness issue. Cache invalidation is automatic on file AST change (structural edits; pure reformatting/comments don't invalidate), runner/timeout/pytest-args change, and per-mutant test-set change via coverage.

See [Caching](../concepts/caching.md) for the model.

### Stage 8 — Batch explain dump (for hand-off)

End-of-session: dump every ExplainReport so the next session can resume without re-running fermut.

```bash
jq -r '.outcomes | to_entries[] | select(.value.status=="survived") | (.key + 1)' .fermut/last.json > .fermut/survivor_indices.txt
: > .fermut/explain-all.jsonl
while IFS= read -r idx; do
  <fermut> explain .fermut/last.json "$idx" \
    --tests "$PWD/tests" --coverage "$PWD/.coverage" \
    --format json 2>/dev/null | jq -c '.' >> .fermut/explain-all.jsonl
done < .fermut/survivor_indices.txt
```

301 reports took ~80 seconds in practice (~270ms per call). Combine with a per-file/per-symbol summary jq query to triage at the next session.

## Realistic targets

Scale matters more than absolute LOC.

- **Small/medium codebase (~≤10kLOC src, hundreds of tests), focused targeting**: 80% in **one session**, ~30–60 min wall + 2–4 hours of test authoring. Real data point: ~30kLOC backend, 542 tests baseline → 80.2% in 38m wall + ~3h authoring, adding 84 tests across 9 file batches.
- **Large codebase (~60kLOC+ src, thousands of tests)**: 80% is **multi-day to multi-week** — hundreds to thousands of new tests across many modules.
- **70% on any backend**: usually one session with op-skip + targeted tests on 1–3 high-value files (~20–40 new tests).
- **60% baseline**: most established Python backends with decent unit-test coverage land here without effort.

Per-file batch: 6–15 tests typically kill 10–30 mutants. Keyword-dispatch (`if "foo" in x or "bar" in x:`) needs one test per keyword for the per-keyword string mutations.

## Time budget

| Phase | Small/med backend | Large backend |
|---|---|---|
| Coverage gen (`fermut coverage` / full pytest with `--cov-context=test`) | ~40–60s | ~90s |
| Full baseline fermut run | 10–25 min | 60–90 min |
| Per-iteration rerun (cache hits) | 30–60s | 5–15 min |
| Per-iteration rerun (cache-reset miss) | 5–15 min | 30–60 min |
| Per-file test batch (Claude authoring) | 10–30 min | 15–30 min |
| Batch explain dump (~300 survivors) | ~80s | ~5 min |

## Common pitfalls

1. **Release binary stale** — `explain`/`suggest` may be missing. Use debug.
2. **Optional deps not installed** — pytest collection errors, empty `.coverage`. Run `uv sync --all-extras` first.
3. **`--fail-under` is for CI score gates, not this loop** — it exists (score threshold instead of the default exit-nonzero-on-any-survivor), but the skill reads outcomes from JSON, so don't pass it here.
4. **Relative paths** — auto-absolutized now (fixed); `$PWD/...` optional, not required.
5. **`dynamic_context = test_function` produces wrong nodeid format** — use `pytest --cov-context=test`.
6. **`--show-contexts` missing on `coverage json`** — fermut errors out.
7. **Coverage filter drops everything** — path mismatch; verify mutant count via `jq '[.outcomes[] | select(.status != "skipped")] | length' .fermut/last.json`.
8. **pytest not on PATH** — prepend `$PWD/.venv/bin`.
9. **Skipping a 100% kill-rate op lowers overall score** — see Stage 4 math. `break-continue-swap` and `remove-decorator` are common offenders.
10. **Passing `src/` as PATH** — double-`src` path bug; pass project root.
11. **Polling the bg fermut process** — don't; the harness notifies on completion.
12. **Iter rerun unexpectedly slow (10+ min)** — broad cache miss from tests in a shared dir. One-time, not a bug. See Stage 7.
13. **Wasting tokens on `explain --llm` / `suggest`** — both hit the Anthropic API (`suggest` always; `explain` only with `--llm`); when Claude drives, that call is redundant and weaker. Use plain `explain --format json`.

## Reference command

```bash
PATH="$PWD/.venv/bin:$PATH" <fermut> run "$PWD" \
    --tests "$PWD/tests" \
    --coverage "$PWD/.coverage" \
    --skip-ops keyword-arg-drop,constant-replace \
    --json .fermut/last.json \
    --no-history -q
```

Start with a minimal skip-ops set (`keyword-arg-drop,constant-replace` — almost always pure noise across repos). Add more only after running the Stage 4 scenario calculator to confirm each addition raises (or at least doesn't lower) the score.

## Per-session artifacts

1. **Baseline JSON** at `.fermut/last.json` + console score line
2. **Survivor histogram** by operator and by file
3. **Prioritized kill list** (high-value ops × high-survivor files, deprioritizing keyword-arg-only objects and generated code)
4. **Per-survivor explain → killing test** via the Stage 6 inner loop
5. **Batch explain dump** at `.fermut/explain-all.jsonl` (Stage 8, optional, for hand-off)
6. **Phase-timing log** (the user usually wants this)
