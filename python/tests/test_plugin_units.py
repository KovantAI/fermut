import json
import os
import stat
import sys
import textwrap

import pytest
from pytest_fermut import plugin

RED, BOLD, RESET = "\x1b[31m", "\x1b[1m", "\x1b[0m"
YELLOW = "\x1b[33m"


class StubConfig:
    """The slice of ``pytest.Config`` the plugin helpers read."""

    def __init__(self, **options):
        self.options = options

    def getoption(self, name):
        return self.options[name]


def help_text(pytester):
    result = pytester.runpytest("-p", "pytest_fermut.plugin", "--help")
    # argparse wraps help at the terminal width; compare word streams.
    return " ".join(result.stdout.str().split())


def test_help_documents_every_option(pytester):
    text = help_text(pytester)
    assert (
        "mutation testing (fermut): --fermut after a green test session, run fermut "
        "mutation testing and fail the session if the mutation gate fails." in text
    )
    assert (
        "--fermut-since=REF only mutate lines changed since this git ref "
        "(fermut run --since)." in text
    )
    assert (
        "--fermut-min-score=PCT fail only when the mutation score is below PCT "
        "(fermut run --fail-under). Without it any survivor fails, unless the "
        "fermut config sets fail_under." in text
    )
    assert (
        "--fermut-arg=ARG extra argument for `fermut run` (repeatable), e.g. "
        "--fermut-arg=--sample=0.2" in text
    )
    assert (
        "--fermut-no-coverage don't refresh per-test coverage with `fermut coverage` "
        "first." in text
    )
    assert (
        "--fermut-show=N number of surviving mutants to list in the summary "
        "(default 10)." in text
    )


def test_option_defaults(pytester):
    config = pytester.parseconfig("-p", "pytest_fermut.plugin")
    assert config.getoption("fermut") is False
    assert config.getoption("fermut_since") is None
    assert config.getoption("fermut_min_score") is None
    assert config.getoption("fermut_arg") == []
    assert config.getoption("fermut_no_coverage") is False
    assert config.getoption("fermut_show") == 10


def test_option_types(pytester):
    config = pytester.parseconfig(
        "-p", "pytest_fermut.plugin", "--fermut-min-score=80", "--fermut-show=3"
    )
    assert config.getoption("fermut_min_score") == 80.0
    assert isinstance(config.getoption("fermut_min_score"), float)
    assert config.getoption("fermut_show") == 3


def test_active_returns_false_not_falsy(monkeypatch):
    monkeypatch.delenv(plugin.CHILD_ENV, raising=False)
    assert plugin._active(StubConfig(fermut=False)) is False
    assert plugin._active(StubConfig(fermut=True, collectonly=False)) is True
    monkeypatch.setenv(plugin.CHILD_ENV, "1")
    assert plugin._active(StubConfig(fermut=True, collectonly=False)) is False


def test_active_is_inert_in_an_xdist_worker(monkeypatch):
    monkeypatch.delenv(plugin.CHILD_ENV, raising=False)
    config = StubConfig(fermut=True, collectonly=False)
    config.workerinput = {}
    assert plugin._active(config) is False


def test_rootdir_prefers_rootpath_then_rootdir(tmp_path):
    config = StubConfig()
    config.rootpath = tmp_path / "a"
    config.rootdir = tmp_path / "b"
    assert plugin._rootdir(config) == str(tmp_path / "a")
    config.rootpath = None
    assert plugin._rootdir(config) == str(tmp_path / "b")
    del config.rootpath
    assert plugin._rootdir(config) == str(tmp_path / "b")


def test_child_env_keeps_an_explicit_rust_log(monkeypatch):
    monkeypatch.setenv("RUST_LOG", "fermut=debug")
    env = plugin._child_env(StubConfig(verbose=0))
    assert env["RUST_LOG"] == "fermut=debug"
    assert env[plugin.CHILD_ENV] == "1"


