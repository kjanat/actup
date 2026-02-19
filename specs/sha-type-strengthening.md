# F3/N3: SHA Type Strengthening

Status: Design complete, not implemented. Recommend a dedicated PR.

## Problem

`resolved_sha` flows as `Option<String>` through the entire pipeline:

```
provider.resolve_ref_to_sha() -> Result<Sha40>   // validated
  .to_string()                                    // type erasure
    -> CheckResult.resolved_sha: Option<String>   // unvalidated
      -> ActionCheckEntry.resolved_sha: Option<String>
        -> format_pin_text(sha: &str, ...)        // could be anything
```

Meanwhile `Sha40` (a validated 40-char hex newtype at `provider.rs:54`) only lives at provider boundaries. The type erasure at
`resolution.rs:409,423` discards the validation guarantee. Downstream code trusts the String is a valid SHA without re-checking.

Additionally, `version::Info.sha` (`version.rs:306`) and `BranchInfo.sha` (`version.rs:339`) accept any `impl Into<String>` with no
validation, and `ActionRef::is_sha()` / `code_actions::is_sha()` duplicate the same 40-hex-char check that `Sha40::new()` already provides.

## Scope

### Direct type changes (6 sites)

| Struct/Enum                | Field          | File:Line           | Current          | Target          |
| -------------------------- | -------------- | ------------------- | ---------------- | --------------- |
| `CheckResult`              | `resolved_sha` | `resolution.rs:32`  | `Option<String>` | `Option<Sha40>` |
| `ActionCheckEntry`         | `resolved_sha` | `diagnostics.rs:73` | `Option<String>` | `Option<Sha40>` |
| `DiagnosticData::Outdated` | `target_sha`   | `diagnostics.rs:41` | `Option<String>` | `Option<Sha40>` |
| `CheckStatus::Pinned`      | `sha`          | `resolution.rs:87`  | `String`         | `Sha40`         |
| `version::Info`            | `sha`          | `version.rs:306`    | `String`         | `Sha40`         |
| `version::BranchInfo`      | `sha`          | `version.rs:339`    | `String`         | `Sha40`         |

### Prerequisite: `Sha40` trait impls

Currently `Sha40` only has `Debug, Clone, PartialEq, Eq, Hash, Display`. It needs:

- `Serialize` + `Deserialize` — for `version::Info`/`BranchInfo` (file cache) and `DiagnosticData` (LSP JSON)
- `AsRef<str>` — reduce `.as_str()` boilerplate
- Consider `From<Sha40> for String` for ergonomic boundary conversions

### Duplicate validation to consolidate

| Function                 | File:Line                 | Replace with                           |
| ------------------------ | ------------------------- | -------------------------------------- |
| `ActionRef::is_sha()`    | `action_ref.rs:96-98`     | `Sha40::new(self.ref_str()).is_some()` |
| `code_actions::is_sha()` | `code_actions.rs:283-284` | `Sha40::new(ref_str).is_some()`        |

### Cross-crate impact (7 crates)

1. `actup-core` — resolution, version, action_ref, caching_provider, provider
2. `actup-providers` — tags.rs (TagLike.sha() returns &str)
3. `actup-provider-github` — provider.rs, graphql.rs (ProcessableTag.sha: String)
4. `actup-provider-gitlab` — provider.rs
5. `actup-provider-gitea` — provider.rs
6. `actup` (CLI) — check, update, pin, output commands
7. `actup-lsp` — diagnostics, code_actions, hover, test_utils

### Estimated change footprint

- ~120-150 lines of signature/type changes
- ~50 test sites using `make_version_info()` helpers with arbitrary SHA strings
- ~28 calls to `.sha()` on `version::Info` or `BranchInfo`

## Migration Strategy

### Phase 1: Expand `Sha40` API

Add `Serialize`, `Deserialize`, `AsRef<str>` to `Sha40`. Add `Sha40::new_unchecked()` (cfg(test) only) for test ergonomics where arbitrary
strings are used as SHA placeholders.

### Phase 2: Strengthen `version::Info` and `BranchInfo`

Change `sha: String` to `sha: Sha40` in both structs. This is the deepest change and ripples to all 3 provider crates, the cache layer, and
test helpers. Update `Info::new()` to accept `Sha40` instead of `impl Into<String>`.

### Phase 3: Strengthen `CheckResult` and downstream

Propagate `Sha40` through:

- `resolve_current_sha()` return type
- `CheckResult.resolved_sha` and `.with_resolved_sha()`
- `CheckStatus::Pinned.sha`
- `ActionCheckEntry.resolved_sha`
- `DiagnosticData::Outdated.target_sha`

Remove `.to_string()` erasure calls at `resolution.rs:409,423`.

### Phase 4: Consolidate `is_sha()` helpers

Replace inline 40-hex-char checks in `action_ref.rs` and `code_actions.rs` with `Sha40::new().is_some()`.

## Risks

- **Serde compatibility**: File cache format changes if `version::Info.sha` goes from `String` to `Sha40`. Need migration or a
  `#[serde(deserialize_with)]` that accepts both.
- **Test ergonomics**: ~50 test sites pass short/fake SHA strings like `"sha_old_aaa111bbb"` or `"abc123"` to `make_version_info()`. These
  would need 40-char hex strings or `new_unchecked()`.
- **Provider boundary**: `TagLike` trait in `actup-providers` returns `fn sha(&self) -> &str`. Changing to `Sha40` cascades into all
  provider implementations and GitHub's `ProcessableTag` struct.
