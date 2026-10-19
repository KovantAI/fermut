"""``pytest --fermut``: run mutation testing from the pytest command line.

Installed with fermut (``pytest11`` entry point). After a green test session it
refreshes per-test coverage with ``fermut coverage`` (when pytest-cov is
available), runs ``fermut run`` with this interpreter, prints the score and
the top survivors in pytest's terminal summary, and fails the session when the
mutation gate fails. What gets mutated, and how, comes from the project's
fermut config (``[tool.fermut]`` / ``fermut.toml``), exactly as for
``fermut run``; pytest's own test selection only decides whether the suite is
green.

Inert unless ``--fermut`` is given, inside an xdist worker, and inside any
suite run fermut itself spawns (``FERMUT_CHILD`` is set), so ``--fermut`` in
``addopts`` can't recurse.
"""

import contextlib
import json
import os
import shutil
import subprocess
import sys
import sysconfig
import tempfile

import pytest

CHILD_ENV = "FERMUT_CHILD"
BIN_ENV = "FERMUT_BIN"

_STATE = pytest.StashKey["_FermutState"]()


def pytest_addoption(parser):
    group = parser.getgroup("fermut", "mutation testing (fermut)")
    group.addoption(
        "--fermut",
        action="store_true",
        default=False,
        help="after a green test session, run fermut mutation testing and fail "
        "the session if the mutation gate fails.",
    )
    group.addoption(
        "--fermut-since",
        metavar="REF",
        default=None,
        help="only mutate lines changed since this git ref (fermut run --since).",
    )
    group.addoption(
        "--fermut-min-score",
        metavar="PCT",
        type=float,
        default=None,
        help="fail only when the mutation score is below PCT "
        "(fermut run --fail-under). Without it any survivor fails, unless "
        "the fermut config sets fail_under.",
    )
    group.addoption(
        "--fermut-arg",
        metavar="ARG",
        action="append",
        default=[],
        help="extra argument for `fermut run` (repeatable), e.g. --fermut-arg=--sample=0.2",
    )
    group.addoption(
        "--fermut-no-coverage",
        action="store_true",
        default=False,
        help="don't refresh per-test coverage with `fermut coverage` first.",
    )
    group.addoption(
        "--fermut-show",
        metavar="N",
        type=int,
        default=10,
        help="number of surviving mutants to list in the summary (default 10).",
    )


class _FermutState:
    def __init__(self, binary):
        self.binary = binary
        self.lines = []  # (markup, text) for the terminal summary
        self.report = None


def _active(config):
    if not config.getoption("fermut"):
        return False
    if os.environ.get(CHILD_ENV):
        return False  # a suite run fermut spawned
    if hasattr(config, "workerinput"):
        return False  # an xdist worker; the controller runs fermut once
    return not config.getoption("collectonly")


def find_fermut():
    """The fermut binary: ``$FERMUT_BIN``, the one installed beside this
    interpreter (the fermut wheel's script), else ``fermut`` on PATH."""
    explicit = os.environ.get(BIN_ENV)
    if explicit:
        return explicit
    name = "fermut.exe" if os.name == "nt" else "fermut"
    beside = os.path.join(sysconfig.get_path("scripts"), name)
    if os.path.isfile(beside):
        return beside
    return shutil.which("fermut")


def pytest_configure(config):
    if not _active(config):
        return
    binary = find_fermut()
    if not binary:
        raise pytest.UsageError(
            "--fermut: the fermut binary was not found. Install fermut in this "
            "environment (e.g. `uv add --dev fermut`) or set FERMUT_BIN."
        )
    config.stash[_STATE] = _FermutState(binary)


def _rootdir(config):
    root = getattr(config, "rootpath", None) or config.rootdir
    return str(root)


def _child_env(config):
    env = dict(os.environ)
    env[CHILD_ENV] = "1"
    # fermut logs progress at info; inside a pytest session show only warnings
    # and errors unless pytest itself is verbose. An explicit RUST_LOG wins.
    if "RUST_LOG" not in env:
        env["RUST_LOG"] = "fermut=info" if config.getoption("verbose") > 0 else "fermut=warn"
    return env


