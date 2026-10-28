"""fermut schemata helper: build a mutant schema, then serve mutants by fork.

Embedded in the fermut binary (`include_str!`) and run with the project's
interpreter; stdlib only (plus pytest, which the pytest runner already needs).

    python _fermut_schema.py schematize <mirror_root>   < mutants.json
    python _fermut_schema.py serve <mirror_root> <schema.json> [pytest args...]

schematize
    stdin: [{"id", "file", "start", "end", "replacement"}] — file relative to
    <mirror_root>, start/end byte offsets. Every mutant inside a function
    body becomes one case of a switch in its *innermost* function:

        def f(self, x):
            "doc"                         # kept first: __doc__ unchanged
            global g                      # hoisted: must precede any use
            if 17 <= _fm_b._fm_active <= 19:   # this def's contiguous idx range
                match _fm_b._fm_active:
                    case 17: <body with mutant 17 spliced in>
                    ...
            else:
                <original body>

    Switching *inside* the function keeps the function object, its __class__
    cell (zero-arg super), closures, decorators and generator/async-ness
    unchanged. Innermost, so a closure built at import time (decorator
    factories) still switches when called. The active index lives on the
    builtins module, which every module can import whatever the layout.

    Mutants outside a function body (module, class body, signature), touching
    the docstring, or raising a compile-time warning are reported as
    fallbacks for the caller to run the normal way. Rewritten files replace
    the mirror file (new inode — the mirror may hardlink the user's tree) and
    are byte-compiled: the schema never changes during a run, so the bytecode
    can't go stale, and every Python subprocess a test spawns skips
    recompiling the (much larger) schema source.

    stdout: {"mutants": {id: {"idx": n} | {"fallback": why}},
             "functions": {realpath: [[co_firstlineno, lo, hi], ...]}}

serve
    The template: one pytest session that imports and collects the suite
    once, then forks one child per request. Protocol on the original
    stdin/stdout (fds 0/1 are redirected to /dev/null for pytest's output),
    one JSON object per line:

        -> {"ready": true, "unsafe": [[lo, hi], ...]}   once, after collection
        <- {"idx": n, "ids": [nodeid, ...] | null, "all": false}
        -> {"pid": p}
        -> {"status": <raw waitpid status>, "failed": [nodeid, ...],
            "test_secs": s, "unfinished": nodeid | null,
            "error": traceback (only when the child itself broke: exit 3)}

    `unfinished` is the test a child was in when it was killed (a timeout).

    `unsafe`: index ranges of switched functions that already ran in the
    template (SUT import, conftest, collection). Whatever they built from the
    original branch outlives the fork, so the caller must run those mutants the
    normal way. `ids: null` runs every collected item; `all: true` drops -x.
    The caller enforces timeouts by killing the child's process group (the
    child leads its own); the template still reports its exit. EOF on stdin
    ends the template at once — pytest's own session wrap-up is skipped.
"""

import ast
import builtins
import getpass
import glob
import json
import os
import py_compile
import shutil
import signal
import sys
import tempfile
import threading
import traceback
import warnings
from collections import defaultdict

ACTIVE = "_fm_active"  # attribute on the builtins module
SCOPES = (ast.FunctionDef, ast.AsyncFunctionDef, ast.Lambda, ast.ClassDef)

# --- schematize ------------------------------------------------------------------


def line_starts(src):
    return [0] + [i + 1 for i, b in enumerate(src) if b == 0x0A]


def span(node, starts):
    return starts[node.lineno - 1] + node.col_offset, starts[
        node.end_lineno - 1
    ] + node.end_col_offset


def kids(node):
    """Statement-level children. Index paths over these survive a mutation
    inside a function body: nothing above the function changes."""
    return [
        c
        for c in ast.iter_child_nodes(node)
        if isinstance(c, (ast.stmt, ast.excepthandler, ast.match_case))
    ]


