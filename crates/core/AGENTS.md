# actup-core

Provider-neutral engine. No GitHub/GitLab/Gitea specifics allowed here.

## Module Map

| Module             | Purpose                                    | Key Types                        |
| ------------------ | ------------------------------------------ | -------------------------------- |
| `action_ref`       | Parse/normalize `uses:` strings            | `ActionRef`, `ActionRepoId`      |
| `parser`           | Extract `uses:` from YAML with locations   | `UsesLocation`, `extract_uses_*` |
| `discovery`        | Find workflow/action files in filesystem   | `DiscoveredFile`, `FileKind`     |
| `provider`         | Provider trait (host-agnostic interface)   | `Provider`, `ProviderKind`       |
| `caching_provider` | In-memory dedup wrapper around `Provider`  | `CachingProvider`, `Retention`   |
| `version`          | Semver parsing, version sets, comparison   | `Version`, `Info`, `BranchInfo`  |
| `policy`           | Update policy (patch/minor/major/latest)   | `Update`, policy matching        |
| `resolution`       | Orchestrate version checks, compute status | `CheckResult`, `CheckStatus`     |
| `config/`          | Config loading, precedence, validation     | `Config`, `ApiMode`              |
| `error`            | Structured errors (user vs environment)    | `Error`, `Result`                |
| `cache`            | Cache trait, entries, entry file names     | `Cache`, `Entry`, `Record`       |
| `file_cache`       | Disk cache shared by CLI, LSP and MCP      | `FileCache`                      |

### Config Submodule (`config/`)

| File              | Purpose                                          |
| ----------------- | ------------------------------------------------ |
| `mod.rs`          | `Config` struct, figment-based loading           |
| `registration.rs` | Provider `Registration` statics, preset matching |
| `resolution.rs`   | Host resolution, api_base inference, token_env   |
| `validation.rs`   | Hostname, api_base, config validation            |
| `discovery.rs`    | Config file path search                          |

`config/resolution.rs` handles the full provider config resolution algorithm:
merging presets, inferring api_base, resolving token_env, validating host
configs.

## Where to Look

| Task                    | Start Here                           |
| ----------------------- | ------------------------------------ |
| Parse `uses:` string    | `action_ref.rs` → `ActionRef::parse` |
| Add new ref format      | `parser.rs` → regex + `UsesLocation` |
| Implement new provider  | `provider.rs` → `Provider` trait     |
| Change resolution logic | `resolution.rs` → `check_action`     |
| Add policy type         | `policy.rs` → `Update` enum          |
| Modify config schema    | `config/mod.rs` + root `schemas/`    |
| Add provider preset     | `config/registration.rs`             |
| Change host resolution  | `config/resolution.rs`               |
| Add config validation   | `config/validation.rs`               |

## Anti-Patterns (This Crate)

- **No provider-specific code**: No GitHub REST paths, GitLab API quirks, etc.
- **No hardcoded hosts**: `github.com` only appears in default config, never in
  logic
- **No HTTP client usage**: Providers fetch; core orchestrates
- **Disk cache only in `file_cache`**: `FileCache` (native targets) is the one
  storage impl; binaries build it with `FileCache::from_config`

## Conventions

- `Error::NotFound`/`InvalidRef` = user error (bad input)
- `Error::Network`/`RateLimit`/`Auth` = environment error (actionable)
- A foreign error goes in the variant's `source`; show errors with
  `error::chain`, which appends every cause
- All public types re-exported in `lib.rs`
- Tests use `#[allow(clippy::unwrap_used)]` in `#[cfg(test)]` blocks
- `#![forbid(unsafe_code)]`

## Large Files (complexity hotspots)

| File                     | Notes                                    |
| ------------------------ | ---------------------------------------- |
| `config/resolution.rs`   | Host resolution, the most complex module |
| `config/mod.rs`          | Config struct + figment loading          |
| `version.rs`             | Semver parsing, `VersionSet` logic       |
| `config/registration.rs` | Provider presets + registration          |
| `provider.rs`            | Provider trait + `ProviderKind` enum     |
| `resolution.rs`          | `check_action` orchestration             |
| `config/validation.rs`   | Config validation rules                  |
| `policy.rs`              | Update policy matching                   |
| `error.rs`               | Structured error types                   |
| `action_ref.rs`          | ActionRef parsing                        |
| `caching_provider.rs`    | In-memory dedup with call counting       |
