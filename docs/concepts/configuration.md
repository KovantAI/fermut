# Configuration

How fermut decides what to do when you run it: the config file, the
CLI flags, the profiles, and the precedence between them. The full
key-by-key schema lives in the
**[Configuration reference](../reference/configuration.md)**; this
page explains the model.

## The two config files

fermut reads its persistent config from one of two files, in this
order of preference:

1. **`fermut.toml`** — standalone file. Preferred.
2. **`pyproject.toml`** with a `[tool.fermut]` table — useful when you
   already maintain a `pyproject.toml` and want to avoid a second
   file.

Both are discovered by walking *up* from the source path, nearest
ancestor first. The first `fermut.toml` wins; if none exists,
`pyproject.toml` files are searched in the same walk for a
`[tool.fermut]` table.

Paths inside the config file resolve **relative to the config file's
directory**, not the current working directory. Move the file, paths
move with it.

Unknown keys are rejected — typos surface early instead of being
silently ignored.

Generate a starter config with
**[`fermut init`](../reference/cli/init.md)**. It detects the
project layout, picks the right runner, and writes a TOML tuned to
the repo.

## Precedence

```
CLI args   >   config file   >   built-in defaults
```

Each list (`ops`, `skip_ops`, `pytest_args`) merges independently — a
CLI `--ops` overrides the file's `ops` but leaves the file's
`skip_ops` intact, and vice versa.

This makes the common CI pattern clean: keep `fermut.toml` tuned to
your primary profile (usually `pr-gate`), then have *other* CI jobs
override individual knobs from YAML rather than carry a second
config file.

```yaml
# Nightly sweep job — same fermut.toml, broadened via CLI.
- run: fermut run src/ --tests tests/ \
       --experimental --ops "" --timeout 60 \
       --markdown nightly.md
```

## Profiles

Profiles are *opinionated starter templates* for `fermut init`. They
write a `fermut.toml` tuned to one specific use case:

| Profile     | Optimized for                       | Key choices                                                       |
|-------------|-------------------------------------|-------------------------------------------------------------------|
| `pr-gate`   | Pre-merge CI gate. Fail fast.       | `diff_only`, `coverage`, narrow ops, short timeout, seeded.       |
| `nightly`   | Cron full sweep. Catch drift.       | All operators (incl. experimental), longer timeout, no filters.   |
| `local`     | Dev loop. Sub-second cycle.         | `sample = 0.25`, all stable ops, cache on, no coverage.           |
| `library`   | Library authors. Reproducible.      | All stable ops, seeded, moderate timeout.                          |

Profiles are not mutually exclusive — they're templates. After
`init`, the generated `fermut.toml` is plain text; hand-edit
anything. See **[Working on projects](../guides/projects.md)** for
the full rollout playbook.

## Environment variables

A handful of knobs read environment variables for CI ergonomics
(e.g. `FERMUT_ISOLATION`, `RUST_LOG`). The full list is in
**[Environment variables](../reference/environment-variables.md)**.
Precedence is the standard CLI > env > config-file > default.

## Where next

- **[Configuration reference](../reference/configuration.md)** —
  every key with type, default, and effect.
- **[Working on projects](../guides/projects.md)** — picking and
  tightening a profile.
- **[Filters](../guides/filters.md)** — `--ops`, `--skip-ops`,
  `--diff-only`, inline ignore markers.
