//! Common HTTP client constants
//!
//! Shared configuration values for provider HTTP clients.
//! Note: `DEFAULT_PER_PAGE` is intentionally not included here as
//! it differs between providers (GitHub/GitLab: 100, Gitea: 50).

/// Maximum items per page allowed by most APIs
pub const MAX_PER_PAGE: u8 = 100;

/// Minimum items per page
pub const MIN_PER_PAGE: u8 = 1;

/// Default max retry attempts for transient errors
pub const DEFAULT_MAX_RETRIES: u32 = 3;

/// Default base delay in milliseconds for exponential backoff
pub const DEFAULT_RETRY_BASE_DELAY_MS: u64 = 100;
