"""Tests for the schemata helper (python/pytest_fermut/_fermut_schema.py).

The core property: with `_fm_active = idx`, a schematized module behaves
exactly like the source with that one mutant spliced in; with 0, exactly like
the original.
"""

import builtins
import importlib.util
import json
import os
import subprocess
import sys
import textwrap
import time
import types
from pathlib import Path

import pytest

HELPER = Path(__file__).resolve().parents[1] / "pytest_fermut" / "_fermut_schema.py"
spec = importlib.util.spec_from_file_location("_fermut_schema", HELPER)
schema = importlib.util.module_from_spec(spec)
spec.loader.exec_module(schema)

SOURCE = textwrap.dedent(
    '''\
    """Module doc."""
    from __future__ import annotations

    import functools

    LIMIT = 10
    counter = 0


    def plain(x):
        """Plain doc."""
        if x > 2:
            return x + 1
        return x - 1


    async def coro(x):
        await asyncio_sleep0()
        return x * 2


    async def asyncio_sleep0():
        return None


    def gen(n):
        for i in range(n):
            yield i * 3


    class Base:
        def name(self):
            return "base"


    class Child(Base):
        tag = "child"

        def name(self):
            return super().name() + "-" + self.tag


    def outer(k):
        def inner(v):
            return v + k
        return inner


    def requires(scope):
        def deco(fn):
            @functools.wraps(fn)
            def wrapper(*a):
                if scope == "admin":
                    return fn(*a)
                return "denied"
            return wrapper
        return deco


    @requires("admin")
    def admin_only():
        return "ok"


    def bump():
        global counter
        counter += 1
        return counter


    def make_counter():
        n = 0
        def step():
            nonlocal n
            n += 2
            return n
        return step


    def flagger():
        flag = False
        def set_flag(ok):
            if ok:
                try:
                    raise ValueError
                except ValueError:
                    nonlocal flag
                    flag = True
            return "set"
        set_flag(True)
        return flag


    def defaults(x=LIMIT):
        return x


    def warns(x):
        return x == "a"


    def probe():
        import asyncio
        step = make_counter()
        return (
            plain(1), plain(5), plain.__doc__,
            asyncio.run(coro(3)), list(gen(3)), Child().name(),
            outer(2)(3), admin_only(), bump(), bump(), step(), step(),
            defaults(), warns("a"), Child.tag, LIMIT, flagger(),
        )
    '''
)


def mutant(id_, needle, replacement, nth=0):
    """A mutant replacing the `nth` occurrence of `needle` in SOURCE."""
    raw = SOURCE.encode()
    start = -1
    for _ in range(nth + 1):
        start = raw.index(needle.encode(), start + 1)
    return {
        "id": id_,
        "file": "mod.py",
        "start": start,
        "end": start + len(needle.encode()),
        "replacement": replacement,
    }


MUTANTS = [
    mutant("plain-cmp", "x > 2", "x >= 2"),
    mutant("plain-add", "x + 1", "x - 1"),
    mutant("coro", "x * 2", "x * 3"),
    mutant("gen", "i * 3", "i * 4"),
    mutant("super", '"-"', '"+"'),
    mutant("inner", "v + k", "v - k"),
    mutant("wrapper", 'scope == "admin"', 'scope != "admin"'),
    mutant("global", "counter += 1", "counter += 5"),
    mutant("nonlocal", "n += 2", "n += 7"),
    # `nonlocal` nested inside try/except in set_flag: hoisting must still find it.
    mutant("nested-nonlocal", 'return "set"', 'return "unset"'),
    # Fallbacks:
    mutant("docstring", '"""Plain doc."""', '"""XX"""'),
    mutant("warning", 'x == "a"', 'x is "a"'),
    mutant("default", "x=LIMIT", "x=LIMIT + 1"),
    mutant("class-body", 'tag = "child"', 'tag = "kid"'),
    mutant("module", "LIMIT = 10", "LIMIT = 11"),
    mutant("decorator", '@requires("admin")', '@requires("user")'),
]
EXPECTED_FALLBACK = {
    "docstring": "docstring",
    "warning": "compile-warning",
    "default": "signature",
    "class-body": "class",
    "module": "module",
    "decorator": "module",  # a top-level def's decorator runs at module scope
}


