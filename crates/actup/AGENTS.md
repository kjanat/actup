# actup (CLI)

Binary crate. Entry point: `main.rs`. All user-facing output here.

## Module Map

| Module          | Purpose                                  | Key Types/Functions               |
| --------------- | ---------------------------------------- | --------------------------------- |
| `cli.rs`        | Clap args, command enum                  | `Cli`, `Command`, `CliExitCode`   |
| `commands/`     | Subcommand implementations               | `check`, `update`, `cache`, `pin` |
| `output.rs`     | Human/JSON formatting, colors            | `OutputFormat`, print helpers     |
| `common.rs`     | Shared command logic, result aggregation | `process_files`, `aggregate`      |
| `file_cache.rs` | File-based cache storage                 | `FileCache`, TTL handling         |

## Commands

| Command  | File                 | Notes                    |
| -------- | -------------------- | ------------------------ |
| `check`  | `commands/check.rs`  | Largest file (902 lines) |
| `update` | `commands/update.rs` | Alias: `fix`             |
| `pin`    | `commands/pin.rs`    | Force SHA pinning        |
| `cache`  | `commands/cache.rs`  | `info`, `clear`, `path`  |
| `config` | `commands/config.rs` | Show resolved config     |
| `schema` | `commands/schema.rs` | Output JSON schema       |

## Where to Look

| Task                  | Start Here                            |
| --------------------- | ------------------------------------- |
| Add new CLI flag      | `cli.rs` → `Cli`/`Command` structs    |
| Change output format  | `output.rs`                           |
| Add new subcommand    | `commands/mod.rs` + new file          |
| Modify exit codes     | `cli.rs` → `CliExitCode` enum         |
| Change cache location | `file_cache.rs` + `actup-core` config |

## Exit Codes

| Code | Meaning                       | Enum Variant        |
| ---- | ----------------------------- | ------------------- |
| 0    | Success / all up-to-date      | `Success`           |
| 1    | Outdated refs found           | `OutdatedFound`     |
| 2    | Unresolvable refs             | `UnresolvableFound` |
| 3    | Network/auth/rate-limit error | `EnvironmentError`  |

## Anti-Patterns (This Crate)

- **No provider logic here**: Use `actup-core::Provider` trait
- **No direct HTTP calls**: Delegate to provider crates
- **Ignored actions don't affect exit code** (like `UpToDate`)

## Conventions

- `--format json` for machine output; schema must be stable
- `--verbose` enables debug tracing
- `--dry-run` for update/fix/pin shows changes without applying
- Feature flags: `gitea`, `gitlab` for optional providers

## Tests

- Integration tests in `tests/cli_integration.rs`
- Fixtures in `tests/fixtures/` (workflow YAML samples)
- Use `tempfile::TempDir` for filesystem tests
