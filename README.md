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
- **MCP**: stdio server with read tools and dry-runnable mutators confined to
  one root

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

# Turn `@<sha> # v4` back into `@v4` where the tag is still on that commit
actup pin --unpin

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
| `pin --unpin` | Unpin marked SHA pins back to their tag        |
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

With `--format json` or `toml`, `check` prints one entry per finding and
`update` and `pin` print a report with `dry_run` and the lists `edited`,
`skipped` and `errors`. Every entry names the file, line, action, owner, repo,
host, provider and `reference` (the ref as found). `host` is the ref's own host,
or the default host for a ref without one. An `edited` entry adds `to` (the ref
written) and `comment` when the line gets a version comment. A `skipped` entry
adds `reason` (`unplaceable`, `overlap` or `out_of_bounds`). An `errors` entry
adds `kind` (such as `not_found`, `rate_limited`, `provider_not_found`, `write`,
`changed` or `invalid_edit`) and `message`. With no workflow files, the report
has empty lists and `check` prints no findings.

`update` and `pin` write each file to a temp file in its directory, flush it,
give it the mode of the original and rename it over the original. A failed write
leaves the original intact. A write through a symlink replaces the file it
points to, and a leading byte order mark stays. They re-read each file before
writing it. When a line they edit differs from the scan, they leave the file
untouched, report each of its refs with kind `changed` and the message
`edit refused: <path> changed since scan`, and exit 3. They also parse the
edited text before writing it. When it is not valid YAML or holds a different
number of `uses:` values, they leave the file untouched, report each of its refs
with kind `invalid_edit`, and exit 2. A dry run reports the same refusal. The
LSP command `actup-lsp/fixAllInWorkspace` applies both checks and leaves out a
refused file.

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

### Pin Style

`policy.pin` sets how `update`, the LSP quick fixes, fix-all and the wasm
`planUpdates` write an outdated ref. An `[[overrides]]` entry can set it per
action.

| `pin`      | Tag ref `@v4.0.0` | SHA pin `@<old> # v4.0.0` |
| ---------- | ----------------- | ------------------------- |
| `preserve` | `@v4.2.0`         | `@<new> # v4.2.0`         |
| `sha`      | `@<new> # v4.2.0` | `@<new> # v4.2.0`         |
| `tag`      | `@v4.2.0`         | `@v4.2.0`                 |

The `# vX.Y.Z` comment replaces an old version comment. It is added only where
the line can take it: never next to another comment, and never on a line that
holds other `uses:` values or ends inside a scalar.

`actup pin --unpin` and the LSP refactor action `` actup: unpin to `v4` ``
rewrite `@<sha> # v4 note` to `@v4 # note`. They act only on a SHA pin whose
comment starts with a tag, and only when the fetched tags hold that tag on the
pinned commit. `pin --unpin` skips actions that `scan.ignore` matches. Neither
`update` nor fix-all unpins this way.

> [!WARNING]
> `pin = "tag"` unpins. It rewrites an outdated SHA pin to its target's tag,
> which the action's owner can move, and drops the version comment. Set `tag`
> only to leave SHA pins on purpose.

### Config Discovery

1. `-c, --config <path>` (or `ACTUP_CONFIG`) or LSP `configPath` (explicit). A
   relative path resolves against the CLI working directory or the LSP workspace
   root. A missing file is an error (exit code 2).
2. Repo: `actup.toml`, `.actup.toml`, `.config/actup.toml`, searched from the
   scanned path upward. The search stops at the first directory that contains
   `.git` (a directory, or a file in a worktree). The LSP searches only its
   workspace folder.
3. User: `$XDG_CONFIG_HOME/actup/config.toml` on every OS when `XDG_CONFIG_HOME`
   is an absolute path, otherwise `actup/config.toml` in the OS config dir
   (`~/.config`, `~/Library/Application Support`, `%APPDATA%`)

An unknown key in a config file or in an `ACTUP_*` variable is an error (exit
code 2). The message names the file or the variable and suggests the closest
known key.

