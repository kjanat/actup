//! Progress reporting for batched version fetches.

use crate::compat::MaybeSync;

/// Where a batch fetch got a repo's versions, or that it got none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Read from the version cache.
    Cache,
    /// Listed over the git protocol.
    Git,
    /// Fetched through a batched provider API (GraphQL or REST).
    Api,
    /// Fetched through the repo's registered provider.
    Provider,
    /// The fetch failed.
    Failed,
}

/// Receives progress from a batched version fetch.
pub trait Progress: MaybeSync {
    /// The fetch covers `repos` unique repos.
    fn start(&self, repos: usize);

    /// One repo finished with `outcome`.
    fn finished(&self, outcome: Outcome);
}

/// A [`Progress`] that ignores every report.
#[derive(Debug, Clone, Copy, Default)]
pub struct Silent;

impl Progress for Silent {
    fn start(&self, _repos: usize) {}

    fn finished(&self, _outcome: Outcome) {}
}
