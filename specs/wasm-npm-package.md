# WASM + npm Package Spec v1.0

**Status:** Draft\
**Type:** Feature\
**Effort:** XL (phased: M + M + L + L)\
**Breaking:** No (additive — new crate, no changes to existing public API)

## Summary

Compile `actup-core` (parsing, policy, version resolution) to WebAssembly. Publish a thin TypeScript wrapper as `@actup/wasm` on npm.
Enable:

- VS Code web extension (vscode.dev) — `wasm32-unknown-unknown` with JS `fetch()` bridge
- Zed extension — `wasm32-wasip2` with WASI HTTP (future: embed logic instead of shelling out)
- Standalone Node.js/Bun/Deno usage — `@actup/wasm` as a library
- MCP server in JS/TS — wrapping the WASM module

The CLI remains native-only. The existing native LSP binary is unaffected.

## Motivation

Today, both the VS Code and Zed extensions are thin shims that locate and spawn `actup-lsp` as a native binary. This means:

1. **No vscode.dev support** — web extensions can't spawn native processes
2. **Users must pre-install `actup-lsp`** — friction, version mismatch risk
3. **No JS/TS ecosystem integration** — can't use actup from Node scripts, CI tools, or MCP servers without a Rust toolchain
4. **Zed extension is hollow** — compiles to WASM but delegates everything to a native binary

WASM eliminates the native binary requirement for the core check/update pipeline.

## Architecture

### Key insight: the Provider trait already abstracts 80%

The existing `Provider` trait in `actup-core` abstracts all HTTP, retry, rate limiting, and API specifics behind boxed futures. The WASM
strategy is:

1. **Eliminate** filesystem/config/discovery from the WASM surface (host provides content)
2. **Inject** a `Clock` trait for the 3 `SystemTime::now()` call sites
3. **Bridge** JS `Provider` implementations to the Rust `Provider` trait via `wasm-bindgen`

No platform crates. No feature-flag soup. One new `actup-wasm` crate.

### Pipeline inversion

Native CLI pipeline: **discover files → read → parse → resolve → diff → write**

WASM pipeline: **host provides YAML content → parse → resolve → return edits** (host applies)

The npm package never touches the filesystem. The host (VS Code, Node.js, Zed) handles I/O.

### Crate layout

```tree
crates/
  wasm/                 # actup-wasm — wasm-bindgen glue + JS bridge
    Cargo.toml
    src/
      lib.rs            # #[wasm_bindgen] exports
      bridge.rs         # JsProvider → Rust Provider adapter
      clock.rs          # JsClock (Date.now() → SystemTime)
packages/
  actup-wasm/           # npm package (thin TS wrapper + .wasm)
    package.json
    src/
      index.ts          # Re-exports, TypeScript types, WASM loader
      provider.ts       # Default GitHub provider using fetch()
    wasm/               # Built .wasm artifact (gitignored)
```

`actup-wasm` is a workspace member (unlike `extensions/zed/` which is separate).

## Scope & Deliverables

| #   | Deliverable                               | Effort | Depends On |
| --- | ----------------------------------------- | ------ | ---------- |
| D1  | `Clock` trait + thread into call sites    | S      | -          |
| D2  | `Serialize`/`Deserialize` on bridge types | S      | -          |
| D3  | `Send` bound audit / `MaybeSend` for WASM | S      | -          |
| D4  | `actup-wasm` crate (parse — pure, sync)   | M      | D1, D2     |
| D5  | `JsProviderAdapter` bridge + `check_refs` | M      | D3, D4     |
| D6  | npm package `@actup/wasm` + TS types      | M      | D5         |
| D7  | Default `fetch()`-based GitHub provider   | S      | D6         |
| D8  | Wire into VS Code extension (web mode)    | L      | D6, D7     |
| D9  | Wire into Zed extension (embed vs shim)   | L      | D5         |
| D10 | WASM-based MCP server                     | L      | D6, D7     |

## Non-Goals

- CLI in WASM (terminal-coupled: clap, indicatif, console)
- Abstracting filesystem for WASM (eliminated from WASM surface, not abstracted)
- `actup-platform-*` crates (not enough platform-specific code to warrant separate crates)
- Feature-flag-based WASM/native split in existing crates (too few call sites)
- Replacing the native `actup-lsp` binary (WASM LSP is an additional mode, not a replacement)

## Data Model

### Clock trait (new, in `actup-core`)

