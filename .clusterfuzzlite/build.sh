#!/bin/bash -eu
# Built from fuzz/ (own workspace). Toolchain comes from RUSTUP_TOOLCHAIN in the Dockerfile.
cd "$SRC/fermut/fuzz"
cargo fuzz build -O
cp target/x86_64-unknown-linux-gnu/release/mutate "$OUT/"
# Seed with the repo's own committed Python.
cd "$SRC/fermut"
git ls-files '*.py' | zip -q "$OUT/mutate_seed_corpus.zip" -@
