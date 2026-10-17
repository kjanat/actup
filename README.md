# actup

GitHub Actions version manager. CLI + LSP + MCP for checking and updating
`uses:` refs across workflows and composite actions.

**Status:** Pre-alpha. Interfaces may change.

## Features

- **Check** workflows for outdated action versions
- **Update** to latest versions (respecting semver policy)
- **Pin** refs to immutable commit SHAs for security
- **Providers**: GitHub, GitHub Enterprise, GitLab and Gitea/Forgejo
  (`docs/providers.md`)
- **LSP**: Editor diagnostics and code actions
- **MCP**: stdio server with read tools and dry-runnable mutators confined to
  one root

## Installation

```sh
# From source (requires Rust 1.96+)
cargo install --git https://github.com/kjanat/actup-v2 actup
```

### GitHub Action

The action downloads the release that its ref names, checks the archive against
the release's `SHA256SUMS.txt`, and runs it. Linux runners get the static musl
build.

```yaml
- uses: kjanat/actup-v2@<commit SHA of v1.2.3> # v1.2.3
  with:
    version: v1.2.3 # needed when the ref is a commit SHA
    command: check # check, update, pin or config
    args: --strict # split on whitespace
```

| Input               | Default               | Meaning                                                 |
| ------------------- | --------------------- | ------------------------------------------------------- |
| `command`           | `check`               | actup subcommand                                        |
| `args`              | `""`                  | Extra arguments after the subcommand                    |
| `working-directory` | `.`                   | Directory to run actup in                               |
| `version`           | `""`                  | Release tag to run when the action ref is not a release |
| `github-token`      | `${{ github.token }}` | Passed to actup as `GITHUB_TOKEN`                       |

