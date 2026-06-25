# Versioning

fermut is **pre-1.0**. We follow a semver-ish convention with the
following clarifications:

- **`0.MINOR.PATCH`** — bumps the minor for breaking changes,
  bumps the patch for non-breaking changes.
- **Breaking changes** can land in any `0.MINOR` bump, listed in
  `CHANGELOG.md`. We try to give one minor's worth of deprecation
  warnings when feasible.
- **JSON report shape** is treated as API and follows the same
  contract — new optional fields can land in a patch; required
  field renames / removals are minor bumps.
- **CLI flag names** follow the same rule. Removing or renaming a
  flag is a minor bump.

Once fermut hits **1.0**, the contract tightens to standard semver
— breaking changes require a major bump.

For the up-to-date contract see the [versioning
policy](https://github.com/KovantAI/fermut/blob/main/VERSIONING.md)
on GitHub.
