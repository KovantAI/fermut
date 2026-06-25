# Operator-catalogue gap: mutmut → fermut (pyjwt)

Each mutmut mutation classified by its before→after change, then checked against fermut: **exact** = fermut made the identical mutation; **on_line** = fermut mutated that line differently; **none** = fermut produced nothing there.

| mutmut kind | total | fermut exact | on-line | none | exact% |
|---|--:|--:|--:|--:|--:|
| replace-with-None | 521 | 68 | 304 | 149 | 13% |
| deletion (arg/element) | 259 | 35 | 179 | 45 | 13% |
| string-sentinel (XX-wrap) | 245 | 92 | 135 | 18 | 37% |
| string case-swap (UPPER) | 209 | 0 | 186 | 23 | 0% |
| string case-swap (lower) | 97 | 0 | 89 | 8 | 0% |
| bool-literal (True/False) | 50 | 28 | 11 | 11 | 56% |
| number-mutate | 38 | 17 | 15 | 6 | 44% |
| bool-op (and/or) | 27 | 5 | 21 | 1 | 18% |
| other ('='->'!') | 16 | 14 | 2 | 0 | 87% |
| arith/bit-op | 16 | 8 | 7 | 1 | 50% |
| other ('Fals'->'Non') | 14 | 3 | 11 | 0 | 21% |
| None-replaced | 12 | 1 | 5 | 6 | 8% |
| other ('Tru'->'Non') | 8 | 1 | 7 | 0 | 12% |
| other ('!'->'=') | 7 | 2 | 5 | 0 | 28% |
| string-mutate | 5 | 0 | 1 | 4 | 0% |
| other ('stacklevel=2,'->')') | 5 | 0 | 5 | 0 | 0% |
| other ('and will be removed in pyjwt version 3.'->'XXand will be removed in pyjwt version 3. XX') | 4 | 0 | 0 | 4 | 0% |
| other ('audienc'->'Non') | 3 | 0 | 3 | 0 | 0% |
| other ('signatur'->'Non') | 3 | 0 | 1 | 2 | 0% |
| break/continue | 2 | 1 | 1 | 0 | 50% |
| other ('curve.key_siz'->'Non') | 2 | 0 | 0 | 2 | 0% |
| other ('False'->'),') | 2 | 0 | 2 | 0 | 0% |
| other ('passing additional kwargs to decode_complete() is deprecated'->'XXpassing additional kwargs to decode_complete() is deprecated XX') | 2 | 0 | 0 | 2 | 0% |
| other ('passing additional kwargs to decode() is deprecated'->'XXpassing additional kwargs to decode() is deprecated XX') | 2 | 0 | 0 | 2 | 0% |
| other ('key.algorithm_nam'->'Non') | 2 | 0 | 0 | 2 | 0% |
| other ('Expecting a dict object, as JWT only supports'->'XXExpecting a dict object, as JWT only supports XX') | 1 | 0 | 1 | 0 | 0% |
| other ('json_encoder=json_encoder,'->')') | 1 | 0 | 1 | 0 | 0% |
| other ('sort_headers=sort_headers,'->')') | 1 | 0 | 0 | 1 | 0% |
| other ('cls=json_encoder,'->').encode("utf-8")') | 1 | 0 | 1 | 0 | 0% |
| other ('The `verify` argument to `decode` does nothing in PyJWT 2.0 and newer.'->'XXThe `verify` argument to `decode` does nothing in PyJWT 2.0 and newer. XX') | 1 | 0 | 1 | 0 | 0% |
| other ('The equivalent is setting `verify_signature` to False in the `options` dictionary.'->'XXThe equivalent is setting `verify_signature` to False in the `options` dictionary. XX') | 1 | 0 | 0 | 1 | 0% |
| other ('detached_payload=detached_payload,'->')') | 1 | 0 | 1 | 0 | 0% |
| other ('subject=subject,'->')') | 1 | 0 | 1 | 0 | 0% |
| other ('leeway=leeway,'->')') | 1 | 0 | 1 | 0 | 0% |
| other ('aud not in audience_claims for aud in audienc'->'Non') | 1 | 0 | 1 | 0 | 0% |
| other ('SECP521R1'->'),  # Backward compat for #219 fix') | 1 | 0 | 1 | 0 | 0% |
| other ('should not be used as an HMAC secret.'->'XX should not be used as an HMAC secret.XX') | 1 | 0 | 0 | 1 | 0% |
| other ('l'->'r') | 1 | 0 | 0 | 1 | 0% |
| other ('The specified key looks like a JWK and should not be'->'XXThe specified key looks like a JWK and should not be XX') | 1 | 0 | 1 | 0 | 0% |
| other ('used directly as an HMAC secret. Load it via'->'XXused directly as an HMAC secret. Load it via XX') | 1 | 0 | 0 | 1 | 0% |
| other ('self.hash_alg().digest_siz'->'Non') | 1 | 0 | 0 | 1 | 0% |
| other ('upp'->'low') | 1 | 0 | 0 | 1 | 0% |
| other ('low'->'upp') | 1 | 0 | 1 | 0 | 0% |
| other ('context=self.ssl_context'->') as response:') | 1 | 0 | 1 | 0 | 0% |
| other ('respons'->'Non') | 1 | 0 | 0 | 1 | 0% |
| other ('because it is not registered.'->'XX because it is not registered.XX') | 1 | 0 | 0 | 1 | 0% |
| other ('sort_keys=sort_headers'->').encode()') | 1 | 0 | 1 | 0 | 0% |
| other ('options=merged_options,'->')') | 1 | 0 | 1 | 0 | 0% |
| other ('detached_payload=detached_payload'->')') | 1 | 0 | 1 | 0 | 0% |

