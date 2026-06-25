# Python versions

fermut supports the same Python versions that the underlying
`ty` / `ruff_python_parser` support. As of writing: **Python ≥ 3.10**.

Dropping a Python version is treated as a breaking change and
follows the standard minor-bump rule. See
**[Versioning](versioning.md)**.

When a Python version reaches end-of-life upstream, we usually drop
it in the *following* minor release, not immediately — gives
projects on the last-supported version time to upgrade.
