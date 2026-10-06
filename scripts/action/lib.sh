# Shared helpers for the fermut GitHub Actions (action.yml, sweep/, merge/).
# Sourced, not executed. Each step script sources this first.

set -euo pipefail

die() {
  echo "::error title=fermut action::$1" >&2
  exit 1
}

out() {
  echo "$1=$2" >> "$GITHUB_OUTPUT"
}

# An unset caller variable (`${{ vars.X }}`) reaches a composite input as "",
# not as the input's default, so booleans are checked exactly: an unchecked
# `blocking` would otherwise fail open.
check_bool() {
  case "$2" in
    true|false) ;;
    *) die "$1 must be \"true\" or \"false\", got \"$2\". An empty value usually means an unset variable was passed." ;;
  esac
}

check_fail_under() {
  if [ -n "$1" ] && ! [[ "$1" =~ ^[0-9]+(\.[0-9]+)?$ ]]; then
    die "fail-under must be a number between 0 and 100, got \"$1\"."
  fi
}

# Fails the gate, or downgrades the failure to a warning under blocking: false.
gate_fail() {
  if [ "${BLOCKING:-true}" = "false" ]; then
    echo "::warning title=fermut gate::$1 (blocking: false)"
    exit 0
  fi
  echo "::error title=fermut gate::$1" >&2
  exit 1
}
