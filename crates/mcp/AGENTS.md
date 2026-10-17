# actup-mcp

Model Context Protocol server for AI agents, served over stdio with
`rust-mcp-sdk` 0.9 (`server` + `stdio` features only).

## Layout

| File                      | Purpose                                                             |
| ------------------------- | ------------------------------------------------------------------- |
| `src/main.rs`             | Binary: `--root`, `--config`, registry, `FileCache`, stdio          |
| `src/lib.rs`              | `Server` (`ServerHandler`), tool list, `INSTRUCTIONS`, read cache   |
| `src/args.rs`             | Tool argument structs, `schemars` input schemas, ref validation     |
| `src/read.rs`             | `actionGetReadme`, `actionGetActionYml`, `getVersions`, `getLatest` |
| `src/write.rs`            | `updateFile`, `updateAllUsesToVersion`                              |
| `src/confine.rs`          | `Root`: canonical path confinement                                  |
| `src/report.rs`           | `Status` vocabulary and the result envelope                         |
| `src/diff.rs`             | Unified diffs for in-line edits                                     |
| `tests/stdio_contract.rs` | Contract tests over stdio (`harness = false`)                       |

## Rules

- Provider setup goes through `actup_core::provider::Registry::from_config`, the
  same function the CLI and LSP call. Do not build a registry here.
- Edits go through `actup_core::patch::FileEdits` and `retarget_edit`, so a
  `uses:` value that is escaped or spans lines is never rewritten, and the pin
  style follows `Style::writes_sha` as in `update`.
- Writes go through `actup_core::rewrite_atomic`: it refuses a file that
  resolves outside the root, the file is re-read under the write lock,
  `Splices::apply` refuses a file whose edited lines changed since the scan, and
  `SourceFormat::validate_edit` refuses invalid YAML or a changed `uses:` count
  unless `updateFile` passes `validateYaml: false`.
- Each file is read in the `SourceFormat` of its extension. Edits are built from
  `FileEdits::writable`, so a plain-text line never gets a new version comment
  and only a label comment right after the value is rewritten.
- Every path argument resolves through `Root::file` / `Root::dir`, and every
  discovered file through `Root::contain`. Canonicalization rejects `..` and
  symlinks that leave the root.
- Mutators are `destructiveHint: true`, honour `dryRun`, and report a change
  only when a line's text differs, so a repeated call changes nothing.
- `updateFile` is all or nothing and refuses edits that name actions on more
  than one host. `updateAllUsesToVersion` skips refs it cannot place or that
  `scan.ignore` matches, and reports them in `skipped`. When two edits of a file
  overlap, `FileEdits::plan` skips every edit of that file.
- Every result carries `provider`, `host` and `status`. Argument and path errors
  are tool errors (`isError`) without a status.
- Read results are memoized for `cache.ttl` when the provider answered.
  `Registry::from_config` gets `Retention::expiring(config.cache.ttl)`.
  `main.rs` builds `FileCache::from_config` and passes it to `Server::new`;
  `getVersions` and `getLatest` read version sets through `FileCache::versions`,
  the disk cache the CLI and LSP use. `updateAllUsesToVersion` resolves
  `targetRef` with `refresh_ref_to_sha` on every call unless at least one ref
  matches and `scan.ignore` matches all of them.

## Tests

`tests/stdio_contract.rs` re-executes itself with `MCP_CONTRACT_ROOT` set to
serve a mock-provider `Server` on stdio, then drives it over JSON-RPC. Its
binary cases spawn the real `actup-mcp` and use no network. Run one case with
`cargo test -p actup-mcp --test stdio_contract -- <name-substring>`.
