# `fermut mcp`

Run a [Model Context Protocol](https://modelcontextprotocol.io) server over
stdio, exposing fermut to a coding agent as native tools.

```sh
fermut mcp
```

The command speaks newline-delimited JSON-RPC 2.0 on stdin/stdout — one JSON
object per line. It is **launched by an MCP client** (Claude Code, Cursor,
your own scaffolding), not run interactively. stdout is the protocol
channel, so all logs and progress go to stderr.

There are no flags: each tool takes its arguments through the `tools/call`
request. Per-project settings (source root, tests, coverage, operators)
come from the project's `fermut.toml`, so the tools honor the same config a
`fermut run` would.

## Why use it

Without the server, an agent shells out to `fermut run`, writes JSON to a
file, and parses it with `jq`. The MCP server makes those steps native tool
calls with structured results — no CLI scraping, no temp-file juggling. It
exposes the same JSON shapes the CLI subcommands emit.

## Tools

| Tool                     | Mirrors                              | Arguments                                                        |
|--------------------------|--------------------------------------|------------------------------------------------------------------|
| `fermut_doctor`          | [`doctor`](doctor.md)                | `path?`                                                          |
| `fermut_run`             | [`run`](run.md)                      | `path?`, `tests?`, `coverage?`, `since?`, `diff_only?`, `jobs?`, `timeout?`, `max_time?`, `python?`, `report_path?` |
| `fermut_next`            | [`next`](next.md)                    | `report` (required), `limit?`, `max_tokens?`                     |
| `fermut_explain`         | [`explain`](explain.md)              | `report` (required), `target` (required), `context?`, `tests?`, `coverage?` |
| `fermut_score`           | [`score`](score.md)                  | `path?`, `baseline?`, `branch?`                                  |
| `fermut_list_survivors`  | [`show`](show.md) (list mode)        | `report` (required)                                              |

`fermut_explain` runs the heuristic path only (no LLM call) — the agent is
itself a model with the repo in context, so it gets the hint, source
context, coverage signal, and pytest skeleton, and writes the test itself.
`fermut_doctor` returns a `checks` array plus a `healthy` flag (false when
any check failed); run it before `fermut_run` to catch a missing pytest /
coverage / ty early.

`fermut_run` writes its JSON report (default `<project>/.fermut/last.json`)
and appends to the history log, so a typical loop is:

1. `fermut_doctor` → confirm the environment is ready.
2. `fermut_run` → mutate, get the summary + `report_path`.
3. `fermut_next` on that `report_path` → the highest-value survivor to fix.
4. `fermut_explain` on that survivor → hint + killing-test skeleton.
5. write the test, `fermut_run` again.
6. `fermut_score` → the reward delta vs the prior run.

## Why no `fermut_autofix` / `fermut_suggest` tool { #no-generation-tools }

There are **two ways to create a killing test**, and they're for two
different drivers:

| Driver | Who writes the test | How |
|--------|---------------------|-----|
| **Non-LLM** (CI job, shell script, human at a terminal) | fermut, via its built-in Anthropic client (`ANTHROPIC_API_KEY`) | [`fermut autofix`](autofix.md) — generate → verify (mutant dies + suite green) → keep or revert. [`fermut suggest`](suggest.md) generates without verifying. |
| **LLM agent** (over this MCP server) | the agent itself — it *is* a model with the repo already in context | `fermut_next` → `fermut_explain` (hint + skeleton) → agent writes the test → `fermut_run` to confirm the kill + green suite |

So `autofix` and `suggest` are deliberately **not** MCP tools. Exposing them
would make the agent pay for a *second, weaker* model call — fermut's
`claude-sonnet-4-6` with a narrow prompt — when the agent's own model has
the whole repository loaded and writes a better test for free. The agent
already reproduces autofix's two halves: generation (its own model) and
verification (`fermut_run` re-runs the mutant and the suite). Same outcome —
a proven, ready-to-commit test — without the redundant round-trip.

The MCP server therefore exposes the *inputs* to test-writing (`next`,
`explain`) and the *verifier* (`run`), but never a model call of its own.

## Error model

Protocol-level problems (unknown method, malformed request, missing
`tools/call` name) come back as JSON-RPC `error` objects with standard
`-32xxx` codes. **Tool-execution** failures (report not found, no history
yet) come back as a *successful* `tools/call` result with `isError: true`
and the message in the text content — so the agent reads the failure as
data and can recover, rather than treating it as a transport fault.

## Configuring a client

See **[Integrations → MCP](../../guides/integrations.md#mcp-server)** for a
Claude Code / Cursor setup snippet.
