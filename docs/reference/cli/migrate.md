# `fermut migrate`

Translate a [mutmut](https://github.com/boxed/mutmut) or
[cosmic-ray](https://github.com/sixty-north/cosmic-ray) config into a
starter `[tool.fermut]`, and (for mutmut) rewrite
`# pragma: no mutate` markers to `# fermut: ignore`.

```sh
fermut migrate <FROM> [PATH] [--config PATH] [--pyproject] [--force]
               [--dry-run] [--no-pragma-rewrite]
```

| Arg / flag             | Default       | Effect                                                                                                |
|------------------------|---------------|-------------------------------------------------------------------------------------------------------|
| `FROM`                 | (required)    | `mutmut` or `cosmic-ray`.                                                                              |
| `PATH`                 | `.`           | Where to start the project-root walk.                                                                  |
| `--config <path>`      | autodetected  | Explicit source config. Defaults: `pyproject.toml` / `setup.cfg` for mutmut, `cosmic-ray.toml` for cosmic-ray. |
| `--pyproject`          | off           | Write `[tool.fermut]` into `pyproject.toml` instead of a standalone `fermut.toml`.                     |
| `--force`              | off           | Overwrite an existing `fermut.toml` or `[tool.fermut]` block.                                          |
| `--dry-run`            | off           | Print what would be written and which `.py` files would be rewritten, without touching the filesystem. |
| `--no-pragma-rewrite`  | off           | Skip rewriting `# pragma: no mutate` (mutmut only; no-op for cosmic-ray).                              |

## What it translates

| From                                                       | To                                          |
|------------------------------------------------------------|---------------------------------------------|
| `[tool.mutmut].paths_to_mutate` (1.x/2.x)                  | `source_root`                               |
| `[tool.mutmut].source_paths` (3.x)                         | `source_root`                               |
| `[tool.mutmut].tests_dir` (1.x/2.x)                        | `tests`                                     |
| `[tool.mutmut].pytest_add_cli_args_test_selection` (3.x)   | `tests`                                     |
| `[tool.mutmut].runner` (1.x/2.x)                           | `runner` + `pytest_args`                    |
| `[tool.mutmut].pytest_add_cli_args` (3.x)                  | `pytest_args`                               |
| `[tool.mutmut].use_coverage = true` (1.x/2.x)              | `coverage = "coverage.json"`                |
| `[tool.mutmut].mutate_only_covered_lines = true` (3.x)     | `coverage = "coverage.json"`                |
| `# pragma: no mutate` in `.py`                             | `# fermut: ignore`                          |
| `[cosmic-ray].module-path`                                 | `source_root`                               |
| `[cosmic-ray].timeout` (float)                             | `timeout` (rounded up to whole seconds)     |
| `[cosmic-ray].test-command`                                | `runner` + `pytest_args`                    |
| `[cosmic-ray.cloning].method`                              | `isolation`                                 |

## What it doesn't translate

The migrator prints a "manual review" report listing every key it
couldn't map, with a short reason. Common cases:

- mutmut: `backup`, `dict_synonyms`, `also_copy`, `pre_mutation`,
  `post_mutation`, `simple_output`.
- cosmic-ray: `excluded-modules`, `distributor`, `execution-engine`,
  `interceptors`, `badge`.

For the full mapping rationale see
**[Migrate from mutmut](../../guides/migrate-from-mutmut.md)** and
**[Migrate from cosmic-ray](../../guides/migrate-from-cosmic-ray.md)**.

## Examples

Translate a mutmut project living in `pyproject.toml` and rewrite
pragmas in place:

```sh
fermut migrate mutmut
```

Preview the cosmic-ray translation without writing anything:

```sh
fermut migrate cosmic-ray --dry-run
```

Translate from an explicit config file into `pyproject.toml`,
overwriting any existing `[tool.fermut]`:

```sh
fermut migrate mutmut --config setup.cfg --pyproject --force
```
