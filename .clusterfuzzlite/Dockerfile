FROM gcr.io/oss-fuzz-base/base-builder-rust
# Base image pins RUSTUP_TOOLCHAIN to an old nightly (1.91) that ignores
# rust-toolchain.toml; ruff needs rustc >= 1.97, so pin a newer nightly.
ENV RUSTUP_TOOLCHAIN=nightly-2026-10-01
RUN rustup toolchain install $RUSTUP_TOOLCHAIN --profile minimal
RUN apt-get update && apt-get install -y zip
COPY . $SRC/fermut
WORKDIR $SRC/fermut
COPY .clusterfuzzlite/build.sh $SRC/
