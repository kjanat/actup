# actup (CLI + LSP + MCP)

WE ARE STILL NOT EVEN IN ALPHA. WE ARE FREE TO CHANGE SIGNATURES, PUBLIC INTERFACES, ETC.

This repo contains **actup** (CLI), **actup-lsp** (Language Server), and **actup-mcp** (Model Context Protocol server) for checking/updating
GitHub-compatible Actions `uses:` refs across workflows + composite actions. You are an AI agent working in this codebase. Your job is to
make correct, reviewable changes that preserve user intent and don’t brick CI. Be paranoid and precise.

Before every commit, run `just fmt`, or `dprint fmt`.

<!--If you use the Serena mcp server, activate project: `project_name: "actup-v2"`.-->

## Prime directive

1. **Never silently change semantics.** Updating `uses:` refs changes what code runs in CI. Always follow config policy and leave a paper
   trail (diffs, logs, comments).
2. **Prefer deterministic outputs.** Same inputs → same results, regardless of machine/locale/time (except when intentionally fetching
   remote state).
3. **Network is flaky.** Handle offline / rate limit / auth missing gracefully, with actionable diagnostics.
4. **Security matters.** Tag refs are mutable. Pin-to-SHA is the safest default. Never “unpin” unless explicitly configured.
5. **Provider-neutral core.** Do not hardcode GitHub assumptions into `actup-core`. All host/API specifics go behind provider interfaces.

---

## Repo layout (expected)

Cargo workspace (Rust 2024):

- `crates/actup-core/`\
  Provider-neutral engine: parsing, discovery, policy, resolution orchestration, caching interface, patch planning.
- `crates/actup-provider-github/`\
  GitHub (and GitHub Enterprise) implementation: REST/GraphQL calls, ETag handling, rate limit parsing.
- `crates/actup-provider-*/`\
  Future providers (GitLab, Gitea, Bitbucket, local FS).
- `crates/actup-cli/`\
  CLI frontend: args, printing, JSON output, interactive prompts.
- `crates/actup-lsp/`\
  LSP server: diagnostics + code actions + bulk fixes.
- `crates/actup-mcp/`\
  MCP server: safe tools for AI agents to inspect and update.
- `schemas/`\
  JSON Schema / docs for config file(s).
- `docs/`\
  Design notes, release notes, contributor docs.

If the layout differs, keep the same separation: **core is host-agnostic**, providers are host-specific.

---

## What actup must support

### Files to scan

- Workflows: `.github/workflows/**/*.yml` and `.yaml`
- Composite actions: `.github/actions/**/action.yml` and `.yaml`
- Reusable workflows: `uses: owner/repo/.github/workflows/foo.yml@ref`

### What to extract

Parse `uses:` strings:

- `owner/repo@ref`
- `owner/repo/path@ref` (reusable workflow or action subpath)
- Optional host prefix (provider-aware):
  - `github.com/owner/repo@ref`
  - `ghe.mycorp.internal/owner/repo@ref`
  - `gitlab.com/group/project@ref`
  - `localhost/actions/foo@ref` (optional convention)
- Local: `./path` → ignore for version checks (but still validate YAML)

Normalize into a provider-neutral reference type:

```Rust
ActionRef {
  host: Option<String>,      // absent implies default provider host (usually github.com)
  owner_or_group: String,
  repo_or_project: String,
  subpath: Option<String>,   // e.g. ".github/workflows/x.yml" or "path/to/action"
  ref: String                // tag/branch/sha
}
```

### What to resolve

- `ref` can be:

  - Semver tag (`v1.2.3`, `1.2.3`)
  - Major tag (`v1`, `v2`)
  - Branch (`main`, `master`, `release/*`)
  - Full commit SHA
  - Short SHA (treat as invalid unless configured; prefer full SHA)

### What to output

- **Outdated** refs with suggested replacements (respecting policy)
- **Unresolvable** refs with error diagnostics
- **Unknown** status when network fails (do not mark as user error)
- Always include provider/host in structured outputs (CLI JSON, LSP metadata, MCP tool results).

---

## Provider-ready architecture (non-negotiable)

### Core principles

- `actup-core` must not know about:

  - GitHub REST paths (`/repos/.../tags`), GraphQL schemas, headers, pagination quirks
  - specific hostnames (github.com) beyond defaults in config resolution
  - “README.md location” conventions beyond “provider can fetch readme”
- `actup-core` *does* know about:

  - parsing `uses:` references and locating them in files
  - version policy (patch/minor/major/latest), pinning style, branch rules
  - diff planning + safe text edits for YAML
  - cache semantics and keying (but not where/how HTTP fetch works)

