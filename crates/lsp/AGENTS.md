# actup-lsp

Language Server Protocol implementation. Provides diagnostics, code actions,
hover info, and completion for `uses:` refs in GitHub Actions workflow files.

Binary crate (no `lib.rs`). All modules declared in `main.rs`. Uses
`tower-lsp-server` for JSON-RPC transport over stdin/stdout.

## Module Map

| Module                  | Purpose                                         |
| ----------------------- | ----------------------------------------------- |
| `backend.rs`            | `LspBackend` — core LSP trait impl, lifecycle   |
| `diagnostics.rs`        | Diagnostic generation from `CheckResult`        |
| `completion.rs`         | Action-name and version completion for `uses:`  |
| `hover.rs`              | Hover info: version status, README, action.yml  |
| `workspace_commands.rs` | Workspace-level scan + update all               |
| `code_actions.rs`       | Quick-fix code actions (update, pin)            |
| `file_cache.rs`         | Version lookup through the shared `FileCache`   |
| `error_ux.rs`           | User-friendly error messages                    |
| `provider_setup.rs`     | Provider registry construction for LSP          |
| `position.rs`           | Position/range conversion utilities             |
| `test_utils.rs`         | `MockProvider` with builder pattern (test-only) |
| `integration_tests.rs`  | Full LSP protocol flow tests (test-only)        |
| `main.rs`               | Entry point, tracing to stderr, service setup   |

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
| Modify cache behavior      | `actup-core` `file_cache.rs`           |
| Wire new provider          | `provider_setup.rs`                    |

## Code Actions

| Action                   | Trigger                          | Implementation                                               |
| ------------------------ | -------------------------------- | ------------------------------------------------------------ |
| Update single ref        | `Outdated` diagnostic            | `code_actions::code_actions_for_diagnostics`                 |
| Pin branch or tag        | `PinBranch` diagnostic           | `code_actions::code_actions_for_diagnostics`                 |
| Fix all in file          | 2+ outdated or pin-branch refs   | `code_actions::fix_all_in_file` (`source.fixAll.actup`)      |
| Suggest for unresolvable | `Unresolvable` + `suggested_ref` | `code_actions::code_actions_for_diagnostics` (not yet wired) |
| Expand short SHA         | Short SHA `Unresolvable`         | `code_actions::code_actions_for_diagnostics`                 |
| Workspace update all     | Command palette                  | `workspace_commands::fix_all_workspace`                      |

`code_actions::DocumentText` checks every action edit against the open document
text with `SourceFormat::validate_edit` in the document's format and leaves out
an edit that breaks the YAML or changes the `uses:` count. Fix-all keeps the
edits that pass on their own.

## Architecture Notes

- `provider_setup::available_registrations` registers GitHub, GitLab and Gitea
- Tracing goes to **stderr** (stdout is LSP protocol channel)
- `DashMap` for concurrent document state
- `tokio::RwLock` for shared provider/config access
- Workspace root extracted from `workspace_folders` (or deprecated
  `root_uri`/`root_path` fallback)

## Anti-Patterns (This Crate)

- **Never write to stdout** except via LSP protocol (use `tracing` → stderr)
- **Never block the main loop**: All provider calls are async
- **Edits must be minimal**: Replace only `@ref` portion, preserve YAML
  formatting

## Conventions

- LSP `u32` positions use `u32::try_from`
- `#[allow(deprecated)]` for backward-compat with `root_uri`/`root_path` LSP
  fields
- Diagnostics: Warning (outdated), Error (unresolvable), Info
  (network/rate-limited)
- Send `window/showMessage` and `window/logMessage` through
  `error_ux::show_message` and `error_ux::log_message`, which apply
  `actup_core::sanitize_for_terminal`

## Tests

- Integration tests in `src/integration_tests.rs` — `#[cfg(test)]` inside binary
  crate
- Mock provider in `src/test_utils.rs` — `MockProvider` with builder pattern
- `actup_test_support::logged` captures `tracing` output in unit tests
- Custom `TestHarness` wrapping `LspService` with background socket drain
- Test backends use
  `LspBackend::with_providers(.., test_utils::MOCK_PROVIDERS)`: mock providers
  and a disabled file cache, with no network and no user cache
- Tests full LSP protocol flow: initialize, didOpen, diagnostics, codeAction,
  hover, completion, executeCommand
- No `tests/` directory (tests are in-source `#[cfg(test)]` modules)

## Complexity Hotspots

`backend.rs` and `diagnostics.rs` are the most complex modules.
