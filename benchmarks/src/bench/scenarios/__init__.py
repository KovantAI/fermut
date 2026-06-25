"""Benchmark scenarios."""
from __future__ import annotations

from .cold import cold
from .loop import loop
from .score_3pct import score_3pct
from .warm import warm

SCENARIOS = {
    "cold": cold,
    "warm": warm,
    "loop": loop,
    "score_3pct": score_3pct,
}
