# Provider Architecture

Actup's provider architecture demonstrates clean separation of concerns and
platform-agnostic design.

## Design Principles

### 1. Core Remains Platform-Neutral

`actup-core` contains ZERO platform-specific knowledge:

- ✅ Knows how to parse `uses: owner/repo@ref` syntax
- ✅ Knows version resolution policies (patch/minor/major)
- ✅ Knows how to plan YAML edits
- ❌ Does NOT know GitHub API paths
- ❌ Does NOT know GitLab's URL encoding requirements
- ❌ Does NOT know Gitea's pagination style

All platform details live in provider crates.

### 2. Provider Trait is Minimal but Complete

The `Provider` trait requires 7 methods and provides defaults for
`supports_batch_fetch` and `cache_namespace`:

```rust
pub trait Provider: MaybeSend + MaybeSync {
    fn kind(&self) -> Kind;
    fn supports_batch_fetch(&self) -> bool {
        false
    }
    fn host(&self) -> &str;
    fn cache_namespace(&self) -> &str {
        self.kind().cache_namespace()
    }
    fn get_versions(&self, repo: &ActionRepoId) -> BoxFuture<'_, Result<version::Set>>;
    fn resolve_ref_to_sha(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, Result<Sha40>>;
    fn get_default_branch(&self, repo: &ActionRepoId) -> BoxFuture<'_, Result<String>>;
    fn get_readme(
        &self,
        repo: &ActionRepoId,
        ref_name: Option<&str>,
    ) -> BoxFuture<'_, Result<Option<String>>>;
    fn get_action_yml(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, Result<ActionYml>>;
}
```

`BoxFuture` and the `MaybeSend`/`MaybeSync` bounds come from
`actup_core::compat`. They add `Send` and `Sync` on native targets and drop them
on `wasm32`.

This is the complete contract needed to support a new platform.

### 3. Shared Version Processing Logic

All providers share one version processing function in `actup-providers`. Each
provider implements `TagLike` for its tag response type and calls
`actup_providers::process_tags`:

```rust
use actup_core::version;
use actup_providers::{TagLike, process_tags};

impl TagLike for PlatformTag {
    fn name(&self) -> &str {
        &self.name
    }

    fn sha(&self) -> &str {
        &self.commit.sha
    }
}

let version_set: version::Set = process_tags(tags);
```

`process_tags` parses semver tags, tracks the latest release per major, detects
major-only tags (`v1`, `v2`) and builds the `version::Set`. GitHub, GitLab and
Gitea get identical version semantics from this one function.

## Supported Providers

### GitHub (`actup-provider-github`)

**Status**: ✅ Complete, battle-tested

**Features**:

- GitHub.com and GitHub Enterprise support
- GraphQL batch fetching (with `GITHUB_TOKEN`)
- REST API fallback for unauthenticated requests
- Parallel tag fetching
- ETag caching support

**Configuration**:

```toml
# github.com works out of the box with zero config (preset).
# For GitHub Enterprise, add a [hosts] entry:
[hosts."ghe.corp.com"]
kind      = "github"
api_base  = "https://ghe.corp.com/api/v3"
token_env = "GHE_TOKEN"
```

**Token**: `GITHUB_TOKEN` environment variable (optional, but recommended)

### GitLab (`actup-provider-gitlab`)

**Status**: ✅ Complete, ready for use

**Features**:

- GitLab.com and self-hosted GitLab support
- Personal access tokens and project access tokens
- Automatic pagination
- Project path URL encoding (handles namespaces correctly)
- Base64 content decoding

**Configuration**:

```toml
[hosts."gitlab.com"]
kind      = "gitlab"
token_env = "GITLAB_TOKEN"

[hosts."gitlab.example.com"]
kind      = "gitlab"
api_base  = "https://gitlab.example.com"
token_env = "GITLAB_PRIVATE_TOKEN"
```

**Token**: Personal access token with `read_api` scope

**API Differences from GitHub**:

- Project paths must be URL-encoded (`my-org/my-repo` → `my-org%2Fmy-repo`)
- Auth header: `PRIVATE-TOKEN: {token}` (not `Authorization: Bearer`)
- Pagination: `x-next-page` header instead of Link header
- API versioned at `/api/v4`

### Gitea/Forgejo (`actup-provider-gitea`)

**Status**: ✅ Complete, ready for use

**Features**:

- Gitea and Forgejo support (same API)
- API token authentication
- Automatic pagination
- Base64 content decoding
- Compatible with Gitea API v1

**Configuration**:

```toml
[hosts."gitea.example.com"]
kind      = "gitea"
api_base  = "https://gitea.example.com"
token_env = "GITEA_TOKEN"

# Forgejo is a soft fork of Gitea with identical API
[hosts."forgejo.example.com"]
kind      = "gitea"  # Uses "gitea" kind
api_base  = "https://forgejo.example.com"
token_env = "FORGEJO_TOKEN"
```

**Token**: API token from user settings

**API Differences from GitHub**:

- Auth header: `Authorization: token {token}` (lowercase "token")
- Pagination: `page` and `limit` query parameters (no header indicators)
- Stop pagination when empty array returned
- API versioned at `/api/v1`

## Multi-Provider Workflows

Actup automatically routes action references to the appropriate provider based
on the host:

