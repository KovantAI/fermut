"""Mutation-score-uplift scenario (Claude-in-loop).

Drive a Claude-Code subprocess to write tests targeting survivors until the
mutation score climbs >= `delta_target` points above the baseline (warm run);
the target defaults to 3 points. The whole loop is timed; per-iteration deltas
are recorded.

The Claude invocation is intentionally tool-agnostic: we hand it the survivor
report (whatever the adapter dumps) and ask for new tests in a sibling
`tests/_bench_added/` directory so the original suite stays clean and easy
to revert between tools.

Hard requirements:
  - `claude` CLI on PATH (Claude Code).
  - ANTHROPIC_API_KEY (or whatever the local claude config uses).

If `claude` is missing we fall back to recording the baseline only, with
`status == "skipped_no_claude"` so report aggregation still works.
"""
from __future__ import annotations

import json
import shutil
import subprocess
import time
from pathlib import Path

from .. import repos as repos_mod
from .. import venv as venv_mod
from ..adapters import build as build_adapter
from ..config import RepoCfg, ToolCfg
from .cold import cold


CLAUDE_PROMPT = """\
You are improving a Python test suite to kill mutation-testing survivors.

Repo: {repo}
Mutation tool: {tool}
Current mutation score: {score:.2f}%
Target: raise the score by at least 3 points.

Survivor report follows. Write NEW pytest tests that target these specific
survivors. Put new tests under `tests/_bench_added/` (create the dir if
needed). Do not modify existing tests or source. Use only stdlib + the
project's existing test deps.

Survivors:
{survivors}

Write the new test files. When done, stop — the harness re-runs mutation
testing automatically.
"""


def _fermut_survivors(report: Path) -> str:
    """Extract just the surviving mutants from fermut's JSON report.

    The full report holds every outcome (killed/skipped/survived); dumping it
    raw blows the prompt budget before the survivors are even reached. Emit a
    compact, survivor-only list so Claude sees what it actually needs to kill.
    """
    doc = json.loads(report.read_text())
    lines = []
    for o in doc.get("outcomes", []):
        if str(o.get("status", "")).lower() != "survived":
            continue
        m = o.get("mutant", {})
        lines.append(
            f"{m.get('file', '?')}:{m.get('line', '?')} "
            f"[{m.get('operator', '?')}] "
            f"{m.get('original', '?')} -> {m.get('replacement', '?')}"
        )
    return f"{len(lines)} survivors:\n" + "\n".join(lines)


def _survivor_dump(tool: str, worktree: Path) -> str:
    """Best-effort survivor list per tool. Falls back to last-run stdout."""
    if tool == "fermut":
        report = worktree / ".fermut" / "report.json"
        if report.exists():
            return _fermut_survivors(report)
    if tool == "mutmut":
        try:
            return subprocess.check_output(
                ["mutmut", "results"], cwd=worktree, text=True, timeout=60,
            )
        except Exception:
            return ""
    if tool == "cosmic-ray":
        session = worktree / "cosmic-ray.session.sqlite"
        if session.exists():
            try:
                return subprocess.check_output(
                    ["cosmic-ray", "dump", str(session)],
                    cwd=worktree, text=True, timeout=60,
                )
            except Exception:
                return ""
    return ""


def _drive_claude(prompt: str, worktree: Path, *, timeout: int = 1800) -> str:
    """Spawn a non-interactive Claude run inside the worktree.

    Returns a status string: "ok" (exit 0), "failed" (nonzero exit),
    "timeout" (exceeded `timeout`), or "missing" (no claude CLI). We pass the
    prompt via stdin and use --print so the CLI exits after one response (no
    REPL). A timeout must not crash the whole cell — the caller turns it into
    a graceful stop that keeps the iterations gathered so far.
    """
    if shutil.which("claude") is None:
        return "missing"
    try:
        proc = subprocess.run(
            ["claude", "--print", "--dangerously-skip-permissions"],
            input=prompt,
            cwd=worktree,
            text=True,
            timeout=timeout,
            capture_output=True,
        )
    except subprocess.TimeoutExpired:
        return "timeout"
    return "ok" if proc.returncode == 0 else "failed"


def _cleanup_added_tests(worktree: Path, repo: RepoCfg) -> None:
    """Undo the worktree mutations this scenario makes.

    Claude writes new tests under `tests/_bench_added/`; if we leave them
    behind, a later warm/loop run on the same worktree (without a fresh cold)
    inherits an inflated baseline. We also drop fermut's coverage.json — it
    was regenerated to include the added tests, so it now references contexts
    for files we're deleting. Removing it forces a clean regen next run.
    """
    added = worktree / repo.test_path / "_bench_added"
    if added.exists():
        shutil.rmtree(added, ignore_errors=True)
    cov = worktree / "coverage.json"
    if cov.exists():
        cov.unlink()


