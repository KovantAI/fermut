"""Phase 1: run schematized mutants by forking a warm pytest template.

Run with the repo's venv python:
    python forkserver.py <ref.json> <repo> <mirror> <src-root> <tests> <out.json>
                         [--jobs N] [--timeout S]

Template: chdir to the mirror, import pytest, collect the whole suite once (so
SUT + test modules sit in sys.modules), then per mutant fork a child that sets
`_fm_schema.ACTIVE = idx` and runs `pytest.main(-x <selected ids>)`.

Test selection mirrors fermut's (src/filter/coverage, src/kill_order.rs):
coverage contexts on the mutant's line (fallback: statement head line),
ordered by breadth in the mutated file then global breadth, then lifted by
`.fermut/kill-order.json` counts.

Fallback mutants keep the ref verdict and duration (they'd be spawned cold).
"""

import argparse
import getpass
import glob
import json
import os
import shutil
import signal
import sys
import tempfile
import threading
import time
import traceback
from collections import defaultdict
from pathlib import Path

ap = argparse.ArgumentParser()
for name in ("ref", "repo", "mirror", "src_root", "tests", "out"):
    ap.add_argument(name)
ap.add_argument("--jobs", type=int, default=os.cpu_count())
ap.add_argument("--timeout", type=float, default=10.0)
ap.add_argument(
    "--limit", type=int, default=0, help="only the first N schema mutants (smoke test)"
)
ap.add_argument("--only", help="run just this mutant id (triage)")
ap.add_argument(
    "--debug",
    action="store_true",
    help="children print failing reports to the real stderr",
)
ap.add_argument(
    "--collect-once",
    action="store_true",
    help="template runs one real session; children run collected items directly",
)
a = ap.parse_args()
repo, mirror = Path(a.repo).resolve(), Path(a.mirror).resolve()
src_root = (repo / a.src_root).resolve()
out_path, ref_path = Path(a.out).resolve(), Path(a.ref).resolve()

# --- selection -----------------------------------------------------------------
cov = json.loads(Path(repo / "coverage.json").read_text())["files"]
per_line, file_breadth, breadth = (
    {},
    defaultdict(lambda: defaultdict(int)),
    defaultdict(int),
)
for f, data in cov.items():
    lines = {}
    for line, ctxs in data.get("contexts", {}).items():
        ids = list(dict.fromkeys(c.split("|")[0] for c in ctxs if c.split("|")[0]))
        lines[int(line)] = ids
        for t in ids:
            file_breadth[f][t] += 1
            breadth[t] += 1
    per_line[f] = lines
kill_order = {}
ko_path = repo / ".fermut" / "kill-order.json"
if ko_path.exists():
    kill_order = json.loads(Path(ko_path).read_text())


def selection(m):
    rel = str(Path(m["file"]).resolve().relative_to(repo))
    lines = per_line.get(rel, {})
    ids = lines.get(m["line"]) or (
        lines.get(m.get("stmt_line", 0))
        if m.get("stmt_line") not in (0, m["line"])
        else None
    )
    if not ids:
        return None
    fb = file_breadth[rel]
    ids = sorted(ids, key=lambda t: (fb.get(t, 1 << 32), breadth.get(t, 1 << 32)))
    counts = kill_order.get(
        str(Path(m["file"]).resolve().relative_to(src_root)), {}
    ).get(m["operator"], {})
    return sorted(ids, key=lambda t: -counts.get(t, 0))


# --- template ------------------------------------------------------------------
ref = json.loads(Path(ref_path).read_text())
schema = json.loads(Path(mirror / "schema.json").read_text())
os.chdir(mirror)
for r in reversed(schema["roots"]):
    sys.path.insert(0, r)
os.environ["PYTHONPATH"] = os.pathsep.join(schema["roots"])
os.environ["PYTHONDONTWRITEBYTECODE"] = "1"
sys.dont_write_bytecode = True

# Record every schematized function that runs before the fork (SUT import,
# conftest, test-module import, collection). Whatever it built from the
# original branch — a decorator's closure, a registry entry, a cached value —
# outlives the fork, so its mutants can't be trusted warm and go cold instead.
switched = {
    f: {first: (lo, hi) for first, lo, hi in fns}
    for f, fns in schema["functions"].items()
}
ran_before_fork = set()