```rust
/// Platform-agnostic time source.
pub trait Clock: Send + Sync {
    fn now(&self) -> std::time::SystemTime;
}

/// Default: delegates to std.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> std::time::SystemTime {
        std::time::SystemTime::now()
    }
}
```

Call sites to thread `Clock` into:

- `cache.rs:34` — `Entry` construction (stored_at timestamp)
- `version.rs:389` — `Set` freshness check
- `provider-github/graphql.rs:200` — rate limit reset time comparison

### JS ↔ Rust bridge types

```rust
// crates/wasm/src/bridge.rs

/// Host-provided provider — implemented in JS/TS via fetch()
#[wasm_bindgen]
extern "C" {
    pub type JsProvider;

    #[wasm_bindgen(method, catch)]
    pub async fn getVersions(
        this: &JsProvider,
        host: &str,
        owner: &str,
        repo: &str,
    ) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch)]
    pub async fn resolveRefToSha(
        this: &JsProvider,
        host: &str,
        owner: &str,
        repo: &str,
        ref_name: &str,
    ) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch)]
    pub async fn getDefaultBranch(
        this: &JsProvider,
        host: &str,
        owner: &str,
        repo: &str,
    ) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch)]
    pub async fn getReadme(
        this: &JsProvider,
        host: &str,
        owner: &str,
        repo: &str,
        ref_name: Option<String>,
    ) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch)]
    pub async fn getActionYml(
        this: &JsProvider,
        host: &str,
        owner: &str,
        repo: &str,
        ref_name: &str,
    ) -> Result<JsValue, JsValue>;
}
```

### TypeScript public API (Phase 1 — high-level)

```typescript
// packages/actup-wasm/src/index.ts

export interface UsesLocation {
	/** Raw `uses:` string, e.g. "actions/checkout@v4" */
	uses: string;
	/** 0-indexed line number in the YAML file */
	line: number;
	/** Byte offset of the `@ref` portion */
	refOffset: number;
	/** Length of the ref portion */
	refLength: number;
}

export interface CheckResult {
	uses: string;
	status: "up-to-date" | "outdated" | "unresolvable" | "unknown" | "rate-limited";
	current: string;
	latest?: string;
	latestSha?: string;
	message?: string;
}

export interface UpdateEdit {
	line: number;
	oldRef: string;
	newRef: string;
	/** Unified diff snippet for this edit */
	diff: string;
}

/** Parse uses: refs from YAML content. Pure, synchronous. */
export function parseUsesRefs(yaml: string): UsesLocation[];

/** Check refs against a provider. Async (network). */
export function checkRefs(
	refs: UsesLocation[],
	provider: ActupProvider,
	config?: Partial<ActupConfig>,
): Promise<CheckResult[]>;

/** Plan minimal text edits from check results. Pure, synchronous. */
export function planUpdates(
	yaml: string,
	results: CheckResult[],
	config?: Partial<ActupConfig>,
): UpdateEdit[];
```

### TypeScript public API (Phase 2 — provider-level, for MCP/power users)

```typescript
export interface ActupProvider {
	getVersions(host: string, owner: string, repo: string): Promise<VersionSet>;
	resolveRefToSha(host: string, owner: string, repo: string, ref: string): Promise<string>;
	getDefaultBranch(host: string, owner: string, repo: string): Promise<string>;
	getReadme(host: string, owner: string, repo: string, ref?: string): Promise<string | null>;
	getActionYml(host: string, owner: string, repo: string, ref: string): Promise<ActionYml>;
}
```

### Default fetch-based provider (ships with npm package)

```typescript
// packages/actup-wasm/src/provider.ts
export function createGitHubProvider(options?: {
	token?: string;
	apiBase?: string;
}): ActupProvider;
```

Uses the platform's native `fetch()` — works in browsers, Node 18+, Bun, Deno, Cloudflare Workers.

## WASM Target Strategy

| Consumer           | Target                   | HTTP            | Runtime                |
| ------------------ | ------------------------ | --------------- | ---------------------- |
| npm / VS Code web  | `wasm32-unknown-unknown` | JS fetch bridge | `wasm-bindgen-futures` |
| Zed extension      | `wasm32-wasip2`          | WASI HTTP       | WASI reactor           |
| Node.js standalone | `wasm32-unknown-unknown` | JS fetch bridge | Node WASM loader       |

The `actup-wasm` crate targets `wasm32-unknown-unknown` (broadest compat). Zed integration (D9) may need a separate build target or a thin
wasip2 adapter.

## RwLock Strategy

`CachingProvider` uses `tokio::sync::RwLock`. For WASM (`wasm32` is single-threaded):

