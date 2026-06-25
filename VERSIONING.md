# Versioning policy

fermut follows [Semantic Versioning 2.0.0](https://semver.org/) for the
**user-facing contract** described below. Internal Rust APIs (anything not
in this document) are not part of the contract — fermut ships as a
binary, not a library.

## Pre-1.0 rules (current)

While the major is `0`, SemVer minor/patch numbers shift one slot:

| Bump | Triggered by |
|------|--------------|
| `0.MINOR` | Anything that would be a **major** under post-1.0 rules (breaking changes to the user-facing contract). |
| `0.MINOR.PATCH` | Anything that would be a **minor or patch** under post-1.0 rules (new features, bug fixes, doc-only changes). |

We will declare `1.0.0` once the contract has been stable for two
consecutive minor releases without unplanned breakage.

## Post-1.0 rules (future)

Standard SemVer:

| Bump | Triggered by |
|------|--------------|
| `MAJOR` | A breaking change to the user-facing contract below. |
| `MINOR` | A backward-compatible addition (new flag, new operator, new output format, new filter). |
| `PATCH` | A backward-compatible bug fix or doc fix. |

## What counts as the user-facing contract

### CLI surface

Stable:

- Subcommand names (`run`, `list`, `show`, `clean`, `completions`).
- Existing flag names, short forms, value parsers, and defaults.
- Exit codes (`0` success / no survivors; `1` survivors exist; `>1` infra
  error).
- The structure of `--help` output is **not** stable — only the
  command/flag names are.

Breaking changes include:

- Renaming or removing a subcommand or flag.
- Tightening accepted value formats (e.g. `--shard "1/4"` → `--shard 1,4`).
- Changing a flag's default to a value that flips behavior.
- Changing exit code semantics.

### Config file schema

Stable: the set of keys in `[tool.fermut]` (and the top-level
`fermut.toml` equivalent), their types, and their meaning.

Breaking changes include:

- Renaming an existing key.
- Tightening the accepted value (e.g. accepting only `pytest` or
  `unittest` when previously a free-form string).
- Changing a default to a value that flips behavior.

Adding a new optional key is **non-breaking** (file is parsed with
`deny_unknown_fields`, but old files have no business setting a key
they've never seen).

### Operator names

Stable: the kebab-case operator names emitted by `Operator::name()` and
shown in `--ops` / `--skip-ops`, JSON reports, and HTML/JUnit/Markdown
output (`arith-op-swap`, `boundary-shift`, etc.).

Breaking changes include:

- Renaming an operator (e.g. `arith-op-swap` → `binop-swap`).
- Moving an operator between stable and experimental tiers.
- Changing the `exp:` prefix convention.

Adding a new operator is **non-breaking**.

Removing an operator is **breaking**.

### Public enums marked `#[non_exhaustive]`

These enums are explicitly open for extension and may **add new variants
in any minor release**:

- `MutantOutcome` — the `status` discriminant in JSON reports.
- `Operator` — the operator catalog.
- `RunnerKind` — `pytest`, `unittest`, future runners.
- `ReportFormat` — `human`, `json`, future stdout formats.
- `ConfigSource` — `none`, `fermut.toml`, `pyproject.toml`, future config sources.

If you `match` on one of these from another crate, Rust requires a `_ =>`
arm — that's the entire point. New variants then become invisible to your
existing arms and don't break your build. Removing or renaming a variant
remains a **breaking** change.

### Report formats

Stable: the **shape** of the JSON report (the top-level `outcomes` array;
the per-outcome `status` discriminant + nested `mutant` object; field
names and types within `Mutant`).

Breaking changes include:

- Renaming a JSON field.
- Changing a field's type.
- Removing a field.
- Changing the discriminant tag name (`status`) or its set of values.

Adding a new optional field is **non-breaking**. Consumers should ignore
unknown fields.

JUnit XML, HTML, and Markdown formats are **best-effort** — their layout
is documented but not version-locked. Don't parse them; consume the JSON.

### Mutant IDs

Stable: the **format** of `Mutant.id` (`{file}@{byte_offset}:{orig}->{replacement}`).

Caching, sharding, and `fermut show <id>` all depend on this. A
breaking change here invalidates every existing cache file.

### MSRV (Minimum Supported Rust Version)

An MSRV bump is **breaking** under post-1.0 rules. It bumps `MAJOR`. The
declared MSRV is in `Cargo.toml::package.rust-version` and enforced by
the CI `msrv` job.

Pre-1.0, MSRV bumps go in a minor (`0.MINOR`) release.

## What is *not* part of the contract

- The internal Rust API. `pub` items in `src/lib.rs`'s module map are
  public for crate-internal organization, not as a library API.
  `cargo install` users get the binary; nobody depends on
  `crate::engine::run` from another crate. If you do, pin the exact
  `0.x.y` you're using.
- The pinned `ruff` git tag in `Cargo.toml`. Bumping that pin can ship
  in a patch release if behavior is unchanged.
- The set of dependencies in `Cargo.toml`. We may add, remove, or swap
  internal deps freely.
- The internal layout of `.fermut/cache.json`. The cache is
  format-tied to a specific `Mutant.id` shape; bumps that change the ID
  invalidate every cache.
- Log output (`tracing` `info`/`warn`/`debug` lines). Don't parse stderr.
- The exact list of survivors a given mutation operator emits on a given
  Python file. Operator implementations improve over time; the mutation
  *set* may grow or shrink.

## How to propose a breaking change

1. Open an issue describing the change and the impact.
2. Get explicit approval from the Kovant AB maintainers.
3. Land the change behind a deprecation period when possible:
   - New flag/key shipped alongside the old one for at least one minor
     release.
   - Old flag/key emits a `warn` log on use.
   - Old flag/key removed in the next major.
4. Document in `CHANGELOG.md` under both the breaking-change release
   (`### Removed`) and any earlier deprecation release (`### Deprecated`).

## Release cadence

There is no fixed cadence. We cut a release when there's a meaningful
shippable change. CI runs on every PR; release workflow runs on every
`v*` tag.