def profile(frame, event, arg):
    if event == "call":
        code = frame.f_code
        fns = switched.get(code.co_filename)
        if fns and code.co_firstlineno in fns:
            ran_before_fork.add(fns[code.co_firstlineno])


def stop_profiling():
    sys.setprofile(None)
    threading.setprofile(None)
    unsafe = sorted(ran_before_fork)
    keep = []
    for m, idx, o in queue:
        if any(lo <= idx <= hi for lo, hi in unsafe):
            results.append(
                {
                    "id": m["id"],
                    "status": o["status"],
                    "duration_ms": o.get("duration_ms", 0),
                    "fallback": "ran-before-fork",
                }
            )
        else:
            keep.append((m, idx, o))
    queue[:] = keep


sys.setprofile(profile)
threading.setprofile(profile)
t0 = time.perf_counter()
import _fm_schema
import pytest

devnull = os.open(os.devnull, os.O_WRONLY)
saved = os.dup(1), os.dup(2)


def quiet(on):
    sys.stdout.flush()
    sys.stderr.flush()
    os.dup2(devnull if on else saved[0], 1)
    os.dup2(devnull if on else saved[1], 2)


results, queue = [], []
for o in ref["outcomes"]:
    if o["status"] not in ("killed", "survived", "timed_out", "error"):
        continue
    m = o["mutant"]
    entry = schema["mutants"].get(m["id"], {"fallback": "missing"})
    if "idx" in entry:
        queue.append((m, entry["idx"], o))
    else:
        results.append(
            {
                "id": m["id"],
                "status": o["status"],
                "duration_ms": o.get("duration_ms", 0),
                "fallback": entry["fallback"],
            }
        )
if a.only:
    queue = [q for q in queue if q[0]["id"] == a.only]
if a.limit:
    queue = queue[: a.limit]


def child_recollect(idx, ids):
    return pytest.main(["--tb=no", "-q", "-x", "-p", "no:cacheprovider", *ids])


def child_items(session, by_id, idx, ids):
    """-x over already-collected items, same order and nextitem chain as a cold run."""
    items = [by_id.get(i) for i in dict.fromkeys(ids)]
    if None in items:
        return 4  # pytest: "not found" usage error
    # A cold run reorders its selection too (fixtures.py groups items by
    # parametrized higher-scope fixtures); skipping that changes which
    # module/session teardowns run between tests.
    session.config.hook.pytest_collection_modifyitems(
        session=session, config=session.config, items=items
    )
    failed = []

    class Listen:
        def pytest_runtest_logreport(self, report):
            if report.failed:
                failed.append(report.nodeid)
                if a.debug:
                    os.write(
                        saved[1],
                        f"FAILED {report.nodeid} ({report.when})\n{report.longreprtext}\n".encode(),
                    )

    session.config.pluginmanager.register(Listen())
    for i, item in enumerate(items):
        nxt = items[i + 1] if i + 1 < len(items) else None
        item.config.hook.pytest_runtest_protocol(item=item, nextitem=nxt)
        if failed:
            return 1
    return 0


def verdict(code):
    if code == 0:
        return "survived"
    if code in (3, 4, 5):
        return "error"
    return "killed"


real_user = getpass.getuser()
temproot = tempfile.gettempdir()
# The next numbered dir a cold run would get (pytest-<max+1>).
_nums = [
    int(d.rsplit("-", 1)[1])
    for d in glob.glob(os.path.join(temproot, f"pytest-of-{real_user}", "pytest-*"))
    if d.rsplit("-", 1)[1].isdigit()
]
seed_n = max(_nums, default=-1) + 1


def fake_user(pid):
    """Unique per pid; same length as the real user name when that's >= 8 chars
    (shorter names get 8 — pids need 7 digits)."""
    return f"f{pid:0{max(len(real_user), 8) - 1}d}"