```yaml
# .github/workflows/multi-platform.yml
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      # GitHub.com (default provider)
      - uses: actions/checkout@v4

      # GitHub Enterprise
      - uses: github.enterprise.com/internal/setup@v2

      # GitLab.com
      - uses: gitlab.com/my-org/deploy-action@v1.5.0

      # Self-hosted GitLab
      - uses: gitlab.example.com/team/build-action@v3

      # Gitea
      - uses: gitea.example.com/community/test@v2.1.0

      # Forgejo
      - uses: forgejo.example.com/tools/release@v4
```

**Running actup check:**

```bash
$ actup check
✓ actions/checkout@v4 (github.com) up to date
✓ github.enterprise.com/internal/setup@v2 up to date
⚠ gitlab.com/my-org/deploy-action@v1.5.0 → v1.6.0 available
✓ gitlab.example.com/team/build-action@v3 up to date
✓ gitea.example.com/community/test@v2.1.0 up to date
⚠ forgejo.example.com/tools/release@v4 → v5.0.0 available (major)

Scanned 6 actions: 4 up to date, 0 ignored, 2 outdated
```

## Provider Selection Algorithm

1. **Extract host** from `uses:` reference
   - Explicit: `uses: gitlab.com/owner/repo@ref` → host = "gitlab.com"
   - Implicit: `uses: owner/repo@ref` → host = `default_host` from config

2. **Resolve effective host config** via `effective_host(hostname)`
   - Check preset index (built from provider `REGISTRATION` statics)
   - Merge user `[hosts."hostname"]` overrides
   - Look up registration by `kind`
   - Fill defaults/inference (token_env, api_base)
   - Validate required fields and api_mode capability

3. **Construct provider** via `(registration.create)(config)`
   - Provider is registered in the host-to-provider registry
   - Provider knows its own API structure, auth, pagination, encoding
   - Provider returns data in actup-core types

## Adding a New Provider

See [Implementing Providers Guide](./implementing-providers.md) for step-by-step
instructions.

**Time estimate**: ~2-3 hours for a basic provider, including:

- Setting up crate structure (15 min)
- Defining API types (30 min)
- Implementing HTTP client (45 min)
- Implementing Provider trait (45 min)
- Adding tests (30 min)
- Documentation (15 min)

The provider architecture is designed to make this straightforward.

## Feature Flags

Providers beyond GitHub are feature-gated for minimal compile times:

```bash
# Default: all providers enabled
cargo build

# GitHub only:
cargo build --no-default-features --features github

# Specific providers:
cargo build --features "github gitlab"
```

## Provider Quality Checklist

When implementing or reviewing a provider:

- [ ] All Error variants use correct field names (owner/repo/host, not
      resource/provider)
- [ ] Tag type implements `TagLike` and `get_versions` calls
      `actup_providers::process_tags`
- [ ] `get_action_yml` calls `actup_providers::parse_action_yml`
- [ ] Async trait methods extract data from params before `async move`
- [ ] SHAs validated with `Sha40::new()` and errors handled
- [ ] Rate limiting detected and returned as `Error::RateLimited`
- [ ] Auth errors provide helpful messages
- [ ] Pagination implemented correctly (all tags fetched)
- [ ] Base64 decoding handles newlines/whitespace
- [ ] `REGISTRATION` static exported with presets, `infer_api_base`, and
      capabilities
- [ ] `REGISTRATION` wired into `available_registrations()` in the CLI and LSP
- [ ] Tests cover happy path and error cases
- [ ] Documentation includes usage examples
- [ ] Follows Rust API guidelines (C-CASE, C-CONV, etc.)

## Architecture Benefits

### For Users

- **Unified CLI**: Same commands work across all platforms
- **Consistent behavior**: Version resolution works identically everywhere
- **Easy migration**: Switch platforms without changing workflows

### For Contributors

- **Clear contracts**: Provider trait defines exact requirements
- **Reusable patterns**: Copy from existing providers
- **Good examples**: Three reference implementations (GitHub, GitLab, Gitea)
- **Comprehensive docs**: Step-by-step guide with gotchas highlighted

### For Maintainers

- **Core stability**: Platform changes don't affect core
- **Independent versioning**: Providers can evolve separately
- **Feature flags**: Users only compile what they need
- **Testability**: Mock providers for testing core logic

## Migration from Old Config Format

The provider configuration was refactored from `[providers]` with
`[[providers.instances]]` to the current `[fetch]` + `[hosts."hostname"]`
structure.

| Old                             | New                                  |
| ------------------------------- | ------------------------------------ |
| `[providers].api_mode`          | `[fetch].api_mode`                   |
| `[providers].max_tags_per_repo` | `[fetch].max_tags_per_repo`          |
| `[providers].default_host`      | `default_host` (top-level)           |
| `[providers].default_kind`      | Removed (inferred from registration) |
| `[[providers.instances]]`       | `[hosts."hostname"]`                 |
| Instance `host` field           | Key of `[hosts]` table               |

Key differences:

- **Zero-config for preset hosts**: github.com works out of the box without any
  config entry (provider preset supplies all defaults)
- **Hostname as key**: No more `host = "..."` field inside instances; the
  hostname is the TOML table key
- **Flat structure**: `[hosts."ghe.corp.com"]` instead of deeply nested
  `[[providers.instances]]`
- **Per-host fetch overrides**: `api_mode` and `max_tags_per_repo` can be set
  per-host in `[hosts]`, overriding `[fetch]` globals

## Next Steps

See the [Implementing Providers Guide](./implementing-providers.md) to create a
new provider!

Candidates for future providers:

- **Bitbucket**: REST API v2.0
- **Azure DevOps**: Azure Repos API
- **Sourcehut**: git.sr.ht API
- **Codeberg**: Uses Gitea/Forgejo (alias for gitea provider)
