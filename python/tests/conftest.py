import json
import os
import stat
import sys
import textwrap

import pytest

pytest_plugins = ["pytester"]

# Stands in for the fermut binary: logs each invocation (argv + the env vars the
# plugin sets) as a JSON line to $FAKE_FERMUT_LOG, then behaves per
# $FAKE_FERMUT_MODE when called as `run`.
FAKE_FERMUT = textwrap.dedent(
    """\
    #!{python}
    import json, os, sys

    args = sys.argv[1:]
    with open(os.environ["FAKE_FERMUT_LOG"], "a") as fh:
        fh.write(json.dumps({{
            "argv": args,
            "child": os.environ.get("FERMUT_CHILD"),
            "rust_log": os.environ.get("RUST_LOG"),
        }}) + "\\n")
    if args[0] != "run":
        sys.exit(0)
    mode = os.environ.get("FAKE_FERMUT_MODE", "pass")
    if mode == "crash":
        sys.stderr.write("Error: boom\\n")
        sys.exit(1)
    survivors = [
        {{"file": os.path.join(os.getcwd(), "src", "m.py"), "line": 3, "operator": "boundary-shift",
          "original": "<", "replacement": "<="}},
        {{"file": os.path.join(os.getcwd(), "src", "m.py"), "line": 1, "operator": "arith-op-swap",
          "original": "+", "replacement": "-"}},
    ] if mode == "survivors" else []
    report = {{
        "summary": {{"total": 10, "killed": 8, "survived": len(survivors), "timed_out": 0,
                    "skipped": 0, "equivalent": 0, "errored": 0,
                    "mutation_score": 100.0 * 8 / (8 + len(survivors)), "scored": 8 + len(survivors)}},
        "outcomes": [{{"status": "survived", "mutant": m}} for m in survivors],
    }}
    with open(args[args.index("--json") + 1], "w") as fh:
        json.dump(report, fh)
    sys.exit(1 if survivors else 0)
    """
)


class FakeFermut:
    def __init__(self, path, log):
        self.path = path
        self.log = log

    def calls(self):
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def run_call(self):
        runs = [c for c in self.calls() if c["argv"][0] == "run"]
        assert len(runs) == 1, self.calls()
        return runs[0]


@pytest.fixture
def fake_fermut(tmp_path, monkeypatch):
    path = tmp_path / "fake-fermut"
    path.write_text(FAKE_FERMUT.format(python=sys.executable))
    path.chmod(path.stat().st_mode | stat.S_IXUSR)
    log = tmp_path / "fermut-calls.jsonl"
    monkeypatch.setenv("FERMUT_BIN", str(path))
    monkeypatch.setenv("FAKE_FERMUT_LOG", str(log))
    monkeypatch.delenv("FERMUT_CHILD", raising=False)
    monkeypatch.delenv("RUST_LOG", raising=False)
    return FakeFermut(path, log)


@pytest.fixture
def green_suite(pytester):
    pytester.makepyfile(test_ok="def test_ok():\n    assert True\n")
    return pytester


def run(pytester, *args):
    """Run pytest in-process with the plugin loaded explicitly (the tests run
    from a source checkout, where the entry point isn't installed)."""
    return pytester.runpytest("-p", "pytest_fermut.plugin", "-p", "no:cacheprovider", *args)


@pytest.fixture
def run_pytest():
    return run


def pytest_configure(config):
    # Make `pytest_fermut` importable from a source checkout.
    here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    if here not in sys.path:
        sys.path.insert(0, here)
