# `fermut init`

Detect layout, write a config tuned to the repo.

```sh
fermut init [PATH] [--profile NAME] [--list-profiles]
            [--pyproject] [--force] [--with-gha] [--with-coverage]
            [--dry-run]
```

| Flag                | Default     | Effect                                                                                                   |
|---------------------|-------------|----------------------------------------------------------------------------------------------------------|
| `PATH`              | `.`         | Where to start the project-root walk.                                                                    |
| `--profile <name>`  | none        | Pre-seed with `pr-gate`, `nightly`, `local`, or `library`. See [Working on projects](../../guides/projects.md#step-2-pick-a-profile). |
| `--list-profiles`   | off         | Print the profile catalogue and exit.                                                                     |
| `--pyproject`       | off         | Write `[tool.fermut]` into `pyproject.toml` instead of `fermut.toml`.                                     |
| `--force`           | off         | Overwrite an existing config block.                                                                       |
| `--with-gha`        | off         | Also drop a PR-gate workflow at `.github/workflows/fermut.yml`.                                            |
| `--with-coverage`   | off         | Wire `coverage = "coverage.json"` even when no coverage dep is detected.                                  |
| `--dry-run`         | off         | Print what would be written without touching the filesystem.                                              |
