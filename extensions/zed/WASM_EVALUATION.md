# Zed Extension WASM Embedding Evaluation

**Date:** 2026-02-20 **Decision:** Feasible. Implementation started.

## Question

Can `actup-core` be embedded directly in the Zed extension (wasm32-wasip2)
instead of shelling out to `actup-lsp`?

## Findings

### Compilation: No conflicts

`actup-core` and `actup-providers` both compile for `wasm32-wasip2` without
modification. The `MaybeSend`/`MaybeSync` trait aliases (from the earlier
`maybe-send` task) handle the `Send`/`Sync` bounds correctly for both WASM
targets:

- `wasm32-unknown-unknown` (actup-wasm npm package)
- `wasm32-wasip2` (Zed extension)

No separate build target or adapter layer needed. The Zed extension remains a
separate `[workspace]` as before, just with path deps to `actup-core` and
`actup-providers`.

### HTTP: Zed provides a synchronous host-proxied client

Zed extensions get `zed:extension/http-client` (not WASI HTTP). Two functions:

- `fetch(HttpRequest) -> Result<HttpResponse, String>` (blocking)
- `fetch_stream(HttpRequest) -> Result<HttpResponseStream, String>` (streaming)

No URL restrictions. Auth headers work. This is sufficient for the GitHub REST
API.

**Key constraint:** The Zed HTTP client is **synchronous**. The `Provider` trait
returns `BoxFuture`. Rather than adapting the async trait to sync, the
implementation bypasses the `Provider` trait entirely and calls Zed's `fetch()`
directly to build a `version::Set`, then passes it to `check_action()` (which is
synchronous).

### File access: Worktree read-only, no directory listing

`worktree.read_text_file(path)` reads files by known path. No `readdir` or glob
support. The implementation probes a list of common workflow filenames (ci.yml,
build.yml, etc.). This is a pragmatic limitation, not a blocker.

### Binary size

| Configuration                                       | WASM size |
| --------------------------------------------------- | --------- |
| Before (zed_extension_api only)                     | ~17 KB    |
| After (+ actup-core + actup-providers + serde_json) | 371 KB    |

Acceptable for a Zed extension (no explicit size limit; extensions like
Biome/Ruff ship multi-MB WASM binaries).

## Architecture

```txt
Slash command (/actions:update)
  |
  +-> discover_workflow_files()  [probes known paths via worktree API]
  +-> extract_uses_from_yaml()   [actup-core parser, sync]
  +-> parse_uses()               [actup-core, sync]
  +-> fetch_version_set()        [Zed HTTP -> GitHub REST -> version::Set]
  +-> check_action()             [actup-core resolution, sync]
  +-> format output              [markdown for assistant context]
```

Native LSP mode (`actup-lsp`) is unchanged. Desktop Zed still delegates to the
native binary for diagnostics, code actions, hover, and completions. The
embedded WASM path only powers the slash command.

## Limitations

1. **No directory listing** — can't discover arbitrary workflow filenames.
   Hardcoded list of common names. Follow-up: could use
   `worktree.read_text_file` on a manifest or parse `.github/` structure via
   heuristics.

2. **Config loading** — spike uses `Config::default()`. Full config parsing (via
   figment/TOML) needs investigation for WASM compat.

3. **GitHub-only** — the HTTP provider is GitHub REST only. GitLab/Gitea would
   need additional provider implementations using Zed's `fetch()`.

4. **No async** — Zed's `fetch()` is blocking. Sequential repo fetches.
   Acceptable for typical workflow file counts (1-10 repos).

5. **Unit tests can't run** — `cargo test --target wasm32-wasip2` requires
   wasmtime. Tests compile but need a WASM runtime to execute.

## Follow-up work

- [ ] Full config loading from `actup.toml` in WASM
- [ ] GitLab/Gitea provider implementations for Zed
- [ ] Context server integration (`[context_servers.actup-mcp]`)
- [ ] Move version fetching into LSP (native) and cache results for slash cmd
- [ ] wasmtime-based test runner in CI for Zed extension unit tests
