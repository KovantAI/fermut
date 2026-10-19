# Adding an operator

How to teach fermut a new kind of mutation. This walks through both
flavors:

- **Table-driven** — a lexeme-to-lexeme swap (e.g. `+` → `-`). Adding
  one is one line in a swap table; the visitor already knows what to
  do.
- **Bespoke** — a structural mutation that needs custom AST handling
  (e.g. drop a decorator, replace a return value with `None`). One
  visitor branch + one operator catalog entry.

Read **[Internals](index.md)** first if you haven't yet.

## Vocabulary

| Term         | Meaning                                                                                                     |
|--------------|-------------------------------------------------------------------------------------------------------------|
| Operator     | The closed enum `mutator::operators::Operator`. One variant per *kind* of mutation.                          |
| Mutant       | One specific application of an operator: byte range + replacement bytes.                                     |
| Collector    | `mutator::visitor::Collector`. Walks the ruff AST and pushes mutants; `visit_expr` dispatches to `emit_*` helpers. |
| Swap table   | A `&[(&str, &str)]` of `(original, replacement)` lexeme pairs. Drives the table-driven operators.            |
| Lexeme       | The raw source text of a token (`"+"`, `"<="`, `"True"`). The visitor matches on lexemes to know which swap fires. |

## Flavor 1: a table-driven operator

The simplest case. Suppose you want to add **exponent → modulo**
(`**` → `%`) under a new operator name `exponent-to-mod`.

### Step 1 — Add the variant

Edit `src/mutator/operators.rs`. Add a variant to the `Operator` enum:

```rust
pub enum Operator {
    // ... existing variants ...
    ExponentToMod,    // <-- new
}
```

Add its kebab-case name in `Operator::name()`:

```rust
impl Operator {
    pub fn name(self) -> &'static str {
        match self {
            // ... existing arms ...
            Self::ExponentToMod => "exponent-to-mod",
        }
    }
```

Add it to `Operator::all()` too. `from_name` (and so `--ops`,
`--skip-ops` and inline-ignore parsing) only finds operators listed
there. Then bump the expected total in the `all_covers_every_variant`
test (currently 46).

Three more `match` statements outside the mutator have no `_` arm, so
the crate won't compile until each gets an arm for the new variant:

- `operator_hint` in `src/cli/subcmd/explain.rs`: the one-line testing
  hint `fermut explain` prints.
- `operator_slug` in `src/cli/subcmd/explain.rs`: the short slug used in
  generated test names.
- `ease` in `src/cli/subcmd/next.rs`: the kill-ease bucket `fermut next`
  ranks by.

If this operator is high-noise and should be experimental, also add it
to `is_experimental()`, prefix its name with `exp:`, and bump that
test's experimental count. The other tiers work the same way:
`is_parity()` (`parity:` prefix, only under `--parity`) and
`is_minimal_only()` / `is_profile_gated()` (only under an operator
profile that enables them). Skip all of these for a stable operator —
stable is the default.

### Step 2 — Add a swap table (or extend an existing one)

If the swap fits an existing table (e.g. arithmetic), add a row there:

```rust
pub const ARITH_SWAPS: &[(&str, &str)] = &[
    // ... existing pairs ...
    ("**", "%"),    // <-- but this would now be tagged ArithOpSwap, not ExponentToMod
];
```

To tag it under your new operator name, create a dedicated table:

```rust
pub const EXPONENT_TO_MOD_SWAPS: &[(&str, &str)] = &[("**", "%")];
```

### Step 3 — Wire the visitor

Edit `src/mutator/visitor.rs`. The arithmetic swaps fire in
`Collector::emit_binop` (search `ARITH_SWAPS`); add a parallel block
for your new table:

```rust
fn emit_binop(&mut self, b: &ast::ExprBinOp) {
    let lex = binop_lexeme(b.op);
    if let Some(r) = self.op_range_between(b.left.range().end(), b.right.range().start(), lex) {
        for (orig, repl) in ARITH_SWAPS {
            if *orig == lex {
                self.push(Operator::ArithOpSwap, r, repl);
            }
        }
        // ↓ new ↓
        for (orig, repl) in EXPONENT_TO_MOD_SWAPS {
            if *orig == lex {
                self.push(Operator::ExponentToMod, r, repl);
            }
        }
    }
}
```

`op_range_between` finds the operator token in the gap between the
two operands, so the mutant's range covers only the operator.

Don't forget to import `EXPONENT_TO_MOD_SWAPS` at the top of the
file.

### Step 4 — Test

Add a test to the `operator_emission_tests` module in
`src/mutator/visitor.rs`. Its `ops_for(source)` and
`contains(source, op)` helpers wrap `collect(Path, source)`:

```rust
#[test]
fn exponent_to_mod_fires() {
    assert!(contains("x = a ** b\n", Operator::ExponentToMod));
}
```

Run:

```sh
cargo test --lib mutator
```

### Step 5 — Update docs

Add a row to the tier's table: `docs/reference/operators/stable.md`
for a stable operator, `experimental.md` or `parity.md` otherwise. Also
bump the operator counts in `docs/reference/operators/index.md`,
`docs/reference/index.md` and the "Operator catalogue (stable)" row of
`docs/concepts/landscape.md`. The `docs_stay_in_sync_with_operator_catalogue`
test in `src/mutator/operators.rs` fails if `stable.md`'s table or a
"N stable" claim disagrees with the enum. Operator visibility in docs
is the contract; people will set `--ops` against your new name.

