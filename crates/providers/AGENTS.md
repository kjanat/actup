# actup-providers

Shared utility layer between `actup-core` and individual provider crates
(github, gitlab, gitea). Not a "registry of all providers" despite the name.

## Module Map

| Module          | Purpose                                         | Key Types                                           |
| --------------- | ----------------------------------------------- | --------------------------------------------------- |
| `action_yml.rs` | Parse action.yml/action.yaml files              | `RawActionYml`, `parse_action_yml`                  |
| `tags.rs`       | Common tag processing across providers          | `TagLike` trait, `process_tags`                     |
| `ls_refs.rs`    | List remote tags and branches over git protocol | `list_refs`, `RemoteRefs`, `RemoteTag`, `RepoLabel` |
| `client.rs`     | Shared HTTP client constants                    | `DEFAULT_MAX_RETRIES`, `MAX_PER_PAGE`               |
| `lib.rs`        | Re-exports and the loaded-filename helper       | `filename_label`                                    |

## Where to Look

| Task                     | Start Here                                   |
| ------------------------ | -------------------------------------------- |
| Change tag normalization | `actup-core` `version.rs` → `Set::from_tags` |
| Parse action metadata    | `action_yml.rs`                              |
| Change git ref listing   | `ls_refs.rs` → `list_refs`                   |
| Modify HTTP defaults     | `client.rs`                                  |

## Conventions

- All provider crates depend on this crate
- `TagLike` trait: providers implement to normalize their tag API responses
- `process_tags` converts any iterator of `TagLike` items into an
  `actup_core::version::Set` through `Set::from_tags`, which the wasm bridge
  calls directly
- `parse_action_yml(raw, path)` takes the loaded filename from `filename_label`
  for its error messages

## Anti-Patterns

- **Not a provider registry**: Don't add provider-specific logic here
- **Network access is host-neutral**: `list_refs` speaks the git smart-HTTP
  protocol every host shares; host REST and GraphQL APIs belong in provider
  crates