def load(source, active):
    """Exec `source` as a fresh module with `_fm_active = active`; probe it."""
    setattr(builtins, schema.ACTIVE, active)
    try:
        mod = types.ModuleType("mod")
        exec(compile(source, "mod.py", "exec"), mod.__dict__)  # noqa: S102 - the system under test
        return mod.probe()
    finally:
        delattr(builtins, schema.ACTIVE)


def spliced(m):
    raw = SOURCE.encode()
    return (raw[: m["start"]] + m["replacement"].encode() + raw[m["end"] :]).decode()


@pytest.fixture(scope="module")
def built():
    counter = iter(range(1, 1000))
    out, entries, fns = schema.schematize_file(
        SOURCE.encode(), MUTANTS, lambda: next(counter)
    )
    return out, entries, fns


def test_fallback_reasons(built):
    _, entries, _ = built
    assert {
        k: v["fallback"] for k, v in entries.items() if "fallback" in v
    } == EXPECTED_FALLBACK


def test_inactive_schema_behaves_like_original(built):
    out, _, _ = built
    assert load(out, 0) == load(SOURCE, 0)


@pytest.mark.parametrize(
    "m", [m for m in MUTANTS if m["id"] not in EXPECTED_FALLBACK], ids=lambda m: m["id"]
)
def test_each_index_behaves_like_the_spliced_mutant(built, m):
    out, entries, _ = built
    assert load(out, entries[m["id"]]["idx"]) == load(spliced(m), 0)


def test_docstrings_survive(built):
    out, _, _ = built
    tree = __import__("ast").parse(out)
    assert __import__("ast").get_docstring(tree) == "Module doc."


def test_switched_functions_cover_every_index(built):
    _, entries, fns = built
    idxs = {e["idx"] for e in entries.values() if "idx" in e}
    covered = {i for _, lo, hi in fns for i in range(lo, hi + 1)}
    assert covered == idxs


def test_schematize_replaces_hardlinked_file_without_touching_the_original(tmp_path):
    original = tmp_path / "orig.py"
    original.write_text(SOURCE)
    mirror = tmp_path / "mirror"
    mirror.mkdir()
    os.link(original, mirror / "mod.py")
    result = schema.schematize(str(mirror), MUTANTS)
    assert original.read_text() == SOURCE
    assert (mirror / "mod.py").read_text() != SOURCE
    assert any("idx" in e for e in result["mutants"].values())


def test_unparsable_file_falls_back_without_failing(tmp_path):
    (tmp_path / "mod.py").write_text("def broken(:\n")
    result = schema.schematize(str(tmp_path), [MUTANTS[0]])
    assert result["mutants"]["plain-cmp"]["fallback"].startswith("file:")


# --- serve (end to end over the stdio protocol) ---------------------------------

PROJECT = {
    "pkg/__init__.py": "",
    "pkg/core.py": textwrap.dedent(
        """\
        REGISTRY = {}


        def register(name):
            REGISTRY[name] = len(name)
            return name


        def add(a, b):
            return a + b


        def spin(n):
            while n > 0:
                n = n + 1
            return n


        register("alpha")
        """
    ),
    "tests/test_core.py": textwrap.dedent(
        """\
        from pkg.core import REGISTRY, add, spin


        def test_add():
            assert add(2, 3) == 5


        def test_registry():
            assert REGISTRY == {"alpha": 5}


        def test_spin():
            assert spin(0) == 0


        def test_tmp(tmp_path):
            (tmp_path / "f").write_text("x")
            assert (tmp_path / "f").read_text() == "x"
        """
    ),
}


def project_mutant(id_, file, needle, replacement, root):
    raw = (root / file).read_bytes()
    start = raw.index(needle.encode())
    return {
        "id": id_,
        "file": file,
        "start": start,
        "end": start + len(needle),
        "replacement": replacement,
    }


