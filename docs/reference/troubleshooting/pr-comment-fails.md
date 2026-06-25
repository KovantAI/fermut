# PR comment fails: `gh: command not found`

**Symptom.** The PR comment step fails in CI.

**Cause.** `gh` is required for `pr-comment`. It's preinstalled on
GitHub Actions runners but not in self-hosted / custom
environments.

## Fix

Install `gh` (<https://cli.github.com>) on the runner.
