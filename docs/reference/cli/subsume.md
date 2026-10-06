# `fermut subsume`

Find the **dominator mutants** in a recorded run: the few mutants whose kills
guarantee every other kill. Prints how many mutants that saves and the
**dominator score**, and writes the class map to `.fermut/dominators.json`.
Runs no tests.

```sh
fermut run src/pkg --no-cache --record-kill-sets kill-sets.jsonl
fermut subsume kill-sets.jsonl
```

| Flag                  | Effect                                                                     |
|-----------------------|----------------------------------------------------------------------------|
| `<KILL_SETS>`         | JSONL from `fermut run --record-kill-sets` (required).                      |
| `-o, --output <PATH>` | Where to write the store. Default `<project root>/.fermut/dominators.json`. |
| `--format human\|json` | Summary on stdout. `json` prints the `stats` object below.                 |

`--record-kill-sets` is experimental. It drops pytest's `-x`, so every
selected test runs on a killed mutant (about 1.15× a normal run on pyjwt).
Pass `--no-cache`: a cache hit runs nothing and records no kill-set.

## What it computes

A mutant's **kill-set** `K(m)` is the set of tests that fail on it.

- `a` **subsumes** `b` when `K(a) ⊆ K(b)`. Every test that kills `a` also
  kills `b`.
- Mutants with the same `K` form one **class**. No recorded test tells them
  apart.
- **Dominators** are the classes no other class strictly subsumes. Kill all
  dominators and you have killed every killed mutant.

```text
kill-sets: 348 mutants (284 killed, 62 survived)
  left out: 0 killed without a kill-set, 2 timed out/errored
classes:   127 distinct kill-sets
dominators: 38 (86.6% fewer mutants to certify the 284 killed)
dominator score: 38.0% (38 dominators / 38 dominators + 62 survivors)
```

(pyjwt `jwt/api_jws.py`.)

The relations hold for the recorded suite, not for all possible tests
(Kurtz et al., *Mutant Subsumption Graphs*, ICSTW 2014). Re-record after
tests change.

### Dominator score

`D / (D + S)`: `D` killed dominator classes, `S` survivors. After Ammann,
Delamaro & Offutt, *Establishing Theoretical Minimal Sets of Mutants* (ICST
2014). The plain score counts every redundant, easily killed mutant. The
dominator score counts each independent fault once, so it is lower and harder
to inflate. Survivors have no kill-set to compare, so each counts as its own
class, which can only lower the score.

### What is left out

- **Timeouts and errors.** No kill-set was recorded.
- **Kills with an empty kill-set.** A mutant that breaks the module import
  fails collection before pytest names any test. An empty set would be a
  subset of every class and become the only dominator, so these are counted
  apart.

## `dominator_score` in `fermut run`

When `.fermut/dominators.json` exists, `fermut run` adds `dominator_score` to
the JSON `summary`, but only if the store still describes the run:

- every scored mutant's file has the AST hash it had when `subsume` ran
  (comments and formatting don't count), and
- every killed or timed-out mutant has a class in the store.

Otherwise the field is left out. `-v` logs the reason. The score uses the
current run's verdicts: dominator classes with a member killed this run,
against this run's survivors. Survivors that another survivor at the same
compare or `and`/`or` subsumes are folded first, so each counts once (see
[Operator profiles](../operators/profiles.md#folded-survivors)).

The check is per file. Mutant ids include byte offsets, so an edit above a
function already renames its mutants, and a per-function check would not
save their classes.

## `dominators.json`

```json
{
  "version": 1,
  "generated_at": "2026-10-06T19:37:29Z",
  "kill_sets": "kill-sets.jsonl",
  "stats": {
    "records": 348, "killed": 284, "survived": 62,
    "killed_unattributed": 0, "other": 2,
    "classes": 127, "dominators": 38,
    "reduction_pct": 86.6, "dominator_score": 38.0
  },
  "classes": [
    { "rep": "<mutant id>", "members": ["<mutant id>", "..."],
      "kill_set_size": 1, "dominator": true }
  ],
  "file_hashes": { "/abs/path/jwt/api_jws.py": "<ast hash>" }
}
```

`rep` is the smallest member id. Dominator classes come first.
