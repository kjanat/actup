# WASM + npm Package Spec v1.0

**Status:** Implemented except D10 (WASM-based MCP server)\
**Type:** Feature\
**Effort:** XL (phased: M + M + L + L)\
**Breaking:** No (additive: a new crate, plus `Clock`, `MaybeSend`/`MaybeSync`
and a `tsify` feature in `actup-core`)

## Summary

Compile `actup-core` (parsing, policy, version resolution) to WebAssembly.
Publish a thin TypeScript wrapper as `actup-wasm` on npm. Enable:

- VS Code web extension (vscode.dev) — `wasm32-unknown-unknown` with JS
  `fetch()` bridge
- Zed extension — `wasm32-wasip2`, embedding `actup-core` and fetching through
  Zed's HTTP client
- Standalone Node.js/Bun/Deno usage — `actup-wasm` as a library
- MCP server in JS/TS — wrapping the WASM module

The CLI remains native-only. The existing native LSP binary is unaffected.

## Motivation

Before this spec, both the VS Code and Zed extensions were thin shims that
located and spawned `actup-lsp` as a native binary. This meant:

1. **No vscode.dev support** — web extensions can't spawn native processes
2. **Users must pre-install `actup-lsp`** — friction, version mismatch risk
3. **No JS/TS ecosystem integration** — can't use actup from Node scripts, CI
   tools, or MCP servers without a Rust toolchain
4. **Zed extension is hollow** — compiles to WASM but delegates everything to a
   native binary

WASM eliminates the native binary requirement for the core check/update
pipeline.

## Architecture

### Key insight: the Provider trait already abstracts 80%

The existing `Provider` trait in `actup-core` abstracts all HTTP, retry, rate
limiting, and API specifics behind boxed futures. The WASM strategy is:

1. **Eliminate** filesystem access and file discovery from the WASM surface (the
   host provides file and config contents)
2. **Inject** a `Clock` trait at the time reads that `actup-wasm` reaches; it
   passes `JsClock` when it builds a version set (`bridge.rs:219`)
3. **Bridge** JS `Provider` implementations to the Rust `Provider` trait via
   `wasm-bindgen`

No platform crates. No feature-flag soup. One new `actup-wasm` crate.

### Pipeline inversion

Native CLI pipeline: **discover files → read → parse → resolve → diff → write**

WASM pipeline: **host provides YAML content → parse → resolve → return edits**
(host applies)

The npm package never touches the filesystem. The host (VS Code, Node.js, Zed)
handles I/O.

### Crate layout

```tree
crates/
  wasm/                 # actup-wasm — wasm-bindgen glue + JS bridge
    Cargo.toml
    src/
      lib.rs            # crate docs; wasm32-only modules
      wasm_api.rs       # #[wasm_bindgen] exports
      bridge.rs         # JsProvider → Rust Provider adapter
      clock.rs          # JsClock (Date.now() → SystemTime)
    tests/              # node_parse.mjs, node_check.mjs (`just wasm-node-test`)
packages/
  actup-wasm/           # npm package (thin TS wrapper + .wasm)
    package.json
    src/
      index.ts          # WASM loader, wrappers, re-exported tsify types
      types.ts          # ActupProvider, ProviderErrorKind
      provider.ts       # Default GitHub provider using fetch()
    tests/              # node --test suites
    wasm/               # Built .wasm artifact (gitignored)
```

`actup-wasm` is a workspace member.

## Scope & Deliverables

| #   | Deliverable                                 | Effort | Depends On |
| --- | ------------------------------------------- | ------ | ---------- |
| D1  | `Clock` trait + thread into call sites      | S      | -          |
| D2  | `Serialize`/`Deserialize` on bridge types   | S      | -          |
| D3  | `Send` bound audit / `MaybeSend` for WASM   | S      | -          |
| D4  | `actup-wasm` crate (parse — pure, sync)     | M      | D1, D2     |
| D5  | `JsProviderAdapter` bridge + `check_refs`   | M      | D3, D4     |
| D6  | npm package `actup-wasm` + TS types         | M      | D5         |
| D7  | Default `fetch()`-based GitHub provider     | S      | D6         |
| D8  | VS Code web extension (direct WASM, no LSP) | L      | D6, D7     |
| D9  | Wire into Zed extension (embed vs shim)     | L      | D5         |
| D10 | WASM-based MCP server                       | L      | D6, D7     |

## Non-Goals

- CLI in WASM (terminal-coupled: clap, indicatif, console)
- Abstracting filesystem for WASM (eliminated from WASM surface, not abstracted)
- `actup-platform-*` crates (not enough platform-specific code to warrant
  separate crates)
