//! Gitea provider for actup
//!
//! Implements the `Provider` trait for Gitea and Forgejo instances.
//! Handles REST API calls, authentication, rate limiting, and pagination.
//!
//! # Features
//!
//! - Full support for Gitea and Forgejo (soft fork of Gitea)
//! - API token authentication
//! - Automatic pagination for tags and other resources
//! - Base64 decoding for file contents
//! - Compatible with Gitea API v1
//!
//! # Examples
//!
//! ```no_run
//! use actup_provider_gitea::GiteaProvider;
//! use actup_core::provider::Provider;
//!
//! # async fn example() -> actup_core::Result<()> {
//! // Create provider for a Gitea instance
//! let provider = GiteaProvider::new(
//!     "https://gitea.com",
//!     "gitea.com",
//!     Some("your-token")
//! )?;
//!
//! // Works with Forgejo too (same API)
//! let provider = GiteaProvider::new(
//!     "https://forgejo.example.com",
//!     "forgejo.example.com",
//!     Some("your-token")
//! )?;
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod client;
mod provider;
mod types;

pub use client::{DEFAULT_PER_PAGE, GiteaClient};
pub use provider::GiteaProvider;
pub use types::*;
