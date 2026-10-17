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

## Providers

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

**Status**: Not wired (#48). The crate implements `Provider`, but it exports no
`REGISTRATION`, and neither the CLI nor the LSP registers it. A `[hosts]` entry
with `kind = "gitlab"` stops every run with
`provider GitLab not compiled in (available: github)`.

**Crate features**:

- GitLab.com and self-hosted GitLab support
- Personal access tokens and project access tokens
- Automatic pagination
- Project path URL encoding (handles namespaces correctly)
- Base64 content decoding

**Planned configuration** (#48). `api_base` is the full API root:

```toml
[hosts."gitlab.com"]
kind      = "gitlab"
api_base  = "https://gitlab.com/api/v4"
token_env = "GITLAB_TOKEN"

[hosts."gitlab.example.com"]
kind      = "gitlab"
api_base  = "https://gitlab.example.com/api/v4"
token_env = "GITLAB_PRIVATE_TOKEN"
```

**Token**: Personal access token with `read_api` scope

**API Differences from GitHub**:

- Project paths must be URL-encoded (`my-org/my-repo` → `my-org%2Fmy-repo`)
- Auth header: `PRIVATE-TOKEN: {token}` (not `Authorization: Bearer`)
- Pagination: `x-next-page` header instead of Link header
- API versioned at `/api/v4`

### Gitea/Forgejo (`actup-provider-gitea`)

**Status**: Not wired (#48). The crate implements `Provider`, but it exports no
`REGISTRATION`, and neither the CLI nor the LSP registers it. A `[hosts]` entry
with `kind = "gitea"` stops every run with
`provider Gitea not compiled in (available: github)`.

**Crate features**:

- Gitea and Forgejo support (same API)
- API token authentication
- Automatic pagination
- Base64 content decoding
- Compatible with Gitea API v1

**Planned configuration** (#48). `api_base` is the full API root:

```toml
[hosts."gitea.example.com"]
kind      = "gitea"
api_base  = "https://gitea.example.com/api/v1"
token_env = "GITEA_TOKEN"

# Forgejo is a soft fork of Gitea with identical API
[hosts."forgejo.example.com"]
kind      = "gitea"  # Uses "gitea" kind
api_base  = "https://forgejo.example.com/api/v1"
token_env = "FORGEJO_TOKEN"
```

**Token**: API token from user settings

**API Differences from GitHub**:

- Auth header: `Authorization: token {token}` (lowercase "token")
- Pagination: `page` and `limit` query parameters (no header indicators)
- Stop pagination when empty array returned
- API versioned at `/api/v1`

## Host Routing

Actup routes each action reference to the provider registered for its host. The
default build registers GitHub only, so github.com and GitHub Enterprise hosts
resolve. GitLab and Gitea hosts resolve once #48 wires their providers.

```yaml
# .github/workflows/build.yml
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      # GitHub.com (default provider)
      - uses: actions/checkout@v4

      # GitHub Enterprise, with a `[hosts."github.enterprise.com"]` entry
      - uses: github.enterprise.com/internal/setup@v2
```

## Provider Selection Algorithm

1. **Extract host** from `uses:` reference
   - The first segment of the path before `@` names a host when the path has at
     least three segments and the segment is one of:
     - `localhost`
     - a dotted DNS name, such as `ghe.corp.com`
     - a DNS name with a `:port`, such as `localhost:3000` or `host:8443`
     - a bracketed IPv6 literal with an optional `:port`, such as `[::1]` or
       `[::1]:8080`
   - A DNS label holds ASCII letters, digits and `-`, and starts and ends with a
     letter or digit. A port is a number from 0 to 65535.
   - Actup lower-cases the host.
   - Owner and repo must match `[A-Za-z0-9._-]+` and must not be `.` or `..`.
   - Explicit: `uses: gitlab.com/owner/repo@ref` → host = "gitlab.com"
   - Implicit: `uses: owner/repo@ref` → host = `default_host` from config
   - Implicit: `uses: a.b/c@ref` has two segments, so `a.b` is the owner

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

`crates/actup` declares the features `github`, `gitlab` and `gitea`. The default
build enables `github` and is GitHub-only. All three features are placeholders.
`actup-provider-github` is a non-optional dependency of the CLI and the LSP, and
`gitlab` and `gitea` compile nothing until #48 wires them. The LSP declares no
provider features.

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

- **Unified CLI**: Same commands work for every registered provider
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
