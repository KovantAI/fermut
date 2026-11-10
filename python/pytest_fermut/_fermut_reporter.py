"""fermut per-run result reporter (pytest plugin).

fermut loads this into every per-mutant pytest run with ``-p _fermut_reporter``
and points ``FERMUT_RESULT`` at a fresh file. The plugin appends one JSON object
per line as the run progresses, so even a run killed on timeout leaves the
events written so far. Without ``FERMUT_RESULT`` it does nothing.

Schema v1 (every event carries ``pid``; parallel runners such as rstest run
several pytest sessions that append to the same file):

- ``{"v":1,"ev":"start","pytest":"8.3.4"}`` -- session started
- ``{"ev":"conftest_error","path":...,"exc":...}`` -- an initial conftest
  failed to import (before any session exists, so no ``start`` precedes it)
- ``{"ev":"config_error","exc":...,"project":bool}`` -- configuring pytest
  failed some other way, e.g. a ``filterwarnings`` entry that imports the
  package under test; ``project`` says whether the exception chain passed
  through project code (under the rootdir, outside site-packages). pytest
  9.2+ applies ``filterwarnings`` before importing ``-p`` plugins, so there a
  filter that imports broken project code ends the run before this plugin
  loads; fermut's ``--collect-only`` probe reads it from pytest's output.
- ``{"ev":"collect","nodeid":...}`` -- a collector failed (import error)
- ``{"ev":"internal_error","exc":...,"project":bool}`` -- pytest hit an
  internal error (exit 3), e.g. ``SystemExit`` raised while a test module
  imports the package under test; ``project`` as for ``config_error``
- ``{"ev":"deselect","nodeid":...}`` -- a test was deselected (``--deselect``,
  ``-k``, ``-m``, or a plugin calling ``pytest_deselected``)
- ``{"ev":"begin","nodeid":...}`` -- a test started
- ``{"ev":"test","nodeid":...,"when":...,"dur":...}`` -- a test phase failed
- ``{"ev":"dur","nodeid":...,"dur":...}`` -- a passing test's call duration
- ``{"ev":"finish","exit":N}`` -- session finished

With ``FERMUT_COLLECT_CONTEXT`` set (``fermut coverage`` sets it), the plugin
also records import-time coverage: while a test module is collected, the
coverage context is ``<module nodeid>|collect``, so source lines that run when
the module imports them (module-level code, decorators, class bodies, default
arguments) are linked to that module instead of the anonymous ``""`` context.

With ``FERMUT_MIRROR_ROOT`` and ``FERMUT_PROJECT_ROOT`` set (fermut sets both
for runs in its copy of the project), importing this module makes parametrize
ids match the ones pytest gives in the real project; see
``_stable_long_str_ids``. The schemata template imports it for that alone.

Standard library only (``coverage`` is imported only in that mode, where it is
already running); this file is embedded in the fermut binary and must run on
every Python and pytest version fermut supports.
"""

import json
import os

import pytest

SCHEMA_VERSION = 1

_path = os.environ.get("FERMUT_RESULT")
_collect_context = bool(os.environ.get("FERMUT_COLLECT_CONTEXT"))
_fh = None
# The session's rootdir, for ``internal_error`` (that hook gets no config).
_root = None


def _stable_long_str_ids():
    """Give a parametrize value that holds the project path the id it has in
    the real project, not one that depends on where fermut copied it.

    pytest 9.2 ids a str/bytes value longer than 100 characters by argument
    name and index (``parametrize_long_str_id_strategy``, default ``short``)
    instead of by the value. A value built from a test file's path
    (werkzeug: ``str(res_path)``) changes length with the copy's path, so it
    can cross 100 on one side only: coverage recorded
    ``test_from_directory[directory0-test.txt]``, the copy collects
    ``test_from_directory[/tmp/.../res-test.txt]``, and pytest exits 4 on
    the recorded id. Deciding on the value with the project path put back
    gives the project's choice; a value id keeps the copy's path, which
    fermut maps back to the project's like any other id.
    """
    mirror = os.environ.get("FERMUT_MIRROR_ROOT")
    project = os.environ.get("FERMUT_PROJECT_ROOT")
    if not mirror or not project or mirror == project:
        return
    try:
        from _pytest.python import IdMaker, _ascii_escaped_by_config
    except ImportError:
        return
    original = getattr(IdMaker, "_apply_long_str_strategy", None)
    if original is None:  # pytest < 9.2: ids don't depend on length
        return

    def as_project(val):
        if isinstance(val, bytes):
            return val.replace(os.fsencode(mirror), os.fsencode(project))
        return val.replace(mirror, project)

    def apply(self, val, argname, idx):
        in_project = as_project(val)
        if in_project == val:
            return original(self, val, argname, idx)
        out = original(self, in_project, argname, idx)
        if out is not None and out == _ascii_escaped_by_config(in_project, self.config):
            return _ascii_escaped_by_config(val, self.config)
        return out  # by argname and index, or a hash of the project's value

    IdMaker._apply_long_str_strategy = apply


