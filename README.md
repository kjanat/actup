# actup

GitHub Actions version manager. CLI + LSP + MCP for checking and updating
`uses:` refs across workflows and composite actions.

**Status:** Pre-alpha. Interfaces may change.

## Features

- **Check** workflows for outdated action versions
- **Update** to latest versions (respecting semver policy)
- **Pin** refs to immutable commit SHAs for security
- **Providers**: GitHub and GitHub Enterprise. GitLab and Gitea/Forgejo are not
  wired yet (#48)
- **LSP**: Editor diagnostics and code actions
- **MCP**: Tools for AI agents

## Installation

```sh
# From source (requires Rust 1.96+)
cargo install --git https://github.com/kjanat/actup-v2 actup
```

## Quick Start

```sh
# Check current directory for outdated actions
actup check

# Check with strict mode (exit 1 if outdated)
actup check --strict

# Update all actions to latest versions
actup update

# Pin all actions to their current SHA
actup pin

# Preview changes without applying
actup update --dry-run
```

## Commands

| Command       | Description                                    |
| ------------- | ---------------------------------------------- |
| `check`       | Scan workflows, report outdated (read-only)    |
| `update`      | Update actions to latest versions              |
| `fix`         | Alias for `update`                             |
| `pin`         | Pin all actions to their current commit SHA    |
| `config`      | Show effective configuration                   |
| `cache`       | Manage version cache (`info`, `clear`, `path`) |
| `schema`      | Output JSON schema for config file             |
| `completions` | Generate shell completions                     |

### Output Formats

```sh
actup check --format json   # Machine-readable JSON
actup check --format toml   # TOML format
actup check --format human  # Default, colored output
```

With `--format json` or `toml`, `update` and `pin` print a report with `edited`,
`dry_run` and a `skipped` list. Each skipped entry names the file, line, action,
host (when written), provider and `reason` (`unplaceable`, `overlap` or
`out_of_bounds`).

## Configuration

Create `actup.toml` or `.actup.toml` in your repo root:

```toml
# Default host for bare owner/repo refs
default_host = "github.com"

[policy]
bump       = "minor"  # patch | minor | major | latest
pin        = "preserve"  # preserve | tag | sha
track      = "keep"  # keep | major | exact
branch     = "warn"  # ignore | warn | update
prerelease = false

[scan]
ignore      = ["my-org/*", "dtolnay/rust-toolchain@1.91.0"]
extra_paths = [".gitea/workflows"]

# Ordered; for each policy key the last matching entry wins.
# Patterns are `[host/]owner/repo[/subpath][@ref]` globs.
[[overrides]]
pattern = "my-org/*"
policy  = { pin = "sha" }

[[overrides]]
pattern = "ghe.example.com/platform/*"
policy  = { bump = "patch", track = "exact" }

# API fallback, used only when a host refuses a git connection
[fetch]
api_mode          = "graphql"  # "graphql" or "rest"
max_tags_per_repo = 10  # Tags to fetch per repo (1-255)

# Enterprise GitHub
[hosts."ghe.example.com"]
kind      = "github"
api_base  = "https://ghe.example.com/api/v3"
token_env = "GHE_TOKEN"
```

### Action Patterns

`scan.ignore` and `[[overrides]]` patterns read a host with the `uses:` rule.
The first segment is a host when the pattern has at least three segments and
that segment is `localhost`, a dotted DNS name, a DNS name with a `:port`, or a
bracketed IPv6 literal such as `[::1]`. Glob characters count as DNS name
characters, so `*.corp.com/o/*` and `ghe.*/o/r` name hosts. A bracketed IPv6
host matches literally.

Host matching ignores case, and a ref without a host matches as `default_host`.
A pattern without a host matches every host. Owner, repo, subpath and ref match
case-sensitively. `*` stays within one segment and `**` spans segments.

In `uses:` refs, actup lower-cases the host. Owner and repo must match
`[A-Za-z0-9._-]+` and must not be `.` or `..`.

### Config Discovery

1. `--config <path>` or LSP `configPath` (explicit). A relative path resolves
   against the CLI working directory or the LSP workspace root. A missing file
   is an error.
2. Repo: `actup.toml`, `.actup.toml`, `.config/actup.toml`
3. User: `$XDG_CONFIG_HOME/actup/config.toml` on every OS when `XDG_CONFIG_HOME`
   is an absolute path, otherwise `actup/config.toml` in the OS config dir
   (`~/.config`, `~/Library/Application Support`, `%APPDATA%`)

### Environment Variables

**Authentication:**

- `GITHUB_TOKEN` - GitHub API authentication

**CLI defaults:**

- `ACTUP_VERBOSE` - Enable verbose output
- `ACTUP_FORMAT` - Default output format (`human`, `json`, `toml`)
- `ACTUP_STRICT` - Enable strict mode for `check`

## Shell Completions

Shell is auto-detected from `$SHELL` if omitted:

```sh
# Auto-detect shell
actup completions > completions.sh

# Or specify explicitly
actup completions bash > ~/.local/share/bash-completion/completions/actup
actup completions zsh > ~/.zfunc/_actup
actup completions fish > ~/.config/fish/completions/actup.fish
actup completions powershell >> $PROFILE
```

## Exit Codes

| Code | Meaning                                                          |
| ---- | ---------------------------------------------------------------- |
| 0    | Success / all up-to-date                                         |
| 1    | Outdated refs found, or `update`/`pin` skipped a ref             |
| 2    | User error: unresolvable refs, invalid config, invalid CLI input |
| 3    | Network/auth/rate-limit error                                    |

## Files Scanned

- `.github/workflows/**/*.yml` / `.yaml`
- `.github/actions/**/action.yml` / `.yaml`
- Reusable workflows: `uses: owner/repo/.github/workflows/foo.yml@ref`

## Crates

| Crate                   | Purpose                       |
| ----------------------- | ----------------------------- |
| `actup`                 | CLI binary                    |
| `actup-core`            | Provider-neutral engine       |
| `actup-provider-github` | GitHub/GHE provider           |
| `actup-provider-gitlab` | GitLab provider               |
| `actup-provider-gitea`  | Gitea/Forgejo provider        |
| `actup-lsp`             | Language Server Protocol      |
| `actup-mcp`             | Model Context Protocol for AI |

## Development

```sh
# Build
just build          # Debug
just build-release  # Release

# Test
just test           # All tests
just lint           # Format + clippy

# Run
just run check      # Run CLI
just self-check     # Run actup on this repo

# CI
just ci             # Full CI checks locally
```

## License

MIT OR Apache-2.0
