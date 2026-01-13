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
pub mod config;
pub mod discovery;
pub mod error;
pub mod parser;
pub mod policy;
pub mod provider;
pub mod version;

// Re-export primary types at crate root
pub use action_ref::{ActionRef, ActionRepoId};
pub use config::Config;
pub use discovery::{DiscoveredFile, FileKind, discover_files};
pub use error::{Error, Result};
pub use parser::{UsesLocation, extract_uses_from_yaml, parse_uses};
pub use policy::{RefStyle, UpdatePolicy};
pub use provider::{Provider, ProviderKind, ProviderRegistry};
pub use version::{BranchInfo, Version, VersionInfo, VersionSet};
