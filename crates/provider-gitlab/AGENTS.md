# actup-provider-gitlab

GitLab and self-hosted GitLab provider. Implements `actup-core::Provider`.

## Status

Functional provider implementation but **not yet wired into CLI/LSP** — no `REGISTRATION` static exported. The CLI has commented-out
feature-gated support ready for when registration is added.

## Module Map

| Module        | Purpose                         | Key Types                          |
| ------------- | ------------------------------- | ---------------------------------- |
| `provider.rs` | `Provider` trait implementation | `GitLabProvider`                   |
| `client.rs`   | HTTP client for GitLab REST API | `GitLabClient` (427 lines)         |
| `types.rs`    | GitLab API response types       | `GitLabTag`, `GitLabProject`, etc. |
| `lib.rs`      | Constants + re-exports          | `GITLAB_HOST`                      |

## Where to Look

| Task                 | Start Here                                                                  |
| -------------------- | --------------------------------------------------------------------------- |
| Wire into CLI/LSP    | Add `REGISTRATION` static to `lib.rs`, then enable feature in CLI `main.rs` |
| Add API endpoint     | `client.rs`                                                                 |
| Change auth handling | `client.rs`                                                                 |

## Conventions

- Uses `urlencoding` for GitLab project path encoding (unlike GitHub)
- `#![forbid(unsafe_code)]`
- `insta` + `wiremock` as dev-deps but no tests written yet

## Anti-Patterns

- **Never hardcode `gitlab.com`** in logic — use constants from `lib.rs`
