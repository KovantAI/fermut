#!/usr/bin/env python3
"""Empirical gate for the `minimal` operator profile.

The region model (src/mutator/region.rs) says: at a single-op ordering compare
or a 2-operand truth-position `and`/`or`, a suite that kills every minimal-set
mutant kills every other mutant of that site. Python can break the model (sets
are partially ordered, NaN, numpy, custom `__lt__`), so check it on real suites.

Input: JSON reports from `fermut run --operators ror-all --json <report>`,
which test all region-model forms of each site (all 7 per ordering compare).

For every site (grouped by file + `site.start`) whose minimal-set mutants were
all detected, any surviving non-minimal mutant is a violation. Sites where a
minimal mutant survived are skipped: the model makes no claim there.

usage: gate.py report.json [report.json ...]
exit 1 if any violation.
"""
import collections
import json
import sys

DETECTED = {"killed", "timed_out"}
SCORED = DETECTED | {"survived"}


def sites(report):
    by_site = collections.defaultdict(list)
    for o in report["outcomes"]:
        m = o["mutant"]
        site = m.get("site")
        if site is None or o["status"] not in SCORED:
            continue
        by_site[(m["file"], site["start"])].append((o["status"], m))
    return by_site


def main(paths):
    total_sites = checked = 0
    violations = []
    for path in paths:
        with open(path) as f:
            report = json.load(f)
        for (file, start), muts in sites(report).items():
            total_sites += 1
            minimal = [m for s, m in muts if m["site"]["minimal"]]
            if not minimal or any(
                s not in DETECTED for s, m in muts if m["site"]["minimal"]
            ):
                continue
            checked += 1
            for s, m in muts:
                if not m["site"]["minimal"] and s not in DETECTED:
                    violations.append((path, file, m))
    print(f"sites: {total_sites}, all-minimal-killed: {checked}, violations: {len(violations)}")
    for path, file, m in violations:
        print(
            f"  {file}:{m['line']} [{m['operator']}] "
            f"`{m['original']}` -> `{m['replacement']}` survived "
            f"(mask {m['site']['mask']:#06b}) — {path}"
        )
    return 1 if violations else 0


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    sys.exit(main(sys.argv[1:]))
