# actup-wasm (npm package)

TypeScript wrapper around `crates/wasm` WASM output. Provides typed API for
browser, Node.js 18+, Bun, and Deno.

**Separate ecosystem**: bun-based, ES modules, own `tsconfig.json`. Not part of
the Cargo workspace.

## Module Map

| File          | Lines | Purpose                                      | Key Exports                                                                |
| ------------- | ----- | -------------------------------------------- | -------------------------------------------------------------------------- |
| `index.ts`    | 191   | WASM initialization + typed API wrappers     | `initWasm`, `parseUsesRefs`, `checkRefs`, `planUpdates`, `sortVersionTags` |
| `provider.ts` | 428   | Default `fetch()`-based GitHub REST provider | `createGitHubProvider`, `GitHubProviderError`                              |
| `types.ts`    | 137   | TypeScript interfaces mirroring Rust serde   | `ActupProvider`, `CheckResult`, `VersionSet`, etc.                         |

## Package Exports

```typescript
import { checkRefs, initWasm, parseUsesRefs, planUpdates } from "actup-wasm";
import { createGitHubProvider } from "actup-wasm/provider";
```

Two entry points: main (`actup-wasm`) for core functions, `actup-wasm/provider`
for the default GitHub provider.

## Where to Look

| Task                    | Start Here                                     |
| ----------------------- | ---------------------------------------------- |
| Change public API       | `src/index.ts` → wrapper functions             |
| Add/modify TS types     | `src/types.ts` (must match Rust serde)         |
| Change GitHub provider  | `src/provider.ts` → `createGitHubProvider`     |
| Fix WASM initialization | `src/index.ts` → `doInit()` / `loadWasmNode()` |
| Build WASM from Rust    | `scripts/build-wasm.sh`                        |

## Build

```bash
bun run build          # build:wasm (wasm-pack) + build:ts (tsc)
bun run build:wasm     # wasm-pack build → wasm/ directory
bun run build:ts       # tsc → dist/ directory
bun run typecheck      # tsc --noEmit
```

The `wasm/` directory contains wasm-pack output (`actup_wasm.js`,
`actup_wasm_bg.wasm`). The `dist/` directory contains compiled TypeScript.

## Architecture

- WASM loaded lazily: `initWasm()` must be called before any function. Safe to
  call multiple times.
- Node.js: reads `.wasm` from disk via `fs.readFileSync` + `initSync`
  (synchronous)
- Browser: fetches `.wasm` via `init()` (asynchronous)
- `ActupProvider` interface: implement 5 async methods to supply version data
  from any host
- `createGitHubProvider()`: default implementation using `fetch()` + GitHub REST
  API
- Types in `types.ts` mirror Rust structs serialized via `serde-wasm-bindgen` —
  keep in sync manually

## Anti-Patterns (This Package)

- **Types must match Rust serde output** — if you change Rust types in
  `crates/wasm`, update `types.ts`
- **No YAML parser dependency** — `provider.ts` does minimal regex extraction
  for `action.yml` fields
- **Never log tokens** — `GitHubProviderError` exposes rate limit info but never
  auth headers
- **Don't import wasm-bindgen types directly** — `wasm_bindgen.d.ts` types
  everything as `any`; use `types.ts` instead

## Conventions

- `bun` (not npm/yarn) for package management
- `strictest` TypeScript: `strict`, `noUncheckedIndexedAccess`,
  `exactOptionalPropertyTypes`
- ES2022 target, ESM-only (`"type": "module"`)
- `sideEffects: false` for tree-shaking
- `readonly` on all interface properties
- Rate limit warnings via `console.warn` at < 10 remaining
