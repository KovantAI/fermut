#!/usr/bin/env bash
# Fail when a fermut subcommand or flag is missing from its reference page.
#
#   scripts/check-cli-docs.sh [path/to/fermut]
#
# Every subcommand needs docs/reference/cli/<cmd>.md, and every long flag
# its `--help` declares must appear on that page. A page that takes another
# command's flags wholesale says so with `<!-- cli-docs: inherits <cmd> -->`,
# and that command's page then counts too. The global -q/-v flags are
# documented once, on docs/reference/cli/index.md.
set -euo pipefail

fermut=${1:-target/debug/fermut}
docs=docs/reference/cli
status=0

commands=$("$fermut" --help | sed -n '/^Commands:/,/^$/p' | awk 'NR > 1 && NF && $1 != "help" { print $1 }')

for cmd in $commands; do
  page=$docs/$cmd.md
  if [[ ! -f $page ]]; then
    echo "$cmd: no reference page ($page)"
    status=1
    continue
  fi
  pages=("$page")
  for parent in $(sed -n 's/.*<!-- cli-docs: inherits \([a-z-]*\) -->.*/\1/p' "$page"); do
    pages+=("$docs/$parent.md")
  done
  # Only flags that start an option line, not ones quoted in help prose.
  flags=$("$fermut" "$cmd" --help \
    | grep -oE '^ +(-[a-zA-Z], )?--[a-z][a-z0-9-]*' \
    | grep -oE -- '--[a-z][a-z0-9-]*' \
    | grep -vxE -- '--(help|quiet|verbose)' | sort -u || true)
  missing=()
  for flag in $flags; do
    grep -qE -- "${flag}([^a-z0-9-]|$)" "${pages[@]}" || missing+=("$flag")
  done
  if ((${#missing[@]})); then
    echo "$cmd: undocumented in $page: ${missing[*]}"
    status=1
  fi
done

if ((status == 0)); then
  echo "all $(wc -w <<<"$commands" | tr -d ' ') subcommands documented"
fi
exit $status
