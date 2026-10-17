# Pin to SHA: Code Action + Resolution Engine

## Problem Definition

**What are we solving?** Non-SHA `uses:` refs (tags, branches) are mutable — the
commit they point to can change at any time. "Pin to SHA" converts them to
immutable 40-char commit SHAs for supply-chain safety. The LSP has no code
action for this. The CLI has `actup pin` but `actup update` ignores
`policy.pin = "sha"`.

**For whom?** Security-conscious teams, SOC2/FedRAMP compliance workflows,
anyone who wants reproducible CI.

**Cost of not solving?**

- Users must manually run `actup pin` CLI then re-open files — breaks editor
  flow
- `policy.pin = "sha"` config is parsed but never honored by `update` — a silent
  lie
- No "pin current ref" option in the editor; only "update to latest tag"

## Constraints

| Constraint                      | Impact                                                                        |
| ------------------------------- | ----------------------------------------------------------------------------- |
| **`check_action` is sync**      | No provider access; SHA lookup limited to data already in `version::Set`      |
| **Diagnostic range = ref only** | Existing range covers `@ref` portion only — sufficient since prefix unchanged |
| **UpToDate = no diagnostic**    | Can't use diagnostic-based code actions; need cursor-based lookup             |
| **`version::Set` has SHAs**     | All `Info` structs carry `.sha()` — no new API calls needed for tag refs      |
| **Branch SHAs need provider**   | Non-default branches require `resolve_ref_to_sha` (async path only)           |

## Design Decisions

### D1: `resolved_sha` on `CheckResult`, not `CheckStatus`

Add a single `Option<String>` field to `CheckResult` rather than modifying every
`CheckStatus` variant.

```rust
pub struct CheckResult {
    action_ref: ActionRef,
    status: CheckStatus,
    resolved_sha: Option<String>, // NEW: SHA of the current ref, when resolvable
}
```

**Why:** `UpToDate` is a unit variant. Making it carry data breaks every `match`
site. A field on the wrapper is additive — existing `status()` matches are
untouched. `resolved_sha` applies uniformly regardless of status.

**Population strategy:**

| Path                          | Source                                                                                     | Coverage                                  |
| ----------------------------- | ------------------------------------------------------------------------------------------ | ----------------------------------------- |
| `check_action` (sync)         | Look up `action_ref.ref_str()` in `version::Set` (semver_tags, major_tags, default_branch) | Tags + default branch                     |
| `check_action_remote` (async) | Use `resolve_ref_to_sha()` return value (currently discarded)                              | Everything including non-default branches |

**Rejected alternative:** Adding `current_sha` to `CheckStatus::Outdated` —
would only cover Outdated, not UpToDate/branch refs.

### D2: Cursor-based code actions (not diagnostic-only)

Current code actions are generated exclusively from `DiagnosticData` attached to
diagnostics. UpToDate refs produce no diagnostic, so they'd never get a code
action.

**New approach:** Store `CheckResult`s in `DocumentState` alongside diagnostics.
The code action handler checks two sources:

1. **Diagnostic-based** (existing): "Update to vX.Y.Z" for Outdated refs —
   unchanged
2. **Cursor-based** (new): "Pin to SHA" for any non-SHA ref the cursor overlaps

This cleanly separates concerns: diagnostics report problems, cursor actions
offer transformations.

**Rejected alternative:** Adding hint-level diagnostics for UpToDate refs — adds
visual noise to every up-to-date action line.

### D3: Two code actions for Outdated, one for UpToDate

| Status       | Action          | Title                      | SHA source                          |
| ------------ | --------------- | -------------------------- | ----------------------------------- |
| Outdated     | Pin current ref | `Pin {current} to SHA`     | `resolved_sha`                      |
| Outdated     | Update & pin    | `Update to {target} (SHA)` | `target.sha()` via `DiagnosticData` |
| UpToDate     | Pin to SHA      | `Pin to SHA`               | `resolved_sha`                      |
| Branch/other | Pin to SHA      | `Pin to SHA`               | `resolved_sha`                      |
| Already SHA  | —               | (no action)                | —                                   |

All actions use `CodeActionKind::REFACTOR` (not quickfix) since pinning isn't
fixing a problem.

### D4: Comment strategy — config-driven, surface-dependent

```toml
[pin]
comment = true  # default: true
```

| Surface                                  | `comment = true`                     | `comment = false` |
| ---------------------------------------- | ------------------------------------ | ----------------- |
| CLI `pin`                                | `@{sha} # {ref}` (existing behavior) | `@{sha}`          |
| CLI `update` (with `policy.pin = "sha"`) | `@{sha} # {tag}`                     | `@{sha}`          |
| LSP code action                          | `@{sha} # {ref}`                     | `@{sha}`          |
| MCP                                      | Follows config                       | Follows config    |

