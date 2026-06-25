# Minimum Supported Rust Version

fermut declares `rust-version = "1.83"` in `Cargo.toml`. Builds on
earlier toolchains fail with a clear cargo message. CI runs a
dedicated `msrv` job on every PR (`cargo check --locked` on 1.83) to
catch accidental use of post-MSRV features.

**`Cargo.lock` is committed** — it pins exact dependency versions
for reproducible builds and is what the MSRV job uses (`--locked`).
Do not delete it. `cargo update` produces lockfile churn that should
land in its own PR.

## Bump policy

- Bump only when a required dependency forces it (typically a
  `ruff_*` tag upgrade or a hard-pinned `clap` / `toml` major).
- Bump in a dedicated PR, never alongside feature work.
- Bump `Cargo.toml::package.rust-version` **and** the `toolchain:`
  line in `.github/workflows/ci.yml::msrv` together — they're the
  source of truth for the contract.
- Mention the bump in `CHANGELOG.md` as a breaking change.
