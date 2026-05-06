# actup-provider-github

GitHub and GitHub Enterprise provider. Implements `actup-core::Provider`.

Only provider that exports a `REGISTRATION` static for the provider registry
system.

## Module Map

| Module        | Purpose                                | Key Types                         |
| ------------- | -------------------------------------- | --------------------------------- |
| `lib.rs`      | `REGISTRATION` static + `create` fn    | `REGISTRATION`, preset for GH/GHE |
| `provider.rs` | `Provider` trait implementation        | `GitHubProvider`                  |
| `client.rs`   | HTTP client, auth, rate limit handling | `GitHubClient`, response parsing  |
| `graphql.rs`  | GraphQL queries for batch fetching     | `BatchQuery`, tag fetching        |
| `batch.rs`    | Multi-repo batching orchestration      | `BatchFetcher`, parallelism       |
| `types.rs`    | GitHub API response types              | `GitHubTag`, `GitHubRepo`, etc.   |

## Where to Look

| Task                       | Start Here                        |
| -------------------------- | --------------------------------- |
| Add new API endpoint       | `client.rs` → add method          |
| Change GraphQL query       | `graphql.rs`                      |
| Modify rate limit handling | `client.rs` → `handle_response`   |
| Add batch operation        | `batch.rs`                        |
| Change auth header         | `client.rs` → `GitHubClient::new` |
| Modify provider preset     | `lib.rs` → `REGISTRATION`         |

## Performance: Batch Fetching

1. **GraphQL mode** (requires `GITHUB_TOKEN`): Single query for all repos
2. **REST fallback**: Parallel requests via `tokio::spawn`
3. **In-memory dedup**: `CachingProvider` wraps this provider

## Anti-Patterns (This Crate)

- **Never use generic error fields**: Use provider-specific error types
- **Always validate SHAs**: Use `Sha40::new()` from `actup-core`
- **Never log tokens**: Auth header set but never printed
- **Rate limit detection**: Check header VALUES not just presence
  (`x-ratelimit-remaining == "0"`)

## Conventions

- Rate limit info parsed from `X-RateLimit-*` headers
- ETag support for conditional requests
- Timeouts: 30s default, configurable
- User-Agent: `actup/{version}`
- `#![forbid(unsafe_code)]`

## API Endpoints Used

| Endpoint                           | Purpose                    |
| ---------------------------------- | -------------------------- |
| `GET /repos/{owner}/{repo}/tags`   | List tags                  |
| `GET /repos/{owner}/{repo}`        | Repo info (default branch) |
| `POST /graphql`                    | Batch tag fetching         |
| `GET /repos/{owner}/{repo}/readme` | README content             |

## Tests

- Mock HTTP with `wiremock` in `tests/integration.rs` (5 tests)
- Fixtures in `tests/fixtures/` (JSON API responses)
- `insta` listed as dev-dependency but no `.snap` files yet
- Never rely on live network in tests
