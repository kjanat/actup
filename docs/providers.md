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
    fn refresh_ref_to_sha(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, Result<Sha40>> {
        self.resolve_ref_to_sha(repo, ref_name)
    }
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
    fn search_actions(&self, _query: &str) -> BoxFuture<'_, Result<Search>> {
        Box::pin(async { Ok(Search::Unsupported) })
    }
    fn get_repo_state(&self, _repo: &ActionRepoId) -> BoxFuture<'_, Result<Option<RepoState>>> {
        Box::pin(async { Ok(None) })
    }
}
```

`refresh_ref_to_sha` resolves a ref without reusing a cached answer;
`CachingProvider` overrides it. `search_actions` answers LSP action-name
completion. GitHub searches `/search/repositories` with the `github-actions`
topic; GitLab and Gitea keep the `Search::Unsupported` default. `get_repo_state`
feeds the LSP `archived` hint. GitHub reports `archived` and `disabled`, GitLab
and Gitea report `archived`.

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

**Git listing**: tags and branches come from git `ls-refs` first. `github.com`
clones from `https://github.com`. An `api_base` ending in `/api/v3` clones from
the root before `/api/v3`. Any other host clones from `https://{host}`.

### GitLab (`actup-provider-gitlab`)

**Status**: Registered by the CLI (feature `gitlab`, on by default), the LSP and
the MCP server. `REGISTRATION` declares the `gitlab.com` preset
(`https://gitlab.com/api/v4`, `GITLAB_TOKEN`) and infers `https://{host}/api/v4`
for other hosts. GitLab supports the REST API only and uses it whatever
`[fetch] api_mode` prefers. `gitlab.com` works with zero config.

**Crate features**:

- GitLab.com and self-hosted GitLab support
- Personal access tokens and project access tokens
- Automatic pagination
- Project path URL encoding (handles namespaces correctly)
- Base64 content decoding

**Configuration**. `api_base` is the full API root:

```toml
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

**Status**: Registered by the CLI (feature `gitea`, on by default), the LSP and
the MCP server. `REGISTRATION` declares the `codeberg.org` preset
(`https://codeberg.org/api/v1`, `CODEBERG_TOKEN`), infers
`https://{host}/api/v1` for other hosts and reads `GITEA_TOKEN` there. Gitea
supports the REST API only and uses it whatever `[fetch] api_mode` prefers.
`codeberg.org` works with zero config.

**Crate features**:

- Gitea and Forgejo support (same API)
- API token authentication
- Automatic pagination
- Base64 content decoding
- Compatible with Gitea API v1

**Configuration**. `api_base` is the full API root:

```toml
[hosts."gitea.example.com"]
kind = "gitea"

# Forgejo is a soft fork of Gitea with identical API
[hosts."forgejo.example.com"]
kind      = "gitea"                               # Uses "gitea" kind
api_base  = "https://forgejo.example.com/api/v1"
token_env = "FORGEJO_TOKEN"
```

**Token**: API token from user settings

**API Differences from GitHub**:

- Auth header: `Authorization: token {token}` (lowercase "token")
- Pagination: `page` and `limit` query parameters (no header indicators)
- Stop pagination at a page shorter than `limit`
- A tag resolves through `/tags/{tag}` to its commit, then a branch through
  `/branches/{branch}`
- API versioned at `/api/v1`

## Host Routing

Actup routes each action reference to the provider registered for its host. The
registry holds the default host and every host that `[hosts]` names. GitHub,
GitLab and Gitea/Forgejo hosts resolve through their providers.

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
   - Validate required fields; a host's own `api_mode` must be one its provider
     supports, and the global `[fetch] api_mode` is only a preference

3. **Construct provider** via `(registration.create)(&config)`
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

`crates/actup` declares the features `gitlab` and `gitea`, both on by default.
They add `actup-provider-gitlab` and `actup-provider-gitea` and their
registrations. `actup-provider-github` is a non-optional dependency. The LSP and
the MCP server declare no provider features and register all three providers.

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
- **Preset hosts**: every compiled-in preset host (`github.com`, `gitlab.com`,
  `codeberg.org`) is registered without a `[hosts]` entry

## Next Steps

See the [Implementing Providers Guide](./implementing-providers.md) to create a
new provider!

Candidates for future providers:

- **Bitbucket**: REST API v2.0
- **Azure DevOps**: Azure Repos API
- **Sourcehut**: git.sr.ht API
- **Codeberg**: Uses Gitea/Forgejo (alias for gitea provider)
