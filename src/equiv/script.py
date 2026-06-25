"""Layer 1 helper for fermut's equivalent-mutant detector.

Reads `{"orig": str, "mutated": str}` from stdin, compiles both sources
with CPython, and writes `{"equivalent": bool, "error": str|null}` to
stdout. A `True` verdict means the two sources compile to identical
code-object signatures — strong proof of behavioral equivalence.

Both sources are compiled with the same `<f>` filename so co_filename
(embedded in nested code objects' qualnames) does not introduce
spurious differences.
"""

from __future__ import annotations

import json
import sys
import types


def _code_signature(code: types.CodeType) -> tuple:
    nested = []
    scalars = []
    for const in code.co_consts:
        if isinstance(const, types.CodeType):
            nested.append(_code_signature(const))
        else:
            scalars.append(const)
    return (
        bytes(code.co_code),
        tuple(scalars),
        tuple(code.co_names),
        tuple(code.co_varnames),
        tuple(code.co_freevars),
        tuple(code.co_cellvars),
        code.co_flags,
        code.co_argcount,
        code.co_kwonlyargcount,
        code.co_posonlyargcount,
        tuple(nested),
    )


def main() -> None:
    raw = sys.stdin.read()
    try:
        payload = json.loads(raw)
        orig = payload["orig"]
        mutated = payload["mutated"]
    except (ValueError, KeyError) as exc:
        print(json.dumps({"equivalent": False, "error": f"bad payload: {exc}"}))
        return

    try:
        a = compile(orig, "<f>", "exec")
        b = compile(mutated, "<f>", "exec")
    except SyntaxError as exc:
        # A SyntaxError in the mutated source is its own answer — definitely
        # not equivalent. SyntaxError in the original means caller fed us
        # broken input; still surface as not-equivalent.
        print(json.dumps({"equivalent": False, "error": f"compile: {exc}"}))
        return

    equivalent = _code_signature(a) == _code_signature(b)
    print(json.dumps({"equivalent": equivalent, "error": None}))


if __name__ == "__main__":
    main()
