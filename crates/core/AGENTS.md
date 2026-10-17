# actup-core

Provider-neutral engine. No GitHub/GitLab/Gitea specifics allowed here.

## Module Map

| Module             | Purpose                                                   | Key Types                                              |
| ------------------ | --------------------------------------------------------- | ------------------------------------------------------ |
| `action_ref`       | Parse/normalize `uses:` strings                           | `ActionRef`, `ActionRepoId`                            |
| `parser`           | Parse `uses:` values, locate them in YAML, validate edits | `UsesLocation`, `parse_uses`, `extract_uses_from_yaml` |
| `syntax`           | `uses:` extraction per file format (YAML, Markdown, text) | `SourceFormat`                                         |
| `discovery`        | Find workflow/action files in filesystem                  | `DiscoveredFile`, `FileKind`, `ExtraPath`              |
| `patch`            | Line-relative edits planned onto the original bytes       | `FileEdits`, `PendingEdit`, `retarget_edit`            |
| `rewrite`          | Text written for an updated ref and its comment           | `Rewrite`, `Trailing`, `PinMarker`                     |
| `atomic_write`     | Crash-safe file replacement inside a root (native only)   | `write_atomic`, `rewrite_atomic`, `resolve_within`     |
| `provider`         | Provider trait (host-agnostic interface) and registry     | `Provider`, `Kind`, `Registry`                         |
| `caching_provider` | In-memory dedup wrapper around `Provider`                 | `CachingProvider`, `Retention`                         |
| `fetch`            | Progress reporting for batched version fetches            | `Progress`, `Outcome`, `Silent`                        |
| `version`          | Semver parsing, version sets, comparison                  | `Version`, `Set`, `Info`, `BranchInfo`                 |
| `policy`           | Update policy, pin style, tracking, branch handling       | `Update`, `Style`, `Track`, `Branch`                   |
| `resolution`       | Orchestrate version checks, compute status                | `CheckResult`, `CheckStatus`                           |
| `suggest`          | Closest-tag suggestions for refs that name no tag         | `closest_tag`                                          |
| `config/`          | Config loading, precedence, validation                    | `Config`, `ApiMode`                                    |
| `error`            | Structured errors (user vs environment)                   | `Error`, `Result`                                      |
| `cache`            | Cache trait, entries, entry file names                    | `Cache`, `Entry`, `Record`                             |
| `file_cache`       | Disk cache shared by CLI, LSP and MCP (native only)       | `FileCache`, `Access`                                  |
| `clock`            | Time source for TTL checks                                | `Clock`, `System`                                      |
| `compat`           | `Send`/`Sync` bounds and boxed futures for wasm           | `BoxFuture`, `MaybeSend`, `MaybeSync`                  |
| `text`             | Sanitize text for terminal and editor output              | `sanitize_for_terminal`, `sanitize_line`               |
| `url_path`         | Percent-encode the path and query parts of provider URLs  | `encode_path`, `encode_segment`                        |
| `yaml`             | Load YAML config and `action.yml` with the runner's rules | `Dialect`, `from_str`, `Error`                         |
| `yaml/`            | Unicode tables for `yaml`                                 | `SIMPLE_UPPER`                                         |

### Config Submodule (`config/`)

| File              | Purpose                                               |
| ----------------- | ----------------------------------------------------- |
| `mod.rs`          | `Config` struct, figment-based loading                |
| `registration.rs` | `Preset`/`Registration` types, duplicate-preset check |
| `resolution.rs`   | Host resolution, api_base inference, token_env        |
| `validation.rs`   | Hostname, api_base, config validation                 |
| `discovery.rs`    | Config file path search                               |
| `env.rs`          | `ACTUP_*` variables and the CLI flag variables        |
| `matching.rs`     | Action globs for `scan.ignore` and `[[overrides]]`    |

`config/resolution.rs` handles the full provider config resolution algorithm:
merging presets, inferring api_base, resolving token_env, validating host
configs.

### YAML Submodule (`yaml/`)

| File              | Purpose                                                        |
| ----------------- | -------------------------------------------------------------- |
| `simple_upper.rs` | Unicode 15.0 simple uppercase table for `Dialect::Runner` keys |

## Where to Look

| Task                    | Start Here                                                 |
| ----------------------- | ---------------------------------------------------------- |
| Parse `uses:` string    | `parser.rs` → `parse_uses`                                 |
| Add new ref format      | `parser.rs` → `parse_uses` + `action_ref.rs` → `ActionRef` |
| Implement new provider  | `provider.rs` → `Provider` trait                           |
| Change resolution logic | `resolution.rs` → `check_action`                           |
| Add policy type         | `policy.rs` → `Update` enum                                |
| Modify config schema    | `config/mod.rs` + root `schemas/`                          |
| Add provider preset     | `REGISTRATION` static in the provider crate's `lib.rs`     |
| Change host resolution  | `config/resolution.rs`                                     |
| Add config validation   | `config/validation.rs`                                     |

## Anti-Patterns (This Crate)

- **No provider-specific code**: No GitHub REST paths, GitLab API quirks, etc.
- **No hardcoded hosts**: `github.com` only appears in default config, never in
  logic
- **No HTTP client usage**: Providers fetch; core orchestrates
- **Disk cache only in `file_cache`**: `FileCache` (native targets) is the one
  storage impl; binaries build it with `FileCache::from_config`

## Conventions

- `Error::NotFound`/`InvalidRef` = user error (bad input)
- `Error::Network`/`RateLimited`/`Auth` = environment error (actionable)
- A foreign error goes in the variant's `source`; show errors with
  `error::chain`, which appends every cause
- Tests return `Result<(), Box<dyn std::error::Error>>` and use `?`; never
  suppress `unwrap_used` or `expect_used`
- `#![forbid(unsafe_code)]`
