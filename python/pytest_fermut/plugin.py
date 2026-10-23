"""``pytest --fermut``: run mutation testing from the pytest command line.

Installed with fermut (``pytest11`` entry point). After a green test session it
refreshes per-test coverage with ``fermut coverage`` (when pytest-cov is
available), runs ``fermut run`` with this interpreter, prints the score and
the top survivors in pytest's terminal summary, and fails the session when the
mutation gate fails. What gets mutated, and how, comes from the project's
fermut config (``[tool.fermut]`` / ``fermut.toml``), exactly as for
``fermut run``.

A session that selected a subset of the suite (paths, node ids, ``-k``,
``-m``, ``--deselect``, ...) is *narrowed*: only its tests are run against the
mutants (``fermut run --select-tests``), and mutants none of them cover are
skipped as ``selection``. fermut still runs its baseline (only the selected
tests, for a narrowed session) inside its mirror: a green pytest session
doesn't prove the suite is green there (different ``PYTHONPATH``, isolation,
``FERMUT_CHILD``). ``--fermut-arg=--no-verify-baseline`` skips it.

Inert unless ``--fermut`` is given, inside an xdist worker, and inside any
suite run fermut itself spawns (``FERMUT_CHILD`` is set), so ``--fermut`` in
``addopts`` can't recurse.
"""

import contextlib
import glob
import json
import os
import shutil
import subprocess
import sys
import sysconfig
import tempfile
import time

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
        # Absolute node ids ("<abs test file>::<name>") the session selected,
        # or None when the session ran the whole suite.
        self.selected = None
        # For fermut's timeout budget when its own baseline is skipped: when
        # the session started (collection included) and the summed duration
        # of every test phase (under xdist the wall time is only a share).
        self.started = time.monotonic()
        self.test_secs = 0.0

    def suite_secs(self):
        return max(time.monotonic() - self.started, self.test_secs)


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
    config.pluginmanager.register(_TestTime(config.stash[_STATE]))


# Options that select a subset of the collected tests. Any of them set means
# the session ran less than the whole suite. Some come from plugins that may
# be disabled (cacheprovider: --lf), hence the getoption default.
_SELECTION_OPTIONS = (
    "keyword",
    "markexpr",
    "deselect",
    "ignore",
    "ignore_glob",
    "lf",
    "stepwise",
)


def _narrowed(config):
    """Whether the session selected less than the whole suite.

    Besides the selection options, the arguments must be the rootdir or
    *every* ``testpaths`` entry: ``pytest .`` runs the whole suite, and so
    does ``pytest tests`` when ``tests`` is the only entry; one of several
    entries, a file, a node id or a subdirectory (including running pytest
    from one) does not. Over-detecting never mis-scores, but a narrowed run
    isn't recorded in history, so ``testpaths`` is expanded as pytest does.
    """
    if any(config.getoption(name, False) for name in _SELECTION_OPTIONS):
        return True
    root = os.path.realpath(_rootdir(config))
    entries = {
        os.path.realpath(p)
        for entry in config.getini("testpaths")
        for p in glob.glob(os.path.join(root, entry), recursive=True)
    }
    cwd = str(config.invocation_params.dir)
    args = {os.path.realpath(os.path.join(cwd, arg)) for arg in config.args}
    return root not in args and args != entries


def _absolute_id(root, nodeid):
    """``tests/test_x.py::test_a`` → ``<root>/tests/test_x.py::test_a``.
    fermut re-roots it onto its own project root, which can differ from
    pytest's rootdir."""
    return os.path.join(root, nodeid)


class _TestTime:
    """Sums every test phase's duration into the session state. Registered
    only for an active session; on the xdist controller it sees every
    worker's reports."""

    def __init__(self, state):
        self.state = state

    def pytest_runtest_logreport(self, report):
        self.state.test_secs += report.duration


def pytest_collection_finish(session):
    state = session.config.stash.get(_STATE, None)
    if state is None or not _narrowed(session.config):
        return
    root = _rootdir(session.config)
    state.selected = [_absolute_id(root, item.nodeid) for item in session.items]


@pytest.hookimpl(optionalhook=True)
def pytest_xdist_node_collection_finished(node, ids):
    # Under xdist the controller collects nothing; each worker reports the
    # ids it collected (after -k/-m). They are all the same list.
    state = node.config.stash.get(_STATE, None)
    if state is None or state.selected or not _narrowed(node.config):
        return
    root = _rootdir(node.config)
    state.selected = [_absolute_id(root, nodeid) for nodeid in ids]


def _rootdir(config):
    root = getattr(config, "rootpath", None) or config.rootdir
    return str(root)


