//! JS provider bridge: adapts a host-provided JavaScript object to the
//! Rust [`Provider`](actup_core::Provider) trait.
//!
//! The JS side implements a simple async interface (`getVersions`, etc.)
//! and the adapter handles serde round-trips across the WASM boundary.

use std::time::Duration;

use actup_core::{
    ActionRepoId, Provider,
    compat::BoxFuture,
    provider::{ActionYml, Kind, Sha40},
    version,
};
use wasm_bindgen::prelude::*;

/// Host-provided action provider, implemented in JS/TS.
///
/// Each method receives decomposed fields from [`ActionRepoId`] so the JS
/// side never needs to know about Rust structs.
#[wasm_bindgen]
extern "C" {
    /// A JavaScript object implementing the provider interface.
    ///
    /// The `typescript_type` attribute surfaces this in the generated
    /// `.d.ts` as `import("../src/types").ActupProvider` rather than the
    /// wasm-bindgen default `any`.
    #[wasm_bindgen(typescript_type = "import(\"../src/types\").ActupProvider")]
    pub type JsProvider;

    #[wasm_bindgen(method, catch, js_name = "getVersions")]
    async fn get_versions_js(
        this: &JsProvider,
        host: &str,
        owner: &str,
        repo: &str,
    ) -> std::result::Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch, js_name = "resolveRefToSha")]
    async fn resolve_ref_to_sha_js(
        this: &JsProvider,
        host: &str,
        owner: &str,
        repo: &str,
        ref_name: &str,
    ) -> std::result::Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch, js_name = "getDefaultBranch")]
    async fn get_default_branch_js(
        this: &JsProvider,
        host: &str,
        owner: &str,
        repo: &str,
    ) -> std::result::Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch, js_name = "getReadme")]
    async fn get_readme_js(
        this: &JsProvider,
        host: &str,
        owner: &str,
        repo: &str,
        ref_name: Option<String>,
    ) -> std::result::Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch, js_name = "getActionYml")]
    async fn get_action_yml_js(
        this: &JsProvider,
        host: &str,
        owner: &str,
        repo: &str,
        ref_name: &str,
    ) -> std::result::Result<JsValue, JsValue>;
}

/// Version data as returned from JS.
///
/// Matches the shape the JS provider should return from `getVersions()`.
/// Deserialized via `serde_wasm_bindgen` from the JS object.
#[derive(serde::Deserialize, tsify::Tsify)]
#[serde(rename_all = "camelCase")]
pub struct VersionSet {
    /// Semver tags: `[{ version: "v4.2.0", sha: "abc..." }, ...]`
    #[serde(default)]
    semver_tags: Vec<VersionTag>,
    /// Default branch info (optional)
    #[serde(default)]
    default_branch: Option<BranchInfo>,
}

/// A single version tag as returned from JS.
#[derive(serde::Deserialize, serde::Serialize, tsify::Tsify)]
pub struct VersionTag {
    /// Tag string (e.g. "v4.2.0", "v1")
    version: String,
    /// Full 40-char commit SHA
    sha: String,
}

/// Sort tags newest first by core version precedence.
pub fn sort_newest_first(tags: &mut [VersionTag]) {
    tags.sort_by_cached_key(|tag| std::cmp::Reverse(version::Version::parse(&tag.version)));
}

/// Branch info as returned from JS.
#[derive(serde::Deserialize, tsify::Tsify)]
pub struct BranchInfo {
    /// Branch name (e.g. "main")
    name: String,
    /// HEAD commit SHA
    sha: String,
}

/// Adapter wrapping a JS provider object to satisfy the Rust [`Provider`] trait.
///
/// Holds a reference to the JS object and the host/kind metadata.
pub struct JsProviderAdapter {
    inner: JsProvider,
    host: String,
    kind: Kind,
}

impl JsProviderAdapter {
    /// Create a new adapter for the given JS provider and host.
    #[must_use]
    pub const fn new(inner: JsProvider, host: String, kind: Kind) -> Self {
        Self { inner, host, kind }
    }
}

impl std::fmt::Debug for JsProviderAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsProviderAdapter")
            .field("host", &self.host)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// Convert a provider rejection into the `actup_core::Error` its `kind` names.
fn js_err(
    val: &JsValue,
    provider: Kind,
    not_found: impl FnOnce() -> actup_core::Error,
) -> actup_core::Error {
    let field = |name: &str| js_sys::Reflect::get(val, &JsValue::from_str(name)).ok();
    let message = val
        .as_string()
        .or_else(|| field("message").and_then(|m| m.as_string()))
        .unwrap_or_else(|| format!("{val:?}"));
    match field("kind").and_then(|k| k.as_string()).as_deref() {
        Some("notFound") => not_found(),
        Some("rateLimited") => actup_core::Error::RateLimited {
            provider: provider.to_string(),
            retry_after: field("retryAfter")
                .and_then(|secs| secs.as_f64())
                .and_then(|secs| Duration::try_from_secs_f64(secs).ok()),
        },
        Some("auth") => actup_core::Error::Auth {
            provider: provider.to_string(),
            reason: message,
            source: None,
        },
        Some("forbidden") => actup_core::Error::Forbidden {
            provider: provider.to_string(),
            reason: message,
        },
        _ => actup_core::Error::Network {
            operation: "js provider call".into(),
            message,
            source: None,
        },
    }
}

fn repo_not_found(host: &str, owner: &str, repo: &str) -> actup_core::Error {
    actup_core::Error::NotFound {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        host: Some(host.to_owned()),
    }
}