The CLI and the LSP log every config source at `info`: the user file, the repo
file, the explicit file and the `ACTUP_*` variables that set config keys.
`actup config` lists the same sources from lowest to highest precedence, under
`sources` in JSON and TOML output.

### Environment Variables

**Authentication:**

- `GITHUB_TOKEN` - GitHub API authentication

**CLI defaults:**

- `ACTUP_CONFIG` - Config file path, same as `--config`
- `ACTUP_VERBOSE` - Enable verbose output
- `ACTUP_FORMAT` - Default output format (`human`, `json`, `toml`)
- `ACTUP_STRICT` - Enable strict mode for `check`

An empty value of one of these variables counts as unset.

**Config overrides:**

Every other `ACTUP_*` variable sets a config key over every config file. The
name after `ACTUP_` names the key path. `__` separates nesting levels, and a
single `_` stays part of a key name. Names match in any case. A value parses as
a boolean, number, array (`[..]`) or inline table (`{k = v}`), else as a string.

```sh
ACTUP_DEFAULT_HOST=github.com
ACTUP_POLICY__BUMP=patch
ACTUP_CACHE__TTL=3600
ACTUP_FETCH__MAX_TAGS_PER_REPO=20
ACTUP_SCAN__IGNORE='["my-org/*"]'
```

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

## MCP Server

`actup-mcp` serves actup over the Model Context Protocol on stdio:

```sh
cargo install --git https://github.com/kjanat/actup-v2 actup-mcp
```

```json
{
	"mcpServers": {
		"actup": { "command": "actup-mcp", "args": ["--root", "/path/to/repo"] }
	}
}
```

The server loads config from `--root` (default: its working directory) like the
CLI, and refuses every path outside that root, including symlinks that point out
of it.

| Tool                     | Kind    | Purpose                                                 |
| ------------------------ | ------- | ------------------------------------------------------- |
| `actionGetReadme`        | read    | README of an action repository at a ref                 |
| `actionGetActionYml`     | read    | Raw and parsed `action.yml` at a ref                    |
| `getVersions`            | read    | Semver tags, latest tag, optionally branches            |
| `getLatest`              | read    | Version the policy (or `policyOverride`) moves a ref to |
| `updateFile`             | mutator | Replace refs of named `uses:` values in one file        |
| `updateAllUsesToVersion` | mutator | Move every reference to one action under a directory    |

Every result carries `provider`, `host` and `status` (`upToDate`, `outdated`,
`unknown`, `unresolvable`, `rateLimited`). Mutators accept `dryRun`, return each
change with its line, byte range, before and after text, a unified diff and a
reversal plan, and change nothing when called again with the same arguments.
They write like `update`: atomically, after a re-read that refuses a file whose
edited lines changed since the scan. `updateAllUsesToVersion` and `updateFile`
refuse an edited text that is not valid YAML or holds a different number of
`uses:` values. `updateFile` skips that check with `validateYaml: false`.
`updateAllUsesToVersion` writes each ref in its `policy.pin` style, as `update`
does.

## Exit Codes

| Code | Meaning                                                          |
| ---- | ---------------------------------------------------------------- |
| 0    | Success / all up-to-date                                         |
| 1    | Outdated refs found, or `update`/`pin` skipped a ref             |
| 2    | User error: unresolvable refs, invalid config, invalid CLI input |
| 3    | Network/auth/rate-limit error, or no provider for a ref's host   |

## Files Scanned

- `.github/workflows/**/*.yml` / `.yaml`
- `.github/actions/**/action.yml` / `.yaml`
- Reusable workflows: `uses: owner/repo/.github/workflows/foo.yml@ref`
- `scan.extra_paths` globs, relative to the scan root, and any file passed as
  the path argument

actup reads a file by its extension. It parses `.yml` and `.yaml` files as YAML,
and Markdown files (`.md`, `.markdown`) through their fenced `yaml` or `yml`
blocks. In every other file it reads `uses:` values that contain `@` line by
line, and it never appends a version comment there.

actup rejects an `extra_paths` entry that is absolute or contains a `..`
segment. It skips any match that resolves outside the scan root through a
symlink, and `update` and `pin` refuse to write such a file.

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
