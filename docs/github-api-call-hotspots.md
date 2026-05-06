# GitHub API Call Hotspots

This report maps where `actup` currently spends the most GitHub API budget, when those paths fire, and what to fix first.

## Summary

The biggest cost is not one noisy endpoint. It is the repeated use of the expensive `GitHubProvider::get_versions()` primitive across CLI
and LSP flows.

That primitive currently does:

- `GET /repos/{owner}/{repo}/tags` for every tags page
- `GET /repos/{owner}/{repo}`
- `GET /repos/{owner}/{repo}/branches/{default_branch}`

Source:

- `crates/provider-github/src/provider.rs:56-82`
- `crates/provider-github/src/client.rs:133-159`
- `crates/provider-github/src/client.rs:184-244`

Rough cost per repo on a cold cache:

- `ceil(tag_count / 100) + 2` REST calls

Examples:

- 40 tags: 3 calls
- 350 tags: 6 calls

## GitHub Rate Limits

Baseline budgets (per hour, per IP or token):

| Mode            | Limit           |
| --------------- | --------------- |
| Unauthenticated | 60 requests/h   |
| Authenticated   | 5000 requests/h |

Using an auth token multiplies the budget ~83×. For heavy flows (CLI `check` across a large workspace, LSP diagnostics on open), always use
authenticated requests — an unauthenticated budget is exhausted by ~20 repos with moderate tag counts.

Check remaining budget via the `x-ratelimit-remaining` response header (returned on every GitHub API response) or query `GET /rate_limit`
directly. The `GitHubProvider::get_versions()` primitive costs `ceil(tags/100) + 2` calls per repo, so 10 repos with 100 tags each consumes
~40 of the 5000 budget on a cold cache.

## Hotspots

### Core version fetch primitive

|                   |                                                                             |
| ----------------- | --------------------------------------------------------------------------- |
| **Where**         | `crates/provider-github/src/provider.rs:56-82`                              |
| **When**          | Any caller asks for versions                                                |
| **Why expensive** | Fetches all tag pages, then repo info, then default branch                  |
| **Cost**          | `ceil(tags/100)+2` per repo                                                 |
| **Remedy**        | Keep as low-level primitive, but stop calling it repeatedly from bulk flows |

### CLI `check` bulk fetch

|                   |                                                                                                             |
| ----------------- | ----------------------------------------------------------------------------------------------------------- |
| **Where**         | `crates/actup/src/commands/check.rs:177-195`, `348-394`                                                     |
| **When**          | `actup check` on cold or stale cache                                                                        |
| **Why expensive** | Dedupes repos, but calls `provider.get_versions()` per uncached repo with `join_all` (no concurrency bound) |
| **Cost**          | Unique repos × primitive cost, all in one burst                                                             |
| **Remedy**        | Route GitHub repos through `BatchFetcher`; add bounded fallback concurrency                                 |

### CLI `check` fallback SHA resolution

|                   |                                                                            |
| ----------------- | -------------------------------------------------------------------------- |
| **Where**         | `check.rs:214-247`, `core/src/resolution.rs:148-178`, `provider.rs:85-134` |
| **When**          | Batch version data cannot prove a semver ref is valid                      |
| **Why expensive** | Extra `resolve_ref_to_sha()` call per affected action                      |
| **Cost**          | +1 to +2 calls per affected action                                         |
| **Remedy**        | Cache `resolve_ref_to_sha(repo, ref)` results within the run               |

### CLI `update` per-action refetch

|                   |                                                                        |
| ----------------- | ---------------------------------------------------------------------- |
| **Where**         | `crates/actup/src/commands/update.rs:69-90`                            |
| **When**          | `actup update` / `actup fix`                                           |
| **Why expensive** | Calls `provider.get_versions()` once per `uses:` entry, no repo dedupe |
| **Cost**          | Repeats full tag scan for same repo                                    |
| **Remedy**        | Batch by unique repo first, or wrap provider in `CachingProvider`      |

