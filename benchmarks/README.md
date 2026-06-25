# fermut benchmark harness

Compares **fermut**, **mutmut**, **cosmic-ray**, and **poodle** across a set
of pinned real-world Python projects (more-itertools, click, typer, starlette,
pyjwt, trio, markupsafe, jinja2, werkzeug, flask) on four timing scenarios.
See `configs/repos.toml` for the authoritative list and pins.

## Scenarios

| ID            | What it measures                                                            |
|---------------|-----------------------------------------------------------------------------|
| `cold`        | Fresh clone + tool install + repo install + first mutation pass (per phase) |
| `warm`        | Single re-run with all caches populated, no code changes                    |
| `loop`        | Avg of N iterations: small edit → re-run → revert                           |
| `score_3pct`  | Claude-Code-in-loop: write tests until mutation score rises >= 3 pts        |

## Prerequisites

- Python 3.11+ (3.12 recommended; `.python-version` pins it).
- `uv` (recommended) or `python3 -m venv` + `pip`.
- Rust toolchain — fermut is built via `maturin` from the parent crate.
- `git` on PATH.
- For `score_3pct`: `claude` CLI on PATH and Anthropic credentials configured.

The harness installs each tool into its own venv under
`fixtures/.venvs/<tool>-<repo>/` so tool dependency conflicts (click,
attrs versions, etc.) don't bleed across runs.

## Layout

```
benchmarks/
├── configs/
│   ├── repos.toml      # repo url, ref, src/test paths, install/test cmds
│   └── tools.toml      # install cmd, version cmd, cache dirs, score regex
├── src/bench/
│   ├── main.py         # CLI: `fermut-bench run|report`
│   ├── config.py       # toml loaders
│   ├── repos.py        # clone, install, loop-touch helpers
│   ├── venv.py         # per-(tool,repo) venv mgmt
│   ├── timing.py       # `timed()` context manager
│   ├── adapters/       # one per tool
│   └── scenarios/      # one per scenario
├── scripts/            # thin bash wrappers
├── fixtures/           # cloned repos + venvs (gitignored)
└── results/runs/       # one JSON per (scenario, tool, repo) run
```

## Usage

```bash
cd benchmarks

# The fermut wheel is built on demand by the adapter and cached under
# fixtures/.wheels/<git-sha>[-dirty-<digest>]/. Requires `maturin` on PATH;
# install with `pipx install maturin` (or `pip install --user maturin`)
# if needed. No manual pre-build step — a dirty tree rebuilds itself.

# One scenario, one tool, one repo.
./scripts/run_cold.sh fermut more-itertools

# Cross product (slow).
./scripts/run_cold.sh           # all tools x all repos

# Aggregate everything in results/runs/ into a markdown table.
./scripts/report.sh
```

Direct CLI (skip shell wrappers):

```bash
uv run fermut-bench run --scenario warm --tool fermut,mutmut --repo httpx
uv run fermut-bench report
```

`--tool` and `--repo` accept either `all`, a single name, or a comma-list.

## Result format

Each run writes `results/runs/<timestamp>-<scenario>-<tool>-<repo>.json`.
Schema differs by scenario:

- `cold`: `phases` array with `clone`, `tool_install`, `repo_install`,
  `mutation_run`. `total_seconds` sums them.
- `warm`: single `mutation_run` phase.
- `loop`: `iterations` array; `avg_seconds`, `min_seconds`, `max_seconds`.
- `score_3pct`: `baseline_score`, `final_score`, `delta`, per-iteration
  `claude_seconds` + `mutation_seconds`, `status` (`reached` /
  `exhausted_iterations` / `skipped_no_claude` / ...).

## Bumping pinned refs

`configs/repos.toml` pins each repo to a release tag. To re-benchmark
against newer upstream, edit the `ref` and re-run cold so the new clone
takes effect.

## Caveats

- `poodle` is published as `poodle-test` on PyPI; pin tracked in tools.toml.
- mutmut's score lives in `mutmut results`, not `mutmut run` — the adapter
  invokes both and time-accounts the run only.
- cosmic-ray re-times init + exec + dump together; init is small but
  non-trivial on first run.
- Score-regex parsing is per-tool and fragile; check the raw JSON if a
  reported score looks off.
- `score_3pct` makes real Claude API calls — budget accordingly. Skipped
  cleanly if `claude` CLI is absent.
