//! JS provider bridge: adapts a host-provided JavaScript object to the
//! Rust [`Provider`](actup_core::Provider) trait.
//!
//! The JS side implements a simple async interface (`getVersions`, etc.)
//! and the adapter handles serde round-trips across the WASM boundary.

use actup_core::compat::BoxFuture;
use actup_core::provider::{ActionYml, Kind, Sha40};
use actup_core::{ActionRepoId, Provider, version};
use std::collections::HashMap;
use std::sync::Arc;
use wasm_bindgen::prelude::*;

/// Host-provided action provider, implemented in JS/TS.
///
/// Each method receives decomposed fields from [`ActionRepoId`] so the JS
/// side never needs to know about Rust structs.
#[wasm_bindgen]
extern "C" {
    /// A JavaScript object implementing the provider interface.
    ///
    /// Expected shape:
    /// ```typescript
    /// interface ActupProvider {
    ///     getVersions(host: string, owner: string, repo: string): Promise<VersionSet>;
    ///     resolveRefToSha(host: string, owner: string, repo: string, ref: string): Promise<string>;
    ///     getDefaultBranch(host: string, owner: string, repo: string): Promise<string>;
    ///     getReadme(host: string, owner: string, repo: string, ref?: string): Promise<string | null>;
    ///     getActionYml(host: string, owner: string, repo: string, ref: string): Promise<ActionYml>;
    /// }
    /// ```
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
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsVersionSet {
    /// Semver tags: `[{ version: "v4.2.0", sha: "abc..." }, ...]`
    #[serde(default)]
    semver_tags: Vec<JsVersionInfo>,
    /// Default branch info (optional)
    #[serde(default)]
    default_branch: Option<JsBranchInfo>,
}

/// A single version tag as returned from JS.
#[derive(serde::Deserialize)]
struct JsVersionInfo {
    /// Tag string (e.g. "v4.2.0", "v1")
    version: String,
    /// Full 40-char commit SHA
    sha: String,
}

/// Branch info as returned from JS.
#[derive(serde::Deserialize)]
struct JsBranchInfo {
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
    pub fn new(inner: JsProvider, host: String, kind: Kind) -> Self {
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

/// Convert a `JsValue` error from wasm_bindgen into an `actup_core::Error`.
fn js_err(val: JsValue) -> actup_core::Error {
    let msg = val
        .as_string()
        .or_else(|| {
            js_sys::Reflect::get(&val, &JsValue::from_str("message"))
                .ok()
                .and_then(|m| m.as_string())
        })
        .unwrap_or_else(|| format!("{val:?}"));
    actup_core::Error::Network {
        operation: "js provider call".into(),
        message: msg,
    }
}

/// Convert [`JsVersionSet`] into [`version::Set`].
fn build_version_set(js: JsVersionSet, clock: &dyn actup_core::Clock) -> version::Set {
    let mut infos: Vec<Arc<version::Info>> = Vec::with_capacity(js.semver_tags.len());
    let mut latest_per_major: HashMap<u32, Arc<version::Info>> = HashMap::new();
    let mut major_tags: HashMap<u32, Arc<version::Info>> = HashMap::new();
    let mut latest: Option<Arc<version::Info>> = None;

    for tag in js.semver_tags {
        let ver = version::Version::parse(&tag.version);
        let info = Arc::new(version::Info::new(ver.clone(), tag.sha));

        // Track major-only tags
        if ver.is_major_only() {
            if let Some(major) = ver.major() {
                major_tags.insert(major, Arc::clone(&info));
            }
        }

        // Track latest per major
        if ver.is_semver() {
            if let Some(major) = ver.major() {
                let is_newer = latest_per_major
                    .get(&major)
                    .is_none_or(|existing| info.version() > existing.version());
                if is_newer {
                    latest_per_major.insert(major, Arc::clone(&info));
                }
            }

            // Track overall latest
            let is_latest = latest
                .as_ref()
                .is_none_or(|existing| info.version() > existing.version());
            if is_latest {
                latest = Some(Arc::clone(&info));
            }
        }

        infos.push(info);
    }

    let mut set =
        version::Set::build_with_clock(infos, latest_per_major, major_tags, latest, clock);

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
                .map_err(js_err)?;
            let js_set: JsVersionSet = serde_wasm_bindgen::from_value(js_val).map_err(|e| {
                actup_core::Error::Internal {
                    context: "deserialize VersionSet from JS".into(),
                    message: e.to_string(),
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
                .map_err(js_err)?;
            let sha_str: String = serde_wasm_bindgen::from_value(js_val).map_err(|e| {
                actup_core::Error::Internal {
                    context: "deserialize SHA from JS".into(),
                    message: e.to_string(),
                }
            })?;
            Sha40::new(sha_str).ok_or_else(|| actup_core::Error::Internal {
                context: "resolve_ref_to_sha".into(),
                message: "JS provider returned invalid 40-char SHA".into(),
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
                .map_err(js_err)?;
            serde_wasm_bindgen::from_value(js_val).map_err(|e| actup_core::Error::Internal {
                context: "deserialize default branch from JS".into(),
                message: e.to_string(),
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
                .map_err(js_err)?;
            if js_val.is_null() || js_val.is_undefined() {
                return Ok(None);
            }
            let readme: String = serde_wasm_bindgen::from_value(js_val).map_err(|e| {
                actup_core::Error::Internal {
                    context: "deserialize readme from JS".into(),
                    message: e.to_string(),
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
                .map_err(js_err)?;
            serde_wasm_bindgen::from_value(js_val).map_err(|e| actup_core::Error::Internal {
                context: "deserialize ActionYml from JS".into(),
                message: e.to_string(),
            })
        })
    }
}
