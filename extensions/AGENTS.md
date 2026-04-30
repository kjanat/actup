# Editor Extensions

Two editor extensions that wrap `actup-lsp`. Both are **outside the Cargo workspace** — they have independent build systems.

## Structure

| Extension | Stack                | Build   | Binary Output     |
| --------- | -------------------- | ------- | ----------------- |
| `vscode/` | TypeScript + bun     | `tsc`   | `.vsix` package   |
| `zed/`    | Rust WASM (`cdylib`) | `cargo` | `.wasm` extension |

## VSCode Extension (`vscode/`)

- Wraps `actup-lsp` via `vscode-languageclient` (LSP client)
- Activates on YAML files matching `.github/workflows/**` and `action.{yml,yaml}`
- Finds `actup-lsp` binary in PATH or configured `actup.serverPath` setting
- Build: `bun run compile` (TypeScript → `out/extension.js`)
- Package: `vsce package` → `.vsix`
- Entry: `src/extension.ts` (single file, ~200 lines)

## Zed Extension (`zed/`)

- **Separate `[workspace]`** with its own `Cargo.toml` + `Cargo.lock` (not a workspace member)
- Compiles to `cdylib` WASM via `zed::register_extension!(ActupExtension)`
- Provides `/actions:update` slash command + LSP integration (locates `actup-lsp` in PATH)
- Tree-sitter `yaml` grammar fetched at install time via `extension.toml` (`[grammars.yaml]` `repository` + `rev`); `grammars/` is
  git-ignored
- Custom tree-sitter queries in `languages/github_actions/` (highlights, injections, outline)
- Entry: `src/lib.rs` (single file)

## Where to Look

| Task                       | Start Here                                          |
| -------------------------- | --------------------------------------------------- |
| Change VSCode settings     | `vscode/package.json` → `contributes.configuration` |
| Change VSCode activation   | `vscode/package.json` → `activationEvents`          |
| Modify VSCode LSP client   | `vscode/src/extension.ts`                           |
| Modify Zed slash command   | `zed/src/lib.rs`                                    |
| Change Zed language scope  | `zed/extension.toml` → `language_servers`           |
| Update syntax highlighting | `zed/languages/github_actions/*.scm`                |

## Anti-Patterns

- **Never add these to the root Cargo workspace** — VSCode is TypeScript; Zed compiles to WASM with a separate target
- **Don't modify vendored `grammars/yaml/`** directly — it's a tree-sitter-yaml upstream copy
- **LSP binary must be pre-installed** — extensions locate `actup-lsp` at runtime, don't embed it

## Commands

Each extension has its own `justfile`, imported into root via `mod`:

```bash
just zed::check       # cargo check --target wasm32-wasip2
just zed::clippy      # cargo clippy (strict)
just zed::build       # cargo build (release WASM)
just vscode::lint     # bun run lint (ESLint)
just vscode::typecheck # bun run typecheck
just vscode::build    # lint + typecheck + compile
just vscode::package  # build + vsce package
just ext-check        # umbrella: zed::clippy + vscode::lint + vscode::typecheck
just ci-full          # full CI + ext-check
```

Zed recipes auto-install `wasm32-wasip2` target if missing.

## CI

`check-extensions` job in `ci.yml` runs on every push/PR. Soft-gated in `ci-success` (failure blocks, skip tolerated).

## Conventions

- Both extensions version-track with the main project (`0.1.0`)
- VSCode uses `bun` (not npm/yarn) for package management
- Zed extension has a commented-out `[context_servers.actup-mcp]` section for future MCP support
- Zed has its own `[lints]` in Cargo.toml (minimal set, not full workspace config)
