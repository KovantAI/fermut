# fermut profile examples

Reference `fermut.toml` files for each built-in profile. Drop one into
the root of a Python project (or copy it into `[tool.fermut]` in
`pyproject.toml`) and you have a working config without running
`fermut init`. Adjust `source_root` / `tests` if your layout differs
from `src/` + `tests/`.

| Profile                    | When                          | Highlights                                                                            |
|----------------------------|-------------------------------|---------------------------------------------------------------------------------------|
| [`pr-gate.toml`](pr-gate.toml) | Pre-merge CI gate. Fail fast. | `diff_only = "main"`, coverage required, 4-op allowlist, 15s timeout, pinned seed.    |
| [`nightly.toml`](nightly.toml) | Cron full sweep. Catch drift. | `experimental = true`, no diff filter, no coverage, 60s timeout, every op.            |
| [`local.toml`](local.toml)     | Dev loop. Sub-second cycle.   | `sample = 0.25` (`sample_seed = 0`), no coverage, 15s timeout.                        |
| [`library.toml`](library.toml) | Library authors.              | All stable ops, `experimental = false`, pinned `hypothesis_seed`, 30s timeout.        |

## Use a file directly

```sh
cp examples/profiles/pr-gate.toml ./fermut.toml
fermut run src/
```

## Or regenerate via `fermut init`

```sh
fermut init --profile pr-gate
fermut init --profile nightly --force
fermut init --profile local   --force
fermut init --profile library --force
```

`init` re-detects `source_root`, `tests`, `runner`, and `ty_filter` for
your project, then applies the profile's overrides on top. Hand-edit
anything you don't like — the generated TOML is plain text.

## Switching profiles

Profiles are not exclusive. Pick the one closest to your primary use,
then either:

- swap files with `fermut init --profile <other> --force`, or
- keep one checked-in profile (often `pr-gate`) and override per-run
  with CLI flags. See
  [Running the nightly variant with the same config](../../README.md#running-the-nightly-variant-with-the-same-config)
  in the top-level README.

## See also

- Top-level [Profiles](../../README.md#profiles) section — full table
  and switch instructions.
- [`examples/github-actions/`](../github-actions/) — workflows that
  pair with `pr-gate` and `nightly`.
