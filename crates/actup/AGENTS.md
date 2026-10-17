# actup (CLI)

Binary crate. Entry point: `main.rs`. All user-facing output here.

## Module Map

| Module          | Purpose                                  | Key Types/Functions               |
| --------------- | ---------------------------------------- | --------------------------------- |
| `cli.rs`        | Clap args, command enum                  | `Cli`, `Command`, `CliExitCode`   |
| `main.rs`       | Entry point, registry bootstrap          | `run()`, `build_registry()`       |
| `commands/`     | Subcommand implementations               | `check`, `update`, `cache`, `pin` |
| `output.rs`     | Human/JSON formatting, colors            | `OutputFormat`, print helpers     |
| `common.rs`     | Shared command logic, result aggregation | `process_files`, `aggregate`      |
| `file_cache.rs` | File-based cache storage                 | `FileCache`, TTL handling         |

## Commands

| Command       | File                 | Notes                                           |
| ------------- | -------------------- | ----------------------------------------------- |
| `check`       | `commands/check.rs`  | Largest file (902 lines)                        |
| `update`      | `commands/update.rs` | Alias: `fix`                                    |
| `pin`         | `commands/pin.rs`    | Force SHA pinning                               |
| `latest`      | `commands/latest.rs` | Quick lookup: positional arg, no subcommand     |
| `cache`       | `commands/cache.rs`  | `info`, `clear`, `path`                         |
| `config`      | `commands/config.rs` | Show resolved config                            |
| `schema`      | `commands/schema.rs` | Output JSON schema                              |
| `completions` | `main.rs` (inline)   | Shell completion generation, handled before CLI |

## Where to Look

| Task                  | Start Here                            |
| --------------------- | ------------------------------------- |
| Add new CLI flag      | `cli.rs` → `Cli`/`Command` structs    |
| Change output format  | `output.rs`                           |
| Add new subcommand    | `commands/mod.rs` + new file          |
| Modify exit codes     | `cli.rs` → `CliExitCode` enum         |
| Change cache location | `file_cache.rs` + `actup-core` config |
| Provider registration | `main.rs` → `build_registry()`        |

## Exit Codes

| Code | Meaning                                                          | Enum Variant       |
| ---- | ---------------------------------------------------------------- | ------------------ |
| 0    | Success / all up-to-date                                         | `Success`          |
| 1    | Outdated refs found, or `update`/`pin` skipped a ref             | `Outdated`         |
| 2    | User error: unresolvable refs, invalid config, invalid CLI input | `UserError`        |
| 3    | Network/auth/rate-limit error, or no provider for a ref's host   | `EnvironmentError` |

## Anti-Patterns (This Crate)

- **No provider logic here**: Use `actup-core::Provider` trait
- **No direct HTTP calls**: Delegate to provider crates
- **Ignored actions don't affect exit code** (like `UpToDate`)

## Conventions

- `--format json` for machine output; schema must be stable
- `--verbose` enables debug tracing
- Print through `anstream::println!` and `anstream::eprintln!`; `--color` sets
  `anstream::ColorChoice` once in `main.rs`, and the streams strip ANSI codes
  when colour is off
- `--dry-run` for update/fix/pin shows changes without applying
- Feature flags `github`, `gitlab`, `gitea` are placeholders that compile
  nothing; `default = ["github"]`
- `actup-provider-github` is always compiled in

## Provider Wiring

`main.rs` builds the provider `Registry`:

- GitHub: always registered (non-optional)
- GitLab, Gitea: registered behind the default features `gitlab` and `gitea`

## Tests

- Integration tests in `tests/cli_integration.rs` (834 lines, 41 tests)
- Fixtures in `tests/fixtures/` (workflow YAML samples)
- Use `tempfile::TempDir` for filesystem tests
- Schema regression test validates committed JSON schema matches generated
