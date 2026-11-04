"""Phase 1: build a schematized mirror of a repo from a fermut report.

    <venv>/python schematize.py <ref.json> <repo> <mirror>

Run with the repo's venv python: the mirror is byte-compiled for it.

Every executed mutant inside a function body becomes one `case` of a flat
`match` placed inside the *innermost* enclosing function:

    def f(self, x):
        "doc"                       # docstring kept outside, __doc__ unchanged
        global g                    # global/nonlocal hoisted (must precede use)
        if 17 <= _fm_schema.ACTIVE <= 19:   # this function's contiguous idx range
            match _fm_schema.ACTIVE:
                case 17: <body with mutant 17 spliced in>
                ...
        else:
            <original body>

Switching inside the function (not mutmut-style separate variant functions)
keeps the function object, its `__class__` cell (zero-arg super), closures,
decorators and generator/async-ness identical, so none of those need special
handling. Innermost, so closures built at import time still switch per call.

Mutants outside any function body (module, class body, signature), touching
the docstring, or raising a compile-time warning are marked `fallback`: they'd
still be spawned cold.

Writes <mirror>/... (copy of repo, schematized files rewritten and
byte-compiled), _fm_schema.py into each import root, and <mirror>/schema.json:
  mutants:   {mutant id: {"idx": n} | {"fallback": why}}
  functions: {file: [[co_firstlineno, lo, hi], ...]}  (for the fork server's
             "ran before fork" check)
"""

import argparse
import ast
import compileall
import json
import shutil
import warnings
from collections import defaultdict
from pathlib import Path

RAN = {"killed", "survived", "timed_out", "error"}  # report status names
SCOPES = (ast.FunctionDef, ast.AsyncFunctionDef, ast.Lambda, ast.ClassDef)


def line_starts(src: bytes):
    starts = [0]
    starts += [i + 1 for i, b in enumerate(src) if b == 0x0A]
    return starts


def span(node, starts):
    return starts[node.lineno - 1] + node.col_offset, starts[
        node.end_lineno - 1
    ] + node.end_col_offset


def kids(node):
    """Statement-level children; index paths over these survive a mutation
    inside a function body because nothing above the function changes."""
    return [
        c
        for c in ast.iter_child_nodes(node)
        if isinstance(c, (ast.stmt, ast.excepthandler, ast.match_case))
    ]


def innermost_function(tree, starts, lo):
    """Index path to the innermost def whose *body* holds byte `lo`, else a reason.

    Innermost, so a closure built at import time (decorator factories) still
    checks the switch when it is *called*, not when it was built. A nested
    def's decorators/defaults/class bases run in the enclosing body, so they
    belong to the enclosing def."""
    path, node, best, why = [], tree, None, "module"
    while True:
        for i, child in enumerate(kids(node)):
            if not hasattr(child, "lineno"):
                continue  # match_case has no position
            a, b = span(child, starts)
            if a <= lo < b:
                break
        else:
            break
        path.append(i)
        node = child
        if isinstance(node, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)):
            if lo < span(node.body[0], starts)[0]:
                why = "signature"  # decorator, default, annotation, bases
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
    if (
        body
        and isinstance(body[0], ast.Expr)
        and isinstance(body[0].value, ast.Constant)
        and isinstance(body[0].value.value, str)
    ):
        return body[0]
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
    """Body minus docstring and global/nonlocal, never empty."""
    body = body[1:] if docstring(body) else list(body)
    body = [StripDecls().visit(s) for s in body]
    return body or [ast.Pass()]


def hoisted(decls):
    names = defaultdict(list)
    for d in decls:
        for n in d.names:
            if n not in names[type(d)]:
                names[type(d)].append(n)
    return [cls(names=ns) for cls, ns in names.items()]


