# Filters

The filter chain is what makes fermut fast. Every mutant runs through
a stack of cheap checks before any pytest invocation. This guide
covers the practical use of each filter — when to enable, what to
expect, where it lives in config.

The order (cheap → expensive) is fixed:

```
experimental → operator allow/deny → shard → sample → diff/since → coverage → ruff → ty
```

`shard` only runs when `--shard i/n` is set (CI matrix split); it
partitions the surviving mutants by id hash so each worker handles
a disjoint slice. Paths excluded via `--exclude` / `exclude = [...]`
are pruned **before** the chain — they never enter the filter
pipeline.

See **[Mutation testing → the filter chain](../concepts/mutation-testing.md#the-filter-chain)**
for the conceptual model.

## `--ops` and `--skip-ops`

Allowlist + denylist for operators. `--skip-ops` wins over `--ops`.

```sh
# Start narrow; widen as score climbs.
fermut run src/ --ops arith-op-swap,compare-op-swap,boundary-shift

# Everything except a noisy operator.
fermut run src/ --skip-ops number-shift
```

Config:

```toml
ops = ["arith-op-swap", "compare-op-swap", "boundary-shift"]
skip_ops = ["number-shift"]
```

Useful when:

- You're **starting small** and want to grow the set as tests
  improve.
- One operator generates high false-positive noise (e.g.
  `string-sentinel` on internal sentinel strings).
- You're targeting a class of bug (auth → `compare-op-swap` +
  `boundary-shift`).

See the **[operators catalogue](../reference/operators/index.md)** for
every available name.

## `--experimental`

Enables the four experimental operators (`exp:exception-class-swap`,
`exp:bare-except`, `exp:zero-iteration-for-loop`,
`exp:one-iteration-for-loop`). Off by default — they tend to
produce equivalent mutants or behavior the test suite can't usefully
distinguish.

```sh
fermut run src/ --experimental
```

The `nightly` profile enables these; the others don't.

## `--diff-only`

Restrict mutants to lines changed vs. a base ref. **The right
default for PR gates.**

```sh
fermut run src/ --diff-only main
fermut run src/ --diff-only origin/${{ github.base_ref }}
```

Config:

```toml
diff_only = "main"
```

`--diff-only` is a **three-dot** diff (`git diff base...HEAD`): it
scopes to the lines this branch changed relative to the merge base with
`base` — i.e. **committed** changes only. Uncommitted working-tree edits
are **not** included (use `--since` for those). This is the right
semantics for a PR gate, where the PR head is committed and you want the
diff against where the branch forked from `base`, unaffected by `base`
advancing after the fork.

## `--since`

For "what's changed since I last ran fermut?" or "test against a
specific tag":

```sh
fermut run src/ --since v1.2.0
fermut run src/ --since HEAD~10
fermut run src/ --since abc123
fermut run src/ --since 2026-05-01
fermut run src/ --since '1 week ago'
```

`SPEC` is resolved as a git ref first, then (on failure) as a date
that `git log --before=<SPEC>` accepts.

Unlike `--diff-only`, `--since` is a **two-dot** diff (`git diff
<spec>`): it compares the working tree against `<spec>`, so
**uncommitted edits are included**. Use `--since` when you want local,
not-yet-committed changes scoped in (the inner dev loop); use
`--diff-only` for a committed branch-vs-base gate.

`--diff-only` and `--since` are mutually exclusive.

## `--exclude`

Prune files from mutation collection entirely. Patterns are globs
matched against paths relative to `source_root` (not project root).
Excluded files never reach the filter chain, so this is cheaper
than skipping mutants downstream.

```sh
fermut run src/ \
    --exclude 'alembic/**' \
    --exclude 'tests/integration/**' \
    --exclude '**/migrations/*.py'
```

Config:

```toml
exclude = ["alembic/**", "tests/integration/**", "**/migrations/*.py"]
```

CLI `--exclude` (when passed at least once) replaces the config
`exclude` list — no merge. Empty CLI keeps the config list intact.

Useful for:

- Auto-generated code (alembic migrations, protobuf stubs).
- Vendor / third-party trees checked into the source tree.
- Integration test directories where mutation testing is too coarse.

