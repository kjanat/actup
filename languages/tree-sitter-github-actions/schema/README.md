# YAML scalar resolution schemas

Auxiliary tree-sitter sub-grammars that classify plain scalars per
[YAML 1.2 §10].\
Their generated lexers are converted by [`update-schema.js`][update-schema] into
hand-tunable C state machines ([`src/schema.core.c`][core.c],
[`src/schema.json.c`][json.c]) that the main grammar's external scanner consults
at parse time.

So a token like `null` becomes a typed `null_scalar`, `0xFF` becomes an
`integer_scalar`, etc., without inflating the main grammar with type-resolution
rules.

| Sub-grammar       | YAML spec           | Covers                                |
| ----------------- | ------------------- | ------------------------------------- |
| [`core/`](./core) | [§10.3 Core Schema] | Null, bool, int (10/8/16), float, str |
| [`json/`](./json) | [§10.2 JSON Schema] | JSON-strict null/bool/int/float/str   |

Failsafe Schema (§10.1, str-only) is the implicit fallback when neither of the
above matches — no sub-grammar needed.

## Build

```bash
bun run --filter 'tree-sitter-github-actions-*' build
```

regenerates each sub-grammar's `src/parser.c` and re-emits the corresponding
`../src/schema.<name>.c`.\
Run after editing any `schema/<name>/grammar.js`.

[YAML 1.2 §10]: https://yaml.org/spec/1.2.2/#chapter-10-recommended-schemas
[update-schema]: ./update-schema.js
[core.c]: ../src/schema.core.c
[json.c]: ../src/schema.json.c
[§10.3 Core Schema]: https://yaml.org/spec/1.2.2/#103-core-schema
[§10.2 JSON Schema]: https://yaml.org/spec/1.2.2/#102-json-schema