## Operators fermut is missing or under-covers

- **string case-swap (UPPER)** — 209 mutmut mutations, 0 fermut exact (186 same-line different-op, 23 untouched). e.g. 'big'->'BIG' @ jwt/utils.py; 'require'->'REQUIRE' @ jwt/api_jwt.py
- **string case-swap (lower)** — 97 mutmut mutations, 0 fermut exact (89 same-line different-op, 8 untouched). e.g. 'JSON'->'json' @ jwt/api_jwt.py; 'The equivalent is setting `verify_signature` to F'->'the equivalent is setting `verify_signature` to f' @ jwt/api_jwt.py
- **string-mutate** — 5 mutmut mutations, 0 fermut exact (1 same-line different-op, 4 untouched). e.g. '"passing additional kwargs to decode_complete() is deprecated "'->'None,' @ jwt/api_jwt.py; '"passing additional kwargs to decode() is deprecated "'->'None,' @ jwt/api_jwt.py
- **other ('stacklevel=2,'->')')** — 5 mutmut mutations, 0 fermut exact (5 same-line different-op, 0 untouched). e.g.
- **other ('and will be removed in pyjwt version 3.'->'XXand will be removed in pyjwt version 3. XX')** — 4 mutmut mutations, 0 fermut exact (0 same-line different-op, 4 untouched). e.g. 'and will be removed in pyjwt version 3.'->'XXand will be removed in pyjwt version 3. XX' @ jwt/api_jwt.py; 'and will be removed in pyjwt version 3.'->'XXand will be removed in pyjwt version 3. XX' @ jwt/api_jwt.py
- **other ('audienc'->'Non')** — 3 mutmut mutations, 0 fermut exact (3 same-line different-op, 0 untouched). e.g.
- **other ('signatur'->'Non')** — 3 mutmut mutations, 0 fermut exact (1 same-line different-op, 2 untouched). e.g. 'signatur'->'Non' @ jwt/api_jws.py; 'signatur'->'Non' @ jwt/api_jws.py

## Synthesis

- **True coverage holes** (mutmut mutates a line fermut leaves untouched): 295 mutmut mutations across all kinds. Still dominated by `replace-with-None`: fermut's None ops now cover assignments, returns, and call arguments (`arg-to-none`), plus `none-to-value` for the reverse, but NOT whole-call results, bare attribute/name reads, or subscripts replaced with None.
- **Redundantly covered** (fermut mutates the same line with a *different* operator, so the line is still exercised): 1005 mutations. The big one is `string case-swap` (UPPER/lower) — fermut has no case-swap, but its `string-to-empty` + `string-sentinel` hit the same string literals, so detection is largely retained.

**Operators fermut genuinely lacks (catalogue gaps):**
1. `expression → None` beyond call arguments — `arg-to-none` now covers call args, but mutmut also replaces whole-call results, attribute/name reads, and subscripts with None (the residual `replace-with-None` holes).
2. `string case-swap` (UPPER / lower) — no fermut analog (low marginal detection value; string-sentinel overlaps).

**Operators fermut has that mutmut lacks (fermut advantages):** `not-insertion` (150 — wraps booleans in `not(...)`), `string-to-empty`, `boundary-shift`, `unary-op-swap`, `slice-bound-drop`, `bytes-sentinel`.

## fermut operator inventory (executed mutants)

| fermut operator | count |
|---|--:|
| string-to-empty | 313 |
| string-sentinel | 313 |
| arg-to-none | 263 |
| not-insertion | 150 |
| compare-op-swap | 117 |
| number-shift | 74 |
| dict-item-drop | 54 |
| assign-value-to-none | 54 |
| constant-replace | 48 |
| keyword-arg-drop | 47 |
| unary-op-swap | 44 |
| number-to-neg | 29 |
| number-to-zero | 28 |
| bool-op-swap | 25 |
| none-to-value | 22 |
| arith-op-swap | 13 |
| return-value-to-none | 12 |
| bytes-sentinel | 12 |
| boundary-shift | 10 |
| slice-bound-drop | 4 |
| break-continue-swap | 2 |
