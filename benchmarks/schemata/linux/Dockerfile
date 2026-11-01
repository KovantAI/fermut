# Linux parity box for --schemata: the repo is mounted at /fermut; fermut is
# built into /work/target, fixtures and venvs live in /work/fixtures (a volume),
# so nothing here touches the macOS build or fixtures.
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential ca-certificates curl git pkg-config libssl-dev python3 \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:/root/.local/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
RUN curl -LsSf https://astral.sh/uv/install.sh | sh && uv python install 3.13

ENV CARGO_TARGET_DIR=/work/target FX=/work/fixtures
WORKDIR /fermut
