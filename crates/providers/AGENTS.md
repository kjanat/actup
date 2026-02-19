# actup-providers

Shared utility layer between `actup-core` and individual provider crates (github, gitlab, gitea). Not a "registry of all providers" despite
the name.

## Module Map

| Module          | Purpose                                | Key Types                             |
| --------------- | -------------------------------------- | ------------------------------------- |
| `action_yml.rs` | Parse action.yml/action.yaml files     | `RawActionYml`, `parse_action_yml`    |
| `tags.rs`       | Common tag processing across providers | `TagLike` trait, `process_tags`       |
| `client.rs`     | Shared HTTP client constants           | `DEFAULT_MAX_RETRIES`, `MAX_PER_PAGE` |

## Where to Look

| Task                     | Start Here                 |
| ------------------------ | -------------------------- |
| Change tag normalization | `tags.rs` → `process_tags` |
| Parse action metadata    | `action_yml.rs`            |
| Modify HTTP defaults     | `client.rs`                |

## Conventions

- All provider crates depend on this crate
- `TagLike` trait: providers implement to normalize their tag API responses
- `process_tags` converts provider-specific tag lists into `actup-core::Version` sets
- `parse_action_yml` has `cognitive_complexity` + `too_many_lines` suppressed — refactoring candidate

## Anti-Patterns

- **Not a provider registry**: Don't add provider-specific logic here
- **No HTTP calls**: Only constants and parsing utilities
