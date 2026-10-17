# Implementing Providers for Actup

This guide walks through creating a new provider for actup, using GitLab and
Gitea as reference examples.

## Overview

Providers implement the `Provider` trait from `actup-core`, enabling actup to
work with different Git hosting platforms (GitHub, GitLab, Gitea, Forgejo,
etc.). The provider architecture is designed to keep `actup-core` completely
platform-agnostic.

## Quick Start

### 1. Create Crate Structure

```bash
mkdir -p crates/provider-yourplatform/{src,tests}
```

Create `Cargo.toml`:

```toml
[package]
name              = "actup-provider-yourplatform"
version.workspace = true
edition.workspace = true
description       = "YourPlatform provider for actup"
# ... other metadata

[dependencies]
actup-core      = { workspace = true }
actup-providers = { workspace = true }
base64          = { workspace = true }
reqwest         = { workspace = true }
serde           = { workspace = true }
serde_json      = { workspace = true }
tokio           = { workspace = true }
tracing         = { workspace = true }

[lints]
workspace = true
```

### 2. Define API Response Types

Create `src/types.rs` with structs matching your platform's API responses:

```rust
use actup_providers::TagLike;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YourTag {
    pub name: String,
    pub commit: YourCommit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YourCommit {
    pub sha: String, // Must be 40-char hex
}

impl TagLike for YourTag {
    fn name(&self) -> &str {
        &self.name
    }

    fn sha(&self) -> &str {
        &self.commit.sha
    }
}

// ... other types for repos, branches, files, etc.
```

Implement `actup_providers::TagLike` for your tag type. `sha()` must return the
commit SHA the tag points to. For annotated tags, return the peeled commit SHA.

### 3. Implement HTTP Client

Create `src/client.rs`:

```rust
use std::{num::NonZeroU8, sync::Arc, time::Duration};

use actup_core::{Error, Result};
use reqwest::{
    Client, Response, StatusCode,
    header::{HeaderMap, HeaderValue, USER_AGENT},
};

use crate::types::YourTag;

pub struct YourClient {
    client: Client,
    api_base: String,
    host: String,
}

impl YourClient {
    pub fn new(api_base: &str, host: &str, token: Option<&str>) -> Result<Self> {
        // Set up headers (auth, user-agent, etc.)
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("actup/0.1"));

        if let Some(t) = token {
            // Add auth header appropriate for your platform
            headers.insert(
                "Authorization",
                HeaderValue::from_str(&format!("Bearer {t}")).map_err(|e| Error::Auth {
                    provider: "yourplatform".to_string(),
                    reason: "invalid token format".to_string(),
                    source: Some(Arc::new(e)),
                })?,
            );
        }

        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Network {
                operation: "create HTTP client".to_string(),
                message: String::new(),
                source: Some(Arc::new(e)),
            })?;

        // `api_base` is the full API root from `ProviderConfig`, e.g. `https://host/api/v1`
        Ok(Self {
            client,
            api_base: api_base.trim_end_matches('/').to_string(),
            host: host.to_string(),
        })
    }

    /// The first `max_tags` tags, newest first.
    pub async fn list_tags(
        &self,
        owner: &str,
        repo: &str,
        max_tags: NonZeroU8,
    ) -> Result<Vec<YourTag>> {
        // Page through your API and stop once `max_tags` tags are collected
        let url = format!(
            "{}/repos/{}/{}/tags?limit={max_tags}",
            self.api_base,
            actup_core::encode_path(owner)?,
            actup_core::encode_path(repo)?
        );
        let response = self.get(&url).await?;
        let tags: Vec<YourTag> = self.handle_response(response, owner, repo).await?;
        Ok(tags.into_iter().take(usize::from(max_tags.get())).collect())
    }

    // `get_repo`, `get_branch`, `resolve_ref` and `get_file` follow the same pattern

    /// Helper method to perform GET requests with configured headers
    async fn get(&self, url: &str) -> Result<Response> {
        self.client
            .get(url)
            .send()
            .await
            .map_err(|e| Error::Network {
                operation: "HTTP GET".to_string(),
                message: String::new(),
                source: Some(Arc::new(e)),
            })
    }

    async fn handle_response<T: serde::de::DeserializeOwned>(
        &self,
        response: Response,
        owner: &str,
        repo: &str,
    ) -> Result<T> {
        match response.status() {
            StatusCode::OK => {
                let text = response.text().await.map_err(|e| Error::Network {
                    operation: "read response".to_string(),
                    message: String::new(),
                    source: Some(Arc::new(e)),
                })?;

                serde_json::from_str(&text).map_err(|e| Error::Network {
                    operation: "parse response".to_string(),
                    message: "JSON parse error".to_string(),
                    source: Some(Arc::new(e)),
                })
            }
            StatusCode::NOT_FOUND => Err(Error::NotFound {
                owner: owner.to_string(),
                repo: repo.to_string(),
                host: Some(self.host.clone()),
            }),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(Error::Auth {
                provider: "yourplatform".to_string(),
                reason: "authentication required".to_string(),
                source: None,
            }),
            StatusCode::TOO_MANY_REQUESTS => Err(Error::RateLimited {
                provider: "yourplatform".to_string(),
                retry_after: response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(Duration::from_secs),
            }),
            _ => Err(Error::Network {
                operation: format!("GET {}", response.url()),
                message: format!("HTTP {}", response.status()),
                source: None,
            }),
        }
    }
}
```

### 4. Implement Provider Trait

Create `src/provider.rs`:

```rust
use std::num::NonZeroU8;

