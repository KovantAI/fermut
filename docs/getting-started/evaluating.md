# Evaluating fermut for your team

The questions a team lead asks before adopting fermut, with short answers
and links to the detail. For the rollout itself, see
**[Rolling out to a team](../guides/projects.md)**.

## Who maintains it

fermut is built and maintained by [Kovant AB](https://github.com/KovantAI),
dual-licensed MIT OR Apache-2.0 (see [License](../reference/license.md)).
Kovant runs fermut in CI on its own Python services and on fermut's own
pytest plugin, whose score is the mutation badge in the
[README](https://github.com/KovantAI/fermut#readme).

fermut is a tool Kovant AB builds for its own use and maintains actively.
Support is best-effort through GitHub issues.

- **Bugs and feature requests:** [GitHub issues](https://github.com/KovantAI/fermut/issues).
  See [Getting help](getting-help.md) for what to include.
- **Security:** report privately to <security@kovant.com>, never in a
  public issue. Acknowledgement target 3 business days, fix or status
  within 30 days. See [Security](../reference/policies/security.md).

## Release cadence and stability

- **Cadence.** No fixed schedule. Since `0.3.0` (2026-08-31), a release
  has shipped every two to three weeks; the
  [changelog](https://github.com/KovantAI/fermut/blob/main/CHANGELOG.md)
  lists each one.
- **Pre-1.0 contract.** A `0.MINOR` bump may break the user-facing
  contract (CLI flags, exit codes, config keys, operator names, the JSON
  report shape); a `0.MINOR.PATCH` bump never does. Every breaking change
  is in the changelog, usually after one release of deprecation warnings.
  Pin `fermut==0.MINOR.*` to upgrade on your own schedule. See
  [Versioning](../reference/policies/versioning.md).
- **1.0** is declared once the contract has held for two consecutive
  minor releases without unplanned breakage.
- **Security fixes** go to the latest `0.x` release only.
- **Install** from public PyPI as a prebuilt wheel (no Rust toolchain)
  for Linux, macOS and Windows x86_64; see
  [Platforms](../reference/policies/platforms.md) and, for mirrors and
  provenance checks, [Enterprise and offline installs](enterprise-install.md).

## What it costs in CI

Mutation testing runs your tests once per mutant, so cost depends on the
suite more than on lines of code. Two numbers matter: the full sweep, and
the per-PR run.

**Full sweep.** From real sessions on Python application backends (not
libraries):

| Codebase | First full run | Re-run, mostly cached |
|----------|----------------|-----------------------|
| Small/medium (≤ ~10k LOC src, hundreds of tests) | 10–25 min | 30–60 s |
| Large (~60k LOC+ src, thousands of tests) | 60–90 min | 5–15 min |

For pinned open-source libraries measured on one machine, see
[Benchmarks](../reference/benchmarks.md): 15 s (markupsafe) to ~12 min
(trio), most under 4 min. Windows runs are slower: no
[schemata](../reference/cli/run-schemata.md), see
[Windows support](../reference/policies/windows.md).

**Keeping it cheap:**

- **Gate PRs on the diff only** (`--diff-only origin/main`): a PR pays for
  the mutants on lines it changed, not the whole project.
- **Run the full sweep nightly or weekly**, split across runners with
  `--shard i/n` and joined with `fermut merge`. The
  [GitHub Action](../guides/github-action.md) does both.
- **Keep the cache warm.** Verdicts are cached and invalidated only when
  the code, settings or covering tests change, so a re-run pays only for
  what moved. See [Caching](../concepts/caching.md).
- **Build per-test coverage once** (`fermut coverage`) so each mutant runs
  only the tests that reach it. Without it every mutant runs the whole
  suite, roughly 8–10× slower on the benchmark repos.

## How much noise to expect

A survivor is either a missing test or an **equivalent mutant**: a change
no test can detect because the behavior didn't change. Equivalent mutants
are the noise in mutation testing.

- **fermut's own rate is not measured yet.** Published studies put
  equivalent mutants at about 1.5–7.5% of all mutants (1.6% for PyTation and
  5.7% for Cosmic Ray on Python, 5.7–7.4% for Java and C). As a share of
  *survivors* it's higher, since most mutants get killed.
- **fermut removes some automatically.** Mutants whose bytecode matches
  the original are marked `equivalent` and left out of the score; the
  default `minimal` operator profile and the `ty` filter (which drops
  30–40% of covered mutants as type errors before they run) keep low-value
  mutants out to begin with.
- **The rest you mark once.** `# fermut: ignore[op]` on the line keeps a
  known-equivalent mutant out of every later run. See
  [Equivalent mutants](../guides/equivalent-mutants.md) and
  [Survivor triage](../guides/survivor-triage.md).
- **Gate on new code first.** `--diff-only` shows each developer only the
  survivors on lines they just wrote, which is when they're cheapest to
  judge.

## Reporting progress upward

- **Trend.** `fermut trend` records each run's score in
  `.fermut/history.jsonl` and fails CI when the score drops more than you
  allow. In GitHub Actions, the `KovantAI/fermut/trend` action keeps the
  history on a branch and writes the trend to the job summary. See
  [Trends](../guides/trends.md).
- **Dashboard.** `fermut dashboard` writes a single HTML file with the
  score chart and the current survivors, ready to attach to a report. See
  [`fermut dashboard`](../reference/cli/dashboard.md).
- **Badge.** The trend action's `score` output feeds a
  [Shields endpoint](https://shields.io/badges/endpoint-badge) badge. A
  job like this, run after `trend` on the default branch, pushes the
  badge JSON to an orphan `badges` branch:

    ```yaml
    badge:
      needs: trend
      if: github.ref == 'refs/heads/main' && needs.trend.outputs.score != ''
      runs-on: ubuntu-latest
      permissions:
        contents: write
      steps:
        - env:
            SCORE: ${{ needs.trend.outputs.score }}
            GH_TOKEN: ${{ github.token }}
            REPO: ${{ github.repository }}
          run: |
            set -euo pipefail
            color=$(awk -v s="$SCORE" 'BEGIN { print (s >= 80 ? "green" : s >= 70 ? "yellow" : "red") }')
            mkdir badges && cd badges
            git init -q -b badges
            printf '{"schemaVersion":1,"label":"mutation","message":"%.1f%%","color":"%s"}\n' \
              "$SCORE" "$color" > mutation.json
            git add mutation.json
            git -c user.name=github-actions -c user.email=github-actions@users.noreply.github.com \
              commit -q -m "mutation badge: $SCORE%"
            git push -q -f "https://x-access-token:${GH_TOKEN}@github.com/${REPO}.git" badges
    ```

    The trend job exposes the score with `outputs: score: ${{ steps.trend.outputs.score }}`
    (its trend step has `id: trend`). Then add to your README:

    ```markdown
    ![mutation](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/<org>/<repo>/badges/mutation.json)
    ```

    Private repositories need a token-authenticated badge host instead,
    since Shields can't read their raw files.
