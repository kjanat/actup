# actup-provider-gitea

Gitea and Forgejo provider. Implements `actup-core::Provider`.

## Status

Exports `REGISTRATION` (`codeberg.org` preset, REST only). The CLI registers it
behind a default feature; the LSP and the MCP server always register it.

## Module Map

| Module        | Purpose                         | Key Types                       |
| ------------- | ------------------------------- | ------------------------------- |
| `provider.rs` | `Provider` trait implementation | `GiteaProvider`                 |
| `client.rs`   | HTTP client for Gitea REST API  | `GiteaClient`                   |
| `types.rs`    | Gitea API response types        | `GiteaTag`, `GiteaRepo`, etc.   |
| `lib.rs`      | Constants + re-exports          | (no default host/URL constants) |

## Where to Look

| Task                 | Start Here                 |
| -------------------- | -------------------------- |
| Change presets       | `REGISTRATION` in `lib.rs` |
| Add API endpoint     | `client.rs`                |
| Change auth handling | `client.rs`                |

## Conventions

- `#![forbid(unsafe_code)]`
- `tests/integration.rs`: `wiremock` tests for `REGISTRATION`, versions, ref
  resolution, files and every error status
- `api_base` is the full API root; the client appends no API version
- Uses `pub use types::*` glob re-export (unlike GitLab which enumerates)

## Anti-Patterns

- **Never hardcode `codeberg.org`** in logic. Use the constants from `lib.rs`.