Prefer this over inline `# fermut: ignore-file` markers when you can
describe the excluded set with a glob — one config entry beats
sprinkling markers across hundreds of files.

## `--coverage`

Per-test selection — narrow which tests run per mutant. See the
dedicated **[Coverage guide](coverage.md)**.

```sh
fermut run src/ --coverage .coverage
```

fermut reads coverage.py's native `.coverage` SQLite database directly
and sniffs the file's format, so a legacy `coverage.json` export works
here too. With a `.coverage` at the project root the flag is optional —
fermut auto-discovers it (precedence: `--coverage` > config
`coverage = "…"` > auto-discovered `.coverage` > none).

## `--sample`

Random subset of mutants. Useful for local dev cycles when you
want to iterate fast and don't need full signal.

```sh
fermut run src/ --sample 0.25 --sample-seed 0
```

Deterministic per `--sample-seed`. The `local` profile uses
`sample = 0.25, sample_seed = 0`.

## `--ruff-filter`

Skip mutants that introduce new ruff lints. Off by default.

```sh
fermut run src/ --ruff-filter
```

Lower-value than the ty filter — ruff lints are advisory, so
"introduces a lint" doesn't always mean "the mutant is broken".
Enable when you want maximum noise reduction.

## `--no-ty-filter`

Disables the ty pre-filter. **Do not turn this off unless you have
a specific reason** — it's the single biggest speed win on
type-hinted codebases (20–40% of naive mutants are type-invalid).

```sh
fermut run src/ --no-ty-filter
```

Off-switch for environments where `ty` isn't installable.

### Embedded vs subprocess ty (default: embedded)

fermut runs ty in-process by default, using the `ty_project` crate.
On a 9,762-mutant project this is ~13× faster than spawning a `ty`
subprocess per mutant (10.86s vs 141.72s cold, 4.97s vs 142.23s warm
on the same hardware). The first run after a fermut upgrade may take
longer while typeshed warms.

Force the legacy subprocess path for debugging:

```sh
FERMUT_TY_EMBEDDED=0 fermut run src/
```

Use this only when you suspect the embedded checker is misclassifying
a mutant. Cross-platform parity on agent-platform-backend (29,792
mutants): embedded's reject set is a strict subset of subprocess's —
the only divergence is 156 mutants in `alembic/versions/` that the
subprocess path over-rejected because tempfile invocations couldn't
resolve project imports. Embedded surfaces strictly more
mutation-testing signal than the subprocess path; there is no known
case where embedded over-rejects.

### ty result cache

ty verdicts persist to `.fermut/ty-cache.json` so unchanged mutants
short-circuit on subsequent runs — that's where the warm-run number
above comes from. Cache key is
`(ty_version, ast_hash(file), mutant_range, replacement)`:

- AST hash invalidates on any structural source change.
- ty version invalidates on `ty` binary upgrade.
- Schema bumps drop the cache entirely.

No flag — the cache is always on. Delete `.fermut/ty-cache.json` to
force a cold rerun.

## Inline ignore markers

Drop mutations on specific lines without changing flags:

```python
x = compute() + offset    # fermut: ignore
y = a if flag else b      # fermut: ignore[arith-op-swap, bool-op-swap]
return value or fallback  # fermut: ignore[bool-op-swap]
```

Use for known-equivalent mutants or boundary cases your tests
intentionally don't probe. Prefer this over `--skip-ops` when the
problem is one specific line, not a whole operator.

See the **[operators reference → inline ignore markers](../reference/operators/inline-ignore.md)**
for the full syntax.

## Composing filters in CI

The PR-gate sweet spot is the full stack:

```sh
fermut run src/ --tests tests/ \
    --diff-only origin/main \
    --coverage .coverage \
    --ops arith-op-swap,compare-op-swap,boundary-shift,return-value-to-none \
    --timeout 15
```

This is what `fermut init --profile pr-gate` writes for you.

## Where next

- **[Operators](../reference/operators/index.md)** — every operator name +
  example.
- **[Coverage](coverage.md)** — the per-test context setup.
- **[Configuration](../concepts/configuration.md)** — file vs CLI
  precedence.