def _child_env(config):
    env = dict(os.environ)
    env[CHILD_ENV] = "1"
    # fermut logs progress at info; inside a pytest session show only warnings
    # and errors unless pytest itself is verbose. An explicit RUST_LOG wins.
    if "RUST_LOG" not in env:
        env["RUST_LOG"] = (
            "fermut=info" if config.getoption("verbose") > 0 else "fermut=warn"
        )
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
            (
                {"yellow": True},
                (
                    "pytest-cov not installed: mutants run the whole suite "
                    "(slow). `pip install pytest-cov` to select tests by coverage."
                ),
            )
        )
        return
    cmd = [state.binary, "coverage", root, "--python", sys.executable, "-q"]
    with tempfile.TemporaryFile() as log:
        rc = subprocess.call(
            cmd, cwd=root, env=_child_env(config), stdout=log, stderr=log
        )
        if rc != 0:
            log.seek(0)
            tail = log.read().decode("utf-8", "replace").strip().splitlines()[-5:]
            state.lines.append(
                (
                    {"yellow": True},
                    (
                        f"`fermut coverage` exited {rc}; mutating with "
                        "whatever coverage exists:"
                    ),
                )
            )
            state.lines.extend(({}, "  " + line) for line in tail)


def _run_command(state, config, report_path, selection_path=None, suite_secs=None):
    cmd = [state.binary, "run", "--python", sys.executable, "--json", report_path]
    if selection_path is not None:
        cmd += ["--select-tests", selection_path]
    extra = config.getoption("fermut_arg")
    # With fermut's baseline skipped, a whole-suite session stands in for it
    # when fermut sizes `--timeout-factor` timeouts (ignored without one).
    if (
        "--no-verify-baseline" in extra
        and selection_path is None
        and suite_secs is not None
        and not any(a.startswith("--baseline-secs") for a in extra)
    ):
        cmd.append(f"--baseline-secs={suite_secs:.3f}")
    since = config.getoption("fermut_since")
    if since:
        cmd += ["--since", since]
    min_score = config.getoption("fermut_min_score")
    if min_score is not None:
        cmd += ["--fail-under", repr(min_score)]
    cmd += extra
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
    score_text = "n/a" if score is None else f"{score:.1f}%"
    if state.selected is not None:
        outside = sum(
            1
            for o in report.get("outcomes", [])
            if o.get("status") == "skipped" and o.get("filter") == "selection"
        )
        score_text += (
            f" ({len(state.selected)} selected tests, "
            f"{outside} mutants outside the selection)"
        )
    state.lines.append(
        (
            {"bold": True},
            (
                f"mutation score {score_text}: {s.get('killed', 0)} killed, "
                f"{s.get('survived', 0)} survived, {s.get('timed_out', 0)} timed out, "
                f"{s.get('errored', 0)} errored ({s.get('skipped', 0)} skipped)"
            ),
        )
    )
    survivors = [
        o["mutant"] for o in report.get("outcomes", []) if o.get("status") == "survived"
    ]
    if not survivors or show <= 0:
        return
    survivors.sort(key=lambda m: (m.get("file", ""), m.get("line", 0)))
    shown = survivors[:show]
    head = (
        "surviving mutants"
        if len(shown) == len(survivors)
        else f"surviving mutants ({len(shown)} of {len(survivors)})"
    )
    state.lines.append(({}, head + ":"))
    for m in shown:
        state.lines.append(
            (
                {"red": True},
                "  {}:{} [{}] `{}` -> `{}`".format(
                    _relative(m.get("file", "?"), root),
                    m.get("line", "?"),
                    m.get("operator", "?"),
                    m.get("original", ""),
                    m.get("replacement", ""),
                ),
            )
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
    # Before the coverage refresh, which would count toward it.
    suite_secs = state.suite_secs()
    root = _rootdir(config)
    capman = config.pluginmanager.getplugin("capturemanager")
    uncaptured = (
        capman.global_and_fixture_disabled() if capman else contextlib.nullcontext()
    )
    fd, report_path = tempfile.mkstemp(prefix="fermut-", suffix=".json")
    os.close(fd)
    selection_path = None
    try:
        if state.selected is not None:
            fd, selection_path = tempfile.mkstemp()
            with os.fdopen(fd, "wb") as fh:
                fh.write("".join(f"{n}\n" for n in state.selected).encode())
        with uncaptured:
            _refresh_coverage(state, config, root)
            sys.stderr.write(f"\nfermut: mutation testing {root}\n")
            sys.stderr.flush()
            # stdout carries fermut's human report, rebuilt below from the
            # JSON; stderr (progress, warnings, errors) streams through.
            rc = subprocess.call(
                _run_command(state, config, report_path, selection_path, suite_secs),
                cwd=root,
                env=_child_env(config),
                stdout=subprocess.DEVNULL,
            )
        report = _load_report(report_path)
    finally:
        os.unlink(report_path)
        if selection_path is not None:
            os.unlink(selection_path)
    if report is None:
        # No report means fermut failed before scoring (its error is on stderr).
        state.lines.append(({"red": True}, f"fermut exited {rc} without a report"))
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
