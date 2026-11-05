# Agent skill & plugin

fermut ships an [Agent Skill](https://agentskills.io),
`fermut-mutation-testing`, that teaches a coding agent (Claude Code, Codex
and others) the whole mutation-testing loop: check the environment, build
coverage, run fermut, triage survivors, and write the tests that kill them.
This page covers installing it and what to expect from it. For the loop
itself and the JSON it reads, see **[Coding agents](coding-agents.md)**.

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
fermut 0.6.1 skills in /path/to/project/.claude/skills. Claude Code picks up project and user skills live; if they don't show up, start a new session.
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

## What the skill does

Asked something like "get the mutation score above 80%", the agent:

1. Runs [`fermut doctor`](../reference/cli/doctor.md) and fixes what fails,
   then [`fermut baseline`](../reference/cli/baseline.md) for the starting
   score.
2. Runs a full `fermut run --json .fermut/last.json` and checks the
   noisy-operator hint at the end of the run before skipping any operator.
   Skipping an operator whose mutants mostly die *lowers* the score.
3. Works through survivors in the order
   [`fermut next`](../reference/cli/next.md) ranks them, using
   [`fermut explain --format json`](../reference/cli/explain.md) for
   each one, and writes the killing test in its own session.
4. After touching tests, refreshes coverage with
   [`fermut coverage`](../reference/cli/coverage.md) and re-runs. The
   cache means only the affected mutants run again.

It writes the tests itself instead of calling `fermut suggest` or
`explain --llm`: the agent already has your repo in context, and a second
model call would cost tokens and know less.

## Realistic targets

From real sessions on Python backends; your numbers will differ.

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

- **Optional dependencies missing.** If test modules import packages from
  `[project.optional-dependencies]`, collection fails and coverage comes
  back empty. Install them first (`uv sync --all-extras --dev`, or
  `pip install -e ".[all]"`).
- **`dynamic_context = test_function` in `.coveragerc`.** It produces
  contexts like `module.test_func`, which fermut can't hand back to
  pytest. Use `pytest --cov-context=test` (what `fermut coverage` runs)
  and remove the setting. See [Coverage](coverage.md#generate-coverage-with-per-test-contexts).
- **A slow re-run after adding tests to a shared file.** New tests in a
  directory that already covers many source files change the test set
  behind many mutants, so they all re-run once. Put new tests in their
  own file, or accept the one-time cost.
- **Deleting `.fermut/cache.json` "to be safe".** It costs hours and fixes
  nothing: entries are invalidated automatically when the code, the run
  settings or the covering tests change. See
  [Caching](../concepts/caching.md).
