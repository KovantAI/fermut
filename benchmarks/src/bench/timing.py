"""Timing helpers shared across scenarios."""
from __future__ import annotations

import contextlib
import time
from dataclasses import dataclass


@dataclass
class Timing:
    label: str
    seconds: float
    extra: dict | None = None

    def to_dict(self) -> dict:
        out = {"label": self.label, "seconds": round(self.seconds, 3)}
        if self.extra:
            out.update(self.extra)
        return out


@contextlib.contextmanager
def timed(label: str, sink: list[Timing], extra: dict | None = None, *, verbose: bool = True):
    """Append a Timing row to `sink` when the block exits.

    Uses time.perf_counter — monotonic, sub-microsecond on macOS/Linux.
    Prints `[start] label` / `[done ] label (Ns)` so the operator can see
    progress in long runs.
    """
    if verbose:
        print(f"    [start] {label}", flush=True)
    start = time.perf_counter()
    try:
        yield
    finally:
        dur = time.perf_counter() - start
        sink.append(Timing(label=label, seconds=dur, extra=extra))
        if verbose:
            print(f"    [done ] {label} ({dur:.1f}s)", flush=True)