use actup_core::{
    ActionRepoId, Error, Result,
    compat::BoxFuture,
    provider::{ActionYml, Kind, Provider, Sha40},
    version,
};
use actup_providers::{filename_label, parse_action_yml, process_tags};

use crate::client::YourClient;

/// `YourPlatform` implementation of [`Provider`].
pub struct YourProvider {
    client: YourClient,
    host: String,
    max_tags: NonZeroU8,
}

impl YourProvider {
    /// Creates a provider for the API root `api_base` serving `host`.
    ///
    /// # Errors
    ///
    /// Returns an error if the token is not a valid header value or the HTTP
    /// client cannot be built.
    pub fn new(api_base: &str, host: &str, token: Option<&str>) -> Result<Self> {
        Ok(Self {
            client: YourClient::new(api_base, host, token)?,
            host: host.to_string(),
            max_tags: actup_core::config::DEFAULT_MAX_TAGS_PER_REPO,
        })
    }

    /// Fetch at most `max_tags` tags per repository.
    #[must_use]
    pub const fn max_tags(mut self, max_tags: NonZeroU8) -> Self {
        self.max_tags = max_tags;
        self
    }
}

impl Provider for YourProvider {
    fn kind(&self) -> Kind {
        Kind::YourPlatform // Add to Kind enum in actup-core first
    }

    fn host(&self) -> &str {
        &self.host
    }

    fn get_versions(&self, repo: &ActionRepoId) -> BoxFuture<'_, Result<version::Set>> {
        // IMPORTANT: Extract all needed data from `repo` before async block
        let owner = repo.owner().to_string();
        let repo_name = repo.repo().to_string();