### CLI `pin` per-action refetch + SHA lookup

|                   |                                                                 |
| ----------------- | --------------------------------------------------------------- |
| **Where**         | `crates/actup/src/commands/pin.rs:31-68`, `99-111`              |
| **When**          | `actup pin`                                                     |
| **Why expensive** | Fetches versions per action, then may also resolve ref directly |
| **Cost**          | Primitive cost, sometimes +1 to +2 more                         |
| **Remedy**        | Reuse unique repo version map and memoize SHA resolution        |

### CLI `latest` one-off lookup

|                   |                                                                      |
| ----------------- | -------------------------------------------------------------------- |
| **Where**         | `crates/actup/src/commands/latest.rs:131-141`                        |
| **When**          | `actup owner/repo`                                                   |
| **Why expensive** | Still pays full `get_versions()` cost                                |
| **Cost**          | Primitive cost once per invocation                                   |
| **Remedy**        | Lower priority; improved automatically if shared fetch path is fixed |

### LSP diagnostics repo fetch

|                   |                                                                     |
| ----------------- | ------------------------------------------------------------------- |
| **Where**         | `crates/lsp/src/diagnostics.rs:114-172`, `293-326`                  |
| **When**          | On open, save, and after debounced edits                            |
| **Why expensive** | Dedupes repos per document, but fetches uncached repos sequentially |
| **Cost**          | Unique repos in document × primitive cost                           |
| **Remedy**        | Use GitHub batch fetcher and keep semaphore-bounded fallback        |

### LSP workspace fix-all

|                   |                                                                                    |
| ----------------- | ---------------------------------------------------------------------------------- |
| **Where**         | `crates/lsp/src/workspace_commands.rs:223-225`, `329-405`                          |
| **When**          | `fixAllInWorkspace` command                                                        |
| **Why expensive** | Runs `compute_diagnostics()` per file; cold-cache workspace scans pay repo-by-repo |
| **Cost**          | Sum of unique repos across scanned files × primitive cost                          |
| **Remedy**        | Add workspace-level repo aggregation before per-file diagnostics                   |

### LSP cache host mismatch

|                   |                                                                                       |
| ----------------- | ------------------------------------------------------------------------------------- |
| **Where**         | `crates/lsp/src/file_cache.rs:110-124`, `diagnostics.rs:129-143`                      |
| **When**          | Any non-default host lookup in LSP                                                    |
| **Why expensive** | Cache key uses `default_host` instead of `provider.host()`, causing misses/collisions |
| **Cost**          | Turns expected cache hits into refetches                                              |
| **Remedy**        | Use `provider.host()` in LSP cache keys                                               |

### Hover metadata fetch

|                   |                                                                                   |
| ----------------- | --------------------------------------------------------------------------------- |
| **Where**         | `crates/lsp/src/hover.rs:135-188`, `312-359`, `provider.rs:191-242`               |
| **When**          | First hover on a unique action ref                                                |
| **Why expensive** | Fetches versions, then `action.yml`; `.yaml` fallback can add one more request    |
| **Cost**          | Primitive version cost + 1–2 content calls                                        |
| **Remedy**        | Lower priority; keep cache, skip metadata fetch until hover panel actually needed |

## Dead Or Unwired Capacity

The repo contains GitHub batching infrastructure that production paths do not yet use, alongside in-process memoization that IS wired in.

### Wired in (active)

- **`CachingProvider` with `Semaphore`-based singleflight** (`crates/core/src/caching_provider.rs:51-237`): Both CLI (`main.rs:191`) and LSP
  (`provider_setup.rs:55`) wrap providers in `CachingProvider`. The singleflight deduplicates concurrent requests for the same resource —
  e.g., many files needing `actions/checkout` versions simultaneously collapse into a single upstream fetch. Followers wait on the
  `Semaphore` and read the leader's cached result.

### Unwired (dead code in production)

