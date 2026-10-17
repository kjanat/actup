# actup-wasm

WASM bindings for `actup-core`. Compiles to `wasm32-unknown-unknown` via
`wasm-bindgen`. Excluded from `default-members` because `JsValue` is `!Send` —
native builds see an empty crate.

## Module Map

| Module        | Purpose                                     | Key Types                                                                                                                                                                                                   |
| ------------- | ------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `lib.rs`      | Conditional compilation gate                | `bridge`, `wasm_api`                                                                                                                                                                                        |
| `wasm_api.rs` | WASM API                                    | `parseUsesRefs`, `checkRefs`, `planUpdates`, `sortVersionTags`, `parseUses`, `parseActionPath`, `isHostSegment`, `parseConfig`, `parseExplicitConfig`, `parseRepoAndExplicitConfig`, `repoConfigCandidates` |
| `bridge.rs`   | JS provider → Rust `Provider` trait adapter | `JsProvider`, `JsProviderAdapter`                                                                                                                                                                           |
| `clock.rs`    | `Date.now()` → `SystemTime` for `Clock`     | `JsClock`                                                                                                                                                                                                   |

## WASM API (10 exports)

| Export                       | Sync/Async | Purpose                                             |
| ---------------------------- | ---------- | --------------------------------------------------- |
| `parseUsesRefs`              | sync       | Extract `uses:` locations from YAML                 |
| `checkRefs`                  | async      | Check refs via JS-provided `ActupProvider`          |
| `planUpdates`                | sync       | Compute text edits from `LocatedCheckResult` array  |
| `sortVersionTags`            | sync       | Sort `VersionTag[]` newest first                    |
| `parseUses`                  | sync       | Parse one `uses:` value into an `ActionRef`         |
| `parseActionPath`            | sync       | Parse `[host/]owner/repo[/subpath]`                 |
| `isHostSegment`              | sync       | Apply core's host rule to a first path segment      |
| `parseConfig`                | sync       | Parse a repo config file into a `RepoConfig`        |
| `parseExplicitConfig`        | sync       | Parse a named config file, every key applying       |
| `parseRepoAndExplicitConfig` | sync       | Layer a named config file over the repo config file |
| `repoConfigCandidates`       | sync       | Repo config paths of one directory, by priority     |

## Where to Look

| Task                       | Start Here                                       |
| -------------------------- | ------------------------------------------------ |
| Add new WASM export        | `wasm_api.rs`                                    |
| Change JS↔Rust bridging    | `bridge.rs` → `JsProviderAdapter`                |
| Fix serde round-trip issue | `bridge.rs` → `VersionSet` / `build_version_set` |
| Change clock behavior      | `clock.rs` → `JsClock`                           |

## Build

Not built by `cargo build --all` (excluded from `default-members`). Must target
wasm32:

```bash
wasm-pack build crates/wasm --target web --out-dir ../../packages/actup-wasm/wasm
```

Or via the npm package's build script: `bun run build:wasm` from
`packages/actup-wasm/`.

Workspace clippy builds this crate for the host target, which skips every
module. Lint the wasm32 build with `just clippy-wasm`.

## Architecture

- `#[cfg(target_arch = "wasm32")]` guards all functional code — native builds
  compile to an empty crate
- `crate-type = ["cdylib", "rlib"]` — `cdylib` for wasm-pack output, `rlib` for
  workspace `cargo check`
- JS provider objects are wrapped via `wasm_bindgen extern "C"` bindings →
  `JsProviderAdapter` implements `Provider`
- Serde round-trips use `serde-wasm-bindgen` (not `JsValue::from_serde`)
- `BoxFuture` from `actup_core::compat` for async trait methods (WASM is
  single-threaded, no `Send` needed)

## Anti-Patterns (This Crate)

- **Never add to `default-members`** — `JsValue` is `!Send`, breaks native
  compilation
- **No filesystem or network access** — all I/O goes through the JS provider
  interface
- **Never use `JsValue::from_serde`** — deprecated; use `serde_wasm_bindgen` for
  all conversions
- **Don't publish to crates.io** — distributed via npm (`packages/actup-wasm`)

## Tests

- No Rust tests (wasm32-only code)
- Node.js integration tests in `tests/`:
  - `node_parse.mjs` — tests `parseUsesRefs` export
  - `node_check.mjs` — tests `checkRefs` + `planUpdates` with a JS mock provider
- `just wasm-node-test` runs
  `wasm-pack build crates/wasm --target nodejs --out-dir ../../target/wasm-pkg`
  and both tests. `just ci` and the CI `WASM` job run it
- Tests use `console.log("PASS: ...")` / `throw new Error(...)` — no test
  framework

## Conventions

- `#![forbid(unsafe_code)]`
- `js_err()` maps a provider rejection's `kind` (`notFound`, `rateLimited`,
  `auth`, `forbidden`) to the matching `actup_core::Error`; any other rejection
  becomes `actup_core::Error::Network`
- SHA validation uses `Sha40::new()` from core
- `JsClock` uses `js_sys::Date::now()` for `Clock` trait (needed by
  `version::Set::build_with_clock`)
