# Config Refactor Spec v2.3

**Status:** Ready for implementation\
**Type:** Refactoring\
**Effort:** L (1-2 days)\
**Breaking:** Yes (pre-alpha)

## Summary

Refactor provider configuration from `[providers]` with `Vec<ProviderInstance>`
to:

- `[fetch]` for global fetch settings
- `[hosts."hostname"]` for per-host config (keyed map)
- Explicit `default_host` (no ordering footguns)
- **Provider-neutral core:** presets and inference live in provider crates via a
  registration API

## Motivation

Original design had issues:

1. **Redundancy** — `default_kind`/`default_host` + instance `kind`/`host`
2. **Unclear precedence** — `default_instance` vs `default_host`
3. **Misplaced globals** — `api_mode`/`max_tags` at `[providers]` level
4. **Deep nesting** — `[providers.instances.name]`
5. **Provider-neutrality violation** — core should not hardcode
   GitHub/GitLab/Gitea assumptions

## Config Structure

### TOML Example (full)

```toml
# Default host for bare `owner/repo` refs (if unset: uses provider preset for "github.com" when available)
# Same validation as host keys: lowercase, no scheme/path/port/whitespace
default_host = "github.com"

[policy]
bump       = "latest"
pin        = "preserve"
track      = "keep"
branch     = "ignore"
prerelease = false

[scan]
ignore      = []
extra_paths = []

# Fetch settings
[fetch]
api_mode          = "graphql"  # "graphql" | "rest"
max_tags_per_repo = 10  # 1-255

# Host overrides/additions
[hosts."ghe.corp.com"]
kind              = "github"
api_base          = "https://ghe.corp.com/api/v3"
token_env         = "GHE_TOKEN"
api_mode          = "rest"  # override global
max_tags_per_repo = 50  # override global
```

### TOML Example (minimal — zero config)

```toml
# Empty file works *if* the GitHub provider is compiled in:
# - provider preset registers github.com + GITHUB_TOKEN + https://api.github.com
```

### TOML Example (enterprise only)

```toml
default_host = "ghe.corp.com"

[hosts."ghe.corp.com"]
kind      = "github"
api_base  = "https://ghe.corp.com/api/v3"
token_env = "GHE_TOKEN"
```

## Rust Structs

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub policy: Policy,
    pub scan: Scan,
    pub overrides: Vec<Override>,
    pub cache: CacheConfig,

    /// Default host for bare `owner/repo` refs.
    /// If None, resolves via provider preset for "github.com" when present, else errors.
    pub default_host: Option<String>,

    /// Fetch settings (has Default impl)
    pub fetch: FetchSettings,

    /// Host configs (validated: lowercase keys only)
    #[serde(default)]
    pub hosts: HashMap<String, HostConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct FetchSettings {
    pub api_mode: ApiMode,
    pub max_tags_per_repo: NonZeroU8,
}