**Ghost text / inlay hints**: Out of scope for this spec. Complementary future
feature — show tag for SHA refs via `textDocument/inlayHint`. Would replace the
need for comments in editors while keeping files clean.

### D5: `DiagnosticData` extension

Extend `Outdated` variant to carry SHA data for the "Update & pin" action:

```rust
pub enum DiagnosticData {
    Outdated {
        target_ref: String,
        target_sha: Option<String>,  // NEW: for "Update & pin to SHA"
    },
    Unresolvable { ... },
}
```

`target_sha` is `Option` for backward compatibility — older diagnostics without
it simply don't offer the "Update & pin" action.

## Data Flow

### Current

```
Provider → version::Set → check_action() → CheckResult(status only)
    → status_to_diagnostic() → Diagnostic { data: DiagnosticData { target_ref } }
        → code_action handler → deserialize DiagnosticData → TextEdit(ref_range, target_ref)
```

### Proposed

```
Provider → version::Set → check_action() → CheckResult { status, resolved_sha }
    ├→ status_to_diagnostic() → Diagnostic { data: DiagnosticData { target_ref, target_sha } }
    │      → code_action handler → "Update to vX.Y.Z" (existing)
    │      → code_action handler → "Update & pin to SHA" (new, from target_sha)
    │
    └→ DocumentState.check_results[range] = (action_ref, resolved_sha)
           → code_action handler (cursor overlap) → "Pin to SHA" (new, from resolved_sha)
```

## SHA Resolution in `check_action` (sync path)

New helper function:

```rust
fn resolve_current_sha(ref_str: &str, versions: &version::Set) -> Option<String> {
    let parsed = version::Version::parse(ref_str);
    match &parsed {
        // Full semver: find exact match in semver_tags
        Version::Semver { .. } if parsed.is_full_semver() => versions
            .semver_tags()
            .iter()
            .find(|info| info.version() == &parsed)
            .map(|info| info.sha().to_string()),
        // Major-only (v4): look up in major_tags
        Version::Semver {
            major, minor: None, ..
        } => versions
            .major_tags()
            .get(major)
            .map(|info| info.sha().to_string()),
        // Branch: check default_branch
        Version::Branch(name) => versions
            .default_branch()
            .filter(|b| b.name() == name)
            .map(|b| b.sha().to_string()),
        _ => None,
    }
}
```

Returns `None` for non-default branches, unusual tag formats, etc. The async
path (`check_action_remote`) fills in the gap via `resolve_ref_to_sha()`.

## LSP Document State Extension

```rust
/// Stored per-action check result for cursor-based code actions.
pub struct ActionCheckEntry {
    /// Range of the ref portion (after @) in the document.
    pub ref_range: Range,
    /// Range of the full `uses:` value (for display/context).
    pub value_range: Range,
    /// The original ref string.
    pub ref_str: String,
    /// Resolved SHA of the current ref, if available.
    pub resolved_sha: Option<String>,
}
```

Stored in `DocumentState` as `Vec<ActionCheckEntry>`, populated during
`compute_diagnostics` for ALL `uses:` refs (not just diagnostic-producing ones).

## Code Action Generation

```rust
// In code_action handler, after existing diagnostic-based actions:
fn pin_actions_for_cursor(
    uri: &Uri,
    version: Option<i32>,
    cursor_range: &Range,
    check_entries: &[ActionCheckEntry],
    supports_document_changes: bool,
) -> Vec<CodeActionOrCommand> {
    check_entries.iter()
        .filter(|e| ranges_overlap(cursor_range, &e.ref_range))
        .filter(|e| e.resolved_sha.is_some())
        .filter(|e| !is_sha(&e.ref_str))  // skip already-pinned
        .map(|e| {
            let sha = e.resolved_sha.as_ref().unwrap();
            CodeAction {
                title: format!("Pin to SHA ({}...)", &sha[..7]),
                kind: Some(CodeActionKind::REFACTOR),
                edit: Some(make_edit(uri, version,
                    vec![TextEdit::new(e.ref_range, sha.clone())],
                    supports_document_changes)),
                ..Default::default()
            }
        })
        .map(CodeActionOrCommand::CodeAction)
        .collect()
}
```

For the "Update & pin" action on Outdated diagnostics, extend
`code_actions_for_diagnostics`:

```rust
// After existing "Update to {target_ref}" action:
if let Some(sha) = &data.target_sha {
    actions.push(CodeAction {
        title: format!("Update & pin to SHA ({}...)", &sha[..7]),
        kind: Some(CodeActionKind::REFACTOR),
        edit: Some(make_edit(uri, version,
            vec![TextEdit::new(diag.range, sha.clone())],
            supports_document_changes)),
        ..Default::default()
    });
}
```

