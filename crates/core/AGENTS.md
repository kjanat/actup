# actup-core

Provider-neutral engine. No GitHub/GitLab/Gitea specifics allowed here.

## Module Map

| Module             | Purpose                                    | Key Types                        |
| ------------------ | ------------------------------------------ | -------------------------------- |
| `action_ref`       | Parse/normalize `uses:` strings            | `ActionRef`, `ActionRepoId`      |
| `parser`           | Extract `uses:` from YAML with locations   | `UsesLocation`, `extract_uses_*` |
| `discovery`        | Find workflow/action files in filesystem   | `DiscoveredFile`, `FileKind`     |
| `provider`         | Provider trait (host-agnostic interface)   | `Provider`, `ProviderKind`       |
| `caching_provider` | In-memory dedup wrapper around `Provider`  | `CachingProvider`, `CacheStats`  |
| `version`          | Semver parsing, version sets, comparison   | `Version`, `Info`, `BranchInfo`  |
| `policy`           | Update policy (patch/minor/major/latest)   | `Update`, policy matching        |
| `resolution`       | Orchestrate version checks, compute status | `CheckResult`, `CheckStatus`     |
| `config/`          | Config loading, precedence, validation     | `Config`, `ApiMode`              |
| `error`            | Structured errors (user vs environment)    | `Error`, `Result`                |
| `cache`            | Cache key/entry types (no storage impl)    | `CacheKey`, `CacheEntry`         |

## Where to Look

| Task                    | Start Here                           |
| ----------------------- | ------------------------------------ |
| Parse `uses:` string    | `action_ref.rs` → `ActionRef::parse` |
| Add new ref format      | `parser.rs` → regex + `UsesLocation` |
| Implement new provider  | `provider.rs` → `Provider` trait     |
| Change resolution logic | `resolution.rs` → `check_action`     |
| Add policy type         | `policy.rs` → `Update` enum          |
| Modify config schema    | `config/mod.rs` + root `schemas/`    |

## Anti-Patterns (This Crate)

- **No provider-specific code**: No GitHub REST paths, GitLab API quirks, etc.
- **No hardcoded hosts**: `github.com` only appears in default config, never in logic
- **No HTTP client usage**: Providers fetch; core orchestrates
- **No file I/O for cache**: Define interfaces only; CLI implements storage

## Conventions

- `Error::NotFound`/`InvalidRef` = user error (bad input)
- `Error::Network`/`RateLimit`/`Auth` = environment error (actionable)
- All public types re-exported in `lib.rs`
- Tests use `#[allow(clippy::unwrap_used)]` in `#[cfg(test)]` blocks

## Large Files (complexity hotspots)

| File            | Lines | Notes                                |
| --------------- | ----- | ------------------------------------ |
| `version.rs`    | 951   | Semver parsing, `VersionSet` logic   |
| `provider.rs`   | 889   | Provider trait + `ProviderKind` enum |
| `resolution.rs` | 791   | `check_action` orchestration         |
| `config/mod.rs` | 628   | Config struct + validation           |