- Feature-flag-based WASM/native split in existing crates (too few call sites)
- Replacing the native `actup-lsp` binary (WASM LSP is an additional mode, not a
  replacement)

## Data Model

### Clock trait (new, in `actup-core`)

```rust
/// Platform-agnostic time source.
pub trait Clock: fmt::Debug + MaybeSend + MaybeSync {
    fn now(&self) -> std::time::SystemTime;
}

/// Default: delegates to std.
#[derive(Debug, Clone, Copy, Default)]
pub struct System;

impl Clock for System {
    fn now(&self) -> std::time::SystemTime {
        std::time::SystemTime::now()
    }
}
```

Call sites that take a `Clock`:

- `cache.rs:47` — `Entry::with_clock` stamps `stored_at`
- `version.rs:599` — building a `Set` stamps `fetched_at`
- `caching_provider.rs:51`, `:61` — `Retention::Expiring` stamps and ages
  in-memory provider answers
- `provider-github/src/graphql.rs:227` — GraphQL rate limit reset time
  comparison
- `provider-github/src/client.rs:706` — REST rate limit reset time comparison

These native paths read the system clock directly:

- `version.rs:610` — `Set::from_tags` stamps `fetched_at`. `process_tags`
  (`providers/src/tags.rs:52`) calls it for GitHub REST and GraphQL, git
  `ls-refs`, GitLab and Gitea. The Zed extension calls it at
  `extensions/zed/src/provider.rs:176`.
- `version.rs:577` — `Set::build` stamps `fetched_at`.
- `cache.rs:39` — `Entry::new` stamps `stored_at`. `FileCache`
  (`file_cache.rs:117`) and the GitHub batch fetcher
  (`provider-github/src/batch.rs:237`, `:466`) call it.
- `cache.rs:95` — `Entry::renewed` stamps `stored_at`.
- `cache.rs:56`, `:63` — `Entry::is_expired` and `Entry::age` call
  `SystemTime::elapsed`.

### JS ↔ Rust bridge types

```rust
// crates/wasm/src/bridge.rs

/// Host-provided provider — implemented in JS/TS via fetch()
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(typescript_type = "import(\"../src/types\").ActupProvider")]
    pub type JsProvider;

    #[wasm_bindgen(method, catch, js_name = "getVersions")]
    async fn get_versions_js(
        this: &JsProvider,
        host: &str,
        owner: &str,
        repo: &str,
    ) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch, js_name = "resolveRefToSha")]
    async fn resolve_ref_to_sha_js(
        this: &JsProvider,
        host: &str,
        owner: &str,
        repo: &str,
        ref_name: &str,
    ) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch, js_name = "getDefaultBranch")]
    async fn get_default_branch_js(
        this: &JsProvider,
        host: &str,
        owner: &str,
        repo: &str,
    ) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch, js_name = "getReadme")]
    async fn get_readme_js(
        this: &JsProvider,
        host: &str,
        owner: &str,
        repo: &str,
        ref_name: Option<String>,
    ) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch, js_name = "getActionYml")]
    async fn get_action_yml_js(
        this: &JsProvider,
        host: &str,
        owner: &str,
        repo: &str,
        ref_name: &str,
    ) -> Result<JsValue, JsValue>;
}
```

### TypeScript public API (Phase 1 — high-level)

`UsesLocation`, `LocatedCheckResult`, `CheckStatus`, `UpdateEdit` and the
`[policy]` types (`Policy`, `Update`, `Style`, `Track`, `Branch`, `Archived`)
are generated by tsify from the Rust types. `ActupConfig`, `RepoConfig` and
`ConfigFile` are hand-written in a `typescript_custom_section` in
`crates/wasm/src/wasm_api.rs`. `index.ts` re-exports them from
`../wasm/actup_wasm.js` and renames the policy types: `Policy as ActupPolicy`,
`Update as UpdatePolicy`, `Style as PinStyle`, `Track as TrackPolicy`,
`Branch as BranchPolicy` and `Archived as ArchivedPolicy`. `UsesLocation`
carries a 1-indexed `line` and `column`, the `value`, the `raw_line`, byte
ranges (`value_range`, `ref_range`, `scalar_range`) and an optional
`action_key`. `CheckStatus` is `"UpToDate"`, `"Ignored"`, `Outdated`, `Pinned`,
`Floating`, `PinBranch` or `Unresolvable`.

