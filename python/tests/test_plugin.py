import importlib.util
import os
import sys

import pytest
from pytest_fermut import plugin

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
    result.stdout.fnmatch_lines(
        ["*= fermut =*", "mutation score 100.0%: 8 killed, 0 survived*"]
    )
    assert "mutation gate failed" not in result.stdout.str()


def test_survivors_fail_the_session_and_are_listed(
    green_suite, fake_fermut, run_pytest, monkeypatch
):
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


def test_show_limits_listed_survivors(
    green_suite, fake_fermut, run_pytest, monkeypatch
):
    monkeypatch.setenv("FAKE_FERMUT_MODE", "survivors")
    result = run_pytest(
        green_suite, "--fermut", "--fermut-no-coverage", "--fermut-show=1"
    )
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


def test_without_pytest_cov_warns_and_runs_anyway(
    green_suite, fake_fermut, run_pytest, monkeypatch
):
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


def test_missing_binary_is_a_usage_error(
    green_suite, fake_fermut, run_pytest, monkeypatch
):
    monkeypatch.setattr(plugin, "find_fermut", lambda: None)
    result = run_pytest(green_suite, "--fermut")
    assert result.ret == pytest.ExitCode.USAGE_ERROR
    result.stderr.fnmatch_lines(["*fermut binary was not found*"])


def test_find_fermut_prefers_env(monkeypatch, tmp_path):
    monkeypatch.setenv("FERMUT_BIN", str(tmp_path / "custom"))
    assert plugin.find_fermut() == str(tmp_path / "custom")


# --- narrowing to the session's selection (`fermut run --select-tests`) ---

HAS_XDIST = importlib.util.find_spec("xdist") is not None


@pytest.fixture
def two_files(pytester):
    pytester.makepyfile(
        test_a="import pytest\n\n"
        "def test_one():\n    pass\n\n"
        "@pytest.mark.parametrize('x', [1, 2])\n"
        "def test_param(x):\n    pass\n",
        test_b="import pytest\n\n@pytest.mark.slow\ndef test_two():\n    pass\n",
    )
    return pytester


def _selected(fake_fermut, pytester):
    """The run's selection as rootdir-relative node ids, or None."""
    selected = fake_fermut.run_call()["selected"]
    if selected is None:
        return None
    root = os.path.realpath(pytester.path)
    rel = []
    for line in selected:
        path, sep, rest = line.partition("::")
        assert os.path.isabs(path), line
        rel.append(os.path.relpath(os.path.realpath(path), root) + sep + rest)
    return rel


@pytest.mark.parametrize("args", [(), (".",), ("{root}",)])
def test_whole_suite_is_not_narrowed(two_files, fake_fermut, run_pytest, args):
    args = [a.format(root=two_files.path) for a in args]
    run_pytest(two_files, "--fermut", "--fermut-no-coverage", *args)
    assert "--select-tests" not in fake_fermut.run_call()["argv"]


def test_testpaths_are_the_whole_suite(pytester, fake_fermut, run_pytest):
    pytester.makeini("[pytest]\ntestpaths = tests\n")
    pytester.mkdir("tests")
    pytester.path.joinpath("tests", "test_t.py").write_text("def test_t():\n    pass\n")
    for args in [(), ("tests",)]:
        fake_fermut.log.unlink(missing_ok=True)
        run_pytest(pytester, "--fermut", "--fermut-no-coverage", *args)
        assert fake_fermut.run_call()["selected"] is None, args


def test_one_of_several_testpaths_is_narrowed(pytester, fake_fermut, run_pytest):
    pytester.makeini("[pytest]\ntestpaths = tests integration\n")
    for name in ("tests", "integration"):
        pytester.mkdir(name).joinpath(f"test_{name}.py").write_text(
            f"def test_{name}():\n    pass\n"
        )
    run_pytest(pytester, "--fermut", "--fermut-no-coverage", "tests")
    assert _selected(fake_fermut, pytester) == ["tests/test_tests.py::test_tests"]
    fake_fermut.log.unlink()
    run_pytest(pytester, "--fermut", "--fermut-no-coverage", "integration", "tests")
    assert fake_fermut.run_call()["selected"] is None


