# `tree-sitter-github-actions-json`

Tree-sitter sub-grammar implementing
[YAML 1.2 §10.2 JSON Schema](https://yaml.org/spec/1.2.2/#102-json-schema)
plain-scalar resolution — the JSON-strict subset of Core.

| Type    | Matches                                            |
| ------- | -------------------------------------------------- |
| `null`  | `null` (lowercase only)                            |
| `bool`  | `true`, `false` (lowercase only)                   |
| `int`   | base-10 only, no leading zeros (see regex)         |
| `float` | base-10 with optional decimal/exponent (see regex) |
| `str`   | anything else                                      |

Regex shapes (lifted out of the table because the formatter eats `|` inside
cells):

- `int`: `-?(0|[1-9][0-9]*)`
- `float`: `-?(0|[1-9][0-9]*)(\.[0-9]*)?([eE][-+]?[0-9]+)?`

No `~`, no aliases, no `0xFF`, no `.inf`/`.NaN`. Tighter than Core, matches what
JSON parsers accept.

## Build

```bash
bun run build
```

Runs `tree-sitter generate` for `src/parser.c`, then `postbuild`
(`../update-schema.js json`) emits `../../src/schema.json.c` for the main
grammar's external scanner.

## Layout

```tree
json/
├── grammar.js   # the only hand-edited file
├── package.json # build / postbuild wiring
└── src/         # generated; do not edit
```

Edit `grammar.js`, run `bun run build`, commit both `grammar.js` and the
regenerated `src/`.
