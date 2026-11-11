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
    # Drops gen's only yield: spliced, gen is a plain function; switched, it
    # would still be a generator.
    mutant("unyield", "yield i * 3", "None"),
    mutant("docstring", '"""Plain doc."""', '"""XX"""'),
    mutant("warning", 'x == "a"', 'x is "a"'),
    mutant("default", "x=LIMIT", "x=LIMIT + 1"),
    mutant("class-body", 'tag = "child"', 'tag = "kid"'),
    mutant("module", "LIMIT = 10", "LIMIT = 11"),
    mutant("decorator", '@requires("admin")', '@requires("user")'),
]
EXPECTED_FALLBACK = {
    "unyield": "structure",
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
        import threading
        import time

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


        def guard(n):
            return int(n)


        def start_worker():
            threading.Thread(target=time.sleep, args=(30,), daemon=True).start()


        register("alpha")
        """
    ),
    "tests/conftest.py": textwrap.dedent(
        """\
        import pytest


        @pytest.fixture(scope="session")
        def marker():
            yield
            open("torn-down", "w").close()
        """
    ),
    # Passes unless a request (or an earlier fork) set another suffix.
    "tests/test_db.py": "import os\n\n\ndef test_db_suffix():\n"
    '    assert os.environ.get("TOX_PARALLEL_ENV", "fm3") == "fm3"\n',
    "tests/test_core.py": textwrap.dedent(
        """\
        import os
        import time

        from concurrent.futures import ThreadPoolExecutor

        from pkg.core import REGISTRY, add, guard, spin, start_worker


        def test_add():
            assert add(2, 3) == 5


        def test_registry():
            assert REGISTRY == {"alpha": 5}


        def test_spin():
            assert spin(0) == 0


        def test_tmp(tmp_path):
            (tmp_path / "f").write_text("x")
            assert (tmp_path / "f").read_text() == "x"


        def test_home():
            home = os.environ["HOME"]
            assert os.path.basename(home) == f"h-{os.getpid()}"
            assert os.path.isdir(home)
            assert os.environ["XDG_CONFIG_HOME"] == os.path.join(home, ".config")
            os.makedirs(os.path.join(home, ".config", "app"))


        def test_add_with_marker(marker):
            assert add(1, 1) == 2


        def test_after_marker():
            pass


        def close_inherited_pipes():
            import stat

            for fd in range(3, 256):
                try:
                    if stat.S_ISFIFO(os.fstat(fd).st_mode):
                        os.close(fd)  # the event pipe among them
                except OSError:
                    pass


        def test_closes_inherited_pipes():
            close_inherited_pipes()


        def test_closes_inherited_pipes_then_hangs():
            close_inherited_pipes()
            assert spin(0) == 0  # hangs under the spin mutant


        def test_crashes_leaving_a_helper():
            import builtins
            import subprocess

            if not getattr(builtins, "_fm_active", 0):
                return  # only crashes with a mutant active

            helper = subprocess.Popen(["sleep", "30"])
            with open("helper.pid", "w") as f:
                f.write(str(helper.pid))
            os._exit(1)


        def test_forks_a_helper():
            if os.fork() == 0:  # inherits the child's event pipe
                time.sleep(30)
                os._exit(0)
            assert add(2, 3) == 5


        def test_guard():
            assert guard(1) == 1


        def test_worker():
            start_worker()


        def test_leaves_an_idle_executor():
            ThreadPoolExecutor(1).submit(add, 1, 2).result()


        def test_add_in_a_scrubbed_subprocess():
            import subprocess
            import sys

            check = "from pkg.core import add; assert add(2, 3) == 5"
            env = {"PYTHONPATH": os.getcwd()}  # no FM_ACTIVE
            subprocess.run([sys.executable, "-c", check], env=env, check=True)
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
    def __init__(self, root, schema_path, occupy_profiler=False, args=None):
        self.root = root
        # As fermut runs it: pytest's temp root inside the mirror's workdir.
        self.temproot = root / "pytest-temproot"
        self.temproot.mkdir(exist_ok=True)
        env = {
            **os.environ,
            "PYTHONPATH": str(root),
            "PYTHONDONTWRITEBYTECODE": "1",
            "PYTEST_DEBUG_TEMPROOT": str(self.temproot),
            # As fermut runs it by default: a private home per mirror, and a
            # marker telling the template to give each fork its own.
            "HOME": str(root / "mirror-home"),
            "FERMUT_PRIVATE_HOME": "1",
        }
        env.pop("FM_ACTIVE", None)
        # Another tool holding sys.monitoring's profiler slot pushes the
        # template onto its setprofile fallback.
        launch = (
            [
                "-c",
                (
                    "import runpy, sys; m = sys.monitoring; "
                    "m.use_tool_id(m.PROFILER_ID, 'other'); "
                    "sys.argv = sys.argv[1:]; "
                    "runpy.run_path(sys.argv[0], run_name='__main__')"
                ),
            ]
            if occupy_profiler
            else []
        )
        self.proc = subprocess.Popen(
            [
                sys.executable,
                *launch,
                str(HELPER),
                *(args or ["serve", str(root), str(schema_path)]),
                *([] if args else ["-q", "-p", "no:cacheprovider", "tests"]),
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

    def run(self, idx, ids, timeout=None, chunk=None, after=None, **extra):
        self.send(
            {
                "idx": idx,
                "ids": ids,
                "all": False,
                "timeout": timeout,
                "chunk": chunk,
                "after": after,
                **extra,
            }
        )
        self.recv()  # pid
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
def served(tmp_path, request):
    for rel, text in PROJECT.items():
        (tmp_path / rel).parent.mkdir(parents=True, exist_ok=True)
        (tmp_path / rel).write_text(text)
    muts = [
        project_mutant("add", "pkg/core.py", "a + b", "a - b", tmp_path),
        project_mutant(
            "register", "pkg/core.py", "len(name)", "len(name) + 1", tmp_path
        ),
        project_mutant("spin", "pkg/core.py", "n > 0", "n >= 0", tmp_path),
        project_mutant(
            "guard", "pkg/core.py", "return int(n)", "raise KeyboardInterrupt", tmp_path
        ),
        project_mutant(
            "worker", "pkg/core.py", "daemon=True", "daemon=False", tmp_path
        ),
    ]
    result = schema.schematize(str(tmp_path), muts)
    (tmp_path / "schema.json").write_text(json.dumps(result))
    server = Server(
        tmp_path, tmp_path / "schema.json", getattr(request, "param", False)
    )
    yield server, {k: v["idx"] for k, v in result["mutants"].items()}
    if server.proc.poll() is None:
        server.close()


@pytest.mark.parametrize(
    "served",
    [
        pytest.param(False, id="default-tracer"),
        pytest.param(
            True,
            id="setprofile-fallback",
            marks=pytest.mark.skipif(
                sys.version_info < (3, 12), reason="always setprofile below 3.12"
            ),
        ),
    ],
    indirect=True,
)
def test_serve_kills_survives_and_flags_pre_fork_functions(served):
    server, idx = served
    ready = server.recv()
    assert ready["ready"] is True
    # register() ran at import, in the template: its mutant can't be served.
    assert [idx["register"], idx["register"]] in ready["unsafe"]
    assert [idx["add"], idx["add"]] not in ready["unsafe"]

    killed = server.run(idx["add"], ["tests/test_core.py::test_add"])
    assert killed["exit"] == 1 and killed["failed"] == ["tests/test_core.py::test_add"]
    # The tests it ran, in order: what a kill's confirmation re-runs.
    assert killed["ran"] == ["tests/test_core.py::test_add"]
    both = server.run(
        idx["add"],
        ["tests/test_core.py::test_registry", "tests/test_core.py::test_add"],
    )
    assert both["ran"] == [
        "tests/test_core.py::test_registry",
        "tests/test_core.py::test_add",
    ]

    survived = server.run(idx["add"], ["tests/test_core.py::test_registry"])
    assert survived["exit"] == 0 and survived["failed"] == []
    assert survived["unfinished"] is None and survived["test_secs"] >= 0

    unknown = server.run(idx["add"], ["tests/test_core.py::nope"])
    assert unknown["exit"] == 4

    scrubbed = ["tests/test_core.py::test_add_in_a_scrubbed_subprocess"]
    assert server.run(idx["add"], scrubbed)["exit"] == 1
    assert server.run(idx["spin"], scrubbed)["exit"] == 0

    swept = server.run(0, None)
    assert swept["exit"] == 0

    server.close()
    assert not os.listdir(schema.active_dir(server.root))  # each child's file reaped


def test_serve_checks_one_slice_of_the_suite_per_chunk(served):
    server, idx = served
    assert server.recv()["ready"] is True
    # n tests in collection order; slice k of n is the k-th test alone.
    n = PROJECT["tests/test_core.py"].count("\ndef test_")
    first = server.run(idx["add"], None, chunk=[0, n])
    assert first["exit"] == 1 and first["failed"] == ["tests/test_core.py::test_add"]
    assert server.run(idx["add"], None, chunk=[1, n])["exit"] == 0
    # More slices than tests: an empty slice checks nothing and passes.
    assert server.run(idx["add"], None, chunk=[0, 2 * n])["exit"] == 0
    # Resuming past a test (its fork crashed) skips it and runs the rest:
    # slice 0 of n/2 is test_add, test_registry.
    whole = server.run(idx["add"], None, chunk=[0, n // 2])
    assert whole["failed"] == ["tests/test_core.py::test_add"]
    after = server.run(
        idx["add"], None, chunk=[0, n // 2], after="tests/test_core.py::test_add"
    )
    assert after["exit"] == 0
    server.close()


def test_serve_runs_requests_side_by_side(served):
    server, idx = served
    assert server.recv()["ready"] is True
    server.send({"idx": idx["spin"], "ids": ["tests/test_core.py::test_spin"], "id": 1})
    hung = server.recv()
    assert hung["id"] == 1
    server.send({"idx": idx["add"], "ids": ["tests/test_core.py::test_add"], "id": 2})
    assert server.recv()["id"] == 2  # its fork reply
    # The second request finishes while the first still hangs.
    done = server.recv()
    assert done["id"] == 2 and done["failed"] == ["tests/test_core.py::test_add"]
    os.killpg(hung["pid"], 9)
    killed = server.recv()
    assert killed["id"] == 1 and killed["unfinished"] == "tests/test_core.py::test_spin"
    server.close()


def test_a_mutant_raising_keyboardinterrupt_exits_2_like_pytest(served):
    server, idx = served
    assert server.recv()["ready"] is True
    out = server.run(idx["guard"], ["tests/test_core.py::test_guard"])
    # pytest: "interrupted" — scored as a kill, not a broken child.
    assert out["exit"] == 2 and "error" not in out, out
    assert server.run(0, ["tests/test_core.py::test_guard"])["exit"] == 0
    server.close()


def test_serve_kills_a_hung_child_at_its_timeout_and_recovers(served):
    server, idx = served
    assert server.recv()["ready"] is True
    hung = server.run(idx["spin"], ["tests/test_core.py::test_spin"], timeout=0.5)
    assert hung["timed_out"] is True
    assert hung["exit"] is None and hung["signal"] == 9
    assert hung["unfinished"] == "tests/test_core.py::test_spin"
    assert server.run(0, ["tests/test_core.py::test_spin"])["exit"] == 0
    server.close()


def test_a_non_daemon_thread_left_running_hangs_the_child_like_a_cold_exit(served):
    server, idx = served
    assert server.recv()["ready"] is True
    ids = ["tests/test_core.py::test_worker"]
    assert server.run(0, ids, timeout=5)["exit"] == 0
    hung = server.run(idx["worker"], ids, timeout=1)
    assert hung["timed_out"] is True and hung["signal"] == 9
    # Idle executor workers are non-daemon too, but its exit hook stops them.
    idle = server.run(
        0, ["tests/test_core.py::test_leaves_an_idle_executor"], timeout=5
    )
    assert idle["exit"] == 0 and idle["timed_out"] is False
    server.close()


def test_early_stop_still_tears_down_session_fixtures(served, tmp_path):
    server, idx = served
    assert server.recv()["ready"] is True
    ids = [
        "tests/test_core.py::test_add_with_marker",
        "tests/test_core.py::test_after_marker",
    ]
    killed = server.run(idx["add"], ids)
    assert killed["exit"] == 1
    assert (tmp_path / "torn-down").exists()
    server.close()


def test_a_helper_holding_the_event_pipe_does_not_stall_the_run(served):
    server, idx = served
    assert server.recv()["ready"] is True
    started = time.monotonic()
    killed = server.run(
        idx["add"], ["tests/test_core.py::test_forks_a_helper"], timeout=20
    )
    assert killed["exit"] == 1 and killed["timed_out"] is False
    assert time.monotonic() - started < 10
    server.close()


def test_a_child_that_loses_its_event_pipe_still_exits(served):
    server, _ = served
    assert server.recv()["ready"] is True
    lost = server.run(0, ["tests/test_core.py::test_closes_inherited_pipes"])
    assert lost["exit"] == 0
    assert server.run(0, ["tests/test_core.py::test_add"])["exit"] == 0
    server.close()


def test_a_child_that_loses_its_event_pipe_still_times_out(served):
    server, idx = served
    assert server.recv()["ready"] is True
    hung = server.run(
        idx["spin"],
        ["tests/test_core.py::test_closes_inherited_pipes_then_hangs"],
        timeout=1,
    )
    assert hung["timed_out"] is True and hung["signal"] == 9
    server.close()


@pytest.mark.skipif(not hasattr(os, "waitid"), reason="no waitid: can't sweep safely")
def test_a_crashed_childs_helpers_are_swept(served, tmp_path):
    server, idx = served
    assert server.recv()["ready"] is True
    crashed = server.run(
        idx["spin"], ["tests/test_core.py::test_crashes_leaving_a_helper"]
    )
    assert crashed["exit"] == 1
    helper = int((tmp_path / "helper.pid").read_text())
    for _ in range(100):  # reparented to init, which reaps it
        try:
            os.kill(helper, 0)
        except ProcessLookupError:
            break
        time.sleep(0.05)
    else:
        pytest.fail("helper still running")
    server.close()


def test_startup_tracebacks_reach_stderr(tmp_path):
    (tmp_path / "schema.json").write_text(json.dumps({"functions": {}}))
    out = subprocess.run(
        [
            sys.executable,
            str(HELPER),
            "serve",
            str(tmp_path / "missing"),
            str(tmp_path / "schema.json"),
        ],
        check=False,
        input="",
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert "FileNotFoundError" in out.stderr


def test_children_get_private_tmp_roots_of_cold_length(served, tmp_path):
    server, _ = served
    assert server.recv()["ready"] is True
    assert server.run(0, ["tests/test_core.py::test_tmp"])["exit"] == 0
    server.close()  # the template cleans up after answering
    leftovers = [
        d
        for root in (server.temproot, schema.tempfile.gettempdir())
        for d in os.listdir(root)
        if d.startswith("pytest-of-f")
    ]
    assert leftovers == []


def test_each_child_gets_a_private_home_removed_after(served):
    server, _ = served
    assert server.recv()["ready"] is True
    # Twice: a second fork must not see the first one's app dir.
    assert server.run(0, ["tests/test_core.py::test_home"])["exit"] == 0
    assert server.run(0, ["tests/test_core.py::test_home"])["exit"] == 0
    server.close()
    assert [d for d in os.listdir(server.temproot) if d.startswith("h-")] == []


def test_each_child_gets_its_requests_db_suffix(served):
    server, _ = served
    assert server.recv()["ready"] is True
    ids = ["tests/test_db.py::test_db_suffix"]
    assert server.run(0, ids, db_suffix="fm3")["exit"] == 0
    assert server.run(0, ids, db_suffix="fm4")["exit"] == 1
    assert server.run(0, ids)["exit"] == 0, "not left behind by the last fork"
    server.close()


def test_fake_user_matches_real_user_length():
    t = object.__new__(schema.Template)
    t.user = "drorasaf"
    assert len(t.fake_user(42)) == len("drorasaf")
    assert t.fake_user(42) != t.fake_user(43)
    t.user = "runner"
    assert len(t.fake_user(54321)) == len("runner")
    assert t.fake_user(654321) == "f654321", "a longer pid still fits"


def serve_with_thread(tmp_path, start):
    (tmp_path / "tests").mkdir()
    (tmp_path / "tests/test_bg.py").write_text(
        "import threading, time, queue\n"
        "def loop():\n    time.sleep(30)\n"
        f"{start}.start()\n"
        "def test_ok():\n    pass\n"
    )
    (tmp_path / "schema.json").write_text(json.dumps({"functions": {}}))
    server = Server(tmp_path, tmp_path / "schema.json")
    msg = server.recv()
    server.close()
    return msg


@pytest.mark.parametrize(
    "start, why",
    [
        ("threading.Thread(target=loop, name='poller', daemon=True)", "project code"),
        (
            "threading.Thread(target=time.sleep, args=(30,), name='poller', daemon=True)",
            "unknown",
        ),
        ("threading.Thread(target=queue.Queue().get, name='poller')", "non-daemon"),
    ],
)
def test_serve_refuses_risky_threads(tmp_path, start, why):
    msg = serve_with_thread(tmp_path, start)
    assert msg["ready"] is False and "poller" in msg["error"], why


def test_serve_allows_third_party_daemon_threads(tmp_path):
    # Like sqlmesh's analytics flusher: a daemon thread in library code.
    msg = serve_with_thread(
        tmp_path,
        "threading.Thread(target=queue.Queue().get, name='poller', daemon=True)",
    )
    assert msg["ready"] is True
    assert msg["threads"] == ["poller (queue.Queue.get)"]


def test_serve_refuses_on_collection_errors(tmp_path):
    (tmp_path / "tests").mkdir()
    (tmp_path / "tests/test_bad.py").write_text("import does_not_exist\n")
    (tmp_path / "schema.json").write_text(json.dumps({"functions": {}}))
    server = Server(tmp_path, tmp_path / "schema.json")
    msg = server.recv()
    assert msg["ready"] is False and "collection" in msg["error"]
    server.close()


@pytest.fixture
def rlimit(monkeypatch):
    """A fake `resource` module on Linux: records setrlimit calls."""
    calls = []
    fake = types.SimpleNamespace(
        RLIMIT_DATA=2,
        RLIM_INFINITY=-1,
        hard=-1,
        getrlimit=lambda _: (fake.hard, fake.hard),
        setrlimit=lambda which, lim: calls.append((which, lim)),
    )
    monkeypatch.setitem(sys.modules, "resource", fake)
    monkeypatch.setattr(sys, "platform", "linux")
    fake.calls = calls
    return fake


def test_cap_memory_sets_the_data_limit_capped_by_the_hard_limit(rlimit, monkeypatch):
    monkeypatch.setenv("FM_MEM_CAP", "1000")
    schema.cap_memory()
    rlimit.hard = 500
    schema.cap_memory()
    rlimit.hard = 5000
    schema.cap_memory()
    assert rlimit.calls == [(2, (1000, 1000)), (2, (500, 500)), (2, (1000, 1000))]


def test_cap_memory_is_a_no_op_unset_or_off_linux(rlimit, monkeypatch):
    monkeypatch.delenv("FM_MEM_CAP", raising=False)
    schema.cap_memory()
    monkeypatch.setenv("FM_MEM_CAP", "")
    schema.cap_memory()
    monkeypatch.setenv("FM_MEM_CAP", "1000")
    monkeypatch.setattr(sys, "platform", "darwin")
    schema.cap_memory()
    assert rlimit.calls == []


def test_rstest_runs_on_its_vendored_pytest(tmp_path, monkeypatch):
    vendor = tmp_path / "rstest_worker" / "_vendor"
    vendor.mkdir(parents=True)
    (tmp_path / "rstest_worker" / "__init__.py").write_text("")
    monkeypatch.syspath_prepend(str(tmp_path))
    monkeypatch.setattr(sys, "path", list(sys.path))
    schema.use_rstest_pytest()
    schema.use_rstest_pytest()
    assert sys.path[0] == str(vendor)
    assert sys.path.count(str(vendor)) == 1


def test_rstest_without_its_worker_refuses(monkeypatch):
    monkeypatch.setattr(importlib.util, "find_spec", lambda name: None)
    with pytest.raises(ImportError, match="rstest_worker"):
        schema.use_rstest_pytest()


# --- serve-unittest ----------------------------------------------------------

UNITTEST_PROJECT = {
    "pkg/__init__.py": "",
    "pkg/core.py": textwrap.dedent(
        """\
        def add(a, b):
            return a + b


        def guard(n):
            return int(n)
        """
    ),
    "tests/__init__.py": "",
    # Passes unless a request (or an earlier fork) set another suffix.
    "tests/test_db.py": "import os\n\n\ndef test_db_suffix():\n"
    '    assert os.environ.get("TOX_PARALLEL_ENV", "fm3") == "fm3"\n',
    "tests/test_core.py": textwrap.dedent(
        """\
        import unittest

        from pkg.core import add, guard


        class T(unittest.TestCase):
            @classmethod
            def tearDownClass(cls):
                open("torn-down", "w").close()

            def test_add(self):
                self.assertEqual(add(2, 3), 5)

            def test_guard(self):
                self.assertEqual(guard(1), 1)

            def test_sub(self):
                for i in range(2):
                    with self.subTest(i=i):
                        self.assertEqual(add(0, i), i)

            @unittest.expectedFailure
            def test_xfail(self):
                self.assertEqual(add(2, 2), 5)


        class WithFixture(unittest.TestCase):
            @classmethod
            def setUpClass(cls):
                assert add(1, 1) == 2

            def test_uses_fixture(self):
                pass
        """
    ),
}


@pytest.fixture
def served_unittest(tmp_path):
    def start(extra=None):
        for rel, text in {**UNITTEST_PROJECT, **(extra or {})}.items():
            (tmp_path / rel).parent.mkdir(parents=True, exist_ok=True)
            (tmp_path / rel).write_text(text)
        muts = [
            project_mutant("add", "pkg/core.py", "a + b", "a - b", tmp_path),
            project_mutant("plus1", "pkg/core.py", "a + b", "a + b + 1", tmp_path),
            project_mutant(
                "guard",
                "pkg/core.py",
                "return int(n)",
                "raise KeyboardInterrupt",
                tmp_path,
            ),
        ]
        result = schema.schematize(str(tmp_path), muts)
        (tmp_path / "schema.json").write_text(json.dumps(result))
        args = ["serve-unittest", str(tmp_path), str(tmp_path / "schema.json")]
        args += [str(tmp_path / "tests"), "test*.py"]
        server = Server(tmp_path, tmp_path / "schema.json", args=args)
        servers.append(server)
        return server, {k: v["idx"] for k, v in result["mutants"].items()}

    servers = []
    yield start
    for server in servers:
        if server.proc.poll() is None:
            server.close()


def test_serve_unittest_runs_the_suite_like_a_cold_discover(served_unittest, tmp_path):
    server, idx = served_unittest()
    assert server.recv()["ready"] is True
    # Failfast: the first failure ends the run, and tearDownClass still runs.
    killed = server.run(idx["add"], None)
    assert killed["exit"] == 1, killed
    assert killed["failed"] == ["test_core.T.test_add"]
    assert killed["ran"] == ["test_core.T.test_add"]
    assert (tmp_path / "torn-down").exists()
    assert server.run(0, None)["exit"] == 0
    assert server.run(idx["add"], ["test_core.T.test_guard"])["exit"] == 0
    assert server.run(idx["add"], ["test_core.T.nope"])["exit"] == 4
    sub = server.run(idx["add"], ["test_core.T.test_sub"])
    assert sub["exit"] == 1 and sub["failed"] == ["test_core.T.test_sub"]
    # An expected failure that passes fails the run.
    xfail = ["test_core.T.test_xfail"]
    assert server.run(0, xfail)["exit"] == 0
    assert server.run(idx["plus1"], xfail)["exit"] == 1
    # A failing setUpClass: no test began, so it is named among those run,
    # and a confirmation asking for it gets "not collected".
    fixture = server.run(idx["add"], ["test_core.WithFixture.test_uses_fixture"])
    holder = "setUpClass (test_core.WithFixture)"
    assert fixture["exit"] == 1 and fixture["ran"] == [holder], fixture
    assert server.run(0, [holder])["exit"] == 4
    server.close()


def test_serve_unittest_dies_of_sigint_on_a_keyboardinterrupt(served_unittest):
    server, idx = served_unittest()
    assert server.recv()["ready"] is True
    out = server.run(idx["guard"], ["test_core.T.test_guard"])
    # As a cold `python -m unittest`: not exit 2, a usage error to fermut.
    assert out["signal"] == 2 and "error" not in out, out
    server.close()


def test_serve_unittest_refuses_a_suite_with_import_errors(served_unittest):
    server, _ = served_unittest({"tests/test_broken.py": "import nope\n"})
    ready = server.recv()
    assert ready == {"ready": False, "error": "1 errors during collection"}, ready


DJANGO_PROJECT = {
    "pytest.ini": "[pytest]\nDJANGO_SETTINGS_MODULE = settings\n",
    "settings.py": textwrap.dedent(
        """\
        import os
        SECRET_KEY = "x"
        INSTALLED_APPS = ["app"]
        USE_TZ = True
        DATABASES = {"default": {"ENGINE": "django.db.backends.sqlite3", "NAME": "db.sqlite"}}
        if os.environ.get("FILE_DB"):
            DATABASES["default"]["TEST"] = {"NAME": "test.sqlite"}
        """
    ),
    "app/__init__.py": "",
    "app/models.py": textwrap.dedent(
        """\
        from django.db import models
        class Row(models.Model):
            n = models.IntegerField()
        """
    ),
    "app/migrations/__init__.py": "",
    "app/migrations/0001_initial.py": textwrap.dedent(
        """\
        import os
        from django.db import migrations, models
        def note(apps, schema_editor):
            with open("migrated.log", "a") as f:
                f.write(f"{os.getpid()}\\n")
        class Migration(migrations.Migration):
            initial = True
            operations = [
                migrations.CreateModel("Row", [
                    ("id", models.AutoField(primary_key=True)),
                    ("n", models.IntegerField()),
                ]),
                migrations.RunPython(note),
            ]
        """
    ),
    # A row one fork writes must not show up in the next one.
    "tests/test_rows.py": textwrap.dedent(
        """\
        from django.test import TestCase
        from app.models import Row
        class RowTest(TestCase):
            def test_alone(self):
                assert Row.objects.count() == 0
                Row.objects.create(n=1)
        """
    ),
}


@pytest.fixture
def django_project(tmp_path):
    pytest.importorskip("pytest_django")
    for rel, text in DJANGO_PROJECT.items():
        (tmp_path / rel).parent.mkdir(parents=True, exist_ok=True)
        (tmp_path / rel).write_text(text)
    (tmp_path / "schema.json").write_text(json.dumps({"functions": {}}))
    return tmp_path


def migrations_run(root):
    log = root / "migrated.log"
    return log.read_text().split() if log.exists() else []


def test_in_memory_django_db_is_migrated_once_before_forking(django_project):
    server = Server(django_project, django_project / "schema.json")
    assert server.recv()["ready"] is True
    assert len(migrations_run(django_project)) == 1, "built in the template"
    for _ in range(3):
        assert server.run(0, None)["exit"] == 0
    server.close()
    assert len(migrations_run(django_project)) == 1, "forks inherit it"


def test_file_django_db_is_left_to_each_fork(django_project, monkeypatch):
    monkeypatch.setenv("FILE_DB", "1")
    server = Server(django_project, django_project / "schema.json")
    assert server.recv()["ready"] is True
    assert migrations_run(django_project) == []
    assert server.run(0, None)["exit"] == 0
    assert server.run(0, None)["exit"] == 0
    server.close()
    assert len(migrations_run(django_project)) == 2, "one per fork, as before"


def test_a_failing_django_db_setup_refuses_the_template(django_project):
    migration = django_project / "app/migrations/0001_initial.py"
    migration.write_text(
        migration.read_text().replace(
            "def note(apps, schema_editor):",
            "def note(apps, schema_editor):\n    raise RuntimeError('boom')",
        )
    )
    server = Server(django_project, django_project / "schema.json")
    ready = server.recv()
    server.close()
    assert ready["ready"] is False
    assert "django_db_setup failed" in ready["error"] and "boom" in ready["error"]