def test_run_command_without_min_score_has_no_fail_under():
    state = plugin._FermutState("fermut")
    config = StubConfig(fermut_since=None, fermut_min_score=None, fermut_arg=[])
    assert plugin._run_command(state, config, "r.json") == [
        "fermut",
        "run",
        "--python",
        sys.executable,
        "--json",
        "r.json",
    ]


@pytest.mark.parametrize("content", ['["summary"]', "{}", '"summary"', "not json"])
def test_load_report_rejects_non_reports(tmp_path, content):
    path = tmp_path / "r.json"
    path.write_text(content)
    assert plugin._load_report(str(path)) is None


def test_load_report_reads_utf8(tmp_path):
    path = tmp_path / "r.json"
    path.write_bytes(
        json.dumps({"summary": {}, "x": "é"}, ensure_ascii=False).encode("utf-8")
    )
    assert plugin._load_report(str(path)) == {"summary": {}, "x": "é"}


def test_load_report_missing_file(tmp_path):
    assert plugin._load_report(str(tmp_path / "nope.json")) is None


def test_relative(tmp_path, monkeypatch):
    root = tmp_path / "root"
    monkeypatch.chdir(tmp_path)
    assert plugin._relative(str(root / "src" / "m.py"), str(root)) == os.path.join(
        "src", "m.py"
    )
    outside = str(tmp_path / "other" / "m.py")
    assert plugin._relative(outside, str(root)) == outside


def summarize(report, show=10, root="/r"):
    state = plugin._FermutState("fermut")
    plugin._summarize(state, report, root, show)
    return state.lines


def test_summarize_defaults_missing_counts_to_zero():
    assert summarize({"summary": {}}) == [
        (
            {"bold": True},
            "mutation score n/a: 0 killed, 0 survived, 0 timed out, 0 errored (0 skipped)",
        )
    ]


def test_summarize_counts_only_selection_skips():
    state = plugin._FermutState("fermut")
    state.selected = ["/r/t.py::a", "/r/t.py::b"]
    report = {
        "summary": {},
        "outcomes": [
            {"status": "skipped", "filter": "selection"},
            {"status": "skipped", "filter": "coverage"},
            {"status": "killed", "filter": "selection"},
        ],
    }
    plugin._summarize(state, report, "/r", 0)
    assert "(2 selected tests, 1 mutants outside the selection)" in state.lines[0][1]
    del report["outcomes"]
    plugin._summarize(state, report, "/r", 0)
    assert "(2 selected tests, 0 mutants outside the selection)" in state.lines[1][1]


def test_summarize_reads_each_count():
    summary = {
        "mutation_score": 50.0,
        "killed": 1,
        "survived": 2,
        "timed_out": 3,
        "errored": 4,
        "skipped": 5,
    }
    assert summarize({"summary": summary}) == [
        (
            {"bold": True},
            "mutation score 50.0%: 1 killed, 2 survived, 3 timed out, 4 errored (5 skipped)",
        )
    ]


def test_summarize_ignores_non_survivors_and_tolerates_missing_outcomes():
    report = {
        "summary": {},
        "outcomes": [{"status": "killed", "mutant": {}}, {"mutant": {}}],
    }
    assert len(summarize(report)) == 1
    assert len(summarize({"summary": {}}, show=0)) == 1


def test_summarize_show_zero_lists_nothing():
    report = {
        "summary": {},
        "outcomes": [{"status": "survived", "mutant": {"file": "/r/a.py"}}],
    }
    assert len(summarize(report, show=0)) == 1
    assert len(summarize(report, show=1)) == 4


def test_summarize_sorts_and_defaults_missing_fields():
    mutants = [
        {"file": "/r/b.py", "line": 1},
        {"file": "/r/b.py", "line": 0},
        {"file": "/r/b.py"},
        {"line": 7},
        {
            "file": "/r/a.py",
            "line": 2,
            "operator": "op",
            "original": "<",
            "replacement": ">",
        },
    ]
    report = {
        "summary": {},
        "outcomes": [{"status": "survived", "mutant": m} for m in mutants],
    }
    lines = summarize(report)
    assert lines[1:] == [
        ({}, "surviving mutants:"),
        ({"red": True}, "  ?:7 [?] `` -> ``"),
        ({"red": True}, "  a.py:2 [op] `<` -> `>`"),
        ({"red": True}, "  b.py:0 [?] `` -> ``"),
        ({"red": True}, "  b.py:? [?] `` -> ``"),
        ({"red": True}, "  b.py:1 [?] `` -> ``"),
        ({}, "`fermut next` ranks which survivors to kill first."),
    ]


