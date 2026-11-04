"""Backlog #39 Phase 0: split a template's ready time into what a long-lived
base could keep (interpreter, pytest, third-party) and what each run repays
(SUT import, conftest, collection).

Run from the repo root with the repo's venv python:
    python readysplit.py [--profile] [--importtime] -- <pytest args>
`--profile` installs a setprofile hook like the template's `unsafe` tracer;
`--monitoring` does the same lookup through sys.monitoring (3.12+).
`--importtime` adds -X importtime and attributes each module's self time to
stdlib / third-party / SUT by its __file__. Prints one JSON line.
"""

import json
import os
import subprocess
import sys
import sysconfig
import tempfile
import time

if len(sys.argv) > 1 and sys.argv[1] == "child":
    marks = {"start": time.perf_counter()}
    out_path, mode, args = sys.argv[2], sys.argv[3], sys.argv[4:]
    profile = mode == "1"
    if mode == "mon":
        # sys.monitoring (3.12+): PY_START once per code object, then DISABLE
        # turns the event off for it. Same lookup as the tracer.
        mon = sys.monitoring
        seen = {}

        def on_start(code, offset):
            fns = seen.setdefault(code.co_filename, {})
            fns.get(code.co_firstlineno)
            return mon.DISABLE

        mon.use_tool_id(mon.PROFILER_ID, "readysplit")
        mon.register_callback(mon.PROFILER_ID, mon.events.PY_START, on_start)
        mon.set_events(mon.PROFILER_ID, mon.events.PY_START)
    if profile:
        # Same per-call work as the template's tracer: a dict hit per code object.
        seen = {}

        def tracer(frame, event, arg):
            if event == "call":
                code = frame.f_code
                fns = seen.get(code.co_filename)
                if fns is None:
                    fns = seen[code.co_filename] = {}
                fns.get(code.co_firstlineno)

        sys.setprofile(tracer)
    import threading

    if profile:
        threading.setprofile(tracer)
    import pytest

    marks["pytest"] = time.perf_counter()

    class Plugin:
        @pytest.hookimpl(tryfirst=True)
        def pytest_configure(self, config):
            marks["configure"] = time.perf_counter()

        @pytest.hookimpl(tryfirst=True)
        def pytest_collection(self, session):
            marks["collect_start"] = time.perf_counter()

        @pytest.hookimpl(trylast=True)
        def pytest_collection_finish(self, session):
            marks["collect_end"] = time.perf_counter()
            marks["items"] = len(session.items)

    rc = pytest.main(
        ["--collect-only", "-q", "-p", "no:cacheprovider", *args], plugins=[Plugin()]
    )
    sys.setprofile(None)
    files = {
        name: getattr(m, "__file__", None) for name, m in list(sys.modules.items())
    }
    with open(out_path, "w") as f:
        json.dump({"marks": marks, "rc": int(rc), "files": files}, f)
    os._exit(0)


def classify(path, root, stdlib, site):
    if not path:
        return "builtin"
    path = os.path.realpath(path)
    if any(path.startswith(s) for s in site):
        return "third_party"
    if path.startswith(stdlib):
        return "stdlib"
    if path.startswith(root):
        return "sut"
    return "other"


def main():
    argv = sys.argv[1:]
    profile = "--profile" in argv
    monitoring = "--monitoring" in argv
    importtime = "--importtime" in argv
    args = argv[argv.index("--") + 1 :] if "--" in argv else []
    root = os.path.realpath(os.getcwd())
    paths = sysconfig.get_paths()
    stdlib = os.path.realpath(paths["stdlib"])
    site = {os.path.realpath(paths["purelib"]), os.path.realpath(paths["platlib"])}
    with tempfile.NamedTemporaryFile(suffix=".json", delete=False) as tmp:
        out_path = tmp.name
    cmd = [sys.executable]
    if importtime:
        cmd += ["-X", "importtime"]
    cmd += [
        __file__,
        "child",
        out_path,
        "mon" if monitoring else "1" if profile else "0",
        *args,
    ]
    t0 = time.perf_counter()
    proc = subprocess.run(cmd, capture_output=True, text=True, check=False)
    wall = time.perf_counter() - t0
    with open(out_path) as f:
        child = json.load(f)
    os.unlink(out_path)
    site_mods = sorted(
        n
        for n, f in child["files"].items()
        if classify(f, root, stdlib, site) == "third_party"
    )
    sut_mods = sorted(
        n
        for n, f in child["files"].items()
        if classify(f, root, stdlib, site) == "sut" and "test" not in n
    )
    m = child["marks"]
    ms = lambda a, b: round((m[b] - m[a]) * 1000)
    result = {
        "profile": profile,
        "monitoring": monitoring,
        "importtime": importtime,
        "rc": child["rc"],
        "items": m.get("items"),
        "wall_ms": round(wall * 1000),
        # wall minus everything the child timed: interpreter boot + exit.
        "boot_ms": round(wall * 1000) - ms("start", "collect_end"),
        "import_pytest_ms": ms("start", "pytest"),
        "plugins_conftests_ms": ms("pytest", "configure"),
        "configure_to_collect_ms": ms("configure", "collect_start"),
        "collect_ms": ms("collect_start", "collect_end"),
    }
    # What a long-lived base could keep: a fresh process importing every
    # third-party module the collection loaded, then the SUT on top.
    probe = (
        "import importlib, json, sys, time\n"
        "if sys.argv[3] == '1':\n"
        "    seen = {}\n"
        "    def tracer(frame, event, arg):\n"
        "        if event == 'call':\n"
        "            c = frame.f_code\n"
        "            seen.setdefault(c.co_filename, {}).get(c.co_firstlineno)\n"
        "    sys.setprofile(tracer)\n"
        "t = time.perf_counter()\n"
        "def load(names):\n"
        "    for n in names:\n"
        "        try: importlib.import_module(n)\n"
        "        except Exception: pass\n"
        "load(json.loads(sys.argv[1])); a = time.perf_counter()\n"
        "load(json.loads(sys.argv[2])); b = time.perf_counter()\n"
        "print(json.dumps([round((a - t) * 1000), round((b - a) * 1000)]))\n"
    )
    t0 = time.perf_counter()
    pre = subprocess.run(
        [
            sys.executable,
            "-c",
            probe,
            json.dumps(site_mods),
            json.dumps(sut_mods),
            "1" if profile else "0",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    tp, sut = json.loads(pre.stdout.strip().splitlines()[-1])
    result["preload"] = {
        "proc_ms": round((time.perf_counter() - t0) * 1000),
        "third_party_ms": tp,
        "sut_src_ms": sut,
        "third_party_modules": len(site_mods),
        "sut_src_modules": len(sut_mods),
    }
    if importtime:
        self_us = {}
        for line in proc.stderr.splitlines():
            if not line.startswith("import time:") or "self [us]" in line:
                continue
            self_t, _cum, name = line.split(":", 1)[1].split("|")
            name = name.strip()
            self_us[name] = self_us.get(name, 0) + int(self_t)
        by_class = {}
        for name, us in self_us.items():
            c = classify(child["files"].get(name), root, stdlib, site)
            by_class[c] = by_class.get(c, 0) + us
        result["import_self_ms"] = {
            k: round(v / 1000) for k, v in sorted(by_class.items())
        }
    print(json.dumps(result))


if __name__ == "__main__":
    main()
