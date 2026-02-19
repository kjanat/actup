# Implementing Providers for Actup

This guide walks through creating a new provider for actup, using GitLab and Gitea as reference examples.

## Overview

Providers implement the `Provider` trait from `actup-core`, enabling actup to work with different Git hosting platforms (GitHub, GitLab,
Gitea, Forgejo, etc.). The provider architecture is designed to keep `actup-core` completely platform-agnostic.

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
actup-core    = { workspace = true }
base64        = { workspace = true }
reqwest       = { workspace = true }
serde         = { workspace = true }
serde_json    = { workspace = true }
serde_yaml_ng = { workspace = true }
tokio         = { workspace = true }
tracing       = { workspace = true }
```

### 2. Define API Response Types

Create `src/types.rs` with structs matching your platform's API responses:

```rust
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

// ... other types for repos, branches, files, etc.
```

### 3. Implement HTTP Client

Create `src/client.rs`:

```rust
use actup_core::{Error, Result};
use reqwest::{
    Client, Response, StatusCode,
    header::{HeaderMap, HeaderValue, USER_AGENT},
};
use std::time::Duration;

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
                HeaderValue::from_str(&format!("Bearer {t}")).map_err(|e| Error::Network {
                    operation: "set auth header".to_string(),
                    message: e.to_string(),
                })?,
            );
        }

        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Network {
                operation: "create HTTP client".to_string(),
                message: e.to_string(),
            })?;

        Ok(Self {
            client,
            api_base: format!("{api_base}/api/v1"),
            host: host.to_string(),
        })
    }

    pub async fn list_tags(&self, owner: &str, repo: &str) -> Result<Vec<YourTag>> {
        // Implement pagination logic for your API
        let url = format!("{}/repos/{owner}/{repo}/tags", self.api_base);
        let response = self.get(&url).await?;
        self.handle_response(response, owner, repo).await
    }

    /// Helper method to perform GET requests with configured headers
    async fn get(&self, url: &str) -> Result<Response> {
        self.client
            .get(url)
            .send()
            .await
            .map_err(|e| Error::Network {
                operation: "HTTP GET".to_string(),
                message: e.to_string(),
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
                    message: e.to_string(),
                })?;

                serde_json::from_str(&text).map_err(|e| Error::Network {
                    operation: "parse response".to_string(),
                    message: format!("JSON parse error: {e}"),
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
            }),
        }
    }
}
```

### 4. Implement Provider Trait

Create `src/provider.rs`:

```rust
use actup_core::provider::{ActionYml, Kind, Provider, Sha40};
use actup_core::version;
use std::future::Future;
use std::pin::Pin;

pub struct YourProvider {
    client: YourClient,
    host: String,
}

impl Provider for YourProvider {
    fn kind(&self) -> Kind {
        Kind::YourPlatform // Add to Kind enum in actup-core first
    }

    fn host(&self) -> &str {
        &self.host
    }

    fn get_versions(
        &self,
        repo: &ActionRepoId,
    ) -> Pin<Box<dyn Future<Output = Result<version::Set>> + Send + '_>> {
        // IMPORTANT: Extract all needed data from `repo` before async block
        let owner = repo.owner().to_string();
        let repo_name = repo.repo().to_string();

        Box::pin(async move {
            let tags = self.client.list_tags(&owner, &repo_name).await?;
            let mut version_set = process_tags(tags); // See helper function below

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
    ) -> Pin<Box<dyn Future<Output = Result<Sha40>> + Send + '_>> {
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
            })
        })
    }

    fn get_default_branch(
        &self,
        repo: &ActionRepoId,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> {
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
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>>> + Send + '_>> {
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
                    Err(Error::NotFound { .. }) => continue, // Try next filename
                    Err(e) => return Err(e), // Propagate auth/rate-limit/network errors
                }
            }
            Ok(None)
        })
    }

    fn get_action_yml(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> Pin<Box<dyn Future<Output = Result<ActionYml>> + Send + '_>> {
        // Extract data before async block (crucial for lifetimes!)
        let owner = repo.owner().to_string();
        let repo_name = repo.repo().to_string();
        let ref_name = ref_name.to_string();
        let subpath = repo.subpath().map(ToString::to_string);

        Box::pin(async move {
            let path = match &subpath {
                Some(p) => format!("{p}/action.yml"),
                None => "action.yml".to_string(),
            };

            let content = self
                .client
                .get_file(&owner, &repo_name, &path, &ref_name)
                .await?;
            let raw = decode_base64_content(&content)?;

            // Call module-level parse function, not Self::method
            parse_action_yml(&raw)
        })
    }
}
```

### 5. Helper Functions (Module Level)

**IMPORTANT**: These must be module-level functions, not methods:

```rust
/// Process tags into version::Set
fn process_tags(tags: Vec<YourTag>) -> version::Set {
    let mut semver_tags = Vec::new();
    let mut latest_per_major = HashMap::new();
    let mut major_tags = HashMap::new();
    let mut latest = None;

    for tag in tags {
        let version = version::Version::parse(&tag.name);
        let info = version::Info::new(version.clone(), &tag.commit.sha);

        if let version::Version::Semver {
            major, prerelease, ..
        } = &version
        {
            semver_tags.push(info.clone());

            if version.is_major_only() {
                major_tags.insert(*major, info.clone());
            }

            if prerelease.is_none() {
                // Update latest trackers (see GitHub provider for full logic)
                latest_per_major
                    .entry(*major)
                    .and_modify(|existing| {
                        if existing.version() < &version {
                            *existing = info.clone();
                        }
                    })
                    .or_insert(info.clone());

                latest = match latest {
                    Some(current_latest) if current_latest.version() >= &version => {
                        Some(current_latest)
                    }
                    _ => Some(info),
                };
            }
        }
    }

    semver_tags.sort_by(|a, b| b.version().cmp(a.version()));
    version::Set::build(semver_tags, latest_per_major, major_tags, latest)
}