def innermost_function(tree, starts, lo):
    """(index path to the innermost def whose *body* holds byte `lo`, None),
    else (None, reason). A nested def's decorators, defaults and annotations
    and a class's bases run in the enclosing body, so they belong to it."""
    path, node, best, why = [], tree, None, "module"
    while True:
        for i, child in enumerate(kids(node)):
            if not hasattr(child, "lineno"):
                continue  # match_case carries no position
            a, b = span(child, starts)
            if a <= lo < b:
                break
        else:
            break
        path.append(i)
        node = child
        if isinstance(node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
            if lo < span(node.body[0], starts)[0]:
                why = "signature"
                break
            if isinstance(node, ast.ClassDef):
                why = "class"
            else:
                best = list(path)
    return (best, None) if best else (None, why)


def resolve(tree, path):
    node = tree
    for i in path:
        node = kids(node)[i]
    return node


def docstring(body):
    first = body[0] if body else None
    if (
        isinstance(first, ast.Expr)
        and isinstance(first.value, ast.Constant)
        and isinstance(first.value.value, str)
    ):
        return first
    return None


def dump(node):
    return node and ast.dump(node)


def scope_decls(body):
    """global/nonlocal statements anywhere in this function's own scope."""
    found, stack = [], list(body)
    while stack:
        n = stack.pop()
        if isinstance(n, (ast.Global, ast.Nonlocal)):
            found.append(n)
        elif not isinstance(n, SCOPES):
            stack.extend(ast.iter_child_nodes(n))
    return found


class StripDecls(ast.NodeTransformer):
    def visit_Global(self, node):
        return ast.Pass()

    visit_Nonlocal = visit_Global

    def generic_visit(self, node):
        if isinstance(node, SCOPES):
            return node  # inner scopes keep their own declarations
        return super().generic_visit(node)


def branch_body(body):
    """Body minus docstring and global/nonlocal (hoisted), never empty."""
    body = body[1:] if docstring(body) else list(body)
    return [StripDecls().visit(s) for s in body] or [ast.Pass()]


def hoisted(decls):
    names = defaultdict(list)
    for d in decls:
        for n in d.names:
            if n not in names[type(d)]:
                names[type(d)].append(n)
    return [cls(names=ns) for cls, ns in names.items()]


def active_expr():
    return ast.Attribute(ast.Name("_fm_b", ast.Load()), ACTIVE, ast.Load())


def compile_problem(src):
    """ "warning" / "error" when compiling `src` warns / fails, else None."""
    try:
        with warnings.catch_warnings():
            warnings.simplefilter("error")
            compile(src, "<mutant>", "exec")
    except Warning:
        return "warning"
    except (SyntaxError, ValueError):
        return "error"
    return None


def mutated_function(src, starts, fn, m):
    """(the mutated def parsed on its own, its compile problem). Parsing one
    function instead of the whole file keeps schematizing linear in mutants,
    not mutants x file size. An `if 1:` header lets an indented def stand
    alone; a def that only compiles inside its file (`nonlocal`) is checked
    against the whole mutated file instead."""
    start, end = starts[fn.lineno - 1], span(fn, starts)[1]
    seg = src[start : m["start"]] + m["replacement"].encode() + src[m["end"] : end]
    text = b"if 1:\n" + seg if fn.col_offset else seg
    tree = ast.parse(text)
    mut_fn = tree.body[0].body[0] if fn.col_offset else tree.body[0]
    problem = compile_problem(text)
    if problem == "error":
        problem = compile_problem(
            src[: m["start"]] + m["replacement"].encode() + src[m["end"] :]
        )
    return mut_fn, problem


def schematize_file(src, mutants, next_idx):
    """(new source | None, {id: entry}, [(co_firstlineno, lo, hi)])."""
    tree, starts = ast.parse(src), line_starts(src)
    entries, by_fn = {}, defaultdict(list)
    for m in mutants:
        path, why = innermost_function(tree, starts, m["start"])
        if path is None:
            entries[m["id"]] = {"fallback": why}
        else:
            by_fn[tuple(path)].append(m)

    # Deepest first: an enclosing def's original branch then carries its
    # already-switched inner defs. Its mutant branches come from the plain
    # spliced source, which is right — only one mutant is ever active.
    for path, ms in sorted(by_fn.items(), key=lambda kv: -len(kv[0])):
        fn = resolve(tree, path)
        orig_doc = docstring(fn.body)
        cases = []
        for m in ms:
            try:
                mut_fn, problem = mutated_function(src, starts, fn, m)
            except (SyntaxError, ValueError, IndexError):
                entries[m["id"]] = {"fallback": "unparsable"}
                continue
            # A compile-time warning (`x is ''`) is an import *error* under
            # `filterwarnings = error`. Cold, that kills one mutant; in a shared
            # schema it would break every mutant in the file.
            # (A warning made an error surfaces as SyntaxError on 3.12+, so
            # both kinds of problem land here.)
            if problem:
                entries[m["id"]] = {"fallback": "compile-warning"}
            elif dump(docstring(mut_fn.body)) != dump(orig_doc):
                entries[m["id"]] = {"fallback": "docstring"}
            elif type(mut_fn) is not type(fn) or mut_fn.name != fn.name:
                entries[m["id"]] = {"fallback": "structure"}
            else:
                idx = next_idx()
                entries[m["id"]] = {"idx": idx}
                cases.append(
                    ast.match_case(
                        pattern=ast.MatchValue(ast.Constant(idx)),
                        body=branch_body(mut_fn.body),
                    )
                )
        if not cases:
            continue
        # `match` tests cases one by one, so gate on this def's contiguous idx
        # range first: an unmutated call pays one chained compare, not N.
        lo, hi = cases[0].pattern.value.value, cases[-1].pattern.value.value
        # Before `branch_body(fn.body)`: StripDecls edits nested statements in
        # place, so afterwards a `nonlocal` inside a try/if would be gone.
        decls = hoisted(scope_decls(fn.body))
        switch = ast.If(
            test=ast.Compare(
                ast.Constant(lo),
                [ast.LtE(), ast.LtE()],
                [active_expr(), ast.Constant(hi)],
            ),
            body=[ast.Match(subject=active_expr(), cases=cases)],
            orelse=branch_body(fn.body),
        )
        fn.body = ([orig_doc] if orig_doc else []) + decls + [switch]

    if not any("idx" in e for e in entries.values()):
        return None, entries, []
    # After the module docstring and __future__ imports:
    #   _fm_b = __import__('builtins')   (no import statement: click's
    #   test_light_imports audits the imports its modules make)
    #   if not hasattr(_fm_b, '_fm_active'): _fm_b._fm_active = int(os.environ...)
    # The env default lets Python subprocesses a test spawns see the mutant.
    at = 1 if docstring(tree.body) else 0
    while (
        at < len(tree.body)
        and isinstance(tree.body[at], ast.ImportFrom)
        and tree.body[at].module == "__future__"
    ):
        at += 1
    prelude = ast.parse(
        "_fm_b = __import__('builtins')\n"
        f"if not hasattr(_fm_b, {ACTIVE!r}):\n"
        f"    _fm_b.{ACTIVE} = int(__import__('os').environ.get('FM_ACTIVE', 0))\n"
    ).body
    tree.body[at:at] = prelude
    out = ast.unparse(ast.fix_missing_locations(tree))
    with warnings.catch_warnings():
        warnings.simplefilter("error")
        compile(out, "<schema>", "exec")
    return out, entries, switched_functions(out)


def switched_functions(out):
    """[(co_firstlineno, lo, hi)] per switched def; co_firstlineno is the first
    decorator's line when decorated."""
    found = []
    for node in ast.walk(ast.parse(out)):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            for stmt in node.body:
                test = getattr(stmt, "test", None)
                if (
                    isinstance(stmt, ast.If)
                    and isinstance(test, ast.Compare)
                    and len(test.comparators) == 2
                    and getattr(test.comparators[0], "attr", None) == ACTIVE
                ):
                    first = min([d.lineno for d in node.decorator_list] + [node.lineno])
                    found.append((first, test.left.value, test.comparators[1].value))
    return found


def replace_file(path, text):
    """Write via a sibling temp file + rename: a new inode, so a hardlinked
    mirror never writes through to the user's tree."""
    fd, tmp = tempfile.mkstemp(dir=os.path.dirname(path), prefix=".fermut-schema-")
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        f.write(text)
    os.replace(tmp, path)


def schematize(mirror_root, mutants):
    by_file = defaultdict(list)
    for m in mutants:
        by_file[m["file"]].append(m)
    counter = iter(range(1, 1 << 62))
    entries, functions = {}, {}
    for rel, ms in sorted(by_file.items()):
        path = os.path.join(mirror_root, rel)
        try:
            with open(path, "rb") as f:
                out, file_entries, fns = schematize_file(
                    f.read(), ms, lambda: next(counter)
                )
        except (OSError, SyntaxError, ValueError, RecursionError) as e:
            file_entries, out = (
                {m["id"]: {"fallback": f"file: {type(e).__name__}"} for m in ms},
                None,
            )
        entries.update(file_entries)
        if out is not None:
            replace_file(path, out)
            py_compile.compile(path, doraise=True)
            functions[os.path.realpath(path)] = fns
    return {"mutants": entries, "functions": functions}


# --- serve -----------------------------------------------------------------------


class Template:
    def __init__(self, mirror_root, schema_path, pytest_args):
        self.mirror_root = mirror_root
        self.pytest_args = pytest_args
        with open(schema_path) as f:
            functions = json.load(f)["functions"]
        self.switched = {
            path: {first: (lo, hi) for first, lo, hi in fns}
            for path, fns in functions.items()
        }
        self.by_filename = {}  # co_filename -> switched map (realpath memo)
        self.ran = set()
        # Protocol on the original stdio; pytest writes to /dev/null.
        self.req = os.fdopen(os.dup(0), "r", encoding="utf-8")
        self.resp = os.dup(1)
        devnull = os.open(os.devnull, os.O_RDWR)
        for fd in (0, 1, 2):
            os.dup2(devnull, fd)
        os.set_inheritable(self.resp, False)
        os.set_inheritable(self.req.fileno(), False)
        # Each child gets its own pytest-of-<user> (see `child`); the seed
        # gives its counter the digit count a cold run would have now.
        self.user = getpass.getuser()
        self.temproot = tempfile.gettempdir()
        nums = [
            d.rsplit("-", 1)[1]
            for d in glob.glob(
                os.path.join(self.temproot, f"pytest-of-{self.user}", "pytest-*")
            )
        ]
        self.seed = max((int(n) for n in nums if n.isdigit()), default=-1) + 1

    def send(self, obj):
        os.write(self.resp, (json.dumps(obj) + "\n").encode())

    # Record every switched function that runs before the fork.
    def profile(self, frame, event, arg):
        if event == "call":
            code = frame.f_code
            fns = self.by_filename.get(code.co_filename)
            if fns is None:
                fns = self.by_filename[code.co_filename] = self.switched.get(
                    os.path.realpath(code.co_filename), {}
                )
            hit = fns.get(code.co_firstlineno)
            if hit:
                self.ran.add(hit)

    def run(self):
        os.chdir(self.mirror_root)
        setattr(builtins, ACTIVE, 0)
        sys.setprofile(self.profile)
        threading.setprofile(self.profile)
        import pytest

        template = self

        class Plugin:
            @pytest.hookimpl(tryfirst=True)
            def pytest_runtestloop(self, session):
                sys.setprofile(None)
                threading.setprofile(None)
                if session.testsfailed:
                    # Collection errors: a cold run would stop here too.
                    template.send(
                        {
                            "ready": False,
                            "error": f"{session.testsfailed} errors during collection",
                        }
                    )
                    os._exit(0)
                template.serve(session)
                os._exit(0)

        rc = pytest.main(list(self.pytest_args), plugins=[Plugin()])
        # Only reached when pytest stopped before the test loop (collection
        # errors, usage errors): the caller falls back to spawning.
        sys.setprofile(None)
        self.send(
            {"ready": False, "error": f"pytest exited {int(rc)} before the test loop"}
        )
        os._exit(0)

    def serve(self, session):
        by_id = {item.nodeid: item for item in session.items}
        self.send({"ready": True, "unsafe": sorted(self.ran)})
        for line in self.req:
            req = json.loads(line)
            r, w = os.pipe()
            pid = os.fork()
            if pid == 0:
                os.close(r)
                self.child(session, by_id, req, w)
            os.close(w)
            # Also set from this side: the caller may kill the group as soon as
            # it reads the pid, possibly before the child ran its own setpgid.
            try:
                os.setpgid(pid, pid)
            except OSError:
                pass  # the child already did
            self.send({"pid": pid})
            with os.fdopen(r, "rb") as f:
                events = f.read()  # EOF when the child exits or is killed
            _, status = os.waitpid(pid, 0)
            shutil.rmtree(self.child_temp(pid), ignore_errors=True)
            self.send({"status": status, **summarize(events)})

    def child_temp(self, pid):
        # Same length as the real user name (pids need 7 digits), so tmp_path
        # is exactly as long as a cold run's: tests that assert on
        # width-wrapped text holding the path (typer) see the same wrap.
        return os.path.join(self.temproot, f"pytest-of-{self.fake_user(pid)}")

    def fake_user(self, pid):
        return f"f{pid:0{max(len(self.user), 8) - 1}d}"

    def child(self, session, by_id, req, w):
        """Runs in the forked child; never returns. Streams events to `w` as
        they happen, so a child killed mid-test still names the test it hung
        in."""
        code = 3

        def emit(**event):
            os.write(w, (json.dumps(event) + "\n").encode())

        try:
            os.setpgid(0, 0)
            os.close(self.resp)
            self.req.close()
            # Python's own handler, as in a cold run: SIG_DFL would turn a
            # test's self-sent SIGINT (trio) into a dead child.
            signal.signal(signal.SIGINT, signal.default_int_handler)
            # Private basetemp: siblings sharing pytest-of-<user> would prune
            # each other's tmp_path. Seeded so pytest-<N> has cold digits.
            user = self.fake_user(os.getpid())
            os.makedirs(
                os.path.join(self.child_temp(os.getpid()), f"pytest-{self.seed - 1}"),
                exist_ok=True,
            )
            os.environ["USER"] = os.environ["LOGNAME"] = user
            setattr(builtins, ACTIVE, req["idx"])
            os.environ["FM_ACTIVE"] = str(req["idx"])  # Python subprocesses
            code = self.run_items(session, by_id, req, emit)
        except BaseException:  # noqa: BLE001 - nothing may escape a forked child
            code = 3
            emit(ev="error", tb=traceback.format_exc())
        finally:
            emit(ev="end")
            os._exit(code)

    def run_items(self, session, by_id, req, emit):
        """Exit code a cold `pytest [-x] <ids>` would give."""
        ids = req.get("ids")
        if ids is None:
            items = list(session.items)
        else:
            items = [by_id.get(i) for i in dict.fromkeys(ids)]
            if None in items:
                return 4  # pytest: "not found"
        # A cold run reorders its selection (fixtures.py groups items by
        # parametrized higher-scope fixtures); skipping that moves module and
        # session teardowns and can flip order-dependent tests.
        session.config.hook.pytest_collection_modifyitems(
            session=session, config=session.config, items=items
        )
        if not items:
            return 5
        failed, stop_first = [], not req.get("all")

        class Listen:
            def pytest_runtest_logstart(self, nodeid, location):
                emit(ev="begin", nodeid=nodeid)

            def pytest_runtest_logreport(self, report):  # pluggy matches by name
                if report.failed:
                    failed.append(report.nodeid)
                    emit(ev="fail", nodeid=report.nodeid)
                if report.when == "call" or report.failed:
                    emit(ev="dur", dur=report.duration)

        session.config.pluginmanager.register(Listen())
        hook = session.config.hook
        for i, item in enumerate(items):
            nxt = items[i + 1] if i + 1 < len(items) else None
            hook.pytest_runtest_protocol(item=item, nextitem=nxt)
            if failed and stop_first:
                break
        return 1 if failed else 0


def summarize(events):
    """Fold a child's event stream into one result."""
    out = {"failed": [], "test_secs": 0.0, "unfinished": None}
    ended = False
    for line in events.decode(errors="replace").splitlines():
        try:
            e = json.loads(line)
        except ValueError:
            continue  # a kill can truncate the last line
        ev = e.get("ev")
        if ev == "begin":
            out["unfinished"] = e["nodeid"]
        elif ev == "fail" and e["nodeid"] not in out["failed"]:
            out["failed"].append(e["nodeid"])
        elif ev == "dur":
            out["test_secs"] += e["dur"]
        elif ev == "error":
            out["error"] = e["tb"]
        elif ev == "end":
            ended = True
    if ended:
        out["unfinished"] = None  # only a killed child leaves one behind
    return out


def main(argv):
    cmd = argv[1] if len(argv) > 1 else ""
    if cmd == "schematize":
        json.dump(schematize(argv[2], json.load(sys.stdin)), sys.stdout)
        return 0
    if cmd == "serve":
        Template(argv[2], argv[3], argv[4:]).run()
        return 0
    sys.stderr.write(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
