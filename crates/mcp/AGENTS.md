# actup-mcp

Model Context Protocol server for AI agents. **Currently a stub** — `main.rs` is a placeholder `println!("Hello, world!")`.

## Status

Scaffolding only. Zero dependencies. No runtime, no MCP protocol handling.

## Planned Purpose

Expose actup's check/update capabilities as MCP tools so AI coding agents can:

- Inspect `uses:` refs and their version status (read-only)
- Apply updates with diffs and reversal plans (mutators)
- Respect the same config/policy/provider rules as CLI and LSP

## Planned Tools (from root AGENTS.md spec)

| Tool                     | Type    | Purpose                          |
| ------------------------ | ------- | -------------------------------- |
| `actionGetReadme`        | read    | Fetch README for an action       |
| `actionGetActionYml`     | read    | Fetch action.yml metadata        |
| `getVersions`            | read    | List available versions          |
| `getLatest`              | read    | Get latest version per policy    |
| `updateFile`             | mutator | Apply edits with YAML validation |
| `updateAllUsesToVersion` | mutator | Batch update across workspace    |

## Where to Look

| Task                  | Start Here                                         |
| --------------------- | -------------------------------------------------- |
| Implement MCP server  | `main.rs` — replace stub with real bootstrap       |
| Design tool interface | Root `AGENTS.md` → "actup-mcp" section             |
| Reference impl        | `crates/lsp/` — similar provider/config wiring     |
| Provider setup        | `crates/lsp/src/provider_setup.rs` — reuse pattern |

## Anti-Patterns (This Crate)

See root `AGENTS.md` § "actup-mcp" for full tool principles (read purity, diff+reversal, idempotency).

- **Not a bypass** — must enforce same config/policy/provider rules as CLI/LSP
- **No direct file writes** — return edit plans; let the caller apply them

## Conventions

- `publish = false` — not published to crates.io
- `#![warn(missing_docs)]`
- When implementing: use `tokio` runtime, `tracing` for logs, structured errors
- All responses must include resolved provider+host and status enum