### Provider interface (Rust trait)

Provider crates implement a trait like:

```Rust
trait ActionProvider {
  kind() -> provider::Kind;
  host() -> String;                 // resolved host this provider serves
  cache_namespace() -> String;       // e.g. "github", "gitlab", "gitea"

  // Version discovery
  get_versions(ActionRepoId) -> version::Set;         // tags (+commit), majors, default branch head
  resolve_ref_to_sha(ActionRepoId, ref) -> Sha40;     // tag/branch -> full SHA
  get_default_branch(ActionRepoId) -> BranchName;

  // Metadata
  get_readme(ActionRepoId, ref, subpath?) -> Option<String>;
  get_action_yml(ActionRepoId, ref, subpath?) -> ActionYml; // raw + best-effort parsed

  // Provider errors must be structured: NotFound vs RateLimit vs Auth vs Network vs InvalidRef.
}
```

`ActionRepoId` includes host + owner/group + repo/project + optional subpath identity.

### Provider selection (“injection”)

A `provider::Registry` lives in `actup-core` and resolves providers by host:

- If `ActionRef.host` is absent → use configured `default_provider` / `default_host`.
- Else look up by host match:

  - exact host match (e.g., `ghe.mycorp.internal`)
  - fallback by provider kind mapping (e.g. `github` for any configured GHE hosts)

Providers are constructed with:

- auth config (token sources)
- API base URLs (for enterprise/self-host)
- shared HTTP client, retry policy, timeouts
- cache handle

### Provider-aware cache keys

Cache keys MUST include provider namespace + host:

```text
<provider_namespace>::<host>/<owner>/<repo>
```

Examples:

- `github::github.com/actions/checkout`
- `github::ghe.mycorp.internal/platform/actions-ci`
- `gitlab::gitlab.com/mygroup/ci-actions`
- `local::localhost/actions/foo`

Never let GitHub + GitHub Enterprise collide in cache.

---

## Configuration: discovery + precedence

### Discovery order (highest priority first)

1. Explicit `--config <path>` (CLI) / LSP setting `actup.configPath` / MCP arg `configPath`
2. Repo configs, found by walking up from file/workspace root:

   - `actup.toml`
   - `.actup.toml`
   - `.config/actup.toml`
   - `actup.yaml` / `actup.yml` (optional support; TOML preferred)
3. User/global config:

   - XDG: `$XDG_CONFIG_HOME/actup/config.toml` or `~/.config/actup/config.toml`
   - Windows: `%APPDATA%/actup/config.toml`

### Precedence

CLI flags override repo config override user config override defaults. LSP + MCP must match CLI resolution rules.

### Provider config structure

Config uses `default_host` (top-level), `[fetch]` for global fetch settings, and `[hosts."hostname"]` for per-host overrides:

```toml
# Default host for bare `owner/repo` refs (preset hosts like github.com work with zero config)
default_host = "github.com"

[fetch]
api_mode          = "graphql"  # "graphql" | "rest"
max_tags_per_repo = 10         # 1-255

[hosts."ghe.mycorp.internal"]
kind      = "github"
api_base  = "https://ghe.mycorp.internal/api/v3"
token_env = "GHE_TOKEN"

[hosts."gitlab.com"]
kind      = "gitlab"
api_base  = "https://gitlab.com/api/v4"
token_env = "GITLAB_TOKEN"
```

Provider-specific knowledge (presets, api_base inference, capabilities) is supplied by provider crates via `Registration` statics. See
`specs/config-refactor.md` for the full resolution algorithm.

Also:

- update policy + per-action overrides must match on **(host?, owner/repo, subpath?)**
- cache TTL + cache location strategy
- branch handling per provider if needed

If you add keys, update `schemas/`, docs, and tests.

---

## Version resolution rules (provider-neutral)

### Semver matching

- Accept tags with optional `v` prefix.
- Compare semver properly: `1.10.0` > `1.9.9`.
- Ignore non-semver tags unless current is non-semver or config allows it.

### Policy

- `patch`: stay within same `major.minor`
- `minor`: stay within same `major`
- `major`: allow next majors
- `latest`: max semver overall

### Repos that don’t match user preference

If no newer semver tags exist but branch head is newer:

- Default: **do not jump to branch** unless config allows it.
- Provide secondary suggestions (“default branch has newer commits”) in metadata.
- LSP/MCP may offer optional code action/tool result only if config permits.

### Multiple tags → same commit

- If “latest” tag and current resolve to same SHA: treat as **up-to-date** (no-op).
- Don’t churn between equivalent tags unless user explicitly requests normalization.

