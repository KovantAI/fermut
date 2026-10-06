# Caching

The result cache is what makes iterative use of fermut viable. This
page explains the model. The CI / agent ergonomics live in
**[Coding agents → cache strategy](../guides/coding-agents.md#cache-strategy)**.

## What the cache stores

After every successful mutant evaluation, fermut writes one entry to
`.fermut/cache.json`:

```
(mutant.id, ast_hash(source_file), scope) → outcome
```

- [`mutant.id`](glossary.md#mutant-id) is the deterministic
  identifier for one specific application of one operator to one
  source location — `<file>@<byte-offset>:<operator>:<original>-><replacement>`
  (e.g. `src/calculator.py@216:boundary-shift:<=-><`).
- `ast_hash(source_file)` is the **AST-structural** hash of the
  file containing the mutant. Whitespace, comments, and pure
  formatting changes don't change the AST, so a `ruff format` pass
  doesn't invalidate the cache. With `cache_scope = "scope"`, this
  narrows to the enclosing top-level def/class instead of the whole
  file, so sibling-function edits keep cache hits intact (see
  [`cache_scope`](../reference/configuration.md#schema)).
- `scope` captures every input that could change the outcome without
  touching the AST: runner kind, interpreter, timeout, hypothesis
  seed, pytest args, the per-mutant test selection set when
  coverage-driven selection is on, and a fingerprint of the tests
  that run against the mutant (see
  [Test-suite invalidation](#test-suite-invalidation)). A mismatch on
  any of those invalidates the entry — a stale `survived` from a
  narrow `--coverage` run can't poison a later full-suite run, and
  vice versa.

All of fermut's sidecar state lives in one `.fermut/` directory at the
**project root** (nearest `pyproject.toml`/`setup.cfg`, so a
`source_root = "src"` layout still anchors at the repo root, not under
`src/`): `cache.json` (this cache), `history.jsonl` (trend log), and
`ty-cache.json` (the ty pre-filter's baseline-diagnostic cache). The
whole directory is gitignored by default.

On the next `run`:

1. For each mutant, fermut computes the current `ast_hash` and the
   current run-shape `scope`.
2. If `(mutant.id, current_ast_hash, current_scope)` matches a
   cached entry, the cached outcome is reused — **no pytest
   invocation**.
3. If either component mismatches, the entry is ignored and the
   mutant re-runs. Stale entries are overwritten on the next save.

Result: editing one function in one file only pays for mutants
inside that function (under `cache_scope = "scope"`) or that file
(default `cache_scope = "file"`). Everything else is reused.

## Test-suite invalidation

Editing a test can flip a verdict without touching the source — that's
the whole point of writing a killing test. So each entry's `scope`
folds in a content fingerprint of the tests. *Which* tests depends on
whether fermut knows exactly what runs against the mutant.

**With coverage contexts** (`pytest` or `rstest` runner), the
fingerprint covers only:

- the test files holding the mutant's covering node ids;
- every `conftest.py` from the tests root down to each of those files'
  directories (a sibling directory's `conftest.py` is not included);
- every shared support file under the tests tree — anything that is
  neither a test module (`test_*.py` / `*_test.py`) nor a
  `conftest.py`: helpers, `__init__.py`, fixture data. Conservative:
  changing one invalidates every entry.

The covering node-id set is part of the key too, so a new test that
starts covering the mutant's line invalidates it even if no existing
file changed.

**Otherwise** the fingerprint covers the whole tests tree, and any
test edit invalidates every entry. That fallback applies with no
coverage file, under the `unittest` runner (it runs the full suite
regardless of coverage), for a mutant with no recorded context (the
runner sweeps the whole tests dir), and when a covering file lies
outside the tests tree or couldn't be read.

| Change                                         | With coverage    | Without coverage |
|------------------------------------------------|------------------|------------------|
| Edit/add a test that doesn't cover the mutant  | **kept**         | invalidated      |
| Sibling-directory `conftest.py`                | **kept**         | invalidated      |
| Edit a covering test file                      | invalidated      | invalidated      |
| `conftest.py` above a covering test            | invalidated      | invalidated      |
| Non-test file under the tests tree             | invalidated (all)| invalidated      |
| New test covering the mutant's line            | invalidated      | invalidated      |

Not tracked, in either mode: a helper module outside both the tests
tree and the mutated source, and a `conftest.py` above the tests root.
The scoped fingerprint also misses a test module importing *another
test module* (`from tests.test_a import helper`) — keep shared helpers
in non-test files so they count as support files. Run `fermut clean`
if you hit one of these.

## What's cached

| Outcome    | Cached?         | Why                                                                                  |
|------------|-----------------|--------------------------------------------------------------------------------------|
| killed     | ✓               | Same patched bytes + same test bytes → same outcome.                                  |
| survived   | ✓               | Same.                                                                                |
| timed_out  | ✓               | Same.                                                                                |
| skipped    | ✗               | Filter chain may differ between runs (different `--ops`, `--coverage`, etc.) — reusing a skip would be wrong. |
| errored    | ✗               | Often transient (flaky import, missing env var).                                      |

## When to clear it

`fermut clean` evicts the cache. It **preserves
`history.jsonl`** — so trend tracking keeps its continuity even
across cache rotations.

Clear when:

- You upgraded fermut and want every mutant re-evaluated against new
  operator behavior. (The loader is forward-compatible, but stale
  entries reflect old behavior.)
- A flaky external dependency was fixed and you want the
  re-evaluation.
- You're debugging a "phantom survivor" that doesn't reproduce by
  hand.

**Don't** clear it as a habit. The cache is the reason the inner
loop is fast.

## Why AST-hash, not mtime or raw bytes

mtime is unreliable across machines (git checkout doesn't preserve
it; `cp -a` does, but most tools don't). A raw-byte sha256 fixes
that but still invalidates on every formatting change — a `ruff
format` pass would re-run every mutant in the project despite the
AST being identical.

The structural AST hash makes cache hits identical across:

- Reformats, whitespace edits, comment edits — same AST → same hash.
- Local dev → CI hand-off.
- Agent run #1 → agent run #2 on the same commit.
- Cache restored from `actions/cache` in a fresh GHA runner.

The cost is one parse + hash per source file per run. On a typical
project this is in single-digit milliseconds. The legacy raw-byte
hash is still exported as `cache::hash_file` for callers that need
byte identity (and for the test suite).

## Version field

The cache file carries a `version` integer. Older readers fail closed
(treat the cache as empty). Newer readers handle forward-compat with
`#[serde(default)]` on new fields. You won't normally notice this —
worst case is a one-time cache-miss after upgrading fermut.

## Where next

- **[Coding agents → cache strategy](../guides/coding-agents.md#cache-strategy)**
  — CI / agent persistence patterns and the recommended GHA setup.
- **[Internals](../reference/internals/index.md)** — code-level view of
  the cache module.
