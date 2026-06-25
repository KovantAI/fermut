"""Per-tool adapters."""
from __future__ import annotations

from .base import Adapter
from .cosmic_ray import CosmicRayAdapter
from .fermut import FermutAdapter
from .mutmut import MutmutAdapter
from .poodle import PoodleAdapter

ADAPTERS: dict[str, type[Adapter]] = {
    "fermut": FermutAdapter,
    "mutmut": MutmutAdapter,
    "cosmic-ray": CosmicRayAdapter,
    "poodle": PoodleAdapter,
}


def build(name: str, *args, **kwargs) -> Adapter:
    return ADAPTERS[name](*args, **kwargs)