### Pinning

If `ref_style = sha`:

- Convert selected target tag/branch to full 40-char SHA.
- Optional comment `# vX.Y.Z` where safe.
- Never output short SHAs.

---

## Caching rules

### Where to cache

Default: OS cache dir:

- Linux: `~/.cache/actup/`
- macOS: `~/Library/Caches/actup/`
- Windows: `%LOCALAPPDATA%\\actup\\cache\\`

Repo-local cache is optional but **not default** and never writes into `.git/` unless explicitly configured.

### What to cache

Per repo (provider+host+owner+repo):

- ETag / Last-Modified (if provider supports it)
- fetched-at timestamp
- computed latest per major + overall
- tag -> sha mapping when needed for equivalence checks
- default branch + head sha

### TTL & invalidation

- Respect TTL from config (e.g. 24h default).
- CLI `--refresh` forces network.
- On rate limiting, reuse stale cache and label status as stale.

---

## Git network behavior (provider implementations)

- Use token if available (env or config); never log tokens.
- Detect rate limit responses and surface as **RateLimited** (not Unresolvable).
- Retries: limited, exponential backoff; short timeouts.
- Offline mode: cache-only, mark results unknown/stale.

---

## YAML editing rules (do not destroy formatting)

- Preserve comments/indentation/quoting.
- Avoid whole-file rewrite.
- Prefer range edits: replace only `@ref` portion.
- Support `uses:` plain and quoted forms.

Validation after edit:

- Re-parse YAML to confirm it’s valid.
- Ensure `uses:` strings remain syntactically correct.

---

## LSP behavior

Diagnostics:

- Outdated per policy → Warning
- Unresolvable ref (confirmed) → Error
- Network/rate-limited → Info (or no diag) + status message
- Floating branch refs → per config (warn/ignore/pin suggestion)

Code actions:

- Update to latest allowed
- Update all in file/workspace
- Pin to SHA
- For unresolvable: suggest closest matching tag if confident All edits must be minimal and versioned (don’t stomp user edits).

---

## CLI behavior

Must support:

- `check` (non-destructive)
- `update`/`fix` (applies edits)
- `pin` (force pin)
- output formats: human + JSON (schema stable; include provider+host always)

Exit codes:

- `0` success
- `1` outdated found (in check) if `--fail-on-outdated`
- `2` confirmed unresolvable refs
- `3` network/auth/rate-limit prevented verification (document)

---

## actup-mcp (tools for AI agents)

`actup-mcp` exposes a narrow, safe surface area so agents can inspect and update. Tools must be provider-aware and policy-aware, and
mutators must return diffs + reversible plans.

### MCP tool principles

- Read tools are pure.
- Mutators must return: what changed, file/ranges, unified diff, and a reversal plan.
- Respect the same config and provider selection rules as CLI/LSP.
- Idempotent operations: calling twice yields no extra changes.

### Tools (provider-aware)

- `actionGetReadme({ host?, owner, repo, ref? })`
- `actionGetActionYml({ host?, owner, repo, ref?, path? })`
- `getVersions({ host?, owner, repo, includeBranches?, includePrerelease? })`
- `getLatest({ host?, owner, repo, currentRef, policyOverride? })`
- `updateFile({ path, edits[], validateYaml })`
- `updateAllUsesToVersion({ workspaceRoot, match{host?, owner, repo, path?}, targetRef, includeCompositeActions, includeWorkflows, dryRun })`

All responses must include resolved provider+host and status (`upToDate|outdated|unknown|unresolvable|rateLimited`).

---

## Testing expectations (don’t skip)

Required tests:

- parsing `uses:` strings (with/without host prefix, quoting, subpaths)
- snapshot tests for edits (before → after)
- resolver tests with mocked provider responses:

  - semver tags
  - major tags not moved
  - multiple tags same commit
  - branch head newer than tags
  - rate limit + offline behavior
- config precedence + provider instance selection tests
- LSP diagnostics + code actions (minimal harness)
- MCP contract tests: validation, diff correctness, idempotency, refuses path traversal

Never rely on live network in tests.

---

## Code quality rules

- Rust edition: 2024 everywhere.
- `cargo fmt`, `cargo clippy --all-targets --all-features -D warnings`
- Use `tracing` for logs.
- Structured errors: distinguish user vs environment:

  - `InvalidRef` / `NotFound` (user)
  - `Network` / `RateLimit` / `Auth` (environment)

- No decorative comment separators (e.g., `// ========` or `// ------`). Use doc comments or blank lines instead.

---

## Commit message format

