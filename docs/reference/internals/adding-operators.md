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
| Visitor      | `mutator::visitor::Visitor`. Walks the ruff AST and pushes mutants.                                          |
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

If this operator is high-noise and should be experimental, also add it
to `is_experimental()`. Skip otherwise — stable is the default.

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

Edit `src/mutator/visitor.rs`. Find where the existing arithmetic
swaps fire (search `ARITH_SWAPS`) and add a parallel block for your
new table:

```rust
fn visit_expr(&mut self, expr: &'ast Expr) {
    match expr {
        Expr::BinOp(b) => {
            let lex = binop_lexeme(b.op);
            if let Some(r) = self.op_range(b.range(), lex) {
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
        // ...
    }
}
```

Don't forget to import `EXPONENT_TO_MOD_SWAPS` at the top of the
file.

### Step 4 — Test

Add a test that asserts the visitor emits your mutant. The existing
tests in `src/mutator/visitor.rs` show the pattern:

```rust
#[test]
fn emits_exponent_to_mod() {
    let mutants = collect_from_source("x = a ** b\n");
    assert!(mutants.iter().any(|m| m.operator == Operator::ExponentToMod));
}
```

Run:

```sh
cargo test --lib mutator::visitor
```

### Step 5 — Update docs

Add a row to `docs/reference/operators/index.md` (or to the README's operator
table if it still has one). Operator visibility in docs is the contract;
people will set `--ops` against your new name.

That's the entire flow for a table-driven operator. Total diff is
usually < 20 lines.

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

### Step 2 — Implement the visitor handler

In `src/mutator/visitor.rs`, find a similar bespoke operator (e.g.
`DictItemDrop`) and crib its shape:

```rust
fn visit_expr(&mut self, expr: &'ast Expr) {
    match expr {
        // ... existing arms ...
        Expr::Dict(d) => {
            for item in &d.items {
                if let Some(key) = &item.key {
                    if let Expr::StringLiteral(s) = key {
                        // Replace the key with a typo'd version.
                        let original = self.source_bytes_for(s.range());
                        let mutated = typo(original);
                        self.push(Operator::DictKeyTypo, s.range(), mutated);
                    }
                }
            }
        }
    }
    walk_expr(self, expr);
}
```

`source_bytes_for`, `push`, and the surrounding helpers are already
in the visitor — see how other bespoke operators use them. The key
constraints:

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
    let mutants = collect_from_source(r#"x = {"foo": 1, 42: 2}"#);
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
    let mutants = collect_from_source("x = {42: 1}");
    assert!(!mutants.iter().any(|m| m.operator == Operator::DictKeyTypo));
}
```

### Step 4 — Sample-test against `examples/sample/`

For non-trivial operators, add a sample function to
`examples/sample/src/` that exercises the new mutation and a test in
`examples/sample/tests/` that *catches* the mutation. This makes the
end-to-end e2e harness in `tests/e2e.rs` continue to make sense.

### Step 5 — Update docs

Same as the table-driven case. The operator goes into
`docs/reference/operators/index.md`. If it's experimental, note it in the
experimental section and make sure `is_experimental()` returns `true`.

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
- The CLI / config name is `kebab-case`, matches the variant exactly
  via the standard `kebab-case` serde transform.
- Experimental operator names are prefixed `exp:` in their `name()`
  output (`exp:exception-class-swap`).

## Don't forget

- Run `cargo fmt` and `cargo clippy --all-targets -- -D warnings`.
- Add a one-line entry to `CHANGELOG.md` (when present) under
  "Added".
- If the operator changes the JSON report shape (e.g. carries extra
  metadata in the `mutant` object), bump the JSON schema doc in
  `docs/reference/cli/index.md` and call it out as a breaking change in the
  version bump.
