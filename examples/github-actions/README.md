# fermut · GitHub Actions examples

Copy-paste-ready workflows for the common patterns:

| File | Trigger | Purpose |
|------|---------|---------|
| [`pr-gate.yml`](pr-gate.yml) | every PR | Fail the build if a mutant survives on changed lines. Posts a Markdown summary as a PR comment. |
| [`sharded-matrix.yml`](sharded-matrix.yml) | every push to `main` | Full mutation sweep distributed across 8 runners, then merged into one report. |
| [`nightly-full-sweep.yml`](nightly-full-sweep.yml) | cron, 03:00 UTC daily | Unconditional full sweep with HTML report uploaded as a build artifact. |
| [`incremental-since-tag.yml`](incremental-since-tag.yml) | manual / on tag push | Mutates only what's changed since the last release tag — useful between releases. |

## Installing fermut

fermut publishes to public PyPI. Every workflow installs it the same way:

```yaml
- uses: astral-sh/setup-uv@<sha>
- name: install fermut
  run: uv tool install fermut
```

No registry config, secrets, or `pyproject.toml` index declaration is
needed.

If you'd rather install from source, swap the install step for:

```yaml
- uses: dtolnay/rust-toolchain@<sha>
  with: { toolchain: stable }
- run: cargo install --git https://github.com/KovantAI/fermut --tag v0.1.0
```

## Action SHAs

Every action is pinned to a SHA with a `# vX.Y.Z` comment. Bump via the
fermut repo's Dependabot config or by re-pinning manually:

```sh
git ls-remote --tags https://github.com/<org>/<action> | grep -E 'vX' | tail -3
```

## Notes

The filter flags, output flags, and matrix structure are identical across
all four examples — adapt the `src/` and `tests/` paths to your project
layout and adjust the install step if you prefer the `cargo install --git`
from-source route.
