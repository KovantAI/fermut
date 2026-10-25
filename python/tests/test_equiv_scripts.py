"""Tests for the equivalent-mutant detector's Python helpers.

`src/equiv/script.py` (bytecode identity) is embedded in the fermut binary
with `include_str!` and run as `python -c`. It reads a JSON payload on stdin
and prints one JSON verdict, so the tests load them by path and drive `main()`
in-process, where coverage and mutation testing can see them.
"""

import importlib.util
import io
import json
import subprocess
import sys
import types
from pathlib import Path

import pytest

EQUIV = Path(__file__).resolve().parents[2] / "src" / "equiv"


def _load(name):
    spec = importlib.util.spec_from_file_location(
        f"_fermut_{name}", EQUIV / f"{name}.py"
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


script = _load("script")


@pytest.mark.parametrize(
    ("name", "payload", "key"),
    [
        ("script", {"orig": "x = 1\n", "mutated": "x = 1\n"}, "equivalent"),
    ],
)
def test_runs_as_a_script(name, payload, key):
    # fermut runs each with `python -c <source>`, i.e. as `__main__`.
    source = (EQUIV / f"{name}.py").read_text()
    proc = subprocess.run(
        [sys.executable, "-c", source],
        input=json.dumps(payload),
        capture_output=True,
        text=True,
        check=True,
    )
    assert key in json.loads(proc.stdout)


@pytest.fixture
def call(monkeypatch, capsys):
    """Run a script's `main()` on a payload (a dict, or raw stdin text) and
    return its parsed JSON output."""

    def _call(mod, payload):
        raw = payload if isinstance(payload, str) else json.dumps(payload)
        monkeypatch.setattr(sys, "stdin", io.StringIO(raw))
        mod.main()
        out = capsys.readouterr().out
        assert out.count("\n") == 1, out
        return json.loads(out)

    return _call


# --- script.py: bytecode identity -----------------------------------------


def bytecode(call, orig, mutated):
    return call(script, {"orig": orig, "mutated": mutated})


def test_constant_folded_mutant_is_equivalent(call):
    # CPython folds `1 + 2` to `3`, so both compile to the same code.
    assert bytecode(call, "x = 1 + 2\n", "x = 3\n") == {
        "equivalent": True,
        "error": None,
    }


def test_comments_and_layout_do_not_matter(call):
    orig = "def f(a):\n    return a + 1\n"
    mutated = "def f(a):  # same\n\n    return (a + 1)\n"
    assert bytecode(call, orig, mutated)["equivalent"] is True


@pytest.mark.parametrize(
    ("orig", "mutated"),
    [
        pytest.param("x = 1\n", "x = 2\n", id="module-constant"),
        pytest.param("x = y\n", "x = z\n", id="module-name"),
        pytest.param(
            "def f(a):\n    return a + 1\n",
            "def f(a):\n    return a - 1\n",
            id="nested-opcode",
        ),
        pytest.param(
            "def f():\n    return 1\n", "def f():\n    return 2\n", id="nested-constant"
        ),
        pytest.param(
            "def f(a):\n    return a\n",
            "def f(b):\n    return b\n",
            id="renamed-argument",
        ),
    ],
)
def test_behavioural_changes_are_not_equivalent(call, orig, mutated):
    assert bytecode(call, orig, mutated) == {"equivalent": False, "error": None}


def _function_code():
    module = compile(
        "def f(a):\n    x = 1\n    def g():\n        return x\n    return g\n",
        "<f>",
        "exec",
    )
    return next(c for c in module.co_consts if isinstance(c, types.CodeType))


@pytest.mark.parametrize(
    ("field", "value"),
    [
        ("co_names", ("other",)),
        ("co_varnames", ("b", "g")),
        ("co_freevars", ("y",)),
        ("co_cellvars", ("y",)),
        ("co_flags", _function_code().co_flags ^ 0x20),
        ("co_argcount", 0),
        ("co_kwonlyargcount", 1),
        ("co_posonlyargcount", 1),
    ],
)
def test_signature_covers_every_field(field, value):
    # Each field alone must change the signature: two functions that differ
    # only there are not interchangeable.
    code = _function_code()
    assert script._code_signature(code) == script._code_signature(code.replace())
    assert script._code_signature(code) != script._code_signature(
        code.replace(**{field: value})
    )


def test_nested_code_is_compared_recursively():
    outer = compile(
        "def f():\n    def g():\n        return 1\n    return g\n", "<f>", "exec"
    )
    other = compile(
        "def f():\n    def g():\n        return 2\n    return g\n", "<f>", "exec"
    )
    sig = script._code_signature(outer)
    assert sig[-1], "f's code object is a nested signature, not a scalar"
    assert sig != script._code_signature(other)


def test_mutant_that_does_not_compile_is_not_equivalent(call):
    out = bytecode(call, "x = 1\n", "x = = 1\n")
    assert out["equivalent"] is False
    assert out["error"].startswith("compile: ")


def test_original_that_does_not_compile_is_reported(call):
    out = bytecode(call, "def (:\n", "x = 1\n")
    assert out["equivalent"] is False
    assert out["error"].startswith("compile: ")


@pytest.mark.parametrize("payload", ["not json", json.dumps({"orig": "x = 1\n"})])
def test_bad_payload(call, payload):
    out = call(script, payload)
    assert out["equivalent"] is False
    assert out["error"].startswith("bad payload: ")