        Box::pin(async move {
            let tags = self
                .client
                .list_tags(&owner, &repo_name, self.max_tags)
                .await?;
            let mut version_set = process_tags(tags);

            // Get default branch
            let repo_info = self.client.get_repo(&owner, &repo_name).await?;
            let branch = self
                .client
                .get_branch(&owner, &repo_name, &repo_info.default_branch)
                .await?;

            version_set.set_default_branch(version::BranchInfo::new(
                repo_info.default_branch,
                branch.commit.sha,
            ));

            Ok(version_set)
        })
    }

    fn resolve_ref_to_sha(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, Result<Sha40>> {
        let owner = repo.owner().to_string();
        let repo_name = repo.repo().to_string();
        let ref_name = ref_name.to_string();

        Box::pin(async move {
            let commit_sha = self
                .client
                .resolve_ref(&owner, &repo_name, &ref_name)
                .await?;

            Sha40::new(&commit_sha).ok_or_else(|| Error::Internal {
                context: format!("commit SHA from API: {commit_sha}"),
                message: "expected 40-character hex string".to_string(),
                source: None,
            })
        })
    }

    fn get_default_branch(&self, repo: &ActionRepoId) -> BoxFuture<'_, Result<String>> {
        let owner = repo.owner().to_string();
        let repo_name = repo.repo().to_string();

        Box::pin(async move {
            let repo_info = self.client.get_repo(&owner, &repo_name).await?;
            Ok(repo_info.default_branch)
        })
    }

    fn get_readme(
        &self,
        repo: &ActionRepoId,
        ref_name: Option<&str>,
    ) -> BoxFuture<'_, Result<Option<String>>> {
        let owner = repo.owner().to_string();
        let repo_name = repo.repo().to_string();
        let ref_name = ref_name.unwrap_or("HEAD").to_string();

        Box::pin(async move {
            // Try common README filenames
            for filename in ["README.md", "readme.md", "README"] {
                match self
                    .client
                    .get_file(&owner, &repo_name, filename, &ref_name)
                    .await
                {
                    Ok(content) => {
                        // Decode base64 content - propagate decode errors
                        let text = decode_base64_content(&content)?;
                        return Ok(Some(text));
                    }
                    Err(Error::NotFound { .. }) => {} // Try next filename
                    Err(e) => return Err(e),          // Propagate auth/rate-limit/network errors
                }
            }
            Ok(None)
        })
    }

    fn get_action_yml(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, Result<ActionYml>> {
        // Extract data before async block (crucial for lifetimes!)
        let owner = repo.owner().to_string();
        let repo_name = repo.repo().to_string();
        let ref_name = ref_name.to_string();
        let subpath = repo.subpath().map(ToString::to_string);

        Box::pin(async move {
            let dir = subpath.map(|p| format!("{p}/")).unwrap_or_default();

            // GitHub accepts both extensions; try action.yml first
            for path in [format!("{dir}action.yml"), format!("{dir}action.yaml")] {
                match self
                    .client
                    .get_file(&owner, &repo_name, &path, &ref_name)
                    .await
                {
                    Ok(content) => {
                        let raw = decode_base64_content(&content)?;
                        return parse_action_yml(&raw, filename_label(&path));
                    }
                    Err(Error::NotFound { .. }) => {} // Try action.yaml
                    Err(e) => return Err(e),          // Propagate auth/rate-limit/network errors
                }
            }

            Err(Error::NotFound {
                owner,
                repo: repo_name,
                host: Some(self.host.clone()),
            })
        })
    }
}
```

### 5. Helper Functions

Add to `src/provider.rs`:

```rust
use std::sync::Arc;

use base64::{Engine, engine::general_purpose::STANDARD};

/// Decode base64 file content, ignoring the line breaks GitHub inserts
fn decode_base64_content(content: &str) -> Result<String> {
    let compact: String = content
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let bytes = STANDARD.decode(compact).map_err(|e| Error::Internal {
        context: "base64 decode".to_string(),
        message: String::new(),
        source: Some(Arc::new(e)),
    })?;
    String::from_utf8(bytes).map_err(|e| Error::Internal {
        context: "UTF-8 decode".to_string(),
        message: String::new(),
        source: Some(Arc::new(e)),
    })
}
```

Parse `action.yml` content with the shared
`actup_providers::parse_action_yml(raw, path)`. Pass the loaded filename as
`path` through `actup_providers::filename_label`. Parse errors name that file.

### 6. Declare Modules and Export REGISTRATION

`src/lib.rs` declares the modules, re-exports `YourProvider`, and exports a
static `REGISTRATION` that supplies presets, inference logic, and a factory
function. This enables zero-config for preset hosts and automatic `api_base`
inference for self-hosted instances.

```rust
//! `YourPlatform` provider for actup.