def schematize_file(src: bytes, mutants, next_idx):
    """Returns (new source or None, {id: entry})."""
    tree, starts = ast.parse(src), line_starts(src)
    entries, by_fn = {}, defaultdict(list)
    for m in mutants:
        path, why = innermost_function(tree, starts, m["range"][0])
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
            a, b = m["range"]
            mut_src = src[:a] + m["replacement"].encode() + src[b:]
            try:
                mut_tree = ast.parse(mut_src)
                mut_fn = resolve(mut_tree, path)
            except (SyntaxError, IndexError, AttributeError):
                entries[m["id"]] = {"fallback": "unparsable"}
                continue
            # A compile-time warning (e.g. `x is ''`) becomes an import error
            # under `filterwarnings = error`. Cold, that kills only this mutant;
            # in a shared schema it would break every mutant in the file.
            try:
                with warnings.catch_warnings():
                    warnings.simplefilter("error")
                    compile(mut_src, "<mutant>", "exec")
            except (SyntaxError, Warning):
                entries[m["id"]] = {"fallback": "compile-warning"}
                continue
            if dump(docstring(mut_fn.body)) != dump(orig_doc):
                entries[m["id"]] = {"fallback": "docstring"}
                continue
            if not isinstance(mut_fn, type(fn)) or mut_fn.name != fn.name:
                entries[m["id"]] = {"fallback": "structure"}
                continue
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
        decls = scope_decls(fn.body)
        # match tests cases linearly, so gate on this function's contiguous idx
        # range first: unmutated calls pay one chained compare, not N.
        lo, hi = cases[0].pattern.value.value, cases[-1].pattern.value.value
        active = ast.Attribute(ast.Name("_fm_schema", ast.Load()), "ACTIVE", ast.Load())
        switch = ast.If(
            test=ast.Compare(
                ast.Constant(lo), [ast.LtE(), ast.LtE()], [active, ast.Constant(hi)]
            ),
            body=[ast.Match(subject=active, cases=cases)],
            orelse=branch_body(fn.body),
        )
        fn.body = ([orig_doc] if orig_doc else []) + hoisted(decls) + [switch]

    if not any("idx" in e for e in entries.values()):
        return None, entries, []
    # `import _fm_schema` after the module docstring and __future__ imports.
    at = 1 if docstring(tree.body) else 0
    while (
        at < len(tree.body)
        and isinstance(tree.body[at], ast.ImportFrom)
        and tree.body[at].module == "__future__"
    ):
        at += 1
    tree.body.insert(at, ast.Import(names=[ast.alias("_fm_schema")]))
    out = ast.unparse(ast.fix_missing_locations(tree))
    with warnings.catch_warnings():
        warnings.simplefilter("error")
        compile(out, "<schema>", "exec")
    return out, entries, switched_functions(out)


def is_gate(stmt):
    return (
        isinstance(stmt, ast.If)
        and isinstance(stmt.test, ast.Compare)
        and len(stmt.test.comparators) == 2
        and isinstance(stmt.test.comparators[0], ast.Attribute)
        and stmt.test.comparators[0].attr == "ACTIVE"
        and getattr(stmt.test.comparators[0].value, "id", "") == "_fm_schema"
    )


def switched_functions(out):
    """[(co_firstlineno, lo, hi)] for each switched def in the written source.
    co_firstlineno is the first decorator's line when decorated."""
    found = []
    for node in ast.walk(ast.parse(out)):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            for stmt in node.body:
                if is_gate(stmt):
                    first = min([d.lineno for d in node.decorator_list] + [node.lineno])
                    found.append(
                        (first, stmt.test.left.value, stmt.test.comparators[1].value)
                    )
    return found


def import_root(path: Path) -> Path:
    d = path.parent
    while (d / "__init__.py").exists():
        d = d.parent
    return d


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("ref")
    ap.add_argument("repo", type=Path)
    ap.add_argument("mirror", type=Path)
    a = ap.parse_args()
    repo, mirror = a.repo.resolve(), a.mirror.resolve()

    ref = json.loads(Path(a.ref).read_text())["outcomes"]
    by_file = defaultdict(list)
    for o in ref:
        if o["status"] in RAN:
            by_file[o["mutant"]["file"]].append(o["mutant"])

    if mirror.exists():
        shutil.rmtree(mirror)
    shutil.copytree(
        repo,
        mirror,
        symlinks=True,
        ignore=shutil.ignore_patterns(
            ".git", ".fermut", "__pycache__", ".pytest_cache", "mutants", ".venv"
        ),
    )

    counter = iter(range(1, 1 << 30))
    schema, roots, functions = {}, set(), {}
    for file, ms in sorted(by_file.items()):
        rel = Path(file).resolve().relative_to(repo)
        out, entries, fns = schematize_file(
            (repo / rel).read_bytes(), ms, lambda: next(counter)
        )
        schema.update(entries)
        if out is not None:
            (mirror / rel).write_text(out)
            functions[str(mirror / rel)] = fns
            roots.add(import_root(mirror / rel))
    for r in roots:
        (r / "_fm_schema.py").write_text(
            'import os\n\nACTIVE = int(os.environ.get("FM_ACTIVE", 0))  # env: cold triage runs\n'
        )
    # Schema files never change during a run, so bytecode can't go stale; this
    # spares every Python subprocess a test spawns from recompiling them.
    compileall.compile_dir(mirror, quiet=1, workers=0)
    (mirror / "schema.json").write_text(
        json.dumps(
            {
                "roots": sorted(map(str, roots)),
                "mutants": schema,
                "functions": functions,
            },
            indent=1,
        )
    )

    n = len(schema)
    kinds = defaultdict(int)
    for e in schema.values():
        kinds["schema" if "idx" in e else "fallback:" + e["fallback"]] += 1
    for k, v in sorted(kinds.items(), key=lambda kv: -kv[1]):
        print(f"{k:22} {v:6} {100 * v / n:5.1f}%")


if __name__ == "__main__":
    main()