impl Default for FetchSettings {
    fn default() -> Self {
        Self {
            api_mode: ApiMode::Graphql,
            max_tags_per_repo: NonZeroU8::new(10).unwrap(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    /// Provider kind. Required for non-preset hosts.
    pub kind: Option<HostKind>,

    /// API base URL. Required after merge (may be inferred by provider registration).
    pub api_base: Option<String>,

    /// Env var name for auth token. Required after merge (may be defaulted by provider registration).
    pub token_env: Option<String>,

    /// Override global api_mode for this host.
    pub api_mode: Option<ApiMode>,

    /// Override global max_tags for this host.
    pub max_tags_per_repo: Option<NonZeroU8>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ApiMode {
    #[default]
    Graphql,
    Rest,
}

/// Provider kind identifiers. Providers register metadata for their kind.
/// Adding a new provider requires adding an enum variant + a provider crate.
///
/// Rationale: kind is used for cache namespacing and keeps compile-time safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum HostKind {
    GitHub,
    GitLab,
    Gitea,
}
```

### Design Decisions

- **Defaults strategy:** Single mechanism — `impl Default` on structs +
  `#[serde(default)]` on containers. No field-level default fns.
- **Typo protection:** `#[serde(deny_unknown_fields)]` on config structs.
  Pre-alpha tradeoff: older binaries reject newer keys.

## Provider Registration Architecture (NEW)

Core must remain provider-neutral. Provider-specific knowledge (presets, default
token envs, api_base inference, capabilities) is supplied by provider crates via
a static registration.

### Core types

```rust
/// Host-specific defaults a provider may register (zero-config ergonomics).
pub struct Preset {
    pub host: &'static str,      // e.g. "github.com"
    pub api_base: &'static str,  // e.g. "https://api.github.com"
    pub token_env: &'static str, // e.g. "GITHUB_TOKEN"
}

/// Provider capabilities (used to validate api_mode).
pub struct Capabilities {
    pub supports_graphql: bool,
    pub supports_rest: bool,
}

/// Provider registration exported by provider crates.
pub struct Registration {
    pub kind: HostKind,
    pub presets: &'static [Preset],

    /// Default token env for non-preset hosts of this kind (optional).
    /// If provided, it is treated as a non-user-specified default.
    pub default_token_env: Option<&'static str>,

    /// Infer an api_base for a non-preset host of this kind.
    /// Return None to disable inference and require api_base in config.
    pub infer_api_base: fn(hostname: &str) -> Option<String>,

    pub capabilities: Capabilities,

    /// Construct provider implementation.
    pub create: fn(ProviderConfig) -> Result<Box<dyn Provider>>,
}
```

### Provider crate exports

Each provider crate exports a single `REGISTRATION: Registration`. Example
(GitHub):

```rust
pub const REGISTRATION: Registration = Registration {
    kind: HostKind::GitHub,
    presets: &[Preset {
        host: "github.com",
        api_base: "https://api.github.com",
        token_env: "GITHUB_TOKEN",
    }],
    default_token_env: Some("GITHUB_TOKEN"),
    infer_api_base: |host| {
        if host == "github.com" {
            None
        } else {
            Some(format!("https://{host}/api/v3"))
        }
    },
    capabilities: Capabilities {
        supports_graphql: true,
        supports_rest: true,
    },
    create: create_github_provider,
};
```

### Registry assembly (feature-gated, compile-time)

The CLI (or `crates/providers`) assembles the set of available registrations
using Cargo features:

```rust
pub fn available_registrations() -> Vec<&'static Registration> {
    vec![
        #[cfg(feature = "github")]
        &actup_provider_github::REGISTRATION,
        #[cfg(feature = "gitlab")]
        &actup_provider_gitlab::REGISTRATION,
        #[cfg(feature = "gitea")]
        &actup_provider_gitea::REGISTRATION,
    ]
}
```

**Rule:** preset hostnames must be unique across all registrations. If
duplicates are detected, startup fails with a clear error.

## Validation Rules

### At Config Load

**Hostname validation** (applies to `hosts` keys AND `default_host`):

- Must be lowercase (error if `key != key.to_ascii_lowercase()`)
- No scheme (`https://`)
- No path (`/api/v3`)
- No port (`:8443`) — use `api_base` for ports
- No whitespace

**`api_base` validation:**

- Scheme required: `http://` or `https://`
- Path allowed (needed for `/api/v3`, `/api/v4`, `/api/v1`)
- Query (`?`) and fragment (`#`) forbidden
- Trailing `/` stripped

**`default_host` validation:**

- Passes hostname rules above
- Must resolve via `effective_host(default_host)` (preset or defined in
  `[hosts]`)

### Post-merge validation (per effective host)

After preset merge + host overrides + provider defaults/inference:

- `kind` required (error: `"kind required for {host}"`)
- `api_base` required (error: `"api_base required for {host}"`)
- `token_env` required (error: `"token_env required for {host}"`)

## Merge Semantics

For each field in `HostConfig`, if user value is `Some`, it replaces
preset/default; otherwise preset/default/inferred value remains. Empty strings
are treated as `Some("")`, not as "inherit."

## Precedence Chain

For `api_mode`, `max_tags_per_repo`:

```txt
per-host override > [fetch] > hardcoded default
```

For `kind`, `api_base`, `token_env`:

```txt
per-host override > preset > provider-default/inference > REQUIRED (error if missing)
```

## Resolution Logic

Inputs:

- parsed `Config`
- `registrations: &[&Registration]` (available providers)

Derived:

- `preset_index: HashMap<hostname, (&Registration, &Preset)>`

Algorithm:

```txt
effective_host(hostname) =
  1. Start from preset(hostname) if present, else empty
     - If preset chosen, kind is implied by registration.kind
  2. Merge user [hosts."hostname"] (user Some fields override)
  3. If kind still missing: error (non-preset host must specify kind)
  4. Lookup registration by kind (error if kind not available in this build)
  5. Fill defaults/inference:
     - token_env: if missing and registration.default_token_env set -> use it (non-user-specified)
     - api_base: if missing -> registration.infer_api_base(hostname)
  6. Validate kind, api_base, token_env present + api_base URL shape
  7. Validate api_mode compatibility with registration.capabilities (error on mismatch)
```

## Token Handling

`token_env` field must be present after merge (from host override, preset, or
provider default).

Runtime (env var resolution):

| `token_env` source             | Env var missing behavior          |
| ------------------------------ | --------------------------------- |
| Preset / provider default      | WARN once + allow unauthenticated |
| User-specified (`hosts` entry) | ERROR                             |

Warning format:
`WARN: {host}: env var {token_env} not set, using unauthenticated mode`

## Defaults (Empty Config)

```rust
// Empty .actup.toml loads as:
Config {
    default_host: None,
    fetch: FetchSettings::default(),
    hosts: HashMap::new(),
    // ...policy defaults...
}

// default_host resolution:
// - if provider preset for "github.com" is available: use it
// - else: error with "no default_host set and no github.com preset available"
```

## Migration Mapping

| Old                             | New                         |
| ------------------------------- | --------------------------- |
| `[providers].api_mode`          | `[fetch].api_mode`          |
| `[providers].max_tags_per_repo` | `[fetch].max_tags_per_repo` |
| `[providers].default_host`      | `default_host` (top-level)  |
| `[providers].default_kind`      | REMOVED                     |
| `[[providers.instances]]`       | `[hosts."hostname"]`        |
| `instance.host`                 | KEY of `[hosts]` table      |

**Note:** Duplicate hosts in old format → invalid in new format. Error on
migration.

## Deliverables

| #  | Task                                                      | Effort | Depends |
| -- | --------------------------------------------------------- | ------ | ------- |
| 1  | Add `HostKind`, `ApiMode` enums                           | S      | -       |
| 2  | Add `FetchSettings` + Default impl                        | S      | -       |
| 3  | Add `HostConfig` struct                                   | S      | 1       |
| 4  | Add `Registration`, `Preset`, `Capabilities` to core      | S      | 1       |
| 5  | Providers export `REGISTRATION` (presets + inference)     | M      | 4       |
| 6  | Feature-gated `available_registrations()` (CLI/providers) | S      | 4,5     |
| 7  | Implement validation (hostname, URL, post-merge)          | M      | 2,3     |
| 8  | Implement resolution logic using registrations            | M      | 6,7     |
| 9  | Update CLI `build_registry()`                             | M      | 8       |
| 10 | Update `config` command display                           | S      | 4       |
| 11 | Update tests                                              | M      | 8       |
| 12 | Regenerate JSON schema                                    | S      | 4       |
| 13 | Update docs + examples                                    | M      | 4       |

**Total effort: L** (1-2 days)

## Out of Scope

- Host aliasing (e.g., `ghe` → `ghe.corp.com`)
- Backward compatibility shim for old `[providers]` format
- Runtime plugin loading (dynamic libraries). Compile-time feature gating only.

## Risks

| Risk                        | Mitigation                                                |
| --------------------------- | --------------------------------------------------------- |
| `https://` in hostname      | Error at load                                             |
| Port in hostname            | Error: "use api_base for ports"                           |
| Uppercase hostname          | Error at load                                             |
| `token_enb` typo            | `deny_unknown_fields` catches it                          |
| Provider kind not available | Error: "provider {kind} not compiled in (available: ...)" |
| Duplicate preset hostnames  | Startup error listing conflicting providers               |
