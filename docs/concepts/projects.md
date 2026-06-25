# Projects

> **Skip on first read.** This page documents fermut's project model
> for monorepos, non-standard layouts, and namespace packages. If
> you're new and your repo has a single `pyproject.toml` with `src/`
> and `tests/` at the root, `fermut init` auto-detects everything
> and you don't need this page. Come back when `init` doesn't pick
> the layout you expected.

A **fermut project** is a Python codebase fermut runs against — the
unit of mutation testing. It's the same concept as a "project" in
pip / uv / poetry, but with one extra requirement: fermut needs to
know where the source lives, where the tests live, and where to
write its own state.

This page covers what makes a project, how fermut discovers it, and
the layout fermut expects. For the task-side ("how do I roll fermut
out across a project?"), see
**[Working on projects](../guides/projects.md)**.

## What counts as a project

Any directory containing one of:

- **`pyproject.toml`** (preferred — modern Python projects).
- **`setup.cfg`** (legacy fallback).

…is treated as a project root. fermut walks **up** from the path you
pass on the CLI, nearest ancestor first, and stops at the first
match. That directory becomes the project root for the rest of the
run.

```
my-monorepo/
├── pyproject.toml           ← repo-wide project root
├── packages/
│   ├── auth/
│   │   ├── pyproject.toml   ← inner project root (auth)
│   │   ├── src/
│   │   └── tests/
│   └── billing/
│       ├── pyproject.toml   ← inner project root (billing)
│       ├── src/
│       └── tests/
```

Running `fermut run packages/auth/src/` resolves to
`packages/auth/` as the root (nearest ancestor with
`pyproject.toml`). Per-package configs let each package have its
own `fermut.toml`, ops allowlist, profile, etc.

## What a project carries

Once resolved, the project root anchors three things:

| Slot                | Default                                       | Override                                                |
|---------------------|-----------------------------------------------|---------------------------------------------------------|
| **Source root**     | `<root>/src` if present, else `<root>`         | `source_root = "..."` in `fermut.toml`, or CLI `PATH`.  |
| **Tests directory** | `<root>/tests`                                 | `tests = "..."` in `fermut.toml`, or `--tests`.         |
| **State directory** | `<root>/.fermut/`                              | `--cache-path`, `--history-path` (or env vars).         |

`fermut init` runs this detection and writes a `fermut.toml` tuned
to what it sees. Re-run with `--force` to overwrite.

## The `.fermut/` directory

Every project gets a `.fermut/` directory at the root. Two files
live here:

- **`cache.json`** — content-hashed result cache. See
  **[Caching](caching.md)**.
- **`history.jsonl`** — append-only run log. See
  **[Trends](../guides/trends.md)**.

Both are safe to commit if you want CI to inherit local state; both
are also safe to gitignore if you want each environment to maintain
its own. The common pattern is **gitignore `cache.json`, commit
`history.jsonl`** so the trend log persists across machines without
the cache fighting CI's `actions/cache`.

## Project layout assumptions

fermut works best when the project follows one of two common
shapes:

**Src layout** (preferred):

```
my-project/
├── pyproject.toml
├── fermut.toml
├── src/
│   └── my_project/
│       └── ...
└── tests/
    └── test_*.py
```

**Flat layout**:

```
my-project/
├── pyproject.toml
├── fermut.toml
├── my_project/
│   └── ...
└── tests/
    └── test_*.py
```

`fermut init` auto-detects which layout you're on by checking for a
`src/` directory. Override with `source_root` if your layout is
unusual.

## Project size and the auto profile

`fermut init` counts the `.py` files under the source root and
classifies the project as **Small** (< 50 files), **Medium**
(50–500), or **Large** (> 500). The classification feeds the
default profile choice when you don't pass `--profile` explicitly.
See **[Configuration](configuration.md#profiles)** for the profile
catalogue.

## Multiple projects in one repo

Each project root is independent — its own `fermut.toml`, its own
`.fermut/`, its own history. Run them separately:

```sh
fermut run packages/auth/src/
fermut run packages/billing/src/
```

Or wire each one as its own CI job. There's no monorepo-aware
aggregation today; the
**[Roadmap](../roadmap.md#engine-and-runtime)** tracks a
`workspace`-style mode for that.

## Where next

- **[Working on projects](../guides/projects.md)** — the
  human-facing rollout playbook.
- **[Configuration](configuration.md)** — file vs CLI vs profile,
  precedence rules.
- **[`fermut init`](../reference/cli/init.md)** — what `init`
  detects and writes.
