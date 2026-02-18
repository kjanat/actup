# actup-provider-github

GitHub and GitHub Enterprise provider. Implements `actup-core::Provider`.

## Module Map

| Module        | Purpose                                | Key Types                        |
| ------------- | -------------------------------------- | -------------------------------- |
| `provider.rs` | `Provider` trait implementation        | `GitHubProvider`                 |
| `client.rs`   | HTTP client, auth, rate limit handling | `GitHubClient`, response parsing |
| `graphql.rs`  | GraphQL queries for batch fetching     | `BatchQuery`, tag fetching       |
| `batch.rs`    | Multi-repo batching orchestration      | `BatchFetcher`, parallelism      |
| `types.rs`    | GitHub API response types              | `GitHubTag`, `GitHubRepo`, etc.  |

## Where to Look

| Task                       | Start Here                        |
| -------------------------- | --------------------------------- |
| Add new API endpoint       | `client.rs` → add method          |
| Change GraphQL query       | `graphql.rs`                      |
| Modify rate limit handling | `client.rs` → `handle_response`   |
| Add batch operation        | `batch.rs`                        |
| Change auth header         | `client.rs` → `GitHubClient::new` |

## Performance: Batch Fetching

1. **GraphQL mode** (requires `GITHUB_TOKEN`): Single query for all repos
2. **REST fallback**: Parallel requests via `tokio::spawn`
3. **In-memory dedup**: `CachingProvider` wraps this provider

GraphQL batching in `graphql.rs:583` lines - most complex file.

## Anti-Patterns (This Crate)

- **Never use generic error fields**: Use provider-specific error types
- **Always validate SHAs**: Use `Sha40::new()` from `actup-core`
- **Never log tokens**: Auth header set but never printed

## Conventions

- Rate limit info parsed from `X-RateLimit-*` headers
- ETag support for conditional requests
- Timeouts: 30s default, configurable
- User-Agent: `actup/{version}`

## API Endpoints Used

| Endpoint                           | Purpose                    |
| ---------------------------------- | -------------------------- |
| `GET /repos/{owner}/{repo}/tags`   | List tags                  |
| `GET /repos/{owner}/{repo}`        | Repo info (default branch) |
| `POST /graphql`                    | Batch tag fetching         |
| `GET /repos/{owner}/{repo}/readme` | README content             |

## Tests

- Mock HTTP with `wiremock` in `tests/integration.rs`
- Fixtures in `tests/fixtures/` (JSON API responses)
- Never rely on live network in tests