/// Decode base64-encoded file content (e.g., from GitLab/GitHub file APIs)
fn decode_base64_content(content: &str) -> Result<String> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let bytes = STANDARD.decode(content).map_err(|e| Error::Internal {
        context: "base64 decode".to_string(),
        message: e.to_string(),
    })?;
    String::from_utf8(bytes).map_err(|e| Error::Internal {
        context: "UTF-8 decode".to_string(),
        message: e.to_string(),
    })
}

/// Parse action.yml content (module-level function, not method!)
fn parse_action_yml(raw: &str) -> Result<ActionYml> {
    #[derive(Deserialize)]
    struct RawInput {
        description: Option<String>,
        required: Option<bool>,
        default: Option<String>,
    }

    #[derive(Deserialize)]
    struct RawOutput {
        description: Option<String>,
        value: Option<String>,
    }

    #[derive(Deserialize)]
    struct RawStep {
        name: Option<String>,
        uses: Option<String>,
        run: Option<String>,
        #[serde(rename = "if")]
        condition: Option<String>,
    }

    #[derive(Deserialize)]
    struct RawRuns {
        using: Option<String>,
        main: Option<String>,
        pre: Option<String>,
        post: Option<String>,
        steps: Option<Vec<RawStep>>,
    }

    #[derive(Deserialize)]
    struct RawActionYml {
        name: Option<String>,
        description: Option<String>,
        author: Option<String>,
        inputs: Option<HashMap<String, RawInput>>,
        outputs: Option<HashMap<String, RawOutput>>,
        runs: Option<RawRuns>,
    }

    let parsed: RawActionYml = serde_yaml_ng::from_str(raw).map_err(|e| Error::YamlParse {
        path: "action.yml".into(),
        reason: e.to_string(),
    })?;

    // Build ActionYml using builder pattern
    let mut action = ActionYml::new(raw);
    if let Some(name) = parsed.name {
        action = action.with_name(name);
    }
    // ... continue building

    Ok(action)
}
```

### 6. Export REGISTRATION

Each provider crate must export a static `REGISTRATION` that supplies presets, inference logic, and a factory function. This enables
zero-config for preset hosts and automatic `api_base` inference for self-hosted instances.

```rust
use actup_core::{Capabilities, HostKind, Preset, ProviderConfig, Registration};

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

fn create_provider(config: ProviderConfig) -> actup_core::Result<Box<dyn actup_core::Provider>> {
    let token = std::env::var(&config.token_env).ok();
    let provider = YourProvider::new(&config.api_base, &config.host, token.as_deref())?;
    Ok(Box::new(provider))
}
```

Then add the provider to `available_registrations()` in the CLI crate behind a feature gate.

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
fn get_versions(
    &self,
    repo: &ActionRepoId,
) -> Pin<Box<dyn Future<Output = Result<version::Set>> + Send + '_>> {
    // Extract ALL needed data from params before async block
    let owner = repo.owner().to_string();
    let repo_name = repo.repo().to_string();
    let subpath = repo.subpath().map(ToString::to_string);

    Box::pin(async move {
        // Now safe to use self.client because we're not holding repo reference
        let tags = self.client.list_tags(&owner, &repo_name).await?;
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

### Module-Level vs Methods

**DO:**

```rust
// parse_action_yml as module-level function
fn parse_action_yml(raw: &str) -> Result<ActionYml> {
    // ...
}

