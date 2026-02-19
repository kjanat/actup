# actup-lsp

Language Server Protocol implementation. Provides diagnostics, code actions, hover info, and completion for `uses:` refs in GitHub Actions
workflow files.

Binary crate (no `lib.rs`). All modules declared in `main.rs`. Uses `tower-lsp-server` for JSON-RPC transport over stdin/stdout.

## Module Map

| Module                  | Lines | Purpose                                        |
| ----------------------- | ----- | ---------------------------------------------- |
| `backend.rs`            | 1516  | `LspBackend` — core LSP trait impl, lifecycle  |
| `diagnostics.rs`        | 1103  | Diagnostic generation from `CheckResult`       |
| `completion.rs`         | 841   | Version/tag completion for `uses:` refs        |
| `hover.rs`              | 873   | Hover info: version status, README, action.yml |
| `workspace_commands.rs` | 685   | Workspace-level scan + update all              |
| `code_actions.rs`       | 625   | Quick-fix code actions (update, pin)           |
| `file_cache.rs`         | 456   | LSP-specific file-based cache                  |
| `error_ux.rs`           | 420   | User-friendly error messages                   |
| `provider_setup.rs`     | ~100  | Provider registry construction for LSP         |
| `position.rs`           | ~100  | Position/range conversion utilities            |
| `main.rs`               | 47    | Entry point, tracing to stderr, service setup  |

## Where to Look

| Task                       | Start Here                             |
| -------------------------- | -------------------------------------- |
| Change diagnostic severity | `diagnostics.rs`                       |
| Add new code action        | `code_actions.rs`                      |
| Modify hover content       | `hover.rs`                             |
| Add completion items       | `completion.rs`                        |
| Change workspace commands  | `workspace_commands.rs`                |
| Fix position calculation   | `position.rs`                          |
| Improve error messages     | `error_ux.rs`                          |
| Change LSP lifecycle       | `backend.rs` → `initialize`/`shutdown` |
| Modify cache behavior      | `file_cache.rs`                        |
| Wire new provider          | `provider_setup.rs`                    |

## Architecture Notes

- **Only supports GitHub provider** — no GitLab/Gitea wiring yet (unlike CLI)
- Tracing goes to **stderr** (stdout is LSP protocol channel)
- `DashMap` for concurrent document state
- `tokio::RwLock` for shared provider/config access
- Workspace root extracted from `workspace_folders` (or deprecated `root_uri`/`root_path` fallback)

## Anti-Patterns (This Crate)

- **Never write to stdout** except via LSP protocol (use `tracing` → stderr)
- **Never block the main loop**: All provider calls are async
- **Edits must be minimal**: Replace only `@ref` portion, preserve YAML formatting

## Conventions

- `#[allow(clippy::cast_possible_truncation)]` for LSP u32 position conversions
- `#[allow(deprecated)]` for backward-compat with `root_uri`/`root_path` LSP fields
- Diagnostics: Warning (outdated), Error (unresolvable), Info (network/rate-limited)

## Tests

- Integration tests in `src/integration_tests.rs` (972 lines, 20 tests) — `#[cfg(test)]` inside binary crate
- Mock provider in `src/test_utils.rs` — `MockProvider` with builder pattern
- Custom `TestHarness` wrapping `LspService` with background socket drain
- Tests full LSP protocol flow: initialize, didOpen, diagnostics, codeAction, hover, completion, executeCommand
- No `tests/` directory (tests are in-source `#[cfg(test)]` modules)

## Complexity Hotspots

`backend.rs` (1516 lines) and `diagnostics.rs` (1103 lines) are the most complex. `workspace_commands.rs` has both `too_many_lines` and
`too_many_arguments` lint suppressions.
