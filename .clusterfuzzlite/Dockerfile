FROM gcr.io/oss-fuzz-base/base-builder-rust@sha256:3428d0a90b1e7efbe2ba947bd1d0c9a70594986b4b6553804547f1fb24cfc92f
# Base image pins RUSTUP_TOOLCHAIN to an old nightly (1.91) that ignores
# rust-toolchain.toml; ruff needs rustc >= 1.97, so pin a newer nightly.
ENV RUSTUP_TOOLCHAIN=nightly-2026-10-01
RUN rustup toolchain install $RUSTUP_TOOLCHAIN --profile minimal
RUN apt-get update && apt-get install -y zip && rm -rf /var/lib/apt/lists/*
COPY . $SRC/fermut
WORKDIR $SRC/fermut
COPY .clusterfuzzlite/build.sh $SRC/