class Server:
    def __init__(self, root, schema_path):
        env = {**os.environ, "PYTHONPATH": str(root), "PYTHONDONTWRITEBYTECODE": "1"}
        env.pop("FM_ACTIVE", None)
        self.proc = subprocess.Popen(
            [
                sys.executable,
                str(HELPER),
                "serve",
                str(root),
                str(schema_path),
                "-q",
                "-p",
                "no:cacheprovider",
                "tests",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
            cwd=root,
            env=env,
        )

    def recv(self):
        return json.loads(self.proc.stdout.readline())

    def send(self, obj):
        self.proc.stdin.write(json.dumps(obj) + "\n")
        self.proc.stdin.flush()

    def run(self, idx, ids, kill=False):
        self.send({"idx": idx, "ids": ids, "all": False})
        pid = self.recv()["pid"]
        if kill:
            time.sleep(0.5)  # let it get into the test
            os.killpg(pid, 9)
        result = self.recv()
        status = result.pop("status")
        result["exit"] = (
            os.waitstatus_to_exitcode(status) if os.WIFEXITED(status) else None
        )
        result["signal"] = os.WTERMSIG(status) if os.WIFSIGNALED(status) else None
        return result

    def close(self):
        self.proc.stdin.close()
        assert self.proc.wait(timeout=10) == 0


@pytest.fixture
def served(tmp_path):
    for rel, text in PROJECT.items():
        (tmp_path / rel).parent.mkdir(parents=True, exist_ok=True)
        (tmp_path / rel).write_text(text)
    muts = [
        project_mutant("add", "pkg/core.py", "a + b", "a - b", tmp_path),
        project_mutant(
            "register", "pkg/core.py", "len(name)", "len(name) + 1", tmp_path
        ),
        project_mutant("spin", "pkg/core.py", "n > 0", "n >= 0", tmp_path),
    ]
    result = schema.schematize(str(tmp_path), muts)
    (tmp_path / "schema.json").write_text(json.dumps(result))
    server = Server(tmp_path, tmp_path / "schema.json")
    yield server, {k: v["idx"] for k, v in result["mutants"].items()}
    if server.proc.poll() is None:
        server.close()


def test_serve_kills_survives_and_flags_pre_fork_functions(served):
    server, idx = served
    ready = server.recv()
    assert ready["ready"] is True
    # register() ran at import, in the template: its mutant can't be served.
    assert [idx["register"], idx["register"]] in ready["unsafe"]
    assert [idx["add"], idx["add"]] not in ready["unsafe"]

    killed = server.run(idx["add"], ["tests/test_core.py::test_add"])
    assert killed["exit"] == 1 and killed["failed"] == ["tests/test_core.py::test_add"]

    survived = server.run(idx["add"], ["tests/test_core.py::test_registry"])
    assert survived["exit"] == 0 and survived["failed"] == []
    assert survived["unfinished"] is None and survived["test_secs"] >= 0

    unknown = server.run(idx["add"], ["tests/test_core.py::nope"])
    assert unknown["exit"] == 4

    swept = server.run(0, None)
    assert swept["exit"] == 0

    server.close()


def test_serve_recovers_after_the_caller_kills_a_hung_child(served):
    server, idx = served
    assert server.recv()["ready"] is True
    hung = server.run(idx["spin"], ["tests/test_core.py::test_spin"], kill=True)
    assert hung["exit"] is None and hung["signal"] == 9
    assert hung["unfinished"] == "tests/test_core.py::test_spin"
    assert server.run(0, ["tests/test_core.py::test_spin"])["exit"] == 0
    server.close()


def test_children_get_private_tmp_roots_of_cold_length(served, tmp_path):
    server, _ = served
    assert server.recv()["ready"] is True
    assert server.run(0, ["tests/test_core.py::test_tmp"])["exit"] == 0
    leftovers = [
        d
        for d in os.listdir(schema.tempfile.gettempdir())
        if d.startswith("pytest-of-f")
    ]
    assert leftovers == []
    server.close()


def test_fake_user_matches_real_user_length():
    t = object.__new__(schema.Template)
    t.user = "drorasaf"
    assert len(t.fake_user(42)) == len("drorasaf")
    assert t.fake_user(42) != t.fake_user(43)
    t.user = "bob"
    assert len(t.fake_user(99999)) == 8


def test_serve_refuses_on_collection_errors(tmp_path):
    (tmp_path / "tests").mkdir()
    (tmp_path / "tests/test_bad.py").write_text("import does_not_exist\n")
    (tmp_path / "schema.json").write_text(json.dumps({"functions": {}}))
    server = Server(tmp_path, tmp_path / "schema.json")
    msg = server.recv()
    assert msg["ready"] is False and "collection" in msg["error"]
    server.close()