def score_3pct(
    repo: RepoCfg,
    tool: ToolCfg,
    *,
    delta_target: float = 3.0,
    max_iterations: int = 5,
    timeout: int | None = None,
) -> dict:
    worktree = repos_mod.worktree_for(tool.name, repo)
    venv = venv_mod.venv_for(tool.name, repo.name)
    if not worktree.exists() or not venv.exists():
        cold(repo, tool, timeout=timeout)

    venv = venv_mod.ensure(tool.name, repo.name, python=tool.python)
    adapter = build_adapter(tool.name, tool, venv)

    try:
        return _score_3pct_inner(
            repo, tool, adapter,
            delta_target=delta_target, max_iterations=max_iterations, timeout=timeout,
        )
    finally:
        # Always restore the worktree, even on early return / timeout / error,
        # so the next scenario starts from a clean suite.
        _cleanup_added_tests(worktree, repo)


def _score_3pct_inner(
    repo: RepoCfg,
    tool: ToolCfg,
    adapter,
    *,
    delta_target: float,
    max_iterations: int,
    timeout: int | None,
) -> dict:
    worktree = repos_mod.worktree_for(tool.name, repo)

    overall_start = time.perf_counter()

    print("  -- baseline warm run", flush=True)
    baseline = adapter.run(repo, worktree, timeout=timeout)
    print(f"    -> baseline score={baseline.score} ({baseline.seconds:.1f}s)", flush=True)
    if baseline.score is None:
        return {
            "scenario": "score_3pct",
            "tool": tool.name,
            "repo": repo.name,
            "status": "skipped_no_baseline_score",
        }

    iterations: list[dict] = []
    score = baseline.score
    reached = False
    timed_out = False

    for i in range(max_iterations):
        print(f"  -- iteration {i + 1}/{max_iterations}: claude writing tests", flush=True)
        survivors = _survivor_dump(tool.name, worktree)
        prompt = CLAUDE_PROMPT.format(
            repo=repo.name,
            tool=tool.name,
            score=score,
            survivors=(survivors or "<no survivor dump available>")[:60_000],
        )
        claude_start = time.perf_counter()
        claude_status = _drive_claude(prompt, worktree)
        claude_dur = time.perf_counter() - claude_start
        print(f"    -> claude {claude_status} ({claude_dur:.1f}s); re-running mutation", flush=True)
        if claude_status == "timeout":
            # Don't sink the cell — Claude just ran long. Stop iterating and
            # report what we have, marked so the partial result is obvious.
            print("    -> claude timed out; stopping with partial result", flush=True)
            timed_out = True
            break
        if claude_status != "ok":
            return {
                "scenario": "score_3pct",
                "tool": tool.name,
                "repo": repo.name,
                "status": "skipped_no_claude" if claude_status == "missing" else "claude_failed",
                "baseline_score": baseline.score,
                "iterations": iterations,
            }

        # Tools that key their cache on source only (mutmut, cosmic-ray) won't
        # notice Claude's new tests and would replay stale verdicts — wipe the
        # cache so the re-run actually exercises the added tests. fermut keys
        # on coverage scope and invalidates affected mutants on its own.
        if adapter.rescore_needs_cache_clear:
            adapter.clear_cache(worktree)

        result = adapter.run(repo, worktree, timeout=timeout)
        delta = None if result.score is None else round(result.score - baseline.score, 2)
        print(
            f"    -> score={result.score} delta={delta} mutation={result.seconds:.1f}s",
            flush=True,
        )
        iterations.append({
            "iteration": i,
            "claude_seconds": round(claude_dur, 3),
            "mutation_seconds": round(result.seconds, 3),
            "score": result.score,
            "mutants": result.mutants,
            "delta": delta,
        })
        if result.score is not None:
            score = result.score
            if score - baseline.score >= delta_target:
                reached = True
                break

    total = time.perf_counter() - overall_start
    return {
        "scenario": "score_3pct",
        "tool": tool.name,
        "repo": repo.name,
        "status": (
            "reached" if reached
            else "partial_claude_timeout" if timed_out
            else "exhausted_iterations"
        ),
        "baseline_score": baseline.score,
        "final_score": score,
        "delta": round(score - baseline.score, 2),
        "iterations": iterations,
        "total_seconds": round(total, 3),
        "mutants": iterations[-1]["mutants"] if iterations else baseline.mutants,
    }
