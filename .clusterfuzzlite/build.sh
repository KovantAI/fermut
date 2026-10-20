#!/bin/bash -eu
# Built from fuzz/ so its nightly rust-toolchain.toml wins over the root's stable pin.
cd "$SRC/fermut/fuzz"
cargo fuzz build -O
cp target/x86_64-unknown-linux-gnu/release/mutate "$OUT/"
# Seed with the repo's own committed Python.
cd "$SRC/fermut"
git ls-files '*.py' | zip -q -j "$OUT/mutate_seed_corpus.zip" -@
