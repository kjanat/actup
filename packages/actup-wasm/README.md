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
escaped), and `ref_range` (byte offsets of the `@ref` portion, or `null`).

### `checkRefs(yaml, provider, config?)`

Check `uses:` references against a provider. Async.

Parses the YAML, resolves each non-local ref through the provider, and returns
`LocatedCheckResult[]`. Each entry has the `location` (`UsesLocation`) of the
value it checked, the parsed `action_ref`, and a status: `"UpToDate"`,
`{ Outdated: ... }`, or `{ Unresolvable: ... }`.

Optional `config` accepts `{ default_host?, policy?: { bump?, pin? } }` where
`bump` is `"patch"` | `"minor"` | `"major"` | `"latest"` and `pin` is
`"preserve"` | `"tag"` | `"sha"`. Unknown keys reject the call.

### `planUpdates(results, config?)`

Compute minimal text edits from the `LocatedCheckResult[]` that `checkRefs`
returns. Pure, synchronous.

Returns `UpdateEdit[]` with one entry per outdated result, placed at that
result's `location`. Each entry has `line`, `refStart`, `refEnd`, `replaceEnd`,
`original`, `newRef` and `newText`. Replace the bytes `refStart..replaceEnd` of
the line with `newText`. A result whose `location.ref_range` is `null` gets no
edit.

`config.policy.pin` picks the written form, as in `actup update`. `preserve`
(the default) keeps a tag a tag and re-pins a SHA to the new SHA with a
`# vX.Y.Z` comment. `sha` writes every target as a SHA with that comment.
**`tag` unpins:** it writes outdated SHA pins as mutable tags and drops their
version comment.

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

## Bundle Size

|              | Size    |
| ------------ | ------- |
| WASM raw     | ~280 KB |
| WASM gzipped | ~119 KB |

## License

MIT OR Apache-2.0
