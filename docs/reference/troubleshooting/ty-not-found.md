# ty not found

**Symptom.** `fermut run` exits with:

```
Error: ty binary not found; install from astral-sh/ty or pass --no-ty-filter

Caused by:
    `ty` not found in the project venv or on PATH
```

or `fermut doctor` reports the `ty` check as `[fail]`.

**Cause.** By default the ty pre-filter runs **in-process** (an
embedded ty checker), and the `ty` binary is optional: a missing binary
only stops the run when the embedded checker is unavailable. That
happens when:

- `FERMUT_TY_EMBEDDED=0` is set, which forces the subprocess path; or
- the embedded checker can't be built for the project (for example, a
  malformed `pyproject.toml`). fermut warns
  `embedded ty checker unavailable; falling back to subprocess` and then
  needs the binary.

`fermut doctor` always treats a missing binary as `[fail]`, even though
an embedded-mode run works without it.

## Fix

Pick one of the three:

**Install ty** (it's the biggest single speed win on type-hinted
codebases):

```sh
uv tool install ty
```

fermut looks in the project venv first, then on `PATH`. `fermut doctor`
confirms it was found.

**Re-enable the embedded path** if you set `FERMUT_TY_EMBEDDED=0`.
Unset it, and the run no longer needs the binary:

```sh
unset FERMUT_TY_EMBEDDED
```

If the warning above says the embedded checker failed to build, fix
the project config it names, or install the binary so the subprocess
fallback works.

**Disable the ty filter** if you accept the ~20–40% extra mutants
that ty would have caught:

```sh
fermut run --no-ty-filter
```

Or persist it in `fermut.toml`:

```toml
ty_filter = false
```

## Why no silent no-op

The ty filter rejects ~20–40% of mutants on typical type-hinted
projects (type errors the patched code introduces). If fermut silently
ran without it, `fermut run` would be ~1.5–2× slower with no
indication anything was wrong, which is easy to miss on a CI
dashboard. So when neither the embedded checker nor the binary is
available, fermut fails loudly and makes you choose: install ty, or
opt out explicitly.
