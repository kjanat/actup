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
			`${r.action_ref.owner}/${r.action_ref.repo}: ${r.action_ref.git_ref} → ${target.version}`,
		);
	}
}

const edits = planUpdates(yaml, results);
// edits: [{ line, refStart, refEnd, original, newRef }]
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
and `ref_range` (byte offsets of the `@ref` portion, or `null`).

### `checkRefs(yaml, provider, config?)`

Check `uses:` references against a provider. Async.

Parses the YAML, resolves each non-local ref through the provider, and returns
`LocatedCheckResult[]`. Each entry has the `location` (`UsesLocation`) of the
value it checked, the parsed `action_ref`, and a status: `"UpToDate"`,
`{ Outdated: ... }`, or `{ Unresolvable: ... }`.

Optional `config` accepts `{ default_host?, policy?: { bump? } }` where `bump`
is `"patch"` | `"minor"` | `"major"` | `"latest"`. Unknown keys reject the call.

### `planUpdates(yaml, results)`

Compute minimal text edits from check results. Pure, synchronous.

Returns `UpdateEdit[]` — each entry has `line`, `refStart`, `refEnd`,
`original`, and `newRef`. Only generates edits for outdated refs.

### `sortVersionTags(tags)`

Sort `VersionTag[]` newest first by `actup-core` version precedence. Pure,
synchronous. Tags of equal precedence keep their input order.

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