def run_loop(run_child):
    running = {}
    while queue or running:
        while queue and len(running) < a.jobs:
            m, idx, _ = queue.pop(0)
            ids = selection(m)
            if not ids:
                results.append(
                    {
                        "id": m["id"],
                        "status": "error",
                        "duration_ms": 0,
                        "note": "no selection",
                    }
                )
                continue
            pid = os.fork()
            if pid == 0:
                # Private basetemp per child: pytest prunes old numbered dirs
                # under pytest-of-<user>, so siblings sharing one would delete
                # each other's tmp_path. A same-length fake USER keeps the path
                # the same length as a cold run's — typer asserts on 80-column
                # wrapped error text that contains it — and the seeded counter
                # gives pytest-<N> the cold run's digit count.
                user = fake_user(os.getpid())
                tmp = os.path.join(temproot, f"pytest-of-{user}")
                os.makedirs(os.path.join(tmp, f"pytest-{seed_n - 1}"), exist_ok=True)
                os.environ["USER"] = os.environ["LOGNAME"] = user
                os.setpgid(0, 0)
                signal.signal(signal.SIGINT, signal.SIG_DFL)
                _fm_schema.ACTIVE = idx
                # Python subprocesses a test spawns import the schema afresh.
                os.environ["FM_ACTIVE"] = str(idx)
                try:
                    code = run_child(idx, ids)
                except BaseException:  # noqa: BLE001 - nothing may escape a forked child
                    code = 3
                shutil.rmtree(tmp, ignore_errors=True)
                os._exit(int(code))
            running[pid] = (m, time.perf_counter())
        pid, status = os.waitpid(-1, os.WNOHANG)
        now = time.perf_counter()
        if pid:
            if pid not in running:
                os.write(saved[1], f"reaped unknown pid {pid}\n".encode())
                continue
            m, start = running.pop(pid)
            code = os.waitstatus_to_exitcode(status)
            results.append(
                {
                    "id": m["id"],
                    "status": verdict(code),
                    "exit": code,
                    "duration_ms": round((now - start) * 1000),
                }
            )
            continue
        for pid, (m, start) in list(running.items()):
            if now - start > a.timeout:
                try:
                    os.killpg(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                os.waitpid(pid, 0)
                running.pop(pid)
                shutil.rmtree(
                    os.path.join(temproot, f"pytest-of-{fake_user(pid)}"),
                    ignore_errors=True,
                )
                results.append(
                    {
                        "id": m["id"],
                        "status": "timeout",
                        "duration_ms": round((now - start) * 1000),
                    }
                )
        time.sleep(0.0005)


def finish(template_s, phase_s):
    """Write results and exit at once: pytest's own session wrap-up after the
    loop (sessionfinish, tmp cleanup, shutdown) can take minutes and a real
    runner would never wait for it."""
    quiet(False)
    out_path.write_text(
        json.dumps(
            {
                "template_s": round(template_s, 2),
                "phase_s": round(phase_s, 2),
                "jobs": a.jobs,
                "outcomes": results,
            },
            indent=1,
        )
    )
    by = defaultdict(int)
    for r in results:
        by[r["status"] + (" (fallback)" if "fallback" in r else "")] += 1
    print(
        json.dumps(
            {"template_s": round(template_s, 2), "phase_s": round(phase_s, 2), **by}
        )
    )
    sys.stdout.flush()
    os._exit(0)


quiet(True)
if a.collect_once:

    class ForkServer:
        @pytest.hookimpl(tryfirst=True)
        def pytest_runtestloop(self, session):
            stop_profiling()
            template_s = time.perf_counter() - t0
            by_id = {item.nodeid: item for item in session.items}
            t1 = time.perf_counter()
            try:
                run_loop(lambda idx, ids: child_items(session, by_id, idx, ids))
            except BaseException:  # noqa: BLE001 - report, then hard-exit the template
                os.write(saved[1], traceback.format_exc().encode())
                os._exit(3)
            finish(template_s, time.perf_counter() - t1)

    rc = pytest.main(
        ["--tb=no", "-q", "-p", "no:cacheprovider", a.tests], plugins=[ForkServer()]
    )
else:
    rc = pytest.main(["--co", "-q", "-p", "no:cacheprovider", a.tests])
    stop_profiling()
    if rc == 0:
        template_s = time.perf_counter() - t0
        t1 = time.perf_counter()
        run_loop(child_recollect)
        finish(template_s, time.perf_counter() - t1)
quiet(False)
sys.exit(f"template session failed: {rc}")