## Comment Handling in Edits

When `config.pin.comment` is true and the code action produces a SHA edit,
append `# {original_ref}` to the line.

This requires the `TextEdit` to extend beyond just the ref range. Two
approaches:

1. **Single edit**: Replace from ref start to end-of-line with `{sha} # {ref}`
2. **Additional edit**: Insert `# {ref}` at end of line as a separate `TextEdit`

Option 2 is safer (doesn't interfere with trailing comments already present).
The code action handler should check for existing trailing comments and skip
insertion if one exists.

For the LSP, the `ActionCheckEntry` already stores `ref_str` (the original ref),
so the comment content is available.

## CLI: Honor `policy.pin` in `update`

Currently `update` always emits tag-style refs (`target.version().to_string()`).
When `policy.pin = "sha"` (global or from a matching `[[overrides]]` entry):

```rust
let new_ref = match config.policy_for(&action_ref).pin {
    Style::Sha => {
        let sha = resolve_sha(&registry, &action_ref, target.sha()).await?;
        format_pin_ref(&sha, target.version(), config.pin.comment)
    }
    Style::Tag | Style::Preserve => target.version().to_string(),
};
```

This reuses the existing `resolve_sha` logic from the `pin` command.

## Deliverables

| #  | Deliverable                                                                                            | Effort | Depends on |
| -- | ------------------------------------------------------------------------------------------------------ | ------ | ---------- |
| D1 | Add `resolved_sha: Option<String>` to `CheckResult` + populate in `check_action`/`check_action_remote` | S      | —          |
| D2 | Add `target_sha` to `DiagnosticData::Outdated` + populate in `status_to_diagnostic`                    | S      | D1         |
| D3 | Add `ActionCheckEntry` vec to `DocumentState` + populate during `compute_diagnostics`                  | S      | D1         |
| D4 | "Pin to SHA" cursor-based code action (UpToDate + branch refs)                                         | M      | D3         |
| D5 | "Update & pin to SHA" diagnostic-based code action (Outdated refs)                                     | S      | D2         |
| D6 | "Pin current to SHA" diagnostic-based code action (Outdated refs)                                      | S      | D2, D3     |
| D7 | Config key `[pin] comment = true` + wire into code actions and CLI pin                                 | S      | —          |
| D8 | CLI `update` honors `policy.pin = "sha"`                                                               | M      | D1, D7     |
| D9 | Tests: snapshot edits, code action generation, config interaction                                      | M      | D4-D8      |

**Total effort: L** (1-2 days, cross-cutting across core + LSP + CLI)

## Non-Goals

- **MCP implementation**: MCP is a stub. Define tool interface in this spec but
  don't implement.
- **Inlay hints for SHA refs**: Complementary feature (show tag as ghost text).
  Separate spec.
- **"Unpin to tag" code action**: Inverse operation. Separate feature.
- **Closest-tag suggestion for Unresolvable**: Existing TODO, orthogonal to this
  work.
- **Short SHA support**: Always use full 40-char SHA. No short SHA output.

## Risks

| Risk                                                                            | Likelihood | Impact           | Mitigation                                                                                                      |
| ------------------------------------------------------------------------------- | ---------- | ---------------- | --------------------------------------------------------------------------------------------------------------- |
| SHA from `version::Set` may differ from `resolve_ref_to_sha` for annotated tags | Low        | High (wrong pin) | `process_tags` already resolves to commit SHA; validate in tests with annotated tag fixtures                    |
| Cursor-based code actions unfamiliar pattern; may have editor quirks            | Medium     | Medium           | Test with VS Code + Neovim; cursor actions are standard LSP (used by rust-analyzer, typescript-language-server) |
| Adding field to `CheckResult` breaks downstream `new()` call sites              | Low        | Low              | Additive change; add `with_sha()` builder or update `new()` signature with default                              |
| Comment insertion on lines with existing trailing comments                      | Medium     | Low              | Check for existing `#` comment; skip or replace                                                                 |

## MCP Tool Interface (future, not implemented)

For completeness, the MCP `pinToSha` tool shape:

```typescript
pinToSha({
  path: string,          // workflow file path
  owner: string,
  repo: string,
  ref: string,           // current ref to pin
  host?: string,
  dryRun?: boolean,
}) -> {
  sha: string,           // resolved 40-char SHA
  originalRef: string,   // what was replaced
  diff: string,          // unified diff
  comment?: string,      // "# vX.Y.Z" if config enabled
}
```

## Open Questions

All resolved — none remaining.
