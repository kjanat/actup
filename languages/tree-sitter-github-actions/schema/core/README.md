# `tree-sitter-github-actions-core`

Tree-sitter sub-grammar implementing
[YAML 1.2 §10.3 Core Schema](https://yaml.org/spec/1.2.2/#103-core-schema)
plain-scalar resolution.

| Type    | Matches                                                                  |
| ------- | ------------------------------------------------------------------------ |
| `null`  | `~`, `null`, `Null`, `NULL`                                              |
| `bool`  | `true`/`True`/`TRUE`, `false`/`False`/`FALSE`                            |
| `int`   | base-10 (`[-+]?[0-9]+`), base-8 (`0o[0-7]+`), base-16 (`0x[0-9a-fA-F]+`) |
| `float` | decimal/scientific, `[-+]?\.inf`/`.Inf`/`.INF`, `.nan`/`.NaN`/`.NAN`     |
| `str`   | anything else                                                            |

## Build

```bash
bun run build
```

Runs `tree-sitter generate` to refresh `src/parser.c`, then `postbuild`
(`../update-schema.js core`) walks that lexer's state machine and emits
`../../src/schema.core.c`, which is `#include`d by the main grammar's external
scanner.

## Layout

```tree
core/
├── grammar.js   # the only hand-edited file
├── package.json # build / postbuild wiring
└── src/         # generated; do not edit
```

Edit `grammar.js`, run `bun run build`, commit both `grammar.js` and the
regenerated `src/`.
