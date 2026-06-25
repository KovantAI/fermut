"""On-demand fermut wheel build with content-addressed cache.

The adapter builds the wheel on demand — no manual pre-build step. Cache key
is the current git HEAD plus, when the tree is dirty, a digest of the working
changes — so clean rebuilds reuse the same wheel across runs and dirty
edits invalidate the cache automatically.
"""
from __future__ import annotations

import hashlib
import shutil
import subprocess
from functools import lru_cache
from pathlib import Path


@lru_cache(maxsize=8)
def ensure_fermut_wheel(repo_root: Path, wheel_root: Path) -> Path:
    """Return a wheel for the current repo state, building it if missing."""
    key = _cache_key(repo_root)
    target_dir = wheel_root / key
    wheels = sorted(target_dir.glob("fermut-*.whl")) if target_dir.is_dir() else []
    if wheels:
        return wheels[0]

    if shutil.which("maturin") is None:
        raise RuntimeError(
            "maturin not on PATH — install with `pipx install maturin` "
            "(or `pip install --user maturin`)."
        )

    target_dir.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        [
            "maturin", "build", "--release", "--strip",
            "--manifest-path", str(repo_root / "Cargo.toml"),
            "--out", str(target_dir),
        ],
        check=True,
    )
    wheels = sorted(target_dir.glob("fermut-*.whl"))
    if not wheels:
        raise RuntimeError(f"maturin produced no wheel under {target_dir}")
    return wheels[0]


def _cache_key(repo_root: Path) -> str:
    sha = _git(repo_root, "rev-parse", "HEAD")[:12]
    status = _git(repo_root, "status", "--porcelain")
    if not status.strip():
        return sha
    # Dirty tree: fold staged + unstaged diff into the key so edits trigger
    # a rebuild without polluting the clean-HEAD slot.
    diff = _git(repo_root, "diff", "HEAD")
    digest = hashlib.sha256((status + diff).encode("utf-8", "replace")).hexdigest()[:12]
    return f"{sha}-dirty-{digest}"


def _git(repo_root: Path, *args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(repo_root), *args],
        capture_output=True, text=True, check=True,
    ).stdout
