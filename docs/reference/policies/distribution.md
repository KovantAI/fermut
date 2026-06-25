# Distribution channels

| Channel               | Status     | Notes                                                |
|-----------------------|------------|------------------------------------------------------|
| Public PyPI           | Stable     | `uv tool install fermut`                              |
| Build from source     | Stable     | `cargo install --git https://github.com/KovantAI/fermut` |

Wheels publish to [PyPI](https://pypi.org/project/fermut/) on every
`v*` tag via trusted publishing (OIDC) — no long-lived tokens.
Prerelease dry-runs publish to TestPyPI from a manual
`workflow_dispatch`.
