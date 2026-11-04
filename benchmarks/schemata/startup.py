"""Phase 0: per-repo fixed cost of a cold pytest spawn vs a warm fork.

Run with the repo's venv python from the repo root:
    python startup.py <pkg> <nodeid> [n]
Prints one JSON line of medians (ms).
"""

import json
import os
import statistics
import subprocess
import sys
import time

pkg, nodeid = sys.argv[1], sys.argv[2]
n = int(sys.argv[3]) if len(sys.argv) > 3 else 7
env = {**os.environ, "PYTHONDONTWRITEBYTECODE": "1", "PYTHONPATH": os.getcwd()}


def cold(args):
    ts = []
    for _ in range(n):
        t = time.perf_counter()
        subprocess.run(
            [sys.executable, *args], env=env, capture_output=True, check=False
        )  # timing only
        ts.append((time.perf_counter() - t) * 1000)
    return round(statistics.median(ts), 1)


out = {
    "python": cold(["-c", "pass"]),
    "import_pytest": cold(["-c", "import pytest"]),
    "import_pytest_pkg": cold(["-c", f"import pytest, {pkg}"]),
    "spawn_one_test": cold(
        ["-m", "pytest", "-x", "-q", "-p", "no:cacheprovider", nodeid]
    ),
}

# Warm template: import pytest + SUT once, fork per run, child re-collects.
import pytest

__import__(pkg)


def fork_runs():
    ts = []
    for _ in range(n):
        t = time.perf_counter()
        pid = os.fork()
        if pid == 0:
            fd = os.open(os.devnull, os.O_WRONLY)
            os.dup2(fd, 1)
            os.dup2(fd, 2)
            os._exit(pytest.main(["-x", "-q", "-p", "no:cacheprovider", nodeid]))
        os.waitpid(pid, 0)
        ts.append((time.perf_counter() - t) * 1000)
    return round(statistics.median(ts), 1)


out["fork_one_test"] = fork_runs()
# Template also collects once: test modules + their imports stay in sys.modules,
# so each child's re-collection reuses them.
fd, saved = os.open(os.devnull, os.O_WRONLY), os.dup(1)
os.dup2(fd, 1)
pytest.main(["--co", "-q", "-p", "no:cacheprovider", nodeid])
os.dup2(saved, 1)
out["fork_after_collect"] = fork_runs()
out["k"] = round(out["spawn_one_test"] / out["fork_one_test"], 2)
out["k_collected"] = round(out["spawn_one_test"] / out["fork_after_collect"], 2)
print(json.dumps({"pkg": pkg, "nodeid": nodeid, **out}))