FAKE = textwrap.dedent(
    """\
    #!{python}
    import os, sys
    print("fake-stdout", os.getcwd())
    for i in range(7):
        sys.stderr.write("line%d\\n" % i)
    sys.exit({rc})
    """
)


def fake_binary(tmp_path, rc):
    path = tmp_path / "fake"
    path.write_text(FAKE.format(python=sys.executable, rc=rc))
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return str(path)


def refresh(tmp_path, rc):
    root = tmp_path / "root"
    root.mkdir()
    state = plugin._FermutState(fake_binary(tmp_path, rc))
    plugin._refresh_coverage(
        state, StubConfig(fermut_no_coverage=False, verbose=0), str(root)
    )
    return state.lines, str(root)


def test_refresh_coverage_failure_reports_the_log_tail(tmp_path, capfd):
    lines, root = refresh(tmp_path, 3)
    assert lines == [
        (
            {"yellow": True},
            "`fermut coverage` exited 3; mutating with whatever coverage exists:",
        ),
        ({}, "  line3"),
        ({}, "  line4"),
        ({}, "  line5"),
        ({}, "  line6"),
        ({}, "  fake-stdout " + root),
    ]
    out, err = capfd.readouterr()
    assert out == err == ""  # the log went to the tempfile, not the terminal


def test_refresh_coverage_runs_in_the_root(tmp_path):
    root = tmp_path / "root"
    root.mkdir()
    fake = tmp_path / "fake"
    fake.write_text(
        f"#!{sys.executable}\nimport os, sys\nsys.stderr.write(os.getcwd() + '\\n')\nsys.exit(1)\n"
    )
    fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
    state = plugin._FermutState(str(fake))
    plugin._refresh_coverage(
        state, StubConfig(fermut_no_coverage=False, verbose=0), str(root)
    )
    assert os.path.realpath(state.lines[-1][1].strip()) == os.path.realpath(root)


def test_refresh_coverage_success_adds_nothing(tmp_path):
    lines, _ = refresh(tmp_path, 0)
    assert lines == []


def test_refresh_coverage_command(green_suite, fake_fermut, run_pytest):
    pytest.importorskip("pytest_cov")
    run_pytest(green_suite, "--fermut")
    cov = fake_fermut.calls()[0]["argv"]
    assert cov == ["coverage", str(green_suite.path), "--python", sys.executable, "-q"]


def test_missing_pytest_cov_warns_in_yellow(
    green_suite, fake_fermut, run_pytest, monkeypatch
):
    import importlib.util

    real_find_spec = importlib.util.find_spec
    monkeypatch.setattr(
        importlib.util,
        "find_spec",
        lambda name, *a: None if name == "pytest_cov" else real_find_spec(name, *a),
    )
    result = run_pytest(green_suite, "--fermut", "--color=yes")
    assert (
        YELLOW + "pytest-cov not installed: mutants run the whole suite (slow). "
        "`pip install pytest-cov` to select tests by coverage."
        + RESET
        in result.stdout.str()
    )


def test_missing_binary_message(green_suite, fake_fermut, run_pytest, monkeypatch):
    monkeypatch.setattr(plugin, "find_fermut", lambda: None)
    result = run_pytest(green_suite, "--fermut")
    result.stderr.fnmatch_lines(
        [
            (
                "ERROR: --fermut: the fermut binary was not found. Install fermut in this "
                "environment (e.g. `uv add --dev fermut`) or set FERMUT_BIN."
            )
        ]
    )