def test_recursive_testpaths_glob_is_the_whole_suite(pytester, fake_fermut, run_pytest):
    # pytest expands testpaths with recursive=True; so must the plugin.
    pytester.makeini("[pytest]\ntestpaths = src/**/tests\n")
    for pkg in ("a", "a/b"):
        d = pytester.path.joinpath("src", pkg, "tests")
        d.mkdir(parents=True)
        d.joinpath(f"test_{pkg.replace('/', '_')}.py").write_text(
            "def test_t():\n    pass\n"
        )
    run_pytest(pytester, "--fermut", "--fermut-no-coverage")
    assert fake_fermut.run_call()["selected"] is None


def test_running_from_a_subdirectory_is_narrowed(
    pytester, fake_fermut, run_pytest, monkeypatch
):
    # No args → pytest collects the invocation dir, here a subdirectory.
    pytester.makeini("[pytest]\n")
    pytester.makepyfile(test_top="def test_top():\n    pass\n")
    sub = pytester.mkdir("sub")
    sub.joinpath("test_sub.py").write_text("def test_sub():\n    pass\n")
    monkeypatch.chdir(sub)
    run_pytest(pytester, "--fermut", "--fermut-no-coverage")
    assert _selected(fake_fermut, pytester) == ["sub/test_sub.py::test_sub"]


@pytest.mark.parametrize(
    "args, expected",
    [
        (
            ("test_a.py",),
            [
                "test_a.py::test_one",
                "test_a.py::test_param[1]",
                "test_a.py::test_param[2]",
            ],
        ),
        (
            ("test_a.py::test_param",),
            ["test_a.py::test_param[1]", "test_a.py::test_param[2]"],
        ),
        (("-k", "param and 2"), ["test_a.py::test_param[2]"]),
        (("-m", "slow"), ["test_b.py::test_two"]),
        (
            ("--deselect", "test_a.py::test_one"),
            [
                "test_a.py::test_param[1]",
                "test_a.py::test_param[2]",
                "test_b.py::test_two",
            ],
        ),
    ],
)
def test_selection_narrows_the_run(two_files, fake_fermut, run_pytest, args, expected):
    result = run_pytest(two_files, "--fermut", "--fermut-no-coverage", *args)
    assert sorted(_selected(fake_fermut, two_files)) == expected
    result.stdout.fnmatch_lines(
        [
            f"mutation score *({len(expected)} selected tests, 3 mutants outside the selection)*"
        ]
    )


def test_selection_file_is_removed(two_files, fake_fermut, run_pytest):
    run_pytest(two_files, "--fermut", "--fermut-no-coverage", "-k", "one")
    argv = fake_fermut.run_call()["argv"]
    assert not os.path.exists(argv[argv.index("--select-tests") + 1])


def test_narrowed_session_without_fermut_is_inert(two_files, fake_fermut, run_pytest):
    result = run_pytest(two_files, "-k", "one")
    assert result.ret == 0
    assert fake_fermut.calls() == []


@pytest.mark.skipif(not HAS_XDIST, reason="needs pytest-xdist")
def test_xdist_selection_comes_from_the_workers(two_files, fake_fermut, monkeypatch):
    # Under -n the controller collects nothing; the ids come from the workers.
    here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    monkeypatch.setenv(
        "PYTHONPATH", here + os.pathsep + os.environ.get("PYTHONPATH", "")
    )
    result = two_files.runpytest_subprocess(
        "-p",
        "pytest_fermut.plugin",
        "-p",
        "no:cacheprovider",
        "-n",
        "2",
        "--fermut",
        "--fermut-no-coverage",
        "-k",
        "param",
    )
    assert result.ret == 0, result.stdout.str()
    assert sorted(_selected(fake_fermut, two_files)) == [
        "test_a.py::test_param[1]",
        "test_a.py::test_param[2]",
    ]


# --- fermut's baseline run ---


def test_whole_suite_session_keeps_the_baseline(green_suite, fake_fermut, run_pytest):
    # A green session outside fermut's mirror doesn't prove it green inside.
    run_pytest(green_suite, "--fermut", "--fermut-no-coverage")
    assert "--no-verify-baseline" not in fake_fermut.run_call()["argv"]


def test_no_verify_baseline_is_not_passed_twice(green_suite, fake_fermut, run_pytest):
    # fermut (clap) rejects a repeated flag.
    run_pytest(
        green_suite,
        "--fermut",
        "--fermut-no-coverage",
        "--fermut-arg=--no-verify-baseline",
    )
    assert fake_fermut.run_call()["argv"].count("--no-verify-baseline") == 1
