# actup-provider-gitea

Gitea and Forgejo provider. Implements `actup-core::Provider`.

## Status

Functional provider implementation but **not yet wired into CLI/LSP** — no
`REGISTRATION` static exported. The CLI has commented-out feature-gated support
ready for when registration is added.

## Module Map

| Module        | Purpose                         | Key Types                       |
| ------------- | ------------------------------- | ------------------------------- |
| `provider.rs` | `Provider` trait implementation | `GiteaProvider`                 |
| `client.rs`   | HTTP client for Gitea REST API  | `GiteaClient`                   |
| `types.rs`    | Gitea API response types        | `GiteaTag`, `GiteaRepo`, etc.   |
| `lib.rs`      | Constants + re-exports          | (no default host/URL constants) |

## Where to Look

| Task                 | Start Here                                                                  |
| -------------------- | --------------------------------------------------------------------------- |
| Wire into CLI/LSP    | Add `REGISTRATION` static to `lib.rs`, then enable feature in CLI `main.rs` |
| Add API endpoint     | `client.rs`                                                                 |
| Change auth handling | `client.rs`                                                                 |

## Conventions

- `#![forbid(unsafe_code)]`
- `wiremock` as dev-dep but no tests written yet
- Uses `pub use types::*` glob re-export (unlike GitLab which enumerates)

## Anti-Patterns

- **No default host constants defined** — needs adding before registration can
  work
