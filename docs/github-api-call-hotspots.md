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

## Hotspots

| Hotspot                                      | Where                                                                                                                                  | When it fires                                              | Why it gets expensive                                                                                                         | Current cost                                              | Remedy                                                                                     |
| -------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| Core version fetch primitive                 | `crates/provider-github/src/provider.rs:56-82`                                                                                         | Any caller asks for versions                               | Fetches all tag pages, then repo info, then default branch                                                                    | `ceil(tags/100)+2` per repo                               | Keep as low-level primitive, but stop calling it repeatedly from bulk flows                |
| CLI `check` bulk fetch                       | `crates/actup/src/commands/check.rs:177-195`, `348-394`                                                                                | `actup check` on cold or stale cache                       | Dedupes repos, but still calls `provider.get_versions()` for each uncached repo and uses `join_all` with no concurrency bound | Unique repos x primitive cost, all in one burst           | Route GitHub repos through `BatchFetcher`; add bounded fallback concurrency                |
| CLI `check` fallback SHA resolution          | `crates/actup/src/commands/check.rs:214-247`, `crates/core/src/resolution.rs:148-178`, `crates/provider-github/src/provider.rs:85-134` | When batch version data cannot prove a semver ref is valid | Extra `resolve_ref_to_sha()` call per affected action                                                                         | +1 to +2 calls per affected action                        | Cache `resolve_ref_to_sha(repo, ref)` results within the run                               |
| CLI `update` per-action refetch              | `crates/actup/src/commands/update.rs:69-90`                                                                                            | `actup update` / `actup fix`                               | Calls `provider.get_versions()` once per `uses:` entry, with no repo dedupe                                                   | Repeats full tag scan for same repo                       | Batch by unique repo first, or wrap provider in `CachingProvider`                          |
| CLI `pin` per-action refetch plus SHA lookup | `crates/actup/src/commands/pin.rs:31-68`, `99-111`                                                                                     | `actup pin`                                                | Fetches versions per action, then may also resolve ref directly                                                               | Primitive cost, sometimes +1 to +2 more                   | Reuse unique repo version map and memoize SHA resolution                                   |
| CLI `latest` one-off lookup                  | `crates/actup/src/commands/latest.rs:131-141`                                                                                          | `actup owner/repo`                                         | Still pays full `get_versions()` cost                                                                                         | Primitive cost once per invocation                        | Lower priority; improved automatically if shared fetch path is fixed                       |
| LSP diagnostics repo fetch                   | `crates/lsp/src/diagnostics.rs:114-172`, `293-326`                                                                                     | On open, save, and after debounced edits                   | Dedupes repos per document, but fetches uncached repos sequentially via `provider.get_versions()`                             | Unique repos in document x primitive cost                 | Use GitHub batch fetcher and keep semaphore-bounded fallback                               |
| LSP workspace fix-all                        | `crates/lsp/src/workspace_commands.rs:223-225`, `329-405`                                                                              | `fixAllInWorkspace` command                                | Runs `compute_diagnostics()` per file, so cold-cache workspace scans pay repo-by-repo                                         | Sum of unique repos across scanned files x primitive cost | Add workspace-level repo aggregation before per-file diagnostics                           |
| LSP cache host mismatch                      | `crates/lsp/src/file_cache.rs:110-124`, `crates/lsp/src/diagnostics.rs:129-143`                                                        | Any non-default host lookup in LSP                         | Cache key uses `default_host` instead of `provider.host()`, causing cache misses or collisions                                | Turns expected cache hits into refetches                  | Use `provider.host()` in LSP cache keys                                                    |
| Hover metadata fetch                         | `crates/lsp/src/hover.rs:135-188`, `312-359`, `crates/provider-github/src/provider.rs:191-242`                                         | First hover on a unique action ref                         | Hover fetches versions, then `action.yml`; `action.yaml` fallback can add one more request                                    | Primitive version cost + 1 to 2 content calls             | Lower priority; keep cache, maybe skip metadata fetch until hover panel is actually needed |

## Dead Or Unwired Capacity

The repo already contains GitHub batching and in-process memoization, but main CLI and LSP paths do not use them.

Relevant code:

- `crates/provider-github/src/batch.rs:17-212`
- `crates/provider-github/src/graphql.rs:89-125`
- `crates/core/src/caching_provider.rs:51-237`

Two notable gaps:

- `BatchFetcher` exists, but production call sites do not use it.
- Provider creation ignores resolved fetch knobs like `api_mode` and `max_tags_per_repo` even though config defaults to GraphQL.

Relevant code:

- `crates/provider-github/src/lib.rs:79-82`
- `crates/core/src/config/mod.rs:216-283`

Impact:

- The repo advertises GraphQL batching as a normal path.
- Real CLI and LSP traffic still mostly uses per-repo REST.

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
