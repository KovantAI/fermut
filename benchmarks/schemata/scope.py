"""Phase 0: classify mutants by schematizability.

    python scope.py <fermut report.json> [--executed]

Buckets each mutant by its innermost enclosing scope:
  func       inside a function body -> trampoline-able
  trap:<why> inside a function body, but the first-cut spike skips it
  module / class / signature (default arg, annotation, decorator) -> spawn fallback
"""

import ast
import collections
import json
import sys
from pathlib import Path

# Builtin descriptors wrap the trampoline transparently; anything else may
# introspect or cache the function object, so the first cut falls back.
SAFE_DECORATORS = {
    "property",
    "staticmethod",
    "classmethod",
    "setter",
    "getter",
    "deleter",
    "abstractmethod",
    "overload",
    "override",
}


def line_starts(src: bytes):
    starts = [0]
    for i, b in enumerate(src):
        if b == 0x0A:
            starts.append(i + 1)
    return starts


def span(node, starts):
    return starts[node.lineno - 1] + node.col_offset, starts[
        node.end_lineno - 1
    ] + node.end_col_offset


def dec_name(d):
    d = d.func if isinstance(d, ast.Call) else d
    return d.attr if isinstance(d, ast.Attribute) else getattr(d, "id", "")


def trap(fn, parents):
    if isinstance(fn, ast.AsyncFunctionDef):
        return "async"
    if any(
        isinstance(p, (ast.FunctionDef, ast.AsyncFunctionDef, ast.Lambda))
        for p in parents
    ):
        return "nested"
    if {dec_name(d) for d in fn.decorator_list} - SAFE_DECORATORS:
        return "decorator"
    stack = list(fn.body)
    while stack:
        n = stack.pop()
        if isinstance(
            n, (ast.FunctionDef, ast.AsyncFunctionDef, ast.Lambda, ast.ClassDef)
        ):
            continue  # inner scopes are checked on their own
        stack.extend(ast.iter_child_nodes(n))
        if isinstance(n, (ast.Yield, ast.YieldFrom)):
            return "generator"
        if (
            isinstance(n, ast.Call)
            and getattr(n.func, "id", "") == "super"
            and not n.args
        ):
            return "super"
    return None


def classify(tree, starts, lo):
    """Walk down to the innermost scope holding byte offset `lo`."""
    kind, parents, node = "module", [], tree
    while True:
        for child in ast.iter_child_nodes(node):
            if not hasattr(child, "lineno"):
                continue
            a, b = span(child, starts)
            if a <= lo < b:
                break
        else:
            return kind
        if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef)):
            body_a = span(child.body[0], starts)[0]
            if lo < body_a:
                return "signature"  # decorator, default, annotation, name
            t = trap(child, parents)
            if t:
                return f"trap:{t}"
            kind = "func"
            parents.append(child)
        elif isinstance(child, ast.ClassDef):
            if kind == "module":
                kind = "class"
            parents.append(child)
        elif isinstance(child, ast.Lambda):
            parents.append(child)
        node = child


def main():
    report = json.loads(Path(sys.argv[1]).read_text())["outcomes"]
    executed = "--executed" in sys.argv
    trees, counts = {}, collections.Counter()
    for o in report:
        if executed and o["status"] == "skipped":
            continue
        m = o["mutant"]
        if m["file"] not in trees:
            src = Path(m["file"]).read_bytes()
            trees[m["file"]] = (ast.parse(src), line_starts(src))
        tree, starts = trees[m["file"]]
        counts[classify(tree, starts, m["range"][0])] += 1
    total = sum(counts.values())
    for k, v in counts.most_common():
        print(f"{k:16} {v:6} {100 * v / total:5.1f}%")
    print(f"{'total':16} {total:6}")


if __name__ == "__main__":
    main()
