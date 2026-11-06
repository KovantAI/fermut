# Enterprise and offline installs

For platform teams rolling fermut out across many projects: internal
package mirrors, verifying what you install, what touches the network, and
running in containers. For a single-developer setup, see
[Installation](installation.md).

## What ships

fermut is published to [PyPI](https://pypi.org/project/fermut/) as
prebuilt wheels plus an sdist. Each wheel packages the Rust binary as a
console script, plus the pytest plugin; no Rust toolchain is needed to
install.

| Wheel platform        | Notes |
|-----------------------|-------|
| Linux x86_64, aarch64 | manylinux (glibc). No musllinux wheel: Alpine images need a source build. |
| macOS x86_64, arm64   | |
| Windows x86_64        | See [Windows support](../reference/policies/windows.md) for what differs. |

The wheels require **Python ≥ 3.10** in the environment they're installed
into, and fermut drives test suites on Python ≥ 3.10 only
([Python versions](../reference/policies/python-versions.md)). Projects
still on 3.9 can't use it.

## Installing from an internal mirror

Wheels are ordinary PyPI artifacts, so a mirror (Artifactory, Nexus,
devpi, a GitLab package registry) serves them like any other package.
Point the installer at it:

```sh
pip install --index-url https://pypi.internal.example/simple fermut==0.7.0
uv tool install --index-url https://pypi.internal.example/simple fermut==0.7.0
```

`PIP_INDEX_URL` / `UV_INDEX_URL` work too. Installing a wheel needs no
network beyond the index: nothing is downloaded at install time or first
run.

**Building from source is different.** The sdist and `cargo install` pull
Rust crates from crates.io and a pinned `ruff` git dependency from GitHub.
Air-gapped source builds need a crates.io mirror and a vendored copy of that
repo; prefer the wheels.

**Pin the version.** fermut is pre-1.0: the CLI, config schema and JSON
report may change in any `0.MINOR` release
([versioning](../reference/policies/versioning.md)). Pin `fermut==0.7.*`
in your shared template and bump deliberately after reading the
[changelog](https://github.com/KovantAI/fermut/blob/main/CHANGELOG.md). A
minor release can also change default operators and therefore scores;
history only compares runs with the same `config_hash` and fermut version,
so a bump re-baselines trends instead of reporting a fake regression.

## Verifying what you install

- **Trusted publishing.** Wheels reach PyPI from the release workflow over
  OIDC; there is no long-lived upload token to leak
  ([distribution](../reference/policies/distribution.md)).
- **Hash pinning.** PyPI records a SHA-256 for every file. Lock them with
  `pip install --require-hashes -r requirements.txt` (generate with
  `pip-compile --generate-hashes` or `uv pip compile --generate-hashes`),
  so a mirror can't serve a different file.
- **Build provenance.** Every release's wheels and sdist get a GitHub
  build-provenance attestation, and the Sigstore bundle is attached to the
  [GitHub release](https://github.com/KovantAI/fermut/releases) as
  `fermut-v<version>.intoto.jsonl`. Verify a downloaded file with the
  GitHub CLI:

  ```sh
  gh attestation verify <downloaded-wheel>.whl --repo KovantAI/fermut
  ```

There is no published SBOM today. The Rust dependency set is in
`Cargo.lock` in the repository, checked in CI with `cargo-deny`
(`deny.toml`).

## Network access and telemetry

fermut has **no telemetry**: it doesn't phone home, check for updates or
send usage data. A normal run (`run`, `coverage`, `baseline`, `merge`,
`trend`, `next`, `explain`, `mcp`, …) makes no network calls of its own;
your test suite may, as it would under pytest.

Only these touch the network, and only when you invoke them:

| Command | Talks to | Sends |
|---|---|---|
| `fermut explain --llm`, `fermut suggest`, `fermut autofix` | `api.anthropic.com` | Source and test excerpts around a mutant. Needs `ANTHROPIC_API_KEY`. |
| `fermut pr-comment` | GitHub, through the `gh` CLI | The Markdown report. |
| `git` (for `--diff-only` / `--since`) | nothing new | Reads local history only; fetching the base branch is your CI's job. |

Redaction in the LLM commands is **not** a guarantee (see
[SECURITY.md](https://github.com/KovantAI/fermut/blob/main/SECURITY.md)).
To keep source on-premises, don't grant those commands an API key; set
`FERMUT_LLM_MOCK=1` in tests that exercise them.

For repositories you don't fully trust, `FERMUT_NO_CONFIG=1` skips config
discovery, and `FERMUT_CACHE_KEY` HMAC-signs the result cache so a cache
restored from elsewhere can't inject verdicts
([environment variables](../reference/environment-variables.md)).

## Running in containers

A Linux container with Python, your project's dependencies, `pytest`,
`coverage` and `pytest-cov` is all fermut needs; `ty` is embedded.

- **Writable project tree.** fermut writes `.fermut/` (cache, history) at
  the project root and a mirror of the source per worker in the temp dir.
  A read-only checkout fails; see
  [permission denied](../reference/troubleshooting/permission-denied.md).
  Non-root is fine as long as those two places are writable.
- **CPU.** `--jobs` defaults to the CPUs Rust's `available_parallelism`
  reports, which on Linux takes cgroup CPU quotas into account. On
  a shared runner, set `--jobs` explicitly to the CPUs the job is
  entitled to; oversubscribing turns slow mutants into timeouts.
- **Memory.** The [memory cap](../reference/cli/run.md#memory-cap) is
  **per mutant**, not per job: by default 4 × the baseline's peak, at
  least 2 GiB. A container memory limit is shared by all workers, so keep
  `jobs × max-memory` under it, or the kernel's OOM killer ends the whole
  job instead of one mutant. On a 8 GiB runner with 4 jobs, pass
  `--max-memory 2G`.
- **Disk.** Each worker holds a mirror of the project tree. On
  filesystems without copy-on-write (ext4, and the overlayfs most container
  roots use) that's a full copy per worker, so a large repo needs room for
  `jobs` copies; see [isolation](../reference/cli/run-isolation.md).

## Rolling out to many teams

- Put the pinned install, `fermut coverage` and `fermut run` in one shared
  CI template; recipes for GitLab and Jenkins are in
  [GitLab CI and Jenkins](../guides/ci-gitlab-jenkins.md), including how to
  tell a broken run from a failed gate.
- Start every team non-blocking and diff-scoped, then gate; see
  [Rolling out to a team](../guides/projects.md).
- Teams on `unittest` work, but slower: [schemata](../reference/cli/run-schemata.md)
  is pytest-only, and the `unittest` runner runs the full suite for each
  mutant regardless of coverage ([caching](../concepts/caching.md)).
- Dashboards should read the [JSON report](../reference/json.md), never
  stdout, and ignore fields they don't know.
