//! actup-core: Provider-neutral engine for GitHub Actions version management
//!
//! This crate provides:
//! - Parsing and discovery of `uses:` references in workflow/action files
//! - Version policy and resolution orchestration
//! - Provider trait interface (implementations live in separate crates)
//! - Caching interface (not storage implementation)
//! - Patch planning for safe YAML edits

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod action_ref;
pub mod cache;
pub mod caching_provider;
pub mod clock;
pub mod compat;
pub mod config;
pub mod discovery;
pub mod error;
pub mod parser;
pub mod policy;
pub mod provider;
pub mod resolution;
pub mod version;

// Re-export primary types at crate root
pub use action_ref::{ActionRef, ActionRepoId};
pub use caching_provider::{CacheStats, CachingProvider};
pub use clock::{Clock, SystemClock};
pub use compat::{BoxFuture, MaybeSend, MaybeSync};
pub use config::{
    ApiMode, CONFIG_EXTENSIONS, Config, FetchSettings, HostConfig, HostKind, find_repo_config,
    iter_config_candidates,
    registration::{
        Capabilities, Preset, ProviderConfig, Registration, validate_no_duplicate_presets,
    },
    resolution::{effective_host, resolve_default_host},
    user_config_path,
    validation::{validate_api_base, validate_hostname, validate_provider_config},
};
pub use discovery::{DiscoveredFile, FileKind, discover_files};
pub use error::{Error, Result};
pub use parser::{UsesLocation, extract_uses_from_yaml, line_spans, parse_action_path, parse_uses};
pub use policy::Update;
pub use provider::Provider;
pub use resolution::{
    CheckResult, CheckStatus, NO_VERSIONS_FOUND_REASON, SHORT_SHA_REASON, check_action,
    check_action_remote, check_action_with_provider, expand_short_sha,
};
pub use version::{BranchInfo, Info, Version};