Use [Conventional Commits](https://www.conventionalcommits.org/) with these guidelines:

### Structure

```gitcommit
<type>(<scope>): <description>

[optional body]

[optional footer]
```

### Types

- `feat`: New feature
- `fix`: Bug fix
- `refactor`: Code change that neither fixes a bug nor adds a feature
- `docs`: Documentation only
- `test`: Adding or updating tests
- `chore`: Maintenance (deps, CI, tooling)
- `perf`: Performance improvement

Add `!` after type/scope for breaking changes: `feat(policy)!: change default`

### Rules

1. **Use backticks for code** in commit messages:
   - Good: `fix(check): handle None from \`GitHubProvider::new\``
   - Bad: `fix(check): handle None from GitHubProvider::new`

2. **Describe WHY, not just WHAT** — the diff shows what changed; the message explains the reasoning:
   - Good: `fix(discovery): use consistent deduplication to prevent future pattern overlap bugs`
   - Bad: `fix(discovery): check insert() return value`

3. **Never title commits "addresses PR feedback"** — describe the actual changes and their purpose:
   - Good: `fix(check): log provider errors to aid debugging auth failures`
   - Bad: `fix: address PR review comments`

4. **Keep subject line under 72 chars**, use body for details if needed

### Examples

```gitcommit
feat(cache): add file-based persistence with TTL

Store version data in ~/.cache/actup/ to avoid repeated API calls.
Uses JSON format with fetch timestamp for TTL validation.

Closes #42
```

```gitcommit
fix(provider): capture error from `GitHubProvider::new`

Previously discarded the error silently, making auth issues hard to
debug. Now logs with `tracing::error!` including the error message.
```

```gitcommit
refactor(check): use `itertools::unique()` for repo deduplication

Replaces verbose `collect::<HashSet<_>>().into_iter().collect()`
pattern. Clearer intent, same performance.
```

---

## Security notes (critical)

- Never execute workflow code.
- Treat repo files as untrusted input; prevent path traversal.
- Pin-to-SHA: resolve SHAs from provider APIs, not user strings.
- Do not store tokens in cache.
- MCP mutators must refuse writes outside workspace root unless explicitly allowed.

---

## When you change behavior, update docs

If you modify:

- config keys/defaults
- provider selection logic
- version resolution semantics
- diagnostics severity
- cache behavior
- MCP tool shapes/semantics

…update `schemas/`, docs, and tests.

---

## Quick mental checklist

- Did I keep core provider-neutral?
- Did I respect config + precedence?
- Did I avoid rewriting whole YAML files?
- Did I handle offline/rate-limit without false errors?
- Will this surprise CI users?

If any answer is “maybe,” tighten it up.

---

## Development workflow

This project uses [just](https://just.systems) as a command runner.\
Run `just` or `just --dump` to see available recipes.

### Common commands

```bash
# Build
just build          # Debug build
just build-release  # Release build
just cli            # Build CLI only (release)

# Test
just test           # Run all tests
just test-verbose   # Tests with output
just test-pkg actup-core  # Test specific package

# Lint
just lint           # Full lint (fmt + clippy strict)
just clippy         # Clippy only
just fmt            # Format code

# Run
just run check      # Run CLI with args
just run-release check --verbose

# Cache
just cache-info     # Show cache stats
just cache-clear    # Clear cache

# CI
just ci             # Run full CI checks locally

# Docs
just doc            # Generate docs
just schema         # Generate JSON schema

# Self-check
just self-check     # Run actup on this repo
```

### Crate structure

| Crate                   | Purpose                                                                |
| ----------------------- | ---------------------------------------------------------------------- |
| `actup-core`            | Provider-neutral engine: parsing, discovery, policy, caching interface |
| `actup-provider-github` | GitHub/GHE implementation: REST, GraphQL, rate limiting                |
| `actup-cli`             | CLI binary: check, update, pin, cache commands                         |
| `actup-lsp`             | Language Server Protocol implementation                                |
| `actup-mcp`             | Model Context Protocol server for AI agents                            |

### Performance notes

The CLI uses batch fetching for optimal performance:

1. **In-memory deduplication** (`CachingProvider`): Same repo only fetched once per run
2. **GraphQL batching**: Single query for all repos (requires `GITHUB_TOKEN`)
3. **Parallel REST fallback**: Concurrent requests when no token available
4. **File-based cache**: Results cached in `~/.cache/actup/` with configurable TTL

First run with cold cache: ~1s (network). Subsequent runs: ~30ms (cache hits).

---

## Reference and design

@DESIGN.md

@REFERENCE.md
