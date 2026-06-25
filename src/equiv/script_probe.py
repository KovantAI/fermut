"""Layer 4 helper for fermut's equivalent-mutant detector.

Reads `{"orig": str, "mutated": str, "qualname": str, "examples": int}` from
stdin. Writes one JSON object to stdout describing the probe outcome:

```
{
  "verdict": "equivalent" | "not_equivalent" | "ineligible" | "error",
  "reason": "<short string>",
  "counterexample": "<repr of kwargs>" | null
}
```

"equivalent" is a strong prior, never a proof — the search space is bounded
by `hypothesis.strategies.from_type` on the annotated signature, so any
disagreement we *find* is genuine but absence of disagreement under N
samples is not. "ineligible" means the probe declined to fire (no
annotations, side-effects in source, unsupported callable shape, hypothesis
not installed). "error" is reserved for malformed input or unexpected
exceptions inside the harness itself.
"""

from __future__ import annotations

import ast
import importlib.util
import inspect
import json
import os
import sys
import tempfile
import typing


# Identifiers whose presence anywhere in the source disqualifies the file
# from the probe. The list is deliberately broad: a single I/O call inside
# the module is enough to corrupt the differential. False ineligibles are
# cheap; false probes are not.
_BANNED = {
    "open",
    "print",
    "input",
    "exec",
    "eval",
    "compile",
    "requests",
    "urllib",
    "socket",
    "subprocess",
    "os",
    "shutil",
    "pathlib",
    "random",  # nondeterminism breaks the diff
    "time",
    "datetime",
    "logging",
}


def _emit(verdict: str, reason: str = "", counterexample=None) -> None:
    print(
        json.dumps(
            {
                "verdict": verdict,
                "reason": reason,
                "counterexample": counterexample,
            }
        )
    )


def _module_uses_banned(source: str) -> bool:
    try:
        tree = ast.parse(source)
    except SyntaxError:
        return True
    for node in ast.walk(tree):
        if isinstance(node, ast.Name) and node.id in _BANNED:
            return True
        if isinstance(node, ast.Attribute):
            base = node
            while isinstance(base, ast.Attribute):
                base = base.value
            if isinstance(base, ast.Name) and base.id in _BANNED:
                return True
        if isinstance(node, ast.Import):
            for alias in node.names:
                if alias.name.split(".")[0] in _BANNED:
                    return True
        if isinstance(node, ast.ImportFrom):
            mod = (node.module or "").split(".")[0]
            if mod in _BANNED:
                return True
    return False


def _load_module(name: str, path: str):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot build spec for {path}")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def _resolve(mod, qualname: str):
    obj = mod
    for part in qualname.split("."):
        obj = getattr(obj, part)
    return obj


def _is_instance_method(fn) -> bool:
    sig = inspect.signature(fn)
    params = list(sig.parameters.values())
    return bool(params and params[0].name in ("self", "cls"))


def main() -> None:
    raw = sys.stdin.read()
    try:
        payload = json.loads(raw)
        orig = payload["orig"]
        mutated = payload["mutated"]
        qualname = payload["qualname"]
        examples = int(payload.get("examples", 500))
    except (ValueError, KeyError, TypeError) as exc:
        _emit("error", f"bad payload: {exc}")
        return

    try:
        from hypothesis import HealthCheck, given, settings, strategies as st
    except ImportError:
        _emit("ineligible", "hypothesis not installed")
        return

    if _module_uses_banned(orig):
        _emit("ineligible", "source uses an identifier outside the pure-function allowlist")
        return

    with tempfile.TemporaryDirectory() as tmp:
        orig_path = os.path.join(tmp, "_fermut_orig.py")
        mut_path = os.path.join(tmp, "_fermut_mut.py")
        with open(orig_path, "w", encoding="utf-8") as f:
            f.write(orig)
        with open(mut_path, "w", encoding="utf-8") as f:
            f.write(mutated)

        sys.path.insert(0, tmp)
        try:
            try:
                mod_orig = _load_module("_fermut_orig", orig_path)
                mod_mut = _load_module("_fermut_mut", mut_path)
            except Exception as exc:
                _emit("ineligible", f"import failed: {type(exc).__name__}: {exc}")
                return
            try:
                fn_orig = _resolve(mod_orig, qualname)
                fn_mut = _resolve(mod_mut, qualname)
            except AttributeError as exc:
                _emit("ineligible", f"resolve failed: {exc}")
                return
        finally:
            try:
                sys.path.remove(tmp)
            except ValueError:
                pass

        if not callable(fn_orig) or not callable(fn_mut):
            _emit("ineligible", "target is not callable")
            return
        if _is_instance_method(fn_orig):
            _emit("ineligible", "instance/class methods unsupported")
            return

        try:
            sig = inspect.signature(fn_orig)
            hints = typing.get_type_hints(fn_orig)
        except Exception as exc:
            _emit("ineligible", f"signature/hints failed: {exc}")
            return

        kwarg_strategies = {}
        for name, param in sig.parameters.items():
            if param.kind in (
                inspect.Parameter.VAR_POSITIONAL,
                inspect.Parameter.VAR_KEYWORD,
            ):
                _emit("ineligible", "variadic parameters unsupported")
                return
            if name not in hints:
                _emit("ineligible", f"parameter {name!r} not annotated")
                return
            try:
                kwarg_strategies[name] = st.from_type(hints[name])
            except Exception as exc:
                _emit("ineligible", f"from_type({hints[name]!r}) failed: {exc}")
                return

        state = {"counterexample": None}

        def _call_safe(fn, kw):
            try:
                return ("ok", fn(**kw))
            except Exception as exc:
                # Equate exceptions by type name + message string. Equal
                # exceptions count as agreement so error-path-only mutations
                # don't trigger false disagreements.
                return ("exc", f"{type(exc).__name__}: {exc}")

        @settings(
            max_examples=examples,
            deadline=200,
            derandomize=True,
            database=None,
            suppress_health_check=[
                HealthCheck.too_slow,
                HealthCheck.data_too_large,
                HealthCheck.filter_too_much,
            ],
        )
        @given(**kwarg_strategies)
        def _eq(**kw):
            a = _call_safe(fn_orig, kw)
            b = _call_safe(fn_mut, kw)
            if a != b:
                state["counterexample"] = repr(kw)
                # Shrunk failures will overwrite state["counterexample"]
                # with a smaller witness — exactly what we want for suggest.
                assert False, f"divergence at {kw!r}: orig={a!r} mut={b!r}"

        try:
            _eq()
            _emit("equivalent", f"{examples} examples agreed on {qualname}")
        except AssertionError:
            _emit(
                "not_equivalent",
                "differential disagreement",
                counterexample=state["counterexample"],
            )
        except Exception as exc:
            _emit("error", f"{type(exc).__name__}: {exc}")


if __name__ == "__main__":
    main()
