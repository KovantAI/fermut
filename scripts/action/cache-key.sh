#!/usr/bin/env bash
# Resolves the cache key hash. Runs before install so a fresh .venv is never
# hashed.
#
# env: OVERRIDE  caller's cache-key-hash input
#      DEFAULT   hashFiles() over sources, lockfiles and config
# out: hash
source "$(dirname "$0")/lib.sh"

hash="${OVERRIDE:-$DEFAULT}"
[ -n "$hash" ] || die "cache key hash is empty — no Python sources found to hash. Every run would share one cache entry."
out hash "$hash"