mod client;
mod provider;
mod types;

use actup_core::{Capabilities, HostKind, Preset, Provider, ProviderConfig, Registration};
pub use provider::YourProvider;

/// Provider registration for `YourPlatform`.
pub static REGISTRATION: Registration = Registration {
    kind: HostKind::YourPlatform,
    presets: &[Preset {
        host: "yourplatform.com",
        api_base: "https://api.yourplatform.com",
        token_env: "YOURPLATFORM_TOKEN",
    }],
    default_token_env: Some("YOURPLATFORM_TOKEN"),
    infer_api_base: |host| {
        if host == "yourplatform.com" {
            None // Preset already covers this
        } else {
            Some(format!("https://{host}/api/v1"))
        }
    },
    capabilities: Capabilities {
        supports_graphql: false,
        supports_rest: true,
    },
    create: create_provider,
};

fn create_provider(config: &ProviderConfig) -> actup_core::Result<Box<dyn Provider>> {
    let ProviderConfig {
        api_base,
        host,
        token_env,
        max_tags_per_repo,
        ..
    } = config;
    let token = token_env.as_deref().and_then(|var| std::env::var(var).ok());
    let provider =
        YourProvider::new(api_base, host, token.as_deref())?.max_tags(*max_tags_per_repo);
    Ok(Box::new(provider))
}
```

### 7. Wire the Provider

Add `YourPlatform` variants to `actup_core::HostKind` and
`actup_core::provider::Kind`, with arms in their `Display` impls and in
`Kind::cache_namespace`. Run `just schema` to regenerate
`schemas/actup-config.schema.json`, which lists every `HostKind`. The test
`schema_hostkind_variants` in `crates/actup/tests/cli_integration.rs` asserts
the exact list of `HostKind` names. Append the new name to its expected list:

```rust
assert_eq!(consts, vec!["github", "gitlab", "gitea", "yourplatform"]);
```

In the workspace `Cargo.toml`, add the crate to `default-members` and to
`[workspace.dependencies]`. `members` already matches it through the `crates/*`
glob:

```toml
[workspace]
default-members = [
  # ...
  "crates/provider-yourplatform",
]

[workspace.dependencies]
actup-provider-yourplatform.path = "crates/provider-yourplatform"
```

In `crates/actup/Cargo.toml`, add the crate as an optional dependency behind a
feature named after the provider:

```toml
[dependencies]
actup-provider-yourplatform = { workspace = true, optional = true }

[features]
yourplatform = ["dep:actup-provider-yourplatform"]
```

In `crates/lsp/Cargo.toml` and `crates/mcp/Cargo.toml`, add the crate as a
dependency:

```toml
[dependencies]
actup-provider-yourplatform = { workspace = true }
```

Add `REGISTRATION` to `available_registrations()` in `crates/actup/src/main.rs`,
gated on the feature:

```rust
let registrations = vec![
    &actup_provider_github::REGISTRATION,
    #[cfg(feature = "yourplatform")]
    &actup_provider_yourplatform::REGISTRATION,
];
```

Add it to `available_registrations()` in `crates/lsp/src/provider_setup.rs`
without the `#[cfg]` line, and to `REGISTRATIONS` in `crates/mcp/src/lib.rs`
with the array length raised by one. The test
`registrations_cover_every_provider` in `crates/mcp/src/lib.rs` lists the
expected `HostKind`s.

## Common Patterns

### Error Handling

**DO:**

```rust
// Use specific error variants with proper fields
Err(Error::NotFound {
    owner: owner.to_string(),
    repo: repo.to_string(),
    host: Some(self.host.clone()),
})
```

**DON'T:**

```rust
// Never use generic resource/provider fields (they don't exist!)
Err(Error::NotFound {
    resource: format!("{owner}/{repo}"),  // ❌ Wrong!
    provider: "gitlab".to_string(),       // ❌ Wrong!
})
```

### Lifetime Management in Async Traits

**DO:**

```rust
fn get_versions(&self, repo: &ActionRepoId) -> BoxFuture<'_, Result<version::Set>> {
    // Extract ALL needed data from params before async block
    let owner = repo.owner().to_string();
    let repo_name = repo.repo().to_string();
    let subpath = repo.subpath().map(ToString::to_string);

    Box::pin(async move {
        // Now safe to use self.client because we're not holding repo reference
        let tags = self
            .client
            .list_tags(&owner, &repo_name, self.max_tags)
            .await?;
        // ...
    })
}
```

**DON'T:**

```rust
Box::pin(async move {
    // ❌ Don't access repo inside async block - causes lifetime issues
    let owner = repo.owner().to_string();
    self.client.list_tags(&owner, ...).await?;
})
```

## API Differences by Platform

### GitHub

- Uses `/repos/{owner}/{repo}/tags`
- Pagination via Link header: `<url>; rel="next"`
- Auth: `Authorization: Bearer {token}`
- `per_page` is `max_tags_per_repo` up to 100; paging stops at
  `max_tags_per_repo` tags

### GitLab

- Uses `/projects/{owner}%2F{repo}/repository/tags` (URL-encoded path!)
- Pagination via `x-next-page` header
- Auth: `PRIVATE-TOKEN: {token}`
- `per_page` is `max_tags_per_repo` up to 100; paging stops at
  `max_tags_per_repo` tags or without `x-next-page`
- **Gotcha**: Project paths must be URL-encoded as one segment, `/` included
  (`actup_core::encode_segment`)

### Gitea/Forgejo

- Uses `/repos/{owner}/{repo}/tags`
- Pagination via `page` and `limit` parameters, with a `Link` header naming
  `rel="next"` while more pages exist
- Auth: `Authorization: token {token}` (note: "token", not "Bearer")
- `limit` is `max_tags_per_repo` up to 100; paging stops at `max_tags_per_repo`
  tags or on a page without a `rel="next"` link
- **Gotcha**: The server caps `limit` at its `MAX_RESPONSE_ITEMS` (50 by
  default), so a page can hold fewer tags than `limit` while more pages exist
- **Gotcha**: Percent-encode each path segment and the `ref` query value. A raw
  `+` reads as a space and a raw `#` cuts the URL

## Version Processing

Every provider builds its `version::Set` with the shared
`actup_providers::process_tags`. Implement `TagLike` for your tag type and pass
the fetched tags to it:

```rust
use actup_providers::process_tags;

let tags: Vec<YourTag> = self.client.list_tags(&owner, &repo_name, self.max_tags).await?;
let mut version_set = process_tags(tags);
```

`process_tags` accepts any `IntoIterator<Item = T>` where `T: TagLike`. It keeps
semver tags sorted newest first, records major-only tags such as `v4`, and
tracks the latest stable release overall and per major. Prereleases stay in the
tag list and are filtered at query time. Version processing lives only in
`crates/providers/src/tags.rs`. Change it there.

## Testing

Create `tests/integration.rs` with tests for your provider:

```rust
//! Integration tests for the `YourPlatform` provider.

use actup_core::provider::{Kind, Provider};
use actup_provider_yourplatform::YourProvider;

#[test]
fn provider_reports_kind_and_host() -> Result<(), Box<dyn std::error::Error>> {
    let provider = YourProvider::new("https://api.example.com", "example.com", None)?;
    assert_eq!(provider.kind(), Kind::YourPlatform);
    assert_eq!(provider.host(), "example.com");
    Ok(())
}

// Add tests with wiremock for API mocking
```

## Common Gotchas

### 1. Lifetime Issues in Async Traits

**Problem**: Compiler error "lifetime may not live long enough"

**Solution**: Extract ALL data from parameters before the `async move` block:

```rust
fn get_action_yml(&self, repo: &ActionRepoId, ref_name: &str) -> ... {
    let owner = repo.owner().to_string();      // ✅ Extract before async
    let repo_name = repo.repo().to_string();   // ✅ Extract before async
    let ref_name = ref_name.to_string();       // ✅ Extract before async
    let subpath = repo.subpath().map(ToString::to_string);  // ✅ Extract before async

    Box::pin(async move {
        // Now safe to use self
        self.client.get_file(&owner, ...).await
    })
}
```

### 2. Parse Errors

**Problem**: No `Error::Parse` variant

**Solutions**:

- For JSON parsing API responses: Use
  `Error::Network { operation: "parse response", ... }`
- For YAML parsing action files: Use `Error::YamlParse { path, reason, source }`
- For unexpected data format: Use `Error::Internal { context, message, source }`

`source` holds the foreign error, shared as an `Arc`. `Display` leaves it out,
and `actup_core::error::chain` renders the error with every cause.

### 3. SHA Validation

Always validate SHAs from API:

```rust
Sha40::new(&commit_sha).ok_or_else(|| Error::Internal {
    context: format!("commit SHA from API: {commit_sha}"),
    message: "expected 40-character hex string".to_string(),
    source: None,
})
```

### 4. Base64 Decoding

Most platforms return file content base64-encoded. `actup_core::Error` has no
`From` impl for `base64::DecodeError` or `std::string::FromUtf8Error`, and `?`
on either result does not compile. Call `decode_base64_content` from step 5. It
strips whitespace and maps both errors into `Error::Internal`:

```rust
if encoding == "base64" {
    let text = decode_base64_content(&content)?;
}
```

## Checklist

- [ ] API types in `src/types.rs` match platform's JSON responses
- [ ] Tag type implements `actup_providers::TagLike`
- [ ] `get_versions` builds its `version::Set` with
      `actup_providers::process_tags`
- [ ] HTTP client handles auth, rate limiting, pagination
- [ ] Provider trait impl extracts data before async blocks
- [ ] `get_action_yml` calls `actup_providers::parse_action_yml`
- [ ] Error handling uses proper `actup_core::Error` variants
- [ ] SHAs validated with `Sha40::new()`
- [ ] `REGISTRATION` static exported with presets, inference, and capabilities
- [ ] `src/lib.rs` declares the modules and re-exports `YourProvider`
- [ ] `HostKind` and `provider::Kind` variants added, schema regenerated with
      `just schema`
- [ ] New `HostKind` name appended to the expected list in
      `schema_hostkind_variants`
- [ ] Crate added to `default-members` and `[workspace.dependencies]` in the
      workspace `Cargo.toml`
- [ ] Optional dependency and feature added to `crates/actup/Cargo.toml`
- [ ] Dependency added to `crates/lsp/Cargo.toml` and `crates/mcp/Cargo.toml`
- [ ] `REGISTRATION` wired into both `available_registrations()` functions and
      MCP `REGISTRATIONS`
- [ ] `create` passes `max_tags_per_repo` to the provider, and `list_tags` stops
      at it
- [ ] Tests added to `tests/integration.rs`
- [ ] Documentation includes usage examples
- [ ] Follows Rust API guidelines (C-CASE, C-GETTER, etc.)

## Reference Implementations

- **GitHub**: `crates/provider-github/` - Reference implementation
- **GitLab**: `crates/provider-gitlab/` - Shows URL encoding patterns
- **Gitea**: `crates/provider-gitea/` - Shows Forgejo compatibility

## Resources

- [Provider trait definition](../crates/core/src/provider.rs)
- [Error types guide](../crates/core/src/error.rs)
- [Version resolution](../crates/core/src/version.rs)
- [Rust API Guidelines](./api-guidelines.md)