A ref that is neither a release tag like `v1.2.3` nor paired with `version`
fails the step. Inside another composite action, `github.action_ref` names that
outer action, and actup then needs `version`. The download always comes from the
`kjanat/actup-v2` releases. Outputs: `exit-code` (see [Exit Codes](#exit-codes))
and `actup-bin`, the path of the verified binary for later steps.

`examples/workflows/` holds a check workflow and a reusable workflow that opens
an update pull request with the `token` secret it receives. After tagging a
release, `just examples-pin <tag>` pins both examples to the tag's commit.

### Container Image

Each release publishes `ghcr.io/kjanat/actup:<tag>` for `linux/amd64` and
`linux/arm64`, and `:latest` for releases without a prerelease suffix. The image
runs actup in `/work` as a non-root user.

```sh
docker run --rm -v "$PWD:/work" -e GITHUB_TOKEN ghcr.io/kjanat/actup:v1.2.3 check
# Run as your own user so `update` and `pin` can write your files
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/work" -e GITHUB_TOKEN ghcr.io/kjanat/actup:v1.2.3 update
```

## Quick Start

```sh
# Check current directory for outdated actions
actup check

# Check with strict mode (exit 1 if outdated)
actup check --strict

# Exit 1 only for major bumps or floating branch refs
actup check --fail-on major --fail-on floating

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
actup check --format table  # One aligned row per finding
actup check --format sarif  # SARIF 2.1.0 for code scanning
```

`--format table` prints the columns `LOCATION` (`file:line`), `ACTION`,
`CURRENT`, `TARGET` (`-` when there is none) and `STATUS` (the JSON status
name). Only `check` accepts `table` and `sarif`; other commands reject them,
also when `ACTUP_FORMAT` sets them.

`--format sarif` writes one result per finding other than up-to-date and ignored
refs, under one rule per status (`actup/outdated`, `actup/floating`,
`actup/unresolvable` and so on). Levels follow the LSP severities: `error` for
an unresolvable ref, `warning` for an outdated or floating ref, and `note` for
pinned and unknown refs, environment errors and same-commit pins. Paths are
relative to the scanned directory, and columns count UTF-16 code units. Files
that could not be read appear as execution notifications.

`check --github-annotations` also writes one `::error` or `::warning` workflow
command per error or warning finding to stderr, whatever `--format` is.

```yaml
- run: actup check --format sarif --github-annotations > actup.sarif
- uses: github/codeql-action/upload-sarif@v3
  if: always()
  with:
    sarif_file: actup.sarif
```

`check --fail-on <kind>` exits 1 when a finding of that kind exists. Repeat it
for several kinds. `outdated` covers outdated refs and branch refs to pin,
`major` covers outdated refs whose target is a newer major version, and
`floating` covers floating branch refs and branch refs to pin. `--strict` is
`--fail-on outdated`. Unresolvable refs exit 2 and environment errors exit 3
with or without `--fail-on`.

Human output is coloured only on a terminal. `--color always` and
`--color never` force the choice. Under the default `--color auto`, a non-empty
`NO_COLOR` turns colour off, and a `CLICOLOR_FORCE` other than `0` turns it on
for piped output. `NO_COLOR` wins when both are set.

Human output and LSP messages replace control characters other than tab and line
feed, zero-width and bidi control characters, and the byte order mark with
U+FFFD. In paths, `uses:` values, refs, versions and reasons they replace line
feeds too. JSON and TOML output keep the raw text.

With `--format json` or `toml`, `check` prints one entry per finding and
`update` and `pin` print a report with `dry_run` and the lists `edited`,
`skipped` and `errors`. Every entry names the file, line, action, owner, repo,
host, provider and `reference` (the ref as found). `host` is the ref's own host,
or the default host for a ref without one. An `edited` entry adds `to` (the ref
written) and `comment` when the line gets a version comment. A `skipped` entry
adds `reason`: `ignored` when `scan.ignore` matches the ref, or `unplaceable`,
`overlap` or `out_of_bounds` when the writer cannot edit it. Only the last three
make `update` and `pin` exit 1. When two edits of a file overlap, every edit of
that file is skipped with `overlap` and the file stays unchanged. An `errors`
entry adds `kind` (such as `not_found`, `rate_limited`, `provider_not_found`,
`write`, `changed` or `invalid_edit`) and `message`. With no workflow files, the
report has empty lists and `check` prints no findings.

An unresolvable ref that names no tag can get a suggested tag. A partial version
names the highest tag that completes it (`v4.2` names the highest `v4.2.x`). Any
other ref names the most similar tag when their Jaro-Winkler similarity is at
least 0.9 (`v42.0` names `v4.2.0`). `check` prints the suggestion on a `hint:`
line and adds `suggestion` to the JSON and TOML entry. The LSP offers a
`Change to` quick fix.

`check`, `update` and `pin` read every file first. They then fetch the versions
of each repo that an unignored ref names, once per repo and in one batch through
the version cache, before they report or edit.

While they fetch, they draw a one-line `resolving N repos` counter on stderr
when stderr is a terminal. Piped and redirected runs print no progress.

`--offline` reads versions only from the cache. A repo missing from the cache,
or cached longer ago than `cache.ttl`, makes its refs environment errors (exit
3), and no command touches the network. `--no-cache` neither reads nor writes
the cache, the same as `cache.enabled = false`. The two flags cannot be
combined. The quick lookup `actup owner/repo` uses the cache too.

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

Create `actup.toml` or `.actup.toml` in your repo root, or write the user config
(see [Config Discovery](#config-discovery)):

```toml
# Default host for bare owner/repo refs (user or explicit config only)
default_host = "github.com"

[policy]
bump       = "minor"  # patch | minor | major | latest
pin        = "preserve"  # preserve | tag | sha
track      = "keep"  # keep | major | exact
branch     = "warn"  # ignore | warn | update
prerelease = false

[scan]
ignore      = ["my-org/*", "dtolnay/rust-toolchain@1.91.0"]
extra_paths = [".gitea/workflows"]  # user or explicit config only

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

# Enterprise GitHub (api_base and token_env: user or explicit config only)
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

A pattern whose owner or repo holds any other character is an error at config
load. Glob syntax is the exception: `*`, `?` and a `[...]` class, whose members
may also be `!` and `]`. Outside a class, `!` and `]` are errors. So is a host
with a glob port such as `ghe.corp:*`; write the port number.

### Pin Style

`policy.pin` sets how `update`, the LSP quick fixes, fix-all and the wasm
`planUpdates` write an outdated ref. An `[[overrides]]` entry can set it per
action.

| `pin`      | Tag ref `@v4.0.0` | SHA pin `@<old> # v4.0.0` |
| ---------- | ----------------- | ------------------------- |
| `preserve` | `@v4.2.0`         | `@<new> # v4.2.0`         |
| `sha`      | `@<new> # v4.2.0` | `@<new> # v4.2.0`         |
| `tag`      | `@v4.2.0`         | `@v4.2.0`                 |

The `# vX.Y.Z` comment replaces the version of an old version comment and keeps
the note after it: `# v4.0.0 pinned for node16` becomes
`# v4.2.0 pinned for node16`. A tag ref moved to a new tag refreshes its version
comment the same way. It is added only where the line can take it: never next to
another comment, never on a line that holds other `uses:` values or ends inside
a scalar, and never in a plain-text file. The comment goes after the closing
quote and the flow `}` or `]` that close the value, as in
`{ uses: "o/b@<sha>" } # v4.2.0`. Other text after the value, such as more flow
keys, means no comment. The CLI, the LSP, the MCP server and the wasm package
follow the same rule.

`actup pin --unpin` and the LSP refactor action `` actup: unpin to `v4` ``
rewrite `@<sha> # v4 note` to `@v4 # note`. They act only on a SHA pin whose
comment starts with a tag, and only when the fetched tags hold that tag on the
pinned commit. `pin --unpin` skips actions that `scan.ignore` matches. Neither
`update` nor fix-all unpins this way.

> [!WARNING]
> `pin = "tag"` unpins. It rewrites an outdated SHA pin to its target's tag,
> which the action's owner can move, and drops the version from its version
> comment. A note after the version stays as the comment. Set `tag` only to
> leave SHA pins on purpose.

### Config Discovery

1. `-c, --config <path>` (or `ACTUP_CONFIG`) of the CLI and `actup-mcp`, or LSP
   `configPath` (explicit). A relative path resolves against the working
   directory or the LSP workspace root. A missing file is an error (exit code
   2).
2. Repo: `actup.toml`, `.actup.toml`, `.config/actup.toml`, searched from the
   scanned path upward. The search stops at the first directory that contains
   `.git` (a directory, or a file in a worktree). The LSP searches only its
   workspace folder, and `actup-mcp` only its `--root`.
3. User: `$XDG_CONFIG_HOME/actup/config.toml` on every OS when `XDG_CONFIG_HOME`
   is an absolute path, otherwise `actup/config.toml` in the OS config dir
   (`~/.config`, `~/Library/Application Support`, `%APPDATA%`)

A repo config arrives with the repository, so actup ignores the keys in it that
choose where a token goes, which files get rewritten or where cached SHAs come
from: `default_host`, `scan.extra_paths`, `hosts.<host>.api_base`,
`hosts.<host>.token_env` and `cache.path`. actup warns with the key and the
file, and `actup config` lists the ignored keys. Set them in the user config or
in a file passed with `--config` or `configPath`. A non-preset host that only
the repo config declares gets no token.

The disk cache lives in `cache.path`, else in the OS cache dir
(`~/.cache/actup`, `~/Library/Caches/actup`, `%LOCALAPPDATA%\actup\cache`). With
neither, the CLI and the LSP warn once and run without a disk cache, and
`actup cache info` and `actup cache path` report that there is no cache dir.
`actup cache clear` deletes only the entry files actup writes and keeps every
other file in the directory.

An unknown key in a config file or in an `ACTUP_*` variable is an error (exit
code 2). The message names the file or the variable and suggests the closest
known key. A top-level `$schema` string, such as
`"$schema": "./schemas/actup-config.schema.json"`, is accepted in every format
and ignored. In a nested table, `$schema` is an unknown key.

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
- `ACTUP_COLOR` - Colour mode, same as `--color` (`auto`, `always`, `never`)

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

The server reads the user config and a repo config inside `--root` (default: its
working directory). Its config search stops at that root. `-c, --config <path>`
(or `ACTUP_CONFIG`) loads an explicit config file over the repo and user config.
A relative path resolves against the working directory. A config that fails to
load exits with code 2. The server refuses every path outside the root,
including symlinks that point out of it.

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
`uses:` values. `updateFile` skips that check with `validateYaml: false`, and
refuses edits that name actions on more than one host. `updateAllUsesToVersion`
writes each ref in its `policy.pin` style, as `update` does. `getLatest` adds
`refKind` (`tag`, `branch` or `sha`) when the version list names `currentRef`,
and `sameCommit` (the target version) when the target sits on the commit that
`currentRef` already points at.

## Exit Codes

| Code | Meaning                                                                        |
| ---- | ------------------------------------------------------------------------------ |
| 0    | Success / all up-to-date                                                       |
| 1    | A `check --fail-on` kind found, or `update`/`pin` skipped a ref it cannot edit |
| 2    | User error: unresolvable refs, invalid config, invalid CLI input               |
| 3    | Network/auth/rate-limit error, or no provider for a ref's host                 |

## Files Scanned

- `.github/workflows/*.yml` / `.yaml`
- `action.yml` / `.yaml` and `.github/actions/*/action.yml` / `.yaml`
- Reusable workflows: `uses: owner/repo/.github/workflows/foo.yml@ref`
- `scan.extra_paths` globs, relative to the scan root, and any file passed as
  the path argument. A directory that an entry matches, such as
  `.gitea/workflows`, expands to the `*.yml` and `*.yaml` files directly in it
  and to `*/action.yml` and `*/action.yaml` below it.

actup reads a file by its extension. It parses `.yml` and `.yaml` files as YAML,
and Markdown files (`.md`, `.markdown`) through their fenced `yaml` or `yml`
blocks. In every other file it reads `uses:` values that contain `@` line by
line. It never writes or rewrites a version comment on such a line, and it
checks an edit there for an unchanged `uses:` count only.

actup rejects an `extra_paths` entry that is absolute or contains a `..`
segment. It skips any match that resolves outside the scan root through a
symlink. `update`, `pin` and the `actup-mcp` mutators refuse to write a file
that resolves outside the root.

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
just self-check-pins # Fail on a uses: ref not pinned to a commit SHA

# CI
just ci             # Full CI checks locally
```

## License

MIT OR Apache-2.0
