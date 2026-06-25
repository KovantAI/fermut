# Environment variables

Variables fermut reads from the process environment. CI ergonomics
mostly — file-based config is the recommended primary surface.

Precedence is the standard **CLI flag > env var > config file > built-in default**.

## fermut variables

| Variable                   | Effect                                                                                                                                                  |
|----------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------|
| `FERMUT_ISOLATION`         | Default for `--isolation` (`auto` / `copy` / `hardlink` / `reflink`). See [Isolation modes](cli/run.md#isolation-modes).                                |
| `FERMUT_TY_EMBEDDED`       | When set to `0`, forces the legacy ty subprocess path instead of the embedded in-process checker. Any other value (or unset) keeps embedded on. See [filters → embedded vs subprocess ty](../guides/filters.md#embedded-vs-subprocess-ty-default-embedded). |
| `FERMUT_PYTHON`            | Python interpreter used by the bytecode-identity detector and the hypothesis probe. Defaults to `python3` on `PATH`.                                    |
| `FERMUT_LLM_MOCK`          | When set to a truthy value (`1`, `true`, `yes`, `on`), `fermut explain --llm` and `fermut suggest` use a deterministic offline mock client instead of calling Anthropic. Test/offline only. |
| `FERMUT_ANTHROPIC_API_KEY` | Scoped alternative to `ANTHROPIC_API_KEY` for the LLM-backed subcommands. Read only when `ANTHROPIC_API_KEY` is unset. Useful when a host has a global Anthropic key for another tool but fermut needs a different one. |
| `FERMUT_NO_CONFIG`         | When truthy (`1` / `true` / `yes` / `on`), skip `fermut.toml` / `pyproject.toml` discovery entirely and run on built-in defaults plus CLI flags only. Opt-out switch for hosts that audit before trusting a discovered config. |
| `FERMUT_CACHE_KEY`         | Secret that namespaces **and** HMAC-signs the result cache. Set it (e.g. in CI) to segment or bust the cache by an external key. Setting it empty disables signing (warns); keys shorter than 16 bytes warn but still apply. |

## Inherited from the ecosystem

| Variable             | Effect                                                                                                                              |
|----------------------|-------------------------------------------------------------------------------------------------------------------------------------|
| `RUST_LOG`           | Logging level for fermut's tracing layer. Takes precedence over `-v` / `-q`. E.g. `RUST_LOG=fermut=debug`.                            |
| `ANTHROPIC_API_KEY`  | Required by `fermut explain --llm` and `fermut suggest` (unless `FERMUT_LLM_MOCK=1`). Falls back to `FERMUT_ANTHROPIC_API_KEY`.        |
| `GH_TOKEN`           | Read by `gh` for `fermut pr-comment`. Required in CI; `${{ secrets.GITHUB_TOKEN }}` is sufficient.                                    |
| `GITHUB_REPOSITORY`  | Default for `fermut pr-comment --repo`. Always set on GHA runners.                                                                   |
| `GITHUB_REF`         | One of the inputs `fermut pr-comment --pr` falls back to. Always set on GHA runners.                                                  |
| `PR_NUMBER`          | Alternative input for `fermut pr-comment --pr` when `GITHUB_REF` isn't a pull-request ref.                                            |
| `GITHUB_ACTIONS`     | When `"true"`, `fermut run` emits `::error` / `::warning` annotations by default. Override with `--annotate`.                         |

## See also

- **[CLI](cli/index.md)** — every flag.
- **[Configuration](configuration.md)** — every TOML key.