```typescript
// packages/actup-wasm/src/index.ts

/** Load the WASM module. Call it once before any other function. */
export function initWasm(
	wasmSource?: URL | Response | BufferSource,
): Promise<void>;

/** Parse uses: refs from YAML content. Pure, synchronous. */
export function parseUsesRefs(yaml: string): UsesLocation[];

/** Parse YAML content and check its refs against a provider. Async (network). */
export function checkRefs(
	yaml: string,
	provider: ActupProvider,
	config?: ActupConfig | RepoConfig,
): Promise<LocatedCheckResult[]>;

/** Plan minimal text edits from located check results. Pure, synchronous. */
export function planUpdates(
	results: readonly LocatedCheckResult[],
	config?: ActupConfig | RepoConfig,
): UpdateEdit[];
```

`index.ts` also exports `parseUses`, `parseActionPath`, `isHostSegment`,
`sortVersionTags`, `parseConfig`, `parseExplicitConfig`,
`parseRepoAndExplicitConfig` and `repoConfigCandidates`.

### TypeScript public API (Phase 2 — provider-level, for MCP/power users)

```typescript
export interface ActupProvider {
	getVersions(host: string, owner: string, repo: string): Promise<VersionSet>;
	resolveRefToSha(
		host: string,
		owner: string,
		repo: string,
		ref: string,
	): Promise<string>;
	getDefaultBranch(host: string, owner: string, repo: string): Promise<string>;
	getReadme(
		host: string,
		owner: string,
		repo: string,
		ref?: string | null,
	): Promise<string | null>;
	getActionYml(
		host: string,
		owner: string,
		repo: string,
		ref: string,
	): Promise<ActionYml>;
}
```

### Default fetch-based provider (ships with npm package)

```typescript
// packages/actup-wasm/src/provider.ts
export function createGitHubProvider(options?: {
	token?: string;
	apiBase?: string;
	perPage?: number; // 1-100, default 100
}): ActupProvider;
```

Uses the platform's native `fetch()` — works in browsers, Node 18+, Bun, Deno,
Cloudflare Workers.

## WASM Target Strategy

| Consumer           | Target                   | HTTP            | Runtime                |
| ------------------ | ------------------------ | --------------- | ---------------------- |
| npm / VS Code web  | `wasm32-unknown-unknown` | JS fetch bridge | `wasm-bindgen-futures` |
| Zed extension      | `wasm32-wasip2`          | Zed HTTP client | WASI reactor           |
| Node.js standalone | `wasm32-unknown-unknown` | JS fetch bridge | Node WASM loader       |

The `actup-wasm` crate targets `wasm32-unknown-unknown` (broadest compat). The
Zed extension (D9) embeds `actup-core` directly and builds for `wasm32-wasip2`.

## RwLock Strategy

`CachingProvider` uses `tokio::sync::RwLock` on every target. On `wasm32`,
`actup-core` depends on `tokio` with only the `sync` feature, which works
single-threaded without a runtime.

## Send Bound Strategy

**Decision: `MaybeSend` trait alias** — `Send` on native, empty trait on WASM.

```rust
// crates/core/src/compat.rs
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send> MaybeSend for T {}

#[cfg(target_arch = "wasm32")]
pub trait MaybeSend {}
#[cfg(target_arch = "wasm32")]
impl<T> MaybeSend for T {}
// (same pattern for MaybeSync)
```

Then `pub trait Provider: MaybeSend + MaybeSync`. Provider methods return
`compat::BoxFuture`, which is `+ Send` on native and has no `Send` bound on
`wasm32`. Rust rejects `dyn Future + MaybeSend`; `MaybeSend` is not an auto
trait.

**Why:** Established pattern (futures, tower). Mechanical find-replace — every
native impl already satisfies `Send + Sync`. Dropping `Send` would break
`tokio::spawn` in the LSP. A separate WASM trait creates a maintenance fork.

## Acceptance Criteria

- [x] `parseUsesRefs("uses: actions/checkout@v4")` returns correct
      `UsesLocation` from WASM
- [x] `checkRefs()` with a mock JS provider returns correct
      `LocatedCheckResult`s
- [ ] `planUpdates()` returns minimal text edits matching native CLI output
- [ ] npm package installs and runs in Node.js 18+ without native dependencies
- [ ] npm package works in VS Code web extension host (vscode.dev)
- [x] WASM bundle size < 500KB gzipped (stretch: < 300KB)
- [x] No filesystem access, `dirs` lookup or file discovery in the WASM bundle.
      `figment` (config parsing) and `glob::Pattern` (`scan.ignore`) stay. Check
      with
      `cargo build -p actup-wasm --target wasm32-unknown-unknown --release && llvm-nm -C --defined-only target/wasm32-unknown-unknown/release/actup_wasm.wasm | rg 'std::fs|dirs::|glob::glob|discover_files|read_config_file|user_config_path|cache::default_dir'`,
      which prints nothing. The cargo artifact keeps its `name` section, and
      `wasm-pack` only removes code from it.
