# Windows support

fermut ships a Windows x86_64 wheel and runs the same CLI there, but a few
features depend on Unix process APIs. This page lists what differs, so you
can tell whether a Windows result or timing should match Linux/macOS.

## Feature matrix

| Feature | Linux / macOS | Windows |
|---|---|---|
| Install | wheels (x86_64, arm64/aarch64) | x86_64 wheel; no ARM64 wheel (source build) |
| `run`, `coverage`, `baseline`, `merge`, `trend`, `next`, `explain`, `mcp` | yes | yes |
| Result cache, history | yes | yes |
| [Schemata](../cli/run-schemata.md) (forked warm pytest) | on by default (pytest runner) | **not available**: every mutant starts a fresh test process |
| [Timeout budget](../cli/run.md#timeout-budgets) | yes | yes |
| Killing a timed-out mutant's process tree | process group, `SIGKILL` | new process group, `CTRL_BREAK_EVENT`, then the leader is force-killed |
| [Memory cap](../cli/run.md#memory-cap) | Linux `RLIMIT_DATA`; macOS RSS watchdog | Job Object: caps the mutant's whole process tree, fails with `MemoryError` |
| [Private `HOME`](../cli/run-isolation.md#private-home) | isolates `~` | isolates `~`: also sets `USERPROFILE`, `APPDATA`, `LOCALAPPDATA` |
| [GitHub Action](../../guides/github-action.md) | yes | **no** (Linux and macOS runners); use the CLI |
| `fermut pr-comment` | yes (`gh`) | yes (`gh`) |

## What changes in practice

**Speed.** Schemata is fermut's main speed lever on larger suites: it forks
a pytest process that already imported and collected your tests, instead of
paying interpreter start-up and collection per mutant. Windows has no
`fork`, so with no setting fermut quietly uses the normal runner, and
passing `--schemata` (or `schemata = true` in config) fails with
`` `--schemata` needs fork() ``. Expect Windows runs to be noticeably
slower than the [benchmarks](../benchmarks.md), which are all
Linux/macOS with schemata on. Schemata is built to give the same verdicts
as the normal runner, so scores stay comparable; only time differs.

For schemata speed on a Windows machine, run fermut inside
[WSL2](https://learn.microsoft.com/windows/wsl/install): it is Linux, so
schemata is on by default there. Verdicts match a native Windows run unless
the code under test is platform-specific.

**Memory cap.** Each mutant's run joins a Job Object that caps the memory
of every process in it, test subprocesses included. Past the cap, Python
raises `MemoryError` and the mutant counts as killed, as on Linux.

## Running on a Windows CI runner

The packaged GitHub Action supports Linux and macOS runners only. On
`windows-latest`, install fermut and call the CLI directly:

```yaml
jobs:
  fermut-windows:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          fetch-depth: 0              # --diff-only needs the merge base
      - uses: actions/setup-python@6d4fd22b20465ccc0f7354476588e8df7c9ca0e0 # v7.1.0
        with:
          python-version: "3.12"
      - run: pip install fermut pytest coverage pytest-cov -e .
      - run: fermut coverage
      - run: >
          fermut run src/ --tests tests/ --diff-only origin/main
          --no-verify-baseline --json report.json --markdown report.md
```

Most projects run mutation testing on Linux only and keep Windows for the
ordinary test matrix: a mutant's verdict is a property of your tests, not
the OS, unless the code under test is platform-specific.

## Building from source

The source build pulls a `ruff` git dependency with paths longer than
Windows' legacy 260-character limit. Enable long paths first:

```powershell
git config --global core.longpaths true
$env:CARGO_NET_GIT_FETCH_WITH_CLI = "true"
cargo install --git https://github.com/KovantAI/fermut --locked
```

(System-wide long-path support, `LongPathsEnabled`, may also be needed.)

## How Windows is tested

fermut's Rust test suite runs on `windows-latest` in CI on every change,
including Windows-only tests for the Job Object memory cap, and every
release builds the Windows wheel. Benchmarks and the detection-parity runs
are Linux/macOS only.