/// Convert [`VersionSet`] into [`version::Set`].
fn build_version_set(js: VersionSet, clock: &dyn actup_core::Clock) -> version::Set {
    let tags = js.semver_tags.into_iter().map(|tag| (tag.version, tag.sha));
    let mut set = version::Set::from_tags_with_clock(tags, clock);
    if let Some(branch) = js.default_branch {
        set.set_default_branch(version::BranchInfo::new(branch.name, branch.sha));
    }
    set
}

impl Provider for JsProviderAdapter {
    fn kind(&self) -> Kind {
        self.kind
    }

    fn host(&self) -> &str {
        &self.host
    }

    fn get_versions(&self, repo: &ActionRepoId) -> BoxFuture<'_, actup_core::Result<version::Set>> {
        let host = repo.host().unwrap_or(&self.host).to_owned();
        let owner = repo.owner().to_owned();
        let repo_name = repo.repo().to_owned();
        Box::pin(async move {
            let js_val = self
                .inner
                .get_versions_js(&host, &owner, &repo_name)
                .await
                .map_err(|e| js_err(&e, self.kind, || repo_not_found(&host, &owner, &repo_name)))?;
            let js_set: VersionSet = serde_wasm_bindgen::from_value(js_val).map_err(|e| {
                actup_core::Error::Internal {
                    context: "deserialize VersionSet from JS".into(),
                    message: e.to_string(),
                    source: None,
                }
            })?;
            Ok(build_version_set(js_set, &crate::clock::JsClock))
        })
    }

    fn resolve_ref_to_sha(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, actup_core::Result<Sha40>> {
        let host = repo.host().unwrap_or(&self.host).to_owned();
        let owner = repo.owner().to_owned();
        let repo_name = repo.repo().to_owned();
        let ref_name = ref_name.to_owned();
        Box::pin(async move {
            let js_val = self
                .inner
                .resolve_ref_to_sha_js(&host, &owner, &repo_name, &ref_name)
                .await
                .map_err(|e| {
                    js_err(&e, self.kind, || actup_core::Error::RefNotFound {
                        owner: owner.clone(),
                        repo: repo_name.clone(),
                        ref_name: ref_name.clone(),
                        host: Some(host.clone()),
                    })
                })?;
            let sha_str: String = serde_wasm_bindgen::from_value(js_val).map_err(|e| {
                actup_core::Error::Internal {
                    context: "deserialize SHA from JS".into(),
                    message: e.to_string(),
                    source: None,
                }
            })?;
            Sha40::new(sha_str).ok_or_else(|| actup_core::Error::Internal {
                context: "resolve_ref_to_sha".into(),
                message: "JS provider returned invalid 40-char SHA".into(),
                source: None,
            })
        })
    }

    fn get_default_branch(&self, repo: &ActionRepoId) -> BoxFuture<'_, actup_core::Result<String>> {
        let host = repo.host().unwrap_or(&self.host).to_owned();
        let owner = repo.owner().to_owned();
        let repo_name = repo.repo().to_owned();
        Box::pin(async move {
            let js_val = self
                .inner
                .get_default_branch_js(&host, &owner, &repo_name)
                .await
                .map_err(|e| js_err(&e, self.kind, || repo_not_found(&host, &owner, &repo_name)))?;
            serde_wasm_bindgen::from_value(js_val).map_err(|e| actup_core::Error::Internal {
                context: "deserialize default branch from JS".into(),
                message: e.to_string(),
                source: None,
            })
        })
    }

    fn get_readme(
        &self,
        repo: &ActionRepoId,
        ref_name: Option<&str>,
    ) -> BoxFuture<'_, actup_core::Result<Option<String>>> {
        let host = repo.host().unwrap_or(&self.host).to_owned();
        let owner = repo.owner().to_owned();
        let repo_name = repo.repo().to_owned();
        let ref_name = ref_name.map(String::from);
        Box::pin(async move {
            let js_val = self
                .inner
                .get_readme_js(&host, &owner, &repo_name, ref_name)
                .await
                .map_err(|e| js_err(&e, self.kind, || repo_not_found(&host, &owner, &repo_name)))?;
            if js_val.is_null() || js_val.is_undefined() {
                return Ok(None);
            }
            let readme: String = serde_wasm_bindgen::from_value(js_val).map_err(|e| {
                actup_core::Error::Internal {
                    context: "deserialize readme from JS".into(),
                    message: e.to_string(),
                    source: None,
                }
            })?;
            Ok(Some(readme))
        })
    }

    fn get_action_yml(
        &self,
        repo: &ActionRepoId,
        ref_name: &str,
    ) -> BoxFuture<'_, actup_core::Result<ActionYml>> {
        let host = repo.host().unwrap_or(&self.host).to_owned();
        let owner = repo.owner().to_owned();
        let repo_name = repo.repo().to_owned();
        let ref_name = ref_name.to_owned();
        Box::pin(async move {
            let js_val = self
                .inner
                .get_action_yml_js(&host, &owner, &repo_name, &ref_name)
                .await
                .map_err(|e| js_err(&e, self.kind, || repo_not_found(&host, &owner, &repo_name)))?;
            serde_wasm_bindgen::from_value(js_val).map_err(|e| actup_core::Error::Internal {
                context: "deserialize ActionYml from JS".into(),
                message: e.to_string(),
                source: None,
            })
        })
    }
}
