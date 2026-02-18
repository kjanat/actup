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
pub use config::{
    ApiMode, CONFIG_EXTENSIONS, Config, FetchSettings, HostKind, find_repo_config,
    iter_config_candidates, user_config_path,
};
pub use discovery::{DiscoveredFile, FileKind, discover_files};
pub use error::{Error, Result};
pub use parser::{UsesLocation, extract_uses_from_yaml, parse_uses};
pub use policy::Update;
pub use provider::Provider;
pub use resolution::{
    CheckResult, CheckStatus, NO_VERSIONS_FOUND_REASON, check_action, check_action_remote,
};
pub use version::{BranchInfo, Info, Version};