- **`BatchFetcher`** (`crates/provider-github/src/batch.rs:17-212`): Exists with full GraphQL batching support, but no CLI or LSP call site
  uses it. Only exercised in unit tests.
- **GraphQL query path** (`crates/provider-github/src/graphql.rs:89-125`): Reachable only through `BatchFetcher`, which is itself unwired.
- **`api_mode` and `max_tags_per_repo` config knobs**: Provider creation (`crates/provider-github/src/lib.rs:79-82`) ignores these resolved
  values even though config defaults to GraphQL (`crates/core/src/config/mod.rs:216-283`).

### Impact

- The repo advertises GraphQL batching as a normal path, but real CLI and LSP traffic uses per-repo REST exclusively.
- `CachingProvider` singleflight prevents redundant concurrent fetches within a process, but cannot help with the initial cold-cache burst —
  that requires `BatchFetcher`.

## Highest-Impact Remedies

1. Wire GitHub bulk callers to `BatchFetcher` and honor `api_mode` plus `max_tags_per_repo`. Largest single API quota cost — every repo
   triggers a full REST pagination sequence today even though the batching infrastructure exists.
2. Wrap runtime providers in `CachingProvider` so `update`, `pin`, `latest`, hover, and fallback checks stop repeating the same fetches
   within one process. Most frequent source of redundant calls; affects every command that touches the same repo more than once.
3. Fix the LSP cache key bug to use `provider.host()` instead of `default_host`. Silently poisons cache for GHE users, causing auth failures
   or stale data that are hard to diagnose.
4. Memoize `resolve_ref_to_sha(repo, ref)` in `check` and `pin` paths. Low code complexity to fix; eliminates O(n) duplicate SHA lookups
   when multiple files reference the same action.
5. Add bounded concurrency anywhere REST fallback remains, especially CLI `check`. Without limits, large workspaces can burst hundreds of
   parallel requests, triggering rate limits or socket exhaustion.

## When To Fix What

Fix immediately:

- LSP cache host bug — GHE users hit silent auth/data corruption; high user-pain frequency with no workaround.
- Unused `api_mode` / batch path in production fetches — the code exists but isn't wired in, so every user pays the full REST cost
  unnecessarily.
- CLI `update` and `pin` repeated per-action fetches — each action ref triggers its own fetch even within the same run; quadratic API cost
  in common workflows.

Fix soon if large repos or big workspaces matter:

- Workspace-level LSP repo aggregation — scales poorly as workspace size grows; multiplies diagnostics-time API calls.
- SHA resolution memoization via `resolve_ref_to_sha` — easy win, low complexity fix, but only noticeable in repos with many refs to the
  same action.
- REST concurrency limits in CLI `check` — unbounded parallelism risks rate-limit errors under load; less urgent for small repos.

Lower priority:

- `latest` command optimization — single-repo command, called infrequently; low total API cost.
- Hover metadata fetch tuning — user-initiated, one-at-a-time; rate-limit risk is minimal.

## Expected Wins

If GitHub batching is wired into CLI `check` and LSP diagnostics, GitHub API volume should drop from roughly:

- one multi-call REST sequence per repo

to roughly:

- one GraphQL batch request for many repos, plus occasional fallback calls

**Concrete example:** A workspace with 50 `uses:` refs across 10 distinct repos (~100 tags each). Before: ~120 REST calls (tags pagination +
default-branch lookup per repo, repeated across files). After: ~1–2 GraphQL batch requests + occasional REST fallback for repos that exceed
the query budget. Estimated impact: **60–120× reduction** for repeated lookups within a single CLI `check` or LSP diagnostics pass.

Note: GraphQL batching is subject to GitHub's complexity and node budgets (typically 500k nodes per query). Queries spanning many repos with
large tag sets may hit these limits, forcing partial fallback to REST calls for the remainder.

If `CachingProvider` is also used broadly, repeated same-process lookups for the same repo or ref should collapse to zero extra network
calls.
