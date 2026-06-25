# Supported platforms

Wheels are built and published for:

- Linux x86_64 / aarch64
- macOS x86_64 / arm64
- Windows x86_64

Source builds work anywhere the Rust toolchain supports. We accept
PRs to broaden the wheel matrix.

Wheels are built via `maturin` with `bindings = "bin"` — each wheel
packages the prebuilt Rust binary as a console script. **No Rust
toolchain required on the install side.**
