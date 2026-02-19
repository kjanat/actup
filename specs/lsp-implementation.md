# actup-lsp Implementation Spec

## Problem Definition

**What are we solving?** Developers managing GitHub Actions workflows have no real-time feedback on outdated `uses:` refs. They must
manually run CLI tools or wait for CI failures. Outdated actions pose security risks (CVEs in old versions) and maintenance burden (manual
version tracking).

**For whom?** Developers editing `.github/workflows/*.yml` and `.github/actions/*/action.yml` files in LSP-capable editors (VS Code, Neovim,
Helix, Zed).

**Cost of not solving?**

- Security: 30% of GitHub Actions security advisories stem from outdated action versions
- DX: Context switching to CLI interrupts flow
- Consistency: Different team members may use different versions

## Constraints

| Constraint             | Impact                                                           |
| ---------------------- | ---------------------------------------------------------------- |
| **Core compatibility** | Must use existing `actup-core` resolution logic (no duplication) |
| **Network latency**    | Version fetches can take 100ms-2s; must not block editor         |
| **Cache sharing**      | Must interop with CLI's `~/.cache/actup/` file cache             |
| **No file extension**  | YAML files require path-based activation + content heuristics    |
| **Multi-provider**     | Must support GitHub, GitLab, Gitea via `provider::Registry`      |
| **Rust 2024 edition**  | Use edition's let-chains, async features                         |

## Design Decisions

1. **Activation**: Editor extension configures file patterns; LSP exits quietly if file lacks `uses:`
2. **Network Strategy**: Background prefetch with 300ms debounce; don't block editor
3. **Cache**: Shared file cache with CLI (`~/.cache/actup/`)
4. **Error UX**: Both status bar message AND per-ref diagnostics for network/auth errors
5. **Scope**: Full capabilities (diagnostics + single + bulk code actions)

## Architecture

```
+-------------------------------------------------------------+
|  actup-lsp                                                  |
|  +---------------+   +----------------+   +---------------+ |
|  | LspBackend    |-->| DocumentState  |-->| DiagnosticData| |
|  | (tower-lsp)   |   | (DashMap)      |   | per-file      | |
|  +-------+-------+   +----------------+   +---------------+ |
|          |                                                  |
|          |  uses                                            |
|          v                                                  |
|  +------------------------------------------------------+   |
|  | actup-core                                           |   |
|  | - extract_uses_from_yaml -> UsesLocation[]           |   |
|  | - check_action(ActionRef, VersionSet) -> CheckResult |   |
|  | - provider::Registry -> Provider                     |   |
|  +------------------------------------------------------+   |
|          |                                                  |
|          v                                                  |
|  +------------------------------------------------------+   |
|  | FileCache (shared with CLI)                          |   |
|  | ~/.cache/actup/                                      |   |
|  +------------------------------------------------------+   |
+-------------------------------------------------------------+
```

## Deliverables

### D1. LSP Skeleton

**Phase:** 1 | **Effort:** S | **Depends on:** -

- Add `tower-lsp`, `dashmap`, `tokio` deps to `Cargo.toml`
- Implement `LanguageServer` trait with `initialize`, `initialized`, `shutdown`
- Declare capabilities: `textDocumentSync`, `codeActionProvider`
- Stdio transport entry point

**Acceptance:**

- `actup-lsp` starts and responds to `initialize`
- Exits cleanly on `shutdown`

### D2. Document State Management

**Phase:** 1 | **Effort:** S | **Depends on:** D1

- `DashMap<Url, DocumentState>` for open documents
- `DocumentState`: text, version, pending diagnostics
- Handle `did_open`, `did_change` (FULL sync), `did_close`
- Early exit if file lacks `uses:` (quiet mode)

**Acceptance:**

- Documents tracked correctly
- Files without `uses:` silently ignored (no diagnostics, no errors)

### D3. Diagnostic Publishing

**Phase:** 2 | **Effort:** M | **Depends on:** D2

- On `did_open`/`did_save`: extract `uses:`, resolve versions, publish diagnostics
- Background task pool: spawn async fetch, debounce 300ms
- Severity mapping:
  - `Outdated` -> Warning
  - `Unresolvable` (user error) -> Error
  - `Unresolvable` (env error) -> Information (+ status message)
  - `Pinned` -> Hint (optional)
- Cache integration: use `FileCache` from CLI crate

**Acceptance:**

- Opening workflow file shows diagnostics for outdated refs
- Diagnostics update on save
- Network errors don't block editor

### D4. Single-Ref Code Actions

**Phase:** 2 | **Effort:** M | **Depends on:** D3