```rust
#[cfg(target_arch = "wasm32")]
type AsyncRwLock<T> = std::sync::RwLock<T>;

#[cfg(not(target_arch = "wasm32"))]
type AsyncRwLock<T> = tokio::sync::RwLock<T>;
```

Single type alias, not scattered `#[cfg]`.

## Send Bound Strategy

The `Provider` trait requires `Send + Sync` and returns `+ Send` futures. WASM (`wasm32-unknown-unknown`) is single-threaded — `Send` is
unnecessary and some JS-bridged types won't implement it.

Options:

1. **`MaybeSend` trait alias** — `Send` on native, nothing on WASM
2. **Drop `Send` from trait** — use `Arc` + single-threaded executor everywhere
3. **Separate WASM provider trait** — `WasmProvider` without `Send`, adapter in `actup-wasm`

Decision deferred to D3 (audit). Likely option 1 or 3.

## Acceptance Criteria

- [ ] `parseUsesRefs("uses: actions/checkout@v4")` returns correct `UsesLocation` from WASM
- [ ] `checkRefs()` with a mock JS provider returns correct `CheckResult`
- [ ] `planUpdates()` returns minimal text edits matching native CLI output
- [ ] npm package installs and runs in Node.js 18+ without native dependencies
- [ ] npm package works in VS Code web extension host (vscode.dev)
- [ ] WASM bundle size < 500KB gzipped (stretch: < 300KB)
- [ ] No `std::fs`, `tokio::fs`, `dirs`, `glob`, or `figment` in the WASM dependency tree
- [ ] Existing native CLI, LSP, and tests unaffected (no regressions)

## Test Strategy

| Layer       | What                          | How                                           |
| ----------- | ----------------------------- | --------------------------------------------- |
| Unit (Rust) | `Clock` trait, bridge adapter | Standard `#[cfg(test)]`, mock clock           |
| Unit (TS)   | npm wrapper types, loader     | Vitest / bun:test                             |
| Integration | Full parse → check → plan     | WASM in Node.js with mock provider            |
| Size        | Bundle size regression        | CI check: `wasm-opt --print-size` + threshold |
| E2E         | VS Code web extension         | Manual initially; `@vscode/test-web` later    |

## Risks & Mitigations

| Risk                                | Likelihood | Impact | Mitigation                                                       |
| ----------------------------------- | ---------- | ------ | ---------------------------------------------------------------- |
| `Send` bound incompatible with WASM | High       | M      | D3 audit first; `MaybeSend` or separate trait                    |
| `serde_yaml_ng` bloats WASM         | Medium     | M      | Profile with `twiggy`; consider `serde_yaml_ng` → lighter parser |
| `version::Set` not serializable     | High       | S      | D2 adds derives; flatten `Arc<Info>` if needed                   |
| Async executor mismatch in WASM     | Medium     | M      | `wasm-bindgen-futures` for browser; test early in D4             |
| Zed wasip2 needs different build    | Medium     | L      | Defer D9; may need separate crate or build profile               |

## Trade-offs Made

| Chose                            | Over                             | Because                                                      |
| -------------------------------- | -------------------------------- | ------------------------------------------------------------ |
| Trait injection (`Clock`)        | Feature flags in `actup-core`    | Only 3 call sites; `#[cfg]` soup not justified               |
| Eliminate FS from WASM surface   | Abstract FS behind trait         | Host always has the files; WASM never needs to discover/read |
| High-level API first             | Provider-level API first         | Consumers want results, not a toolkit                        |
| New `actup-wasm` crate           | WASM features in existing crates | Clean boundary; existing crates untouched                    |
| `wasm32-unknown-unknown` primary | `wasm32-wasip2` primary          | Broadest compat (browser + Node); wasip2 only for Zed        |
| `wasm-bindgen` + `wasm-pack`     | Manual WASM instantiation        | Mature toolchain, good TS type generation                    |

## Open Questions

- [ ] `Send` bound resolution — `MaybeSend`, drop `Send`, or separate trait? → Owner: D3 audit
- [ ] `version::Set` internal `Arc<Info>` — serializable across WASM↔JS? → Owner: D2 investigation
- [ ] npm scope: `@actup/wasm` vs `actup-wasm`? → Owner: publisher (kjanat)
- [ ] `actup-wasm` as workspace member or separate workspace (like Zed)? → Owner: spec author
- [ ] WASM LSP transport: `tower-lsp-server` WASM compat, or postMessage adapter? → Owner: D8
- [ ] Ship source maps in WASM npm package? → Owner: packaging decision