- [x] Existing native CLI, LSP, and tests unaffected (no regressions)

## Test Strategy

| Layer       | What                      | How                                                                      |
| ----------- | ------------------------- | ------------------------------------------------------------------------ |
| Unit (Rust) | `Clock` trait             | `#[cfg(test)]` in `crates/core/src/clock.rs`                             |
| Bridge      | `JsProviderAdapter`       | `crates/wasm/tests/node_check.mjs` (Node)                                |
| Unit (TS)   | npm wrapper types, loader | `node --test` over `tests/*.mjs`                                         |
| Integration | Full parse → check → plan | WASM in Node.js with mock provider                                       |
| Size        | Bundle size regression    | `packages/actup-wasm/scripts/build-wasm.ts` fails above 512000 B gzipped |
| E2E         | VS Code web extension     | Manual initially; `@vscode/test-web` later                               |

## Risks & Mitigations

| Risk                              | Likelihood | Impact | Mitigation                                                                            |
| --------------------------------- | ---------- | ------ | ------------------------------------------------------------------------------------- |
| `saphyr-parser` bloats WASM       | Medium     | M      | `packages/actup-wasm/scripts/build-wasm.ts` caps the gzip size; profile with `twiggy` |
| `SystemTime` in `Set::fetched_at` | Low        | S      | `#[serde(skip)]` — WASM consumer won't use cache timestamps                           |
| Async executor mismatch in WASM   | Medium     | M      | `wasm-bindgen-futures` for browser; test early in D4                                  |
| Zed wasip2 needs different build  | Medium     | L      | D9 shipped: `extensions/zed` builds for `wasm32-wasip2`                               |

## Trade-offs Made

| Chose                            | Over                             | Because                                                                                                                                                                                                  |
| -------------------------------- | -------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Trait injection (`Clock`)        | Feature flags in `actup-core`    | Few call sites; `#[cfg]` soup not justified                                                                                                                                                              |
| Eliminate FS from WASM surface   | Abstract FS behind trait         | Host always has the files; WASM never needs to discover/read                                                                                                                                             |
| High-level API first             | Provider-level API first         | Consumers want results, not a toolkit                                                                                                                                                                    |
| New `actup-wasm` crate           | WASM features in existing crates | Clean boundary; `actup-core` keeps its WASM changes small: `Clock`, `MaybeSend`/`MaybeSync`, the `tsify` feature, a `sync`-only `tokio` on `wasm32`, and no `atomic_write` or `file_cache` modules there |
| `wasm32-unknown-unknown` primary | `wasm32-wasip2` primary          | Broadest compat (browser + Node); wasip2 only for Zed                                                                                                                                                    |
| `wasm-bindgen` + `wasm-pack`     | Manual WASM instantiation        | Mature toolchain, good TS type generation                                                                                                                                                                |

## Resolved Questions

- [x] **`Send` bound** → `MaybeSend` trait alias. Mechanical find-replace, no
      breakage.
- [x] **`version::Set` serialization** → Already derives
      `Serialize`/`Deserialize`. `Arc` handled by serde `rc` feature (already
      enabled). Only gap: `#[serde(skip)]` on `fetched_at: SystemTime`.
- [x] **npm scope** → `actup-wasm` (unscoped). No `@actup` org exists; one
      package doesn't justify it.
- [x] **Workspace membership** → Workspace member. `wasm32-unknown-unknown` via
      `wasm-pack` works in the workspace. Separate = duplicated lockfile.
- [x] **WASM LSP transport** → Skip WASM LSP for D8 (web extension). The current
      desktop extension uses `LanguageClient` + `actup-lsp` subprocess, which is
      correct for native. D8's web extension calls WASM functions directly and
      reports through a diagnostic collection, `registerCodeActionsProvider`,
      `registerHoverProvider` and `registerCompletionItemProvider`. LSP protocol
      is overhead when the engine runs in-process. This is what Biome/Ruff/OXC
      do.
- [x] **Source maps** → wasm-pack 0.15.0, which CI pins (`ci.yml:170`, `:203`;
      `release.yml:289`), runs `wasm-opt -O` without `-g` on `.wasm` (wasm-pack
      `src/manifest/mod.rs:304`, `:416`; `src/wasm_opt.rs`). Binaryen's `-O` is
      `-Os`. Ship `.js.map` for TS wrapper (tiny, useful). No DWARF-in-WASM
      (enormous, spotty devtools support).
