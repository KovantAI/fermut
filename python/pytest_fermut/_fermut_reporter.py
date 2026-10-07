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
  through project code (under the rootdir, outside site-packages)
- ``{"ev":"collect","nodeid":...}`` -- a collector failed (import error)
- ``{"ev":"begin","nodeid":...}`` -- a test started
- ``{"ev":"test","nodeid":...,"when":...,"dur":...}`` -- a test phase failed
- ``{"ev":"dur","nodeid":...,"dur":...}`` -- a passing test's call duration
- ``{"ev":"finish","exit":N}`` -- session finished

Standard library only; this file is embedded in the fermut binary and must run
on every Python and pytest version fermut supports.
"""

import json
import os

import pytest

SCHEMA_VERSION = 1

_path = os.environ.get("FERMUT_RESULT")
_fh = None


def _emit(event):
    global _fh
    if not _path:
        return
    if _fh is None:
        # Line-buffered: each event reaches the file as soon as it's written.
        _fh = open(_path, "a", buffering=1, encoding="utf-8")
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
        cause = getattr(err, "cause", None) or (getattr(err, "excinfo", None) or (None, err))[1]
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


def pytest_sessionstart(session):
    _emit({"v": SCHEMA_VERSION, "ev": "start", "pytest": pytest.__version__})


def pytest_collectreport(report):
    if report.failed:
        _emit({"ev": "collect", "nodeid": report.nodeid})


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
    _emit({"ev": "finish", "exit": int(exitstatus)})
    if _fh is not None:
        _fh.flush()