That's the entire flow for a table-driven operator: `operators.rs`,
`visitor.rs`, the three `match` arms in `explain.rs` / `next.rs`, and
the docs table.

## Flavor 2: a bespoke operator

When the mutation doesn't fit a `lexeme → lexeme` swap. For example,
`return-value-to-none` needs to know whether the `return` has an
expression at all, so the visitor pattern-matches on AST shape, not
lexeme.

Suppose you want a new operator `dict-key-typo` that mutates the keys
of dict literals (`{"foo": 1}` → `{"foOO": 1}`) to verify your tests
actually care about dict key spelling.

### Step 1 — Add the variant

Same as above:

```rust
pub enum Operator {
    DictKeyTypo,
}

impl Operator {
    pub fn name(self) -> &'static str {
        Self::DictKeyTypo => "dict-key-typo",
    }
}
```

Also add it to `Operator::all()`, bump `all_covers_every_variant`, and
add arms to `operator_hint` / `operator_slug` (`src/cli/subcmd/explain.rs`)
and `ease` (`src/cli/subcmd/next.rs`).

### Step 2 — Implement the visitor handler

In `src/mutator/visitor.rs`, find a similar bespoke operator (e.g.
`DictItemDrop`, emitted by `Collector::emit_dict`) and crib its shape.
Add an `emit_*` helper and call it from the `visit_expr` dispatcher:

```rust
fn emit_dict_key_typo(&mut self, d: &ast::ExprDict) {
    for item in &d.items {
        if let Some(Expr::StringLiteral(s)) = &item.key {
            // Slice the original text straight out of the source.
            let original = &self.source[s.range()];
            let mutated = typo(original);
            self.push(Operator::DictKeyTypo, s.range(), &mutated);
        }
    }
}
```

`push` and the surrounding helpers already live on `Collector`. See
how other bespoke operators use them. The key constraints:

- **Byte ranges must be exact.** The emitter does a raw byte splice;
  one off-by-one and the patched file won't parse. Use the AST node's
  `.range()`, never a hand-computed offset.
- **Don't emit mutants that produce invalid Python.** The ty filter
  will catch some of these for you, but the visitor should avoid them
  upfront where it can — the cost of producing an invalid mutant is
  not just the wasted ty call but also the user's surprise when
  they ignore it.

### Step 3 — Test

Bespoke operators want tests that lock the AST shape:

```rust
#[test]
fn dict_key_typo_targets_string_keys() {
    let mutants = collect(Path::new("test.py"), r#"x = {"foo": 1, 42: 2}"#).unwrap();
    let dict_typo: Vec<_> = mutants
        .iter()
        .filter(|m| m.operator == Operator::DictKeyTypo)
        .collect();

    // Only "foo" is a string key; 42 is an int key — leave it alone.
    assert_eq!(dict_typo.len(), 1);
    assert!(dict_typo[0].replacement.contains("XX"));
}

#[test]
fn dict_key_typo_ignores_non_string_keys() {
    assert!(!contains("x = {42: 1}", Operator::DictKeyTypo));
}
```

### Step 4 — Sample-test against `examples/sample/`

For non-trivial operators, add a sample function to
`examples/sample/src/` that exercises the new mutation and a test in
`examples/sample/tests/` that *catches* the mutation. This makes the
end-to-end e2e harness in `tests/e2e.rs` continue to make sense.

### Step 5 — Update docs

Same as the table-driven case: a row in `stable.md` (or
`experimental.md` / `parity.md`) and the counts on the index pages.
If it's experimental, make sure `is_experimental()` returns `true`.

## Choosing "stable" vs "experimental"

Operators are experimental when:

- They produce many mutants that are *equivalent* (no observable
  behavior change).
- They depend on test fixtures the operator can't see — e.g. broader
  exception classes that still catch the same errors at runtime.
- The mutation often triggers runtime crashes that aren't useful test
  signal.

Operators are stable when they reliably perturb observable behavior in
ways a reasonable test would catch. When in doubt, ship experimental
first; a future PR can graduate it.

## Naming conventions

- Variant names are `UpperCamelCase` and end in a noun describing the
  *result* of the mutation. `ArithOpSwap`, not `MutateArithOps`.
- The CLI / config name is the hand-written `kebab-case` string in
  `Operator::name()`. It usually mirrors the variant, but not always
  (`RelationalToEquality` → `ror-equality`, `BoolOpToConst` →
  `bool-const`), so look it up there rather than deriving it.
- Experimental operator names are prefixed `exp:` and parity names
  `parity:` in their `name()` output (`exp:exception-class-swap`).
  `from_name` accepts them with or without the prefix.

## Don't forget

- Run `cargo fmt` and `cargo clippy --all-targets -- -D warnings`.
- Add a one-line entry to `CHANGELOG.md` (when present) under
  "Added".
- If the operator changes the JSON report shape (e.g. carries extra
  metadata in the `mutant` object), bump the JSON schema doc in
  `docs/reference/cli/index.md` and call it out as a breaking change in the
  version bump.
