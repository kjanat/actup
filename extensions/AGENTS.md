# Editor Extensions

Two editor extensions. The VSCode extension is outside the Cargo workspace; the
Zed extension is a workspace member.

## Structure

| Extension | Stack                | Build   | Binary Output     |
| --------- | -------------------- | ------- | ----------------- |
| `vscode/` | TypeScript + bun     | tsdown  | `.vsix` package   |
| `zed/`    | Rust WASM (`cdylib`) | `cargo` | `.wasm` extension |

## VSCode Extension (`vscode/`)

- Desktop: wraps `actup-lsp` via `vscode-languageclient` (LSP client)
- Web: runs the `actup-wasm` checks in the browser (`src/web/`)
- Activates on the `github-actions-workflow` language and in workspaces that
  contain `action.{yml,yaml}` or `.*/workflows/**/*.{yml,yaml}` files
- Runs the `actup.server.path` setting, or `actup-lsp` from PATH
- Build: `bun run compile` (tsdown → `out/extension.mjs` and
  `dist/web/extension.js`)
- Package: `bun run package` (`vsce package`) → `.vsix`
- Entries: `src/extension.ts` (desktop), `src/web/extension.ts` (web)

## Zed Extension (`zed/`)

- Workspace member: `Cargo.toml` uses the workspace package fields and lints
- Compiles to `cdylib` WASM via `zed::register_extension!(ActupExtension)`
- Starts `actup-lsp` (the `lsp.actup-lsp.binary.path` setting, or `actup-lsp`
  from PATH) and declares the `actup-mcp` context server
- Tree-sitter grammar fetched at install time via `extension.toml`
  (`[grammars.github_actions]` `repository` + `rev`, built from
  `languages/tree-sitter-github-actions`); Zed clones it into the git-ignored
  `grammars/`
- Tree-sitter queries in `languages/github_actions/` (highlights, injections,
  outline, brackets, overrides, redactions, textobjects)
- Entry: `src/lib.rs`

## Where to Look

| Task                       | Start Here                                          |
| -------------------------- | --------------------------------------------------- |
| Change VSCode settings     | `vscode/package.json` → `contributes.configuration` |
| Change VSCode activation   | `vscode/package.json` → `activationEvents`          |
| Modify VSCode LSP client   | `vscode/src/extension.ts`                           |
| Modify Zed server commands | `zed/src/lib.rs`                                    |
| Change Zed language scope  | `zed/extension.toml` → `language_servers`           |
| Update syntax highlighting | `zed/languages/github_actions/*.scm`                |

## Anti-Patterns

- **Never add VSCode to the Cargo workspace** — root `Cargo.toml` excludes
  `extensions/vscode`
- **Don't edit `zed/grammars/`** — Zed clones the grammar there; change it in
  `languages/tree-sitter-github-actions/`
- **LSP binary must be pre-installed** — the desktop extensions locate
  `actup-lsp` at runtime, don't embed it

## Commands

Each extension has its own `justfile`, imported into root via `mod`:

```bash
just zed::check        # cargo check --target wasm32-wasip2
just zed::clippy       # cargo clippy (strict)
just zed::build        # cargo build (release WASM)
just vscode::lint      # bun run lint (ESLint)
just vscode::typecheck # bun run typecheck
just vscode::test      # bun run test
just vscode::build     # lint + typecheck + compile
just vscode::package   # build + vsce package
just ext-check         # umbrella: zed::clippy + vscode::lint + vscode::typecheck + vscode::test
just ci-full           # ci + wasm::build + wasm::test + ext-check
```

Zed recipes auto-install `wasm32-wasip2` target if missing.

## CI

`check-extensions` job in `ci.yml` runs on pushes to `master` and on pull
requests, except changes that touch only ignored paths such as `**.md`.
`ci-success` fails unless it succeeds.

## Conventions

- Both extensions version-track with the main project (`0.1.0`)
- VSCode uses `bun` (not npm/yarn) for package management
- Zed extension declares the `actup-mcp` context server in `extension.toml`
