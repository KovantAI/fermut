# Caching

The result cache is what makes iterative use of fermut viable. This
page explains the model. The CI / agent ergonomics live in
**[Coding agents → cache strategy](../guides/coding-agents.md#cache-strategy)**.

![How the cache decides: each entry is keyed on the mutant id, the source hash and the scope. An exact match reuses the verdict; otherwise a kill is reused if its killing test is unchanged and still covers the line, with about 5% re-run as an audit; otherwise the mutant runs and its verdict is stored under the new key.](../assets/cache-lookup.svg)

## What the cache stores

After every successful mutant evaluation, fermut writes one entry to
`.fermut/cache.json`:

```
(key, ast_hash(source_file), scope) → outcome
```

- `key` says which mutant: its file, operator and edit, and where it
  sits. Inside a top-level `def` or `class` that is the offset from the
  start of that def (`src/calculator.py::total@41:boundary-shift:<=-><`),
  so an edit above the def, or inside another function, doesn't move
  it. At module level it is the
  [`mutant.id`](glossary.md#mutant-id), whose byte offset is from the
  start of the file. Reports always show the `mutant.id`.
- `ast_hash(source_file)` is the **AST-structural** hash of the
  file containing the mutant. Whitespace, comments, and pure
  formatting changes don't change the AST hash (see
  [Why AST-hash](#why-ast-hash-not-mtime-or-raw-bytes)). With
  `cache_scope = "scope"`, this narrows to the enclosing top-level
  def/class instead of the whole file, so editing one function keeps
  the cached verdicts of its siblings (see
  [`cache_scope`](../reference/configuration.md#schema)).
- `scope` captures every input that could change the outcome without
  touching the AST: runner kind (with or without schemata: the verdicts
  match, so they share the cache), interpreter, timeout, hypothesis
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
`src/`): `cache.json` (this cache), `history.jsonl` (trend log),
`kill-order.json` (smart test ordering's kill history),
`ty-cache.json` (the ty pre-filter's baseline-diagnostic cache), plus
`dominators.json` and `llm-cache.json` once you use `fermut subsume`
or the LLM-backed commands.
fermut does not write a `.gitignore` for it — add `.fermut/` to your
own `.gitignore` (or commit only `history.jsonl`, see
[Projects](projects.md)).

On the next `run`:

1. For each mutant, fermut computes the current `ast_hash` and the
   current run-shape `scope`.
2. If `(key, current_ast_hash, current_scope)` matches a
   cached entry, the cached outcome is reused — **no pytest
   invocation**.
3. If either component mismatches, the entry is ignored and the
   mutant re-runs; the new outcome overwrites the entry for that key.
   Entries whose key no longer exists (the mutant moved within its
   function, or vanished) are never pruned — they accumulate until
   `fermut clean`.

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

### Killed verdicts: keyed on the killing test

A `killed` verdict only depends on the test that killed it. When fermut
knows that test (pytest/rstest with coverage contexts — the runner reads
it from pytest's summary, or it's the only selected test), the entry
also stores a **killer scope**: the killer's node id plus the
fingerprint of *that test*, its `conftest.py` chain, and the shared
support files (see [Test fingerprints](#test-fingerprints)). If the full key misses — say you edited another test
file that also covers the mutant — the kill is still reused as long as:

- the mutant's source hash is unchanged;
- the killing test still covers the mutant's line;
- the killer scope is unchanged.

Editing the killing test, a non-test statement in its file (or its
conftest chain, or a support file) re-runs the mutant. `survived` and `timed_out` verdicts never take
this path: a survivor depends on *every* covering test (see the next
section for how it is re-checked).

Trusting a killer hit skips the run entirely, so each run **audits** a
random sample: `ceil(5%)` of the killer hits (at least one) are
re-run against the killing test alone. If the kill doesn't reproduce —
a flaky test, or one that only failed because of state an earlier test
left behind — fermut drops the entry, warns naming the test and the
mutant, and re-runs the mutant fully. Tune with `--cache-audit-rate` /
`cache_audit_rate`; `--no-cache-audit` turns it off for deterministic
benchmarks. The end-of-run `cache:` log line counts full hits, killer
hits, audits (and drops), and misses.

| Change                                         | killed (killer known) | survived / timed_out |
|------------------------------------------------|-----------------------|----------------------|
| Edit another covering test file                | **kept**              | see below            |
| Add or edit another test in the killer's file  | **kept**              | see below            |
| Edit the killing test                          | invalidated           | see below            |
| Edit an import/fixture/helper in its file      | invalidated           | see below            |
| `conftest.py` above the killing test           | invalidated           | see below            |
| Coverage no longer lists the killing test      | invalidated           | see below            |

### Survived verdicts: only new or changed tests run

A `survived` verdict means every covering test passed. When fermut
knows those tests (pytest/rstest with coverage contexts, every covering
file inside the tests tree), the entry also stores them with a
fingerprint of each one (see [Test fingerprints](#test-fingerprints)),
its `conftest.py` chain and the shared support files. If the full key
misses because the covering tests changed, the mutant runs against
**only** the covering tests that are new or changed, as long as the source hash and the run
settings are unchanged:

- an unchanged test passed before and still passes;
- a test that no longer covers the mutant can't kill it, so if tests
  were only removed, the survivor is reused without running anything;
- a new or edited test is the only thing that can kill it now.

This is the inner loop's common case: you add a test for a survivor and
re-run, and fermut runs that test, not the whole covering set again.
A support-file edit changes every file's fingerprint, so every covering
test runs, as before. An `equivalent` verdict takes the same path.

`timed_out` verdicts don't: editing the test that hung can turn a
timeout into a survivor, so they re-run fully.

The new tests run without the old ones before them, so a test that only
fails after another one changed shared state is missed. A sample is
**audited**, at the same `--cache-audit-rate` as killer hits (and always
the first one in a run): it also runs against every covering test, and
that verdict is the one reported. When the two disagree fermut warns,
naming the mutant and the new tests. The end-of-run `cache:` line
counts survivor deltas, how many tests they ran out of how many the full
covering sets hold, and the audits.

| Change                                         | survived (tests known)        | timed_out   |
|------------------------------------------------|-------------------------------|-------------|
| New test covering the mutant's line            | runs that test only           | invalidated |
| Edit a covering test                           | runs that test only           | invalidated |
| Edit an import/fixture/helper in a test file   | runs that file's tests only   | invalidated |
| `conftest.py` above a covering test            | runs the tests below it only  | invalidated |
| Non-test file under the tests tree             | runs every covering test      | invalidated |
| A test stops covering the line (or is deleted) | **kept**, nothing runs        | invalidated |

### Test fingerprints

Killer scopes and survivor bases fingerprint each test on its own, not
its whole file. A test (a `test*` function, or a `test*` method of a
`Test*` class) counts:

- its own source, decorators included;
- every statement in its file that isn't a test: imports, constants
  (`parametrize` data), fixtures, helpers, `pytestmark`;
- for a method, its class's header (decorators, bases) and every
  non-test member (`setup_method`, class attributes, helpers);
- any other test it names, whole: a base class, a test it calls.

Other tests in the file don't count, so adding a test next to a killer
keeps its kills. Comments between statements don't count either. An
inherited method (`class TestB(TestA)` running `TestA.test_m`) counts
its whole class and the bases it names. A file that doesn't parse, or
a test that isn't defined there (generated, imported), counts the
whole file. Names follow pytest's defaults (`test` prefix, `Test`
classes); tests matched by a custom `python_functions` count as shared
statements, which only re-runs more.

Not tracked: another test's decorator runs at import, so one that
changes module state or breaks collection doesn't invalidate. The
killer audit sample still re-runs kills that don't reproduce.

Every `conftest.py` above the tests root (say a repo-root one with
`tests/` as the tests root) loads for every test, so both modes fold it
in: editing, adding or removing one invalidates every entry.

Not tracked, in either mode: a helper module outside both the tests
tree and the mutated source.
The scoped fingerprint also misses a test module importing *another
test module* (`from tests.test_a import helper`) — keep shared helpers
in non-test files so they count as support files. Run `fermut clean`
if you hit one of these.

## What's cached

| Outcome    | Cached?         | Why                                                                                  |
|------------|-----------------|--------------------------------------------------------------------------------------|
| killed     | ✓               | Same patched bytes + same killing-test bytes → same outcome.                          |
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

- Whitespace or comment edits outside a mutant's function, above or
  below it — same AST, same key → hit.
- Agent run #1 → agent run #2 on the same commit.
- Cache restored from `actions/cache` in a fresh GHA runner.

It does **not** survive a reformat or comment edit *inside* the
mutant's own function above the mutant: that moves its offset within
the function, so the key changes and it re-runs (the old entry stays in
the file as an orphan). Module-level mutants are keyed from the start of
the file, so any edit above them does the same.

The cost is one parse + hash per source file per run. On a typical
project this is in single-digit milliseconds.

## File format and compatibility

`cache.json` has no version field. New entry fields are added with
`#[serde(default)]`, so older entries load and simply miss when a new
key component doesn't match. A file that fails to parse is moved
aside to `cache.corrupt-<pid>` with a warning, and the run starts from
an empty cache.

## Where next

- **[Coding agents → cache strategy](../guides/coding-agents.md#cache-strategy)**
  — CI / agent persistence patterns and the recommended GHA setup.
- **[Internals](../reference/internals/index.md)** — code-level view of
  the cache module.
