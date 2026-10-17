# actup-test-support

Unpublished test helpers. `actup-core` and `actup-lsp` use it as a path-only
dev-dependency with no `version`. `cargo package` drops that dependency.

## Module Map

| Module   | Purpose                                     | Key Types |
| -------- | ------------------------------------------- | --------- |
| `lib.rs` | Capture `tracing` output of a closure's run | `logged`  |

## Conventions

- `publish = false`; keep it out of the release crate matrix and
  `[workspace.dependencies]`
- Add a helper here when a second crate needs it
