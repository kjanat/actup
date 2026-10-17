# actup-wasm

Parse and check GitHub Actions `uses:` refs — powered by
[actup-core](https://github.com/kjanat/actup-v2) compiled to WebAssembly.

Works in **Node.js 18+**, **Bun**, **Deno**, and **browsers**. No native
dependencies.

## Installation

```bash
npm install actup-wasm
# or
bun add actup-wasm
# or
pnpm add actup-wasm
```

## Quick Start

```ts
import { initWasm, parseUsesRefs } from "actup-wasm";

await initWasm();

const refs = parseUsesRefs(`
jobs:
  build:
    steps:
      - uses: actions/checkout@v3
      - uses: actions/setup-node@v4
`);

for (const ref of refs) {
	console.log(`${ref.value} at line ${ref.line}`);
}
```

## Full Example

Check refs against the GitHub API and compute text edits:

```ts
import { checkRefs, initWasm, planUpdates } from "actup-wasm";
import { createGitHubProvider } from "actup-wasm/provider";

await initWasm();

const yaml = `
jobs:
  ci:
    steps:
      - uses: actions/checkout@v3
`;

const provider = createGitHubProvider({ token: process.env.GITHUB_TOKEN });
const results = await checkRefs(yaml, provider);

for (const r of results) {
	if (typeof r.status === "object" && "Outdated" in r.status) {
		const { target } = r.status.Outdated;
		console.log(
			`${r.action_ref.owner}/${r.action_ref.repo}: ${r.action_ref.ref_str} → ${target.version}`,
		);
	}
}

const edits = planUpdates(results);
// edits: [{ line, refStart, refEnd, replaceEnd, original, newRef, newText }]
```

## API

### `initWasm(wasmSource?)`

Initialize the WASM module. Must be called before any other function. Safe to
call multiple times — subsequent calls are no-ops.

- **Node.js / Bun / Deno**: auto-loads `.wasm` from disk via `fs.readFileSync`
- **Browser**: auto-fetches `.wasm` via `import.meta.url`
- **Custom**: pass a `URL`, `Response`, or `ArrayBuffer` to override
  auto-detection

```ts
await initWasm(); // auto-detect
await initWasm(new URL("./my.wasm")); // explicit URL
await initWasm(wasmBytes); // explicit ArrayBuffer
```

### `parseUsesRefs(yaml)`

Parse `uses:` references from workflow YAML. Pure, synchronous. No network
access.

Returns `UsesLocation[]` — each entry has `line`, `column`, `value`, `raw_line`,
`value_range` (byte offsets of the value, or `null` when the line holds it
escaped), `ref_range` (byte offsets of the `@ref` portion, or `null`),
`scalar_range` (byte offsets of the scalar as written, quotes included, up to
the line end when the scalar continues on later lines), and `action_key`
(`false` when the `uses` key sits outside `jobs.<id>.uses`,
`jobs.<id>.steps[*].uses` and `runs.steps[*].uses`, such as a step input).

### `checkRefs(yaml, provider, config?)`

Check `uses:` references against a provider. Async.

Parses the YAML, resolves each non-local ref through the provider, and returns
`LocatedCheckResult[]`. Each entry has the `location` (`UsesLocation`) of the
value it checked, the parsed `action_ref`, a status (`"UpToDate"`,
`{ Outdated: ... }`, or `{ Unresolvable: ... }`), and `takes_comment`, which is
`true` when the line can hold a comment and holds no other `uses:` value.
`ref_kind` says whether the ref names a `"tag"`, `"branch"` or `"sha"` (or
`null` when the version list lacks it). `same_commit` holds the target
(`{ version, sha }`) when the suggested update sits on the commit the ref
already names, and `null` otherwise. `same_commit_pins` is `true` when that
target names more version components than the ref, such as `v1.0.5` for `v1`. A
value that is not a valid reference reaches no provider: its result is
`Unresolvable` with `is_environment_error: false`, and its `action_ref` holds
the parts as written.

Optional `config` accepts `{ default_host?, policy? }`. `policy` is the
`[policy]` table of `actup.toml` with the same defaults: `bump` is `"patch"` |
`"minor"` | `"major"` | `"latest"`, `pin` is `"preserve"` | `"tag"` | `"sha"`,
`track` is `"keep"` | `"major"` | `"exact"`, `branch` is `"ignore"` | `"warn"` |
`"update"`, and `prerelease` is a boolean. `config` also takes a whole
`RepoConfig` from `parseConfig` or `parseExplicitConfig`, with `scan.ignore` and
`[[overrides]]`. `checkRefs` and `planUpdates` treat `config` as an explicit
config: every key applies, including `default_host`. An unknown key, at the top
level or in a table, and a renamed key reject the call.

### `planUpdates(results, config?)`

Compute minimal text edits from the `LocatedCheckResult[]` that `checkRefs`
returns. Pure, synchronous.

Returns `UpdateEdit[]` with one entry per outdated or pin-branch result, placed
at that result's `location`. A pin-branch result pins the branch or non-semver
tag to its commit as `@<sha> # <ref>`. Each entry has `line`, `refStart`,
`refEnd`, `replaceEnd`, `original`, `newRef` and `newText`. Replace the bytes
`refStart..replaceEnd` of the line with `newText`. A result whose
`location.ref_range` is `null` gets no edit.

`config.policy.pin` picks the written form, as in `actup update`. `preserve`
(the default) keeps a tag a tag and re-pins a SHA to the new SHA with a
`# vX.Y.Z` comment. `sha` writes every target as a SHA with that comment.
**`tag` unpins:** it writes outdated SHA pins as mutable tags and drops the
version from their version comment. A tag ref written as a new tag gets its
version comment refreshed to the new version. A note after the version, as in
`# v4.0.0 pinned for node16`, stays in every case.

An edit writes or rewrites a comment only for a result whose `takes_comment` is
`true`. It adds a missing comment only when nothing but a closing quote, flow
`}`, `]` and `,`, and spaces follows the value, and writes the comment after
them, as in `{ uses: "o/b@<sha>" } # v4.2.0`. It refreshes a version comment at
the end of the line, also after other flow keys, and leaves any other comment
alone.

### `parseConfig(path, contents)`

Parse a repo config file that discovery found, as the CLI parses it. Pure,
synchronous. The extension of `path` picks the format: `toml`, `json`, `jsonc`,
`yaml` or `yml`. No user config and no `ACTUP_` variables apply. Returns a
`RepoConfig` to pass to `checkRefs` and `planUpdates`, and throws for an invalid
config with a message that names `path`.

### `parseExplicitConfig(path, contents)`

Parse a config file the user named, as the CLI parses a `--config` file. Pure,
synchronous. Every key applies, including those a discovered repo config may not
set. Returns a `RepoConfig` like `parseConfig`.

### `repoConfigCandidates()`

The repo config paths to look for in a directory, highest priority first, such
as `actup.toml` and `.config/actup.toml`. Pure, synchronous.

### `sortVersionTags(tags)`

Sort `VersionTag[]` newest first by `actup-core` version precedence. Pure,
synchronous. Tags of equal precedence keep their input order.

### `parseUses(value)`

Parse one `uses:` value with `actup-core`'s parser. Pure, synchronous.

Returns an `ActionRef` (`host`, `owner`, `repo`, `subpath`, `ref_str`), or
`null` for local (`./path`) and `docker://` refs. Throws on an invalid
reference. A first segment counts as a host only in a path of three or more
segments, and the host is lower-cased.

### `parseActionPath(path)`

Parse the `[host/]owner/repo[/subpath]` text before `@`. Pure, synchronous.
Returns an `ActionRepoId` and throws when `path` does not name `owner/repo`.

### `isHostSegment(segment)`

Return whether `segment` names a host when it starts a path of three or more
segments: a DNS name with a dot, `localhost`, either with an optional port, or a
bracketed IPv6 literal.

### `createGitHubProvider(options?)`

*Import from `"actup-wasm/provider"`.*

Create a GitHub REST API provider using native `fetch()`.

```ts
import { createGitHubProvider } from "actup-wasm/provider";

const provider = createGitHubProvider({
	token: "ghp_...", // optional — unauthenticated = 60 req/hr
	apiBase: "https://...", // optional — for GitHub Enterprise
	perPage: 100, // optional — tags per page (1-100)
});
```

## Custom Provider

Implement the `ActupProvider` interface to use any Git host:

```ts
import type { ActupProvider } from "actup-wasm";

const myProvider: ActupProvider = {
  getVersions(host, owner, repo) { ... },
  resolveRefToSha(host, owner, repo, ref) { ... },
  getDefaultBranch(host, owner, repo) { ... },
  getReadme(host, owner, repo, ref?) { ... },
  getActionYml(host, owner, repo, ref) { ... },
};
```

A method that fails rejects with an object whose `kind` says why:

| `kind`        | `checkRefs` result                                      |
| ------------- | ------------------------------------------------------- |
| `notFound`    | Unresolvable user error: repository or ref not found    |
| `rateLimited` | Unresolvable environment error (`retryAfter` seconds)   |
| `auth`        | Unresolvable environment error: authentication failed   |
| `forbidden`   | Unresolvable environment error: permission denied       |
| `network`     | Unresolvable environment error, as for a missing `kind` |

`createGitHubProvider` rejects with a `GitHubProviderError` that sets `kind`
from the HTTP status: 404 `notFound`, 401 `auth`, 403 `forbidden`, 429 or a 403
with rate-limit headers `rateLimited`, any other failure `network`.

## Bundle Size

|              | Size    |
| ------------ | ------- |
| WASM raw     | ~280 KB |
| WASM gzipped | ~119 KB |

## License

MIT OR Apache-2.0
