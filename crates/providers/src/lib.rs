//! Shared utilities for actup provider implementations
//!
//! This crate provides common abstractions and utilities used across
//! all provider implementations (GitHub, GitLab, Gitea, etc.).

mod action_yml;
mod client;
mod ls_refs;
mod tags;

pub use action_yml::{RawActionYml, RawInput, RawOutput, RawRuns, RawStep, parse_action_yml};
pub use client::{DEFAULT_MAX_RETRIES, DEFAULT_RETRY_BASE_DELAY_MS, MAX_PER_PAGE, MIN_PER_PAGE};
pub use ls_refs::{RemoteRefs, RemoteTag, RepoLabel, list_refs};
pub use tags::{TagLike, process_tags};

/// Percent-encode each `/`-separated segment of `path`, keeping the separators.
#[must_use]
pub fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|segment| urlencoding::encode(segment).into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Extract the trailing filename component of a path for diagnostics.
///
/// Returns just the final path segment (e.g. `"action.yaml"` from
/// `"subpath/action.yaml"`). If the path has no valid final component
/// (pathological inputs), returns the input unchanged so error messages
/// still reference something the caller asked for.
///
/// Used by provider implementations to thread the actual loaded filename
/// through to [`parse_action_yml`], so error messages distinguish
/// `action.yml` from `action.yaml`.
#[must_use]
pub fn filename_label(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|os| os.to_str())
        .unwrap_or(path)
}
