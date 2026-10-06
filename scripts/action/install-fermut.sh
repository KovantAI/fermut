#!/usr/bin/env bash
# Installs fermut from PyPI with uv. An empty version follows the action ref
# (`@v0.5.0` → fermut 0.5.0), else the latest release. "project" installs
# nothing; resolve-env.sh then finds the project's own fermut.
#
# env: VERSION     fermut-version input
#      ACTION_REF  github.action_ref
# out: spec
source "$(dirname "$0")/lib.sh"

if [ "$VERSION" = "project" ]; then
  echo "Using the project's fermut; resolved after install."
  out spec project
  exit 0
fi
if [ -z "$VERSION" ] && [[ "$ACTION_REF" =~ ^v([0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
  VERSION="${BASH_REMATCH[1]}"
fi
spec="fermut${VERSION:+==$VERSION}"
echo "Installing $spec"
uv tool install --quiet "$spec"
out spec "$spec"
