# actup-wasm

WASM bindings for `actup-core`. Compiles to `wasm32-unknown-unknown` via
`wasm-bindgen`. Excluded from `default-members` because `JsValue` is `!Send` —
native builds see an empty crate.

## Module Map

| Module      | Lines | Purpose                                     | Key Types                                   |
| ----------- | ----- | ------------------------------------------- | ------------------------------------------- |
| `lib.rs`    | 177   | Conditional compilation gate + WASM API     | `parseUsesRefs`, `checkRefs`, `planUpdates` |
| `bridge.rs` | 326   | JS provider → Rust `Provider` trait adapter | `JsProvider`, `JsProviderAdapter`           |
| `clock.rs`  | 22    | `Date.now()` → `SystemTime` for `Clock`     | `JsClock`                                   |

## WASM API (3 exports)

| Export          | Sync/Async | Purpose                                     |
| --------------- | ---------- | ------------------------------------------- |
| `parseUsesRefs` | sync       | Extract `uses:` locations from YAML         |
| `checkRefs`     | async      | Check refs via JS-provided `ActupProvider`  |
| `planUpdates`   | sync       | Compute text edits from `CheckResult` array |

## Where to Look

| Task                       | Start Here                                         |
| -------------------------- | -------------------------------------------------- |
| Add new WASM export        | `lib.rs` → `wasm_api` module                       |
| Change JS↔Rust bridging    | `bridge.rs` → `JsProviderAdapter`                  |
| Fix serde round-trip issue | `bridge.rs` → `JsVersionSet` / `build_version_set` |
| Change clock behavior      | `clock.rs` → `JsClock`                             |

## Build

Not built by `cargo build --all` (excluded from `default-members`). Must target
wasm32:

```bash
wasm-pack build crates/wasm --target web --out-dir ../../packages/actup-wasm/wasm
```

Or via the npm package's build script: `bun run build:wasm` from
`packages/actup-wasm/`.

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

- No Rust tests (wasm32-only code, `wasm-bindgen-test` listed but unused)
- Node.js integration tests in `tests/`:
  - `node_parse.mjs` — tests `parseUsesRefs` export
  - `node_check.mjs` — tests `checkRefs` + `planUpdates` with a JS mock provider
- Run manually after `wasm-pack build`: `node crates/wasm/tests/node_check.mjs`
- Tests use `console.log("PASS: ...")` / `throw new Error(...)` — no test
  framework

## Conventions

- `#![forbid(unsafe_code)]`
- All JS errors converted to `actup_core::Error::Network` via `js_err()` helper
- SHA validation uses `Sha40::new()` from core
- `JsClock` uses `js_sys::Date::now()` for `Clock` trait (needed by
  `version::Set::build_with_clock`)
