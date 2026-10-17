# Zed Extension WASM Embedding Evaluation

**Date:** 2026-02-20 **Status:** No embedded `actup-core` code ships. Zed no
longer runs extension slash commands (zed #52757), and the `/actions:update`
slash command was the only feature that ran `actup-core` inside the extension.
The extension starts `actup-lsp` and declares the `actup-mcp` context server.

## Question

Can `actup-core` be embedded directly in the Zed extension (wasm32-wasip2)
instead of shelling out to `actup-lsp`?

## Findings

### Compilation

`actup-core` compiles for `wasm32-wasip2`. The `MaybeSend`/`MaybeSync` trait
aliases handle the `Send`/`Sync` bounds for both WASM targets:

- `wasm32-unknown-unknown` (actup-wasm npm package)
- `wasm32-wasip2` (Zed extension)

`actup-providers` does not compile for `wasm32-wasip2`: it enables `tokio`
features that `tokio` refuses on wasm.

The Zed extension is a member of the root Cargo workspace and depends only on
`zed_extension_api`.

### HTTP: Zed provides a synchronous host-proxied client

Zed extensions get `zed:extension/http-client` (not WASI HTTP). Two functions:

- `fetch(HttpRequest) -> Result<HttpResponse, String>` (blocking)
- `fetch_stream(HttpRequest) -> Result<HttpResponseStream, String>` (streaming)

No URL restrictions. Auth headers work. This is sufficient for the GitHub REST
API.

**Key constraint:** The Zed HTTP client is **synchronous**. The `Provider` trait
returns `BoxFuture`, so an embedded provider must complete its future without
yielding.

### File access: Worktree read-only, no directory listing

`worktree.read_text_file(path)` reads files by known path. The worktree API has
no `readdir` or glob.
