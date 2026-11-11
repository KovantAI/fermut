# Environment variables

Variables fermut reads from the process environment. CI ergonomics
mostly — file-based config is the recommended primary surface.

Precedence is the standard **CLI flag > env var > config file > built-in default**.

## fermut variables

| Variable                   | Effect                                                                                                                                                  |
|----------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------|
| `FERMUT_ISOLATION`         | Default for `--isolation` (`auto` / `copy` / `hardlink` / `reflink`). See [Isolation modes](cli/run-isolation.md#isolation-modes).                                |
| `FERMUT_TY_EMBEDDED`       | When set to `0`, forces the legacy ty subprocess path instead of the embedded in-process checker. Any other value (or unset) keeps embedded on. See [filters → embedded vs subprocess ty](../guides/filters.md#embedded-vs-subprocess-ty-default-embedded). |
| `FERMUT_PYTHON`            | Python interpreter used by the bytecode-identity detector and the `--tce` pre-filter. Defaults to `python3` on `PATH`. Not the interpreter that runs your tests (that's `--python` / `VIRTUAL_ENV`). |
| `FERMUT_LLM_MOCK`          | When set to a truthy value (`1`, `true`, `yes`, `on`), `fermut explain --llm`, `fermut suggest` and `fermut autofix` use a deterministic offline mock client instead of calling Anthropic. Test/offline only. |
| `FERMUT_ANTHROPIC_API_KEY` | Fallback for `ANTHROPIC_API_KEY` in the LLM-backed subcommands. Read only when `ANTHROPIC_API_KEY` is unset, so it can't override a global key. |
| `FERMUT_NO_CONFIG`         | When truthy (`1` / `true` / `yes` / `on`), skip `fermut.toml` / `pyproject.toml` discovery entirely and run on built-in defaults plus CLI flags only. Opt-out switch for hosts that audit before trusting a discovered config. |
| `FERMUT_BASELINE_SECS`     | Default for `--baseline-secs`. `pytest --fermut` sets it to a whole-suite session's duration. See [Timeout budgets](cli/run.md#timeout-budgets). |
| `FERMUT_BIN`               | Read by the [pytest plugin](../guides/pytest-plugin.md): the `fermut` binary to call. Defaults to the one installed beside the plugin, then `PATH`. |
| `FERMUT_CACHE_KEY`         | Secret that namespaces **and** HMAC-signs the result cache. Set it (e.g. in CI) to segment or bust the cache by an external key. Setting it empty disables signing (warns); keys shorter than 16 bytes warn but still apply. |

## Set by fermut for your tests

| Variable     | Effect |
|--------------|--------|
| `FERMUT_CHILD` | `1` in every test process fermut starts. |
| `FERMUT_RESULT`, `FERMUT_MIRROR_ROOT`, `FERMUT_PROJECT_ROOT` | Internal plumbing between fermut and its pytest reporter. Don't set them yourself. |
| `FERMUT_PRIVATE_HOME` | Internal: tells the schemata template to give each fork its own private home. Don't set it yourself. |
| `HOME`, `XDG_*` (on Windows also `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`) | Pointed at a private per-worker (and, under schemata, per-fork) directory so tests can't race on a shared home. `--no-private-home` turns this off. |
| `PYTHONHASHSEED` | `1` unless you export your own (`PYTHONHASHSEED=random` opts out), so set/dict order is the same for every mutant. Part of the cache key. |
| `PYTHONPATH`, `PYTHONDONTWRITEBYTECODE` | Pinned to the mirror, and `1`. |
| `OS_ACTIVITY_MODE` | `disable` in the schemata template on macOS (os_log isn't fork-safe), unless you set it. |
| `FM_ACTIVE`  | Under [schemata](cli/run-schemata.md), the active mutant's number in the schema. Exported so a Python subprocess a test starts runs the same mutant; `0` means none. A subprocess started without it (`env={...}`) finds the mutant by its process group instead, unless it starts a new session. Don't set it yourself. |
| `TOX_PARALLEL_ENV` | Set per worker to `fm<k>` (after your own value inside `tox -p`: `py313_fm0`). pytest-django appends it to test database names, so concurrent workers and forks each get their own Postgres/MySQL/file-SQLite test DB instead of failing on a shared one. In-memory SQLite is unaffected. |

fermut also **removes** `PYTEST_ADDOPTS`, `PYTEST_PLUGINS`, `PYTHONWARNINGS`,
`PYTHONOPTIMIZE` and `PYTHONINSPECT` from test processes, so a run depends on
the project alone, not on the calling shell. Put pytest options in your pytest
config or `pytest_args` instead.

## Inherited from the ecosystem

| Variable             | Effect                                                                                                                              |
|----------------------|-------------------------------------------------------------------------------------------------------------------------------------|
| `VIRTUAL_ENV`        | Interpreter discovery when `--python` / `python` is unset. If it is set, the tests run with `$VIRTUAL_ENV/bin/python -m <runner>` (rstest, a native binary, runs as `$VIRTUAL_ENV/bin/rstest`). If it is unset, fermut looks for a `.venv` walking up from the source root, and with neither it uses `pytest` (or `python3` for unittest) from `PATH`. `fermut doctor` probes tools in the same venv. See [Choosing the interpreter](cli/run.md#python-interpreter). |
| `RUST_LOG`           | Logging level for fermut's tracing layer. Takes precedence over `-v` / `-q`. E.g. `RUST_LOG=fermut=debug`.                            |
| `ANTHROPIC_API_KEY`  | Required by `fermut explain --llm`, `fermut suggest` and `fermut autofix` (unless `FERMUT_LLM_MOCK=1`). Falls back to `FERMUT_ANTHROPIC_API_KEY`.        |
| `GH_TOKEN`           | Read by `gh` for `fermut pr-comment`. Required in CI; `${{ secrets.GITHUB_TOKEN }}` is sufficient.                                    |
| `GITHUB_REPOSITORY`  | Default for `fermut pr-comment --repo`. Always set on GHA runners.                                                                   |
| `PR_NUMBER`          | First fallback for `fermut pr-comment --pr` when the flag is omitted. Checked before `GITHUB_REF`.                                     |
| `GITHUB_REF`         | Second fallback for `fermut pr-comment --pr`: parsed when it is a `refs/pull/<N>/merge` or `refs/pull/<N>/head` ref. Always set on GHA runners.               |
| `GITHUB_ACTIONS`     | When `"true"`, `fermut run` emits `::error` / `::warning` annotations. `--annotate` turns them on outside GHA; there is no flag to turn them off inside GHA. |

## See also

- **[CLI](cli/index.md)** — every flag.
- **[Configuration](configuration.md)** — every TOML key.