def test_red_suite_note_is_yellow(pytester, fake_fermut, run_pytest):
    pytester.makepyfile(test_bad="def test_bad():\n    assert False\n")
    result = run_pytest(pytester, "--fermut", "--color=yes")
    assert (
        YELLOW + "skipped: the test suite did not pass" + RESET in result.stdout.str()
    )


def test_session_markup_and_report_state(
    green_suite, fake_fermut, run_pytest, monkeypatch, capfd
):
    monkeypatch.setenv("FAKE_FERMUT_MODE", "survivors")
    # Expose the report the plugin keeps for other plugins.
    green_suite.makeconftest(
        """
        import pytest_fermut.plugin as plugin

        def pytest_unconfigure(config):
            state = config.stash.get(plugin._STATE, None)
            print("REPORT-KILLED", state.report["summary"]["killed"])
        """
    )
    result = run_pytest(green_suite, "--fermut", "--fermut-no-coverage", "--color=yes")
    out = result.stdout.str()
    assert RED + BOLD + "mutation gate failed" + RESET in out
    assert RED + "  src/m.py:1 [arith-op-swap] `+` -> `-`" + RESET in out
    assert BOLD + "mutation score 80.0%" in out
    fd_out, _ = capfd.readouterr()
    assert "REPORT-KILLED 8" in out + fd_out
    assert result.stderr.lines[-1] == f"fermut: mutation testing {green_suite.path}"


def test_crash_message_is_red(green_suite, fake_fermut, run_pytest, monkeypatch):
    monkeypatch.setenv("FAKE_FERMUT_MODE", "crash")
    result = run_pytest(green_suite, "--fermut", "--fermut-no-coverage", "--color=yes")
    assert RED + "fermut exited 1 without a report" + RESET in result.stdout.str()


def test_run_uses_rootdir_and_a_fermut_temp_report(
    green_suite, fake_fermut, run_pytest, monkeypatch, capfd
):
    monkeypatch.setenv("FAKE_FERMUT_MODE", "survivors")
    root = green_suite.mkdir("root")
    green_suite.makepyfile(**{"root/test_in_root": "def test_ok():\n    pass\n"})
    result = run_pytest(
        green_suite, "--fermut", "--fermut-no-coverage", "--rootdir", str(root), "root"
    )
    argv = fake_fermut.run_call()["argv"]
    report = os.path.basename(argv[argv.index("--json") + 1])
    assert report.startswith("fermut-") and report.endswith(".json")
    # The fake reports survivors under its cwd: relative paths prove cwd == root.
    result.stdout.fnmatch_lines(["  src*m.py:1 [[]arith-op-swap[]] `+` -> `-`"])
    out, _ = capfd.readouterr()
    assert "fake-stdout" not in out + result.stdout.str()


def test_find_fermut_prefers_env_then_scripts_dir_then_path(tmp_path, monkeypatch):
    name = "fermut.exe" if os.name == "nt" else "fermut"
    scripts = tmp_path / "scripts"
    scripts.mkdir()
    on_path = tmp_path / "bin"
    on_path.mkdir()
    exe = on_path / name
    exe.write_text("")
    exe.chmod(0o755)
    monkeypatch.setattr(
        plugin.sysconfig,
        "get_path",
        lambda kind: str(scripts) if kind == "scripts" else None,
    )
    monkeypatch.setenv("PATH", str(on_path))
    monkeypatch.delenv(plugin.BIN_ENV, raising=False)
    assert plugin.find_fermut() == str(exe)
    (scripts / name).write_text("")
    assert plugin.find_fermut() == str(scripts / name)
    monkeypatch.setenv(plugin.BIN_ENV, "/explicit/fermut")
    assert plugin.find_fermut() == "/explicit/fermut"


def test_find_fermut_none_when_nowhere(tmp_path, monkeypatch):
    monkeypatch.setattr(plugin.sysconfig, "get_path", lambda kind: str(tmp_path))
    monkeypatch.setenv("PATH", str(tmp_path))
    monkeypatch.delenv(plugin.BIN_ENV, raising=False)
    assert plugin.find_fermut() is None