def _refresh_coverage(state, config, root):
    if config.getoption("fermut_no_coverage"):
        return
    try:
        import importlib.util

        has_cov = importlib.util.find_spec("pytest_cov") is not None
    except (ImportError, ValueError):
        has_cov = False
    if not has_cov:
        state.lines.append(
            ({"yellow": True}, "pytest-cov not installed: mutants run the whole suite "
             "(slow). `pip install pytest-cov` to select tests by coverage.")
        )
        return
    cmd = [state.binary, "coverage", root, "--python", sys.executable, "-q"]
    with tempfile.TemporaryFile() as log:
        rc = subprocess.call(cmd, cwd=root, env=_child_env(config), stdout=log, stderr=log)
        if rc != 0:
            log.seek(0)
            tail = log.read().decode("utf-8", "replace").strip().splitlines()[-5:]
            state.lines.append(
                ({"yellow": True}, "`fermut coverage` exited %d; mutating with "
                 "whatever coverage exists:" % rc)
            )
            state.lines.extend(({}, "  " + line) for line in tail)


def _run_command(state, config, report_path):
    cmd = [state.binary, "run", "--python", sys.executable, "--json", report_path]
    since = config.getoption("fermut_since")
    if since:
        cmd += ["--since", since]
    min_score = config.getoption("fermut_min_score")
    if min_score is not None:
        cmd += ["--fail-under", repr(min_score)]
    cmd += config.getoption("fermut_arg")
    return cmd


def _load_report(path):
    try:
        with open(path, encoding="utf-8") as fh:
            report = json.load(fh)
    except (OSError, ValueError):
        return None
    return report if isinstance(report, dict) and "summary" in report else None


def _relative(path, root):
    try:
        rel = os.path.relpath(path, root)
    except ValueError:  # another drive on Windows
        return path
    return path if rel.startswith("..") else rel


def _summarize(state, report, root, show):
    s = report["summary"]
    score = s.get("mutation_score")
    score_text = "n/a" if score is None else "%.1f%%" % score
    state.lines.append(
        ({"bold": True}, "mutation score %s: %d killed, %d survived, %d timed out, "
         "%d errored (%d skipped)" % (score_text, s.get("killed", 0), s.get("survived", 0),
                                      s.get("timed_out", 0), s.get("errored", 0),
                                      s.get("skipped", 0)))
    )
    survivors = [o["mutant"] for o in report.get("outcomes", []) if o.get("status") == "survived"]
    if not survivors or show <= 0:
        return
    survivors.sort(key=lambda m: (m.get("file", ""), m.get("line", 0)))
    shown = survivors[:show]
    head = "surviving mutants" if len(shown) == len(survivors) else (
        "surviving mutants (%d of %d)" % (len(shown), len(survivors)))
    state.lines.append(({}, head + ":"))
    for m in shown:
        state.lines.append(
            ({"red": True}, "  %s:%s [%s] `%s` -> `%s`" % (
                _relative(m.get("file", "?"), root), m.get("line", "?"),
                m.get("operator", "?"), m.get("original", ""), m.get("replacement", "")))
        )
    state.lines.append(({}, "`fermut next` ranks which survivors to kill first."))


@pytest.hookimpl(trylast=True)
def pytest_sessionfinish(session, exitstatus):
    state = session.config.stash.get(_STATE, None)
    if state is None:
        return
    config = session.config
    if exitstatus != 0:
        state.lines.append(({"yellow": True}, "skipped: the test suite did not pass"))
        return
    root = _rootdir(config)
    capman = config.pluginmanager.getplugin("capturemanager")
    uncaptured = capman.global_and_fixture_disabled() if capman else contextlib.nullcontext()
    fd, report_path = tempfile.mkstemp(prefix="fermut-", suffix=".json")
    os.close(fd)
    try:
        with uncaptured:
            _refresh_coverage(state, config, root)
            sys.stderr.write("\nfermut: mutation testing %s\n" % root)
            sys.stderr.flush()
            # stdout carries fermut's human report, rebuilt below from the
            # JSON; stderr (progress, warnings, errors) streams through.
            rc = subprocess.call(
                _run_command(state, config, report_path),
                cwd=root,
                env=_child_env(config),
                stdout=subprocess.DEVNULL,
            )
        report = _load_report(report_path)
    finally:
        os.unlink(report_path)
    if report is None:
        # No report means fermut failed before scoring (its error is on stderr).
        state.lines.append(({"red": True}, "fermut exited %d without a report" % rc))
        session.exitstatus = pytest.ExitCode.INTERNAL_ERROR
        return
    state.report = report
    _summarize(state, report, root, config.getoption("fermut_show"))
    if rc != 0:
        state.lines.append(({"red": True, "bold": True}, "mutation gate failed"))
        session.exitstatus = pytest.ExitCode.TESTS_FAILED


def pytest_terminal_summary(terminalreporter, exitstatus, config):
    state = config.stash.get(_STATE, None)
    if state is None or not state.lines:
        return
    terminalreporter.write_sep("=", "fermut")
    for markup, text in state.lines:
        terminalreporter.write_line(text, **markup)
