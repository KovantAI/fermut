# ty not found

**Symptom.** `fermut run` exits with an error mentioning `ty` —
typical wording: `ty binary not found on PATH` or, in embedded mode,
a `ty_project` construction failure.

**Cause.** The ty pre-filter is **required when enabled** (which is
the default). It does not silently degrade to a no-op — if ty can't
be located (or, in embedded mode, can't build a `ProjectDatabase`),
the run aborts before any mutant is tested. `fermut doctor` flags
this as a `[fail]` check.

## Fix

Pick one of the three:

**Install ty** (recommended — it's the biggest single speed win on
type-hinted codebases):

```sh
uv tool install ty
```

`fermut doctor` confirms it's on PATH after install.

**Force the subprocess path** if the embedded checker is the one
failing (pre-release crate; the embedded path can refuse to build
in unusual project layouts):

```sh
FERMUT_TY_EMBEDDED=0 fermut run
```

**Disable the ty filter** if you accept the ~20–40% extra mutants
that ty would have caught:

```sh
fermut run --no-ty-filter
```

Or persist it in `fermut.toml`:

```toml
ty_filter = false
```

## Why no silent fallback

The ty filter rejects ~20–40% of mutants on typical type-hinted
projects (type errors the patched code introduces). Silently
running without it would make `fermut run` ~1.5–2× slower with no
indication anything was wrong — easy to never notice on a CI dashboard.
Failing loudly forces the choice: install ty, or opt out explicitly.
