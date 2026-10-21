"""Tests for the equivalent-mutant detector's Python helpers.

`src/equiv/script.py` (bytecode identity) and `src/equiv/script_probe.py`
(Hypothesis differential probe) are embedded in the fermut binary with
`include_str!` and run as `python -c`. Each reads a JSON payload on stdin and
prints one JSON verdict, so the tests load them by path and drive `main()`
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
probe = _load("script_probe")


@pytest.mark.parametrize(
    ("name", "payload", "key"),
    [
        ("script", {"orig": "x = 1\n", "mutated": "x = 1\n"}, "equivalent"),
        ("script_probe", {"orig": "", "mutated": ""}, "verdict"),
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


# --- script_probe.py: Hypothesis differential probe -----------------------


def run_probe(call, orig, mutated, qualname="f", examples=25):
    return call(
        probe,
        {"orig": orig, "mutated": mutated, "qualname": qualname, "examples": examples},
    )


@pytest.mark.parametrize(
    "payload",
    [
        "not json",
        json.dumps({"orig": "", "mutated": ""}),
        json.dumps({"orig": "", "mutated": "", "qualname": "f", "examples": None}),
    ],
)
def test_probe_bad_payload(call, payload):
    out = call(probe, payload)
    assert out["verdict"] == "error"
    assert out["reason"].startswith("bad payload: ")
    assert out["counterexample"] is None


def test_probe_defaults_to_500_examples(call):
    src = "def f(x: int) -> int:\n    return x\n"
    out = call(probe, {"orig": src, "mutated": src, "qualname": "f"})
    assert out["verdict"] == "equivalent"
    assert out["reason"] == "500 examples agreed on f"


def test_probe_without_hypothesis_is_ineligible(call, monkeypatch):
    monkeypatch.setitem(sys.modules, "hypothesis", None)
    out = run_probe(
        call, "def f(x: int):\n    return x\n", "def f(x: int):\n    return x\n"
    )
    assert out == {
        "verdict": "ineligible",
        "reason": "hypothesis not installed",
        "counterexample": None,
    }


def test_probe_finds_a_counterexample(call):
    out = run_probe(
        call,
        "def f(x: int) -> int:\n    return x + 1\n",
        "def f(x: int) -> int:\n    return x + 2\n",
    )
    assert out["verdict"] == "not_equivalent"
    assert out["reason"] == "differential disagreement"
    assert out["counterexample"].startswith("{'x': ")


def test_probe_agreement_is_equivalent(call):
    out = run_probe(
        call,
        "def f(x: int, y: bool) -> int:\n    return x if y else -x\n",
        "def f(x: int, y: bool) -> int:\n    return -x if not y else x\n",
        examples=30,
    )
    assert out == {
        "verdict": "equivalent",
        "reason": "30 examples agreed on f",
        "counterexample": None,
    }


def test_probe_equal_exceptions_agree(call):
    # Both raise the same error for every input: an error-path-only mutant
    # must not count as a disagreement.
    out = run_probe(
        call,
        "def f(x: int) -> int:\n    return x // 0\n",
        "def f(x: int) -> int:\n    return (x + 1) // 0\n",
    )
    assert out["verdict"] == "equivalent"


def test_probe_different_exception_messages_disagree(call):
    out = run_probe(
        call,
        "def f(x: int) -> int:\n    raise ValueError('a')\n",
        "def f(x: int) -> int:\n    raise ValueError('b')\n",
    )
    assert out["verdict"] == "not_equivalent"


def test_probe_different_exception_types_disagree(call):
    out = run_probe(
        call,
        "def f(x: int) -> int:\n    raise ValueError('a')\n",
        "def f(x: int) -> int:\n    raise TypeError('a')\n",
    )
    assert out["verdict"] == "not_equivalent"


def test_probe_mutant_that_is_not_callable_is_ineligible(call):
    out = run_probe(call, "def f(x: int):\n    return x\n", "f = 1\n")
    assert out["verdict"] == "ineligible"
    assert out["reason"] == "target is not callable"


def test_probe_resolves_dotted_qualnames(call):
    src = "class C:\n    @staticmethod\n    def f(x: int) -> int:\n        return x\n"
    mutated = src.replace("return x", "return x * 2")
    assert run_probe(call, src, mutated, qualname="C.f")["verdict"] == "not_equivalent"


@pytest.mark.parametrize(
    ("orig", "qualname", "reason"),
    [
        ("def f(x: int):\n    print(x)\n", "f", "source uses an identifier"),
        (
            "x = 1 / 0\ndef f(y: int):\n    return y\n",
            "f",
            "import failed: ZeroDivisionError",
        ),
        ("def f(x: int):\n    return x\n", "g", "resolve failed:"),
        ("f = 1\n", "f", "target is not callable"),
        (
            "class C:\n    def m(self, x: int):\n        return x\n",
            "C.m",
            "instance/class methods unsupported",
        ),
        ("def f(*xs: int):\n    return xs\n", "f", "variadic parameters unsupported"),
        ("def f(**kw: int):\n    return kw\n", "f", "variadic parameters unsupported"),
        ("def f(x):\n    return x\n", "f", "parameter 'x' not annotated"),
        ("def f(x: 'Missing'):\n    return x\n", "f", "signature/hints failed:"),
    ],
)
def test_probe_ineligible(call, orig, qualname, reason):
    out = run_probe(call, orig, orig, qualname=qualname)
    assert out["verdict"] == "ineligible", out
    assert out["reason"].startswith(reason), out["reason"]
    assert out["counterexample"] is None


def test_probe_strategy_failure_is_ineligible(call, monkeypatch):
    # Current Hypothesis resolves `from_type` lazily; older ones could raise
    # up front, which the probe reports as ineligible.
    import hypothesis.strategies

    def refuse(t):
        raise TypeError("no strategy")

    monkeypatch.setattr(hypothesis.strategies, "from_type", refuse)
    out = run_probe(
        call, "def f(x: int):\n    return x\n", "def f(x: int):\n    return x\n"
    )
    assert out["verdict"] == "ineligible"
    assert out["reason"] == "from_type(<class 'int'>) failed: no strategy"


def test_probe_harness_error(call):
    # The strategy resolves, then fails when Hypothesis draws from it.
    src = (
        "class Opaque:\n"
        "    def __init__(self, thing):\n"
        "        pass\n"
        "def f(x: Opaque):\n"
        "    return 1\n"
    )
    out = run_probe(call, src, src)
    assert out["verdict"] == "error", out
    assert out["reason"].startswith("ResolutionFailed: "), out["reason"]


def test_probe_restores_sys_path(call):
    before = list(sys.path)
    run_probe(
        call,
        "def f(x: int):\n    return x\n",
        "def f(x: int):\n    return x\n",
        examples=5,
    )
    assert sys.path == before


# Spelled out, not read from `probe._BANNED`, so a name dropped from the
# script's list fails here.
BANNED = [
    "open", "print", "input", "exec", "eval", "compile", "requests", "urllib",
    "socket", "subprocess", "os", "shutil", "pathlib", "random", "time",
    "datetime", "logging",
]  # fmt: skip


@pytest.mark.parametrize("name", BANNED)
def test_every_banned_name_disqualifies(name):
    assert probe._module_uses_banned(f"x = {name}\n") is True


@pytest.mark.parametrize(
    "source",
    [
        "print(1)\n",
        "import os.path\n",
        "from subprocess import run\n",
        "from os.path import join\n",
        "x = shutil.copy\n",
        "def (:\n",
    ],
)
def test_banned_sources(source):
    assert probe._module_uses_banned(source) is True


@pytest.mark.parametrize(
    "source",
    [
        "def f(x):\n    return x.open\n",
        "import math\n",
        "from . import sibling\n",
        "from json import dumps\n",
    ],
)
def test_allowed_sources(source):
    assert probe._module_uses_banned(source) is False


def test_self_and_cls_mark_methods():
    def method(self, x):
        pass

    def class_method(cls):
        pass

    def plain(x, self):
        pass

    def nullary():
        pass

    assert probe._is_instance_method(method)
    assert probe._is_instance_method(class_method)
    assert not probe._is_instance_method(plain)
    assert not probe._is_instance_method(nullary)
