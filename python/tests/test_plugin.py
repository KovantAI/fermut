import importlib.util
import sys

import pytest

import pytest_fermut.plugin as plugin

HAS_PYTEST_COV = importlib.util.find_spec("pytest_cov") is not None


def test_inert_without_flag(green_suite, fake_fermut, run_pytest):
    result = run_pytest(green_suite)
    assert result.ret == 0
    assert fake_fermut.calls() == []
    assert "= fermut =" not in result.stdout.str()


def test_passing_gate_reports_score(green_suite, fake_fermut, run_pytest, monkeypatch):
    monkeypatch.setenv("FAKE_FERMUT_MODE", "pass")
    result = run_pytest(green_suite, "--fermut", "--fermut-no-coverage")
    assert result.ret == 0
    result.stdout.fnmatch_lines(["*= fermut =*", "mutation score 100.0%: 8 killed, 0 survived*"])
    assert "mutation gate failed" not in result.stdout.str()


def test_survivors_fail_the_session_and_are_listed(green_suite, fake_fermut, run_pytest, monkeypatch):
    monkeypatch.setenv("FAKE_FERMUT_MODE", "survivors")
    result = run_pytest(green_suite, "--fermut", "--fermut-no-coverage")
    assert result.ret == pytest.ExitCode.TESTS_FAILED
    result.stdout.fnmatch_lines(
        [
            "mutation score 80.0%: 8 killed, 2 survived*",
            "surviving mutants:",
            # Sorted by file then line; paths relative to the rootdir.
            "  src*m.py:1 [[]arith-op-swap[]] `+` -> `-`",
            "  src*m.py:3 [[]boundary-shift[]] `<` -> `<=`",
            "*fermut next*",
            "mutation gate failed",
        ]
    )


def test_show_limits_listed_survivors(green_suite, fake_fermut, run_pytest, monkeypatch):
    monkeypatch.setenv("FAKE_FERMUT_MODE", "survivors")
    result = run_pytest(green_suite, "--fermut", "--fermut-no-coverage", "--fermut-show=1")
    result.stdout.fnmatch_lines(["surviving mutants (1 of 2):"])
    assert "boundary-shift" not in result.stdout.str()


def test_red_suite_skips_mutation_testing(pytester, fake_fermut, run_pytest):
    pytester.makepyfile(test_bad="def test_bad():\n    assert False\n")
    result = run_pytest(pytester, "--fermut")
    assert result.ret == pytest.ExitCode.TESTS_FAILED
    assert fake_fermut.calls() == []
    result.stdout.fnmatch_lines(["skipped: the test suite did not pass"])


def test_fermut_failure_without_report_is_an_internal_error(
    green_suite, fake_fermut, run_pytest, monkeypatch
):
    monkeypatch.setenv("FAKE_FERMUT_MODE", "crash")
    result = run_pytest(green_suite, "--fermut", "--fermut-no-coverage")
    assert result.ret == pytest.ExitCode.INTERNAL_ERROR
    result.stdout.fnmatch_lines(["fermut exited 1 without a report"])


def test_run_command_maps_options(green_suite, fake_fermut, run_pytest):
    run_pytest(
        green_suite,
        "--fermut",
        "--fermut-no-coverage",
        "--fermut-since=origin/main",
        "--fermut-min-score=80",
        "--fermut-arg=--sample=0.5",
        "--fermut-arg=--no-ty-filter",
    )
    call = fake_fermut.run_call()
    argv = call["argv"]
    assert argv[:3] == ["run", "--python", sys.executable]
    assert argv[argv.index("--since") + 1] == "origin/main"
    assert argv[argv.index("--fail-under") + 1] == "80.0"
    assert argv[-2:] == ["--sample=0.5", "--no-ty-filter"]
    # Marked as a fermut child (recursion guard) with quiet logs by default.
    assert call["child"] == "1"
    assert call["rust_log"] == "fermut=warn"
    assert [c["argv"][0] for c in fake_fermut.calls()] == ["run"]


def test_verbose_pytest_shows_fermut_progress(green_suite, fake_fermut, run_pytest):
    run_pytest(green_suite, "--fermut", "--fermut-no-coverage", "-v")
    assert fake_fermut.run_call()["rust_log"] == "fermut=info"


@pytest.mark.skipif(not HAS_PYTEST_COV, reason="needs pytest-cov")
def test_coverage_is_refreshed_before_the_run(green_suite, fake_fermut, run_pytest):
    run_pytest(green_suite, "--fermut")
    calls = fake_fermut.calls()
    assert [c["argv"][0] for c in calls] == ["coverage", "run"]
    cov = calls[0]["argv"]
    assert cov[1] == str(green_suite.path)
    assert cov[cov.index("--python") + 1] == sys.executable
    assert calls[0]["child"] == "1"


def test_without_pytest_cov_warns_and_runs_anyway(green_suite, fake_fermut, run_pytest, monkeypatch):
    real_find_spec = importlib.util.find_spec
    monkeypatch.setattr(
        importlib.util,
        "find_spec",
        lambda name, *a: None if name == "pytest_cov" else real_find_spec(name, *a),
    )
    result = run_pytest(green_suite, "--fermut")
    assert [c["argv"][0] for c in fake_fermut.calls()] == ["run"]
    result.stdout.fnmatch_lines(["pytest-cov not installed*"])


def test_inert_inside_a_fermut_child(green_suite, fake_fermut, run_pytest, monkeypatch):
    # fermut's own suite runs carry FERMUT_CHILD; `--fermut` in addopts must not
    # recurse into another mutation run.
    monkeypatch.setenv("FERMUT_CHILD", "1")
    result = run_pytest(green_suite, "--fermut")
    assert result.ret == 0
    assert fake_fermut.calls() == []


def test_inert_under_collect_only(green_suite, fake_fermut, run_pytest):
    run_pytest(green_suite, "--fermut", "--collect-only")
    assert fake_fermut.calls() == []


def test_missing_binary_is_a_usage_error(green_suite, fake_fermut, run_pytest, monkeypatch):
    monkeypatch.setattr(plugin, "find_fermut", lambda: None)
    result = run_pytest(green_suite, "--fermut")
    assert result.ret == pytest.ExitCode.USAGE_ERROR
    result.stderr.fnmatch_lines(["*fermut binary was not found*"])


def test_find_fermut_prefers_env(monkeypatch, tmp_path):
    monkeypatch.setenv("FERMUT_BIN", str(tmp_path / "custom"))
    assert plugin.find_fermut() == str(tmp_path / "custom")