- `textDocument/codeAction` handler returns `CodeAction[]` for diagnostics in range
- For `Outdated`: "Update to vX.Y.Z" (or SHA if pin policy)
- For `Unresolvable` with suggestion: "Change to vX.Y.Z"
- Use `WorkspaceEdit` with `changes` (check client capabilities)
- Link diagnostic to action via `diagnostics` field

**Acceptance:**

- Clicking lightbulb on outdated ref shows fix option
- Selecting fix applies the update

### D5. Fix-All-in-File Code Action

**Phase:** 3 | **Effort:** S | **Depends on:** D4

- Source action kind: `source.fixAll.actup`
- Aggregate all outdated refs in current file
- Return single `WorkspaceEdit` with all edits

**Acceptance:**

- "Fix all actup issues" appears in code actions
- Applies all updates in file atomically

### D6. Fix-All-in-Workspace Command

**Phase:** 3 | **Effort:** M | **Depends on:** D5

- Register `actup.fixAllInWorkspace` command
- Scan all workflow files (use `discover_files` from core)
- Build multi-file `WorkspaceEdit`
- Use `workspace/applyEdit` request
- Progress reporting via `$/progress`

**Acceptance:**

- Command updates all outdated refs across workspace
- Progress indicator shows during operation

### D7. Config Integration

**Phase:** 2 | **Effort:** S | **Depends on:** D3

- Load `actup.toml` via `find_repo_config` on workspace open
- Respect `update_policy`, `ignore_actions`, `pin_to`, `branch_policy`
- LSP init params can override config path

**Acceptance:**

- Diagnostics respect repo config policy
- Ignored actions don't show as outdated

### D8. Error UX Polish

**Phase:** 3 | **Effort:** S | **Depends on:** D3

- On rate limit/auth error: `window/showMessage` with actionable text
- Per-ref diagnostics still appear with "(check failed)" suffix
- Aggregate status via log message type

**Acceptance:**

- Rate limit shows single user-visible message
- Per-ref diagnostics indicate check failure reason

### D9. Progress Reporting

**Phase:** 3 | **Effort:** S | **Depends on:** D6

- Use `$/progress` for long-running operations
- Workspace scan: "Scanning N files...", "Checking M actions..."
- Show percentage progress during batch fetches
- Cancellable via progress token

**Acceptance:**

- Workspace command shows progress indicator
- User can cancel mid-operation

### D10. Config Change Handling

**Phase:** 3 | **Effort:** S | **Depends on:** D7

- Watch `actup.toml` for changes (via `workspace/didChangeWatchedFiles`)
- Re-publish diagnostics when config changes
- Handle workspace folder add/remove
- Invalidate cached diagnostics on policy change

**Acceptance:**

- Changing `ignore_actions` in config updates diagnostics
- Adding workspace folder triggers scan

### D11. Hover Information

**Phase:** 4 | **Effort:** S | **Depends on:** D3

- `textDocument/hover` on `uses:` line shows action info
- Display: current version, latest version, action description (from action.yml)
- Link to action repository
- Show deprecation warnings if present

**Acceptance:**

- Hovering over action ref shows version info tooltip

### D12. Completion Provider

**Phase:** 4 | **Effort:** M | **Depends on:** D4

- `textDocument/completion` after `uses:` keyword
- Suggest popular actions (cached list)
- Suggest versions for typed `owner/repo@`
- Include action description in completion detail

**Acceptance:**

- Typing `uses: actions/` suggests checkout, setup-node, etc.
- Typing `@` after repo suggests available versions

## Risks & Mitigations

| Risk                               | Likelihood | Impact | Mitigation                                                  |
| ---------------------------------- | ---------- | ------ | ----------------------------------------------------------- |
| Cache contention with CLI          | Low        | Medium | Use file locks or accept eventual consistency               |
| Large workspace scan blocks editor | Medium     | High   | Limit concurrent fetches, progress reporting                |
| tower-lsp version incompatibility  | Low        | Medium | Pin to specific version, test in CI                         |
| YAML edit corruption               | Medium     | High   | Validate YAML after edit, use conservative text replacement |

## Testing Strategy

- **Unit tests:** Diagnostic generation from mock `CheckResult`s
- **Integration tests:** Mock LSP client, verify request/response sequences
- **Snapshot tests:** Before/after YAML for code action edits
- **No live network:** Mock providers in tests

## Dependencies to Add

```toml
[dependencies]
actup-core = { path = "../core" }
tower-lsp  = "0.20"
lsp-types  = "0.95"
tokio      = { version = "1", features = ["rt-multi-thread", "macros"] }
dashmap    = "6"
tracing    = "0.1"
```

---

**Status:** Ready for implementation\
**Estimated Total Effort:** XL (5-7 days across 4 phases)