impl Provider for YourProvider {
    fn get_action_yml(...) -> Pin<Box<...>> {
        Box::pin(async move {
            // Call as free function
            parse_action_yml(&raw)
        })
    }
}
```

**DON'T:**

```rust
impl YourProvider {
    // ❌ Don't make it a method - causes lifetime issues in async traits
    fn parse_action_yml(raw: &str) -> ActionYml {
        // ...
    }
}

impl Provider for YourProvider {
    fn get_action_yml(...) -> Pin<Box<...>> {
        Box::pin(async move {
            Self::parse_action_yml(&raw)  // ❌ Lifetime issues
        })
    }
}
```

## API Differences by Platform

### GitHub

- Uses `/repos/{owner}/{repo}/tags`
- Pagination via Link header: `<url>; rel="next"`
- Auth: `Authorization: Bearer {token}`
- Default per_page: 100 (max)

### GitLab

- Uses `/projects/{owner}%2F{repo}/repository/tags` (URL-encoded path!)
- Pagination via `x-next-page` header
- Auth: `PRIVATE-TOKEN: {token}`
- Default per_page: 100 (max)
- **Gotcha**: Project paths must be URL-encoded (use `urlencoding` crate)

### Gitea/Forgejo

- Uses `/repos/{owner}/{repo}/tags`
- Pagination via `page` parameter (no header indicator for next page)
- Auth: `Authorization: token {token}` (note: "token", not "Bearer")
- Default per_page: 50 (recommended)
- Stop when empty array returned

## Version Processing

All providers use the same version processing logic (copy from GitHub provider):

```rust
fn process_tags(tags: Vec<YourTag>) -> version::Set {
    let mut semver_tags = Vec::new();
    let mut latest_per_major: HashMap<u32, version::Info> = HashMap::new();
    let mut major_tags: HashMap<u32, version::Info> = HashMap::new();
    let mut latest: Option<version::Info> = None;

    for tag in tags {
        let version = version::Version::parse(&tag.name);
        let info = version::Info::new(version.clone(), &tag.commit.sha);

        if let version::Version::Semver {
            major, prerelease, ..
        } = &version
        {
            semver_tags.push(info.clone());

            if version.is_major_only() {
                major_tags.insert(*major, info.clone());
            }

            if prerelease.is_none() {
                // Track latest overall and latest per major
                // (See GitHub provider for full optimized logic)
                latest_per_major
                    .entry(*major)
                    .and_modify(|existing| {
                        if existing.version() < &version {
                            *existing = info.clone();
                        }
                    })
                    .or_insert(info.clone());

                latest = match latest {
                    Some(current_latest) if current_latest.version() >= &version => {
                        Some(current_latest)
                    }
                    _ => Some(info),
                };
            }
        }
    }

    semver_tags.sort_by(|a, b| b.version().cmp(a.version()));
    version::Set::build(semver_tags, latest_per_major, major_tags, latest)
}
```

## Testing

Create `tests/integration.rs` with tests for your provider:

```rust
use actup_core::provider::Provider;
use actup_provider_yourplatform::YourProvider;

#[tokio::test]
async fn test_provider_kind() {
    let provider = YourProvider::new("https://api.example.com", "example.com", None).unwrap();
    assert_eq!(provider.kind(), provider::Kind::YourPlatform);
    assert_eq!(provider.host(), "example.com");
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

- For JSON parsing API responses: Use `Error::Network { operation: "parse response", ... }`
- For YAML parsing action files: Use `Error::YamlParse { path, reason }`
- For unexpected data format: Use `Error::Internal { context, message }`

### 3. SHA Validation

Always validate SHAs from API:

```rust
Sha40::new(&commit_sha).ok_or_else(|| Error::Internal {
    context: format!("commit SHA from API: {commit_sha}"),
    message: "expected 40-character hex string".to_string(),
})
```

### 4. Base64 Decoding

Most platforms return file content base64-encoded:

```rust
use base64::{engine::general_purpose::STANDARD, Engine};

if encoding == "base64" {
    let decoded = STANDARD.decode(&content)?;
    let text = String::from_utf8(decoded)?;
}
```

## Checklist

- [ ] API types in `src/types.rs` match platform's JSON responses
- [ ] HTTP client handles auth, rate limiting, pagination
- [ ] Provider trait impl extracts data before async blocks
- [ ] `parse_action_yml` is module-level function (not method)
- [ ] Error handling uses proper `actup_core::Error` variants
- [ ] SHAs validated with `Sha40::new()`
- [ ] `REGISTRATION` static exported with presets, inference, and capabilities
- [ ] `REGISTRATION` wired into `available_registrations()` behind feature gate
- [ ] Tests added to `tests/integration.rs`
- [ ] Documentation includes usage examples
- [ ] Added to workspace `Cargo.toml` members
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