_stable_long_str_ids()


def _emit(event):
    global _fh
    if not _path:
        return
    if _fh is None:
        # Line-buffered: each event reaches the file as soon as it's written.
        _fh = open(_path, "a", buffering=1, encoding="utf-8")  # noqa: SIM115
    event["pid"] = os.getpid()
    _fh.write(json.dumps(event, separators=(",", ":")) + "\n")


def _in_project(path, root):
    path = os.path.abspath(path)
    return path.startswith(root + os.sep) and "site-packages" not in path


def _raised_in_project(err, root):
    """Whether ``err`` or any exception it chains from was raised in, or
    (for a ``SyntaxError``) points at, a file under ``root``."""
    seen = set()
    while err is not None and id(err) not in seen:
        seen.add(id(err))
        paths = [getattr(err, "filename", None)] if isinstance(err, SyntaxError) else []
        tb = err.__traceback__
        while tb is not None:
            paths.append(tb.tb_frame.f_code.co_filename)
            tb = tb.tb_next
        if any(p and _in_project(p, root) for p in paths):
            return True
        err = err.__cause__ or err.__context__
    return False


# Old-style hookwrapper on purpose: new-style ``wrapper=True`` needs pluggy 1.1+.
@pytest.hookimpl(hookwrapper=True)
def pytest_load_initial_conftests(early_config, parser, args):
    outcome = yield
    if outcome.excinfo is None:
        return
    err = outcome.excinfo[1]
    if type(err).__name__ == "ConftestImportFailure":
        # pytest 8+ keeps the underlying exception in ``cause``, 7.x in ``excinfo``.
        cause = (
            getattr(err, "cause", None)
            or (getattr(err, "excinfo", None) or (None, err))[1]
        )
        _emit(
            {
                "ev": "conftest_error",
                "path": str(getattr(err, "path", "")),
                "exc": type(cause).__name__,
            }
        )
        return
    root = str(getattr(early_config, "rootpath", None) or os.getcwd())
    _emit(
        {
            "ev": "config_error",
            "exc": type(err).__name__,
            "project": _raised_in_project(err, root),
        }
    )


def _switch_context(name):
    """Switch the running coverage measurement's context. False when coverage
    isn't measuring (not installed, not started, or too old to switch)."""
    try:
        import coverage

        cov = coverage.Coverage.current()
        if cov is None:
            return False
        cov.switch_context(name)
        return True
    except Exception:  # noqa: BLE001 - never break the user's test run
        return False


@pytest.hookimpl(hookwrapper=True)
def pytest_make_collect_report(collector):
    # Only test modules import the code under test. pytest-cov leaves the
    # context at "" until the first test runs, so "" is what to restore.
    switched = (
        _collect_context
        and isinstance(collector, pytest.Module)
        and _switch_context(collector.nodeid + "|collect")
    )
    yield
    if switched:
        _switch_context("")


def pytest_internalerror(excrepr, excinfo):
    # Collection only catches ``Exception``: a ``SystemExit`` raised while a
    # test module imports the code under test reaches pytest's internal-error
    # path. Through project code (with a green baseline) that is the mutant.
    _emit(
        {
            "ev": "internal_error",
            "exc": type(excinfo.value).__name__,
            "project": _raised_in_project(excinfo.value, _root or os.getcwd()),
        }
    )


def pytest_sessionstart(session):
    global _root
    _root = str(session.config.rootpath)
    _emit({"v": SCHEMA_VERSION, "ev": "start", "pytest": pytest.__version__})


def pytest_collectreport(report):
    if report.failed:
        _emit({"ev": "collect", "nodeid": report.nodeid})


def pytest_deselected(items):
    for item in items:
        _emit({"ev": "deselect", "nodeid": item.nodeid})


def pytest_runtest_logstart(nodeid, location):
    _emit({"ev": "begin", "nodeid": nodeid})


def pytest_runtest_logreport(report):
    if report.failed:
        _emit(
            {
                "ev": "test",
                "nodeid": report.nodeid,
                "when": report.when,
                "dur": report.duration,
            }
        )
    elif report.when == "call":
        _emit({"ev": "dur", "nodeid": report.nodeid, "dur": report.duration})


def pytest_sessionfinish(session, exitstatus):
    global _fh
    _emit({"ev": "finish", "exit": int(exitstatus)})
    if _fh is not None:
        _fh.close()
        _fh = None
