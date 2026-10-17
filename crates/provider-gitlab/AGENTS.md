# actup-provider-gitlab

GitLab and self-hosted GitLab provider. Implements `actup-core::Provider`.

## Status

Exports `REGISTRATION` (`gitlab.com` preset, REST only). The CLI registers it
behind a default feature; the LSP and the MCP server always register it.

## Module Map

| Module        | Purpose                         | Key Types                          |
| ------------- | ------------------------------- | ---------------------------------- |
| `provider.rs` | `Provider` trait implementation | `GitLabProvider`                   |
| `client.rs`   | HTTP client for GitLab REST API | `GitLabClient` (427 lines)         |
| `types.rs`    | GitLab API response types       | `GitLabTag`, `GitLabProject`, etc. |
| `lib.rs`      | Constants + re-exports          | `GITLAB_HOST`                      |

## Where to Look

| Task                 | Start Here                 |
| -------------------- | -------------------------- |
| Change presets       | `REGISTRATION` in `lib.rs` |
| Add API endpoint     | `client.rs`                |
| Change auth handling | `client.rs`                |

## Conventions

- Encodes the project path, refs and file paths as single segments with
  `actup_core::encode_segment` (unlike GitHub)
- `#![forbid(unsafe_code)]`
- `tests/integration.rs`: `wiremock` tests for `REGISTRATION`, versions, ref
  resolution, files and every error status
- `api_base` is the full API root; the client appends no API version

## Anti-Patterns

- **Never hardcode `gitlab.com`** in logic — use constants from `lib.rs`
