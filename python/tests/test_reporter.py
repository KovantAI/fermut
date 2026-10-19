"""The per-run reporter fermut injects with ``-p _fermut_reporter``.

Runs pytest in a subprocess (the reporter reads ``FERMUT_RESULT`` at import
time, so an in-process run would reuse the first test's path).
"""

import json
import os

import pytest

PLUGIN_DIR = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "pytest_fermut")


@pytest.fixture
def report(pytester, monkeypatch, tmp_path):
    """Run pytest with the reporter on; return (exit code, parsed events)."""
    result_path = tmp_path / "result.jsonl"
    monkeypatch.setenv("FERMUT_RESULT", str(result_path))
    monkeypatch.setenv("PYTHONPATH", os.pathsep.join([PLUGIN_DIR, str(pytester.path)]))

    def run(*args):
        if result_path.exists():
            result_path.unlink()
        res = pytester.runpytest_subprocess("-p", "_fermut_reporter", "-p", "no:cacheprovider", *args)
        events = []
        if result_path.exists():
            events = [json.loads(line) for line in result_path.read_text().splitlines()]
        return res.ret, events

    return run


def kinds(events):
    return [e["ev"] for e in events]


def test_passing_run_records_session_and_durations(pytester, report):
    pytester.makepyfile(test_a="def test_ok():\n    pass\n")
    ret, events = report()
    assert ret == 0
    assert events[0]["ev"] == "start" and events[0]["v"] == 1
    assert kinds(events) == ["start", "begin", "dur", "finish"]
    assert events[-1]["exit"] == 0
    assert all("pid" in e for e in events)


def test_failures_name_the_test_and_phase(pytester, report):
    pytester.makepyfile(
        test_a="""
import pytest

@pytest.fixture
def broken():
    raise RuntimeError("setup")

def test_fails():
    assert False

def test_setup_error(broken):
    pass
"""
    )
    ret, events = report()
    assert ret == 1
    failed = [(e["nodeid"], e["when"]) for e in events if e["ev"] == "test"]
    assert failed == [
        ("test_a.py::test_fails", "call"),
        ("test_a.py::test_setup_error", "setup"),
    ]


def test_import_error_under_node_ids_is_a_collect_event(pytester, report):
    pytester.makepyfile(test_a="import nonexistent_module_xyz\n\ndef test_x():\n    pass\n")
    ret, events = report("-x", "test_a.py::test_x")
    assert ret == 4
    assert any(e["ev"] == "collect" and e["nodeid"] == "test_a.py" for e in events)


def test_stale_node_id_has_no_collect_failure(pytester, report):
    pytester.makepyfile(test_a="def test_x():\n    pass\n")
    ret, events = report("test_a.py::test_missing")
    assert ret == 4
    assert [e for e in events if e["ev"] == "collect" and e["nodeid"]] == []


def test_conftest_import_failure_is_reported_before_any_session(pytester, report):
    pytester.makeconftest("import nonexistent_module_xyz\n")
    pytester.makepyfile(test_a="def test_x():\n    pass\n")
    ret, events = report()
    assert ret == 4
    assert kinds(events) == ["conftest_error"]
    assert events[0]["exc"] == "ModuleNotFoundError"
    assert events[0]["path"].endswith("conftest.py")


def test_config_error_from_project_code_is_flagged(pytester, report):
    # A filterwarnings entry naming a warning class in the project imports it
    # while pytest configures; if that module raises, the run dies there.
    pytester.makepyfile(proj="raise ValueError('broken at import')\nclass W(Warning):\n    pass\n")
    pytester.makeini("[pytest]\nfilterwarnings = ignore::proj.W\n")
    pytester.makepyfile(test_a="def test_x():\n    pass\n")
    ret, events = report()
    assert ret == 4
    assert kinds(events) == ["config_error"]
    assert events[0]["project"] is True


def test_config_error_outside_project_is_not_flagged(pytester, report):
    pytester.makepyfile(test_a="def test_x():\n    pass\n")
    ret, events = report("-W", "ignore::NoSuchWarningClass")
    assert ret == 4
    assert kinds(events) == ["config_error"]
    assert events[0]["project"] is False


def test_inert_without_result_env(pytester, monkeypatch, tmp_path):
    monkeypatch.delenv("FERMUT_RESULT", raising=False)
    monkeypatch.setenv("PYTHONPATH", PLUGIN_DIR)
    pytester.makepyfile(test_a="def test_ok():\n    pass\n")
    res = pytester.runpytest_subprocess("-p", "_fermut_reporter", "-p", "no:cacheprovider")
    assert res.ret == 0
    assert list(pytester.path.glob("*.jsonl")) == []
