# actup for Zed

Zed extension that integrates the `actup-lsp` language server for GitHub Actions
workflow files.

Provides diagnostics, code actions, hover info, and completion for `uses:` refs.

## Prerequisites

`actup-lsp` must be available on your system. Install from the repo root:

```sh
cargo install --path crates/lsp
```

Or build without installing:

```sh
cargo build -p actup-lsp --release
```

## Install (user)

> Not yet published to the Zed extension registry.

Once published, search "actup" in Zed's extension panel and install.

## Install (dev)

1. Open Zed's command palette (`Cmd+Shift+P` / `Ctrl+Shift+P`)
2. Run **zed: install dev extension**
3. Select the `extensions/zed/` directory

Zed will compile the WASM extension and load it. Repeat this step after editing
extension source to pick up changes.

## Configuration

### actup config for `/actions:update`

The `/actions:update` slash command reads actup config from the worktree root
only. It takes the first repo config candidate in `actup-core` discovery order
(`actup.*`, `.actup.*`, `.config/actup.*`, `.github/actup.*`; TOML, JSON, JSONC
or YAML). When `lsp.actup-lsp.initialization_options.configPath` names a file,
the command reads that file over the candidate as an explicit config, where
every key applies, as `actup-lsp` does. A relative `configPath` starts at the
worktree root. On Windows, a drive-relative `configPath` such as
`C:ci\actup.toml` starts at the worktree root when its drive is the worktree's
drive, and a path rooted without a drive, such as `\repo\actup.toml`, takes the
worktree's drive or UNC share. The command reads files inside the worktree only,
and reports an error for a `configPath` outside it. A `\\?\` or `\\.\` device
path is an error even when it names a file inside the worktree. It applies
`ACTUP_*` variables from the worktree shell environment on top. Parent-directory
configs and the user config do not apply. The first line of the output names the
files it used, or `defaults`.

`actup-lsp` resolves config the same way as the `actup` CLI.

### Binary path override

If `actup-lsp` is not on `PATH`, point Zed to the binary in your settings
(`~/.config/zed/settings.json`):

<!--dprint-ignore-start-->

```jsonc
{
  "lsp": {
    "actup-lsp": {
      "binary": {
        "path": "/absolute/path/to/actup-lsp"
      }
    }
  }
}
```

<!--dprint-ignore-end-->

### File type detection

Zed does not auto-detect GitHub Actions files by path. Add this to your settings
so workflow files use the "GitHub Actions" language (which this extension
defines):

<!--dprint-ignore-start-->

```jsonc
{
  "file_types": {
    "GitHub Actions": [
      "**/.github/workflows/*.yml",
      "**/.github/workflows/*.yaml"
    ]
  }
}
```

<!--dprint-ignore-end-->

Files detected as plain YAML also work -- the extension attaches the LSP to both
"GitHub Actions" and "YAML" languages.

## Features

| Feature      | Description                                          |
| ------------ | ---------------------------------------------------- |
| Diagnostics  | Warning on outdated refs, error on unresolvable refs |
| Code actions | Quick-fix to update single ref or all refs in file   |
| Hover        | Version status, README excerpt, action.yml summary   |
| Completion   | Tag/version completion triggered by `@`              |

## Extension structure

```tree
extensions/zed/
  Cargo.toml                            # cdylib crate, wasm32-wasip2 target
  extension.toml                        # Zed extension manifest
  src/lib.rs                            # Extension entry point
  languages/github_actions/config.toml  # "GitHub Actions" language definition
```

## Development

### Build check

```sh
cd extensions/zed
cargo check --target wasm32-wasip2
```

Requires the WASM target:

```sh
rustup target add wasm32-wasip2
```

### How it works

The extension is a thin WASM shim. It does not bundle `actup-lsp` -- it locates
the binary on the host system (via `PATH` or user settings) and tells Zed to
spawn it over stdio. All LSP logic lives in `crates/lsp/`.
