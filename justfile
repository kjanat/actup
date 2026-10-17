# actup justfile - https://just.systems
# Run `just` or `just --list` to see available recipes

set unstable := true

mod zed 'extensions/zed/justfile'
mod vscode 'extensions/vscode/justfile'
mod wasm 'packages/actup-wasm/justfile'

alias b := build
alias t := test
alias r := run
alias ch := check
alias c := clippy
alias l := lint
alias i := inst-all

coverage-lcov := "target" / "lcov.info"
coverage-html := "target" / "llvm-cov" / "html" / "index.html"
schema_dir := "schemas"
actup_config := "actup-config.schema.json"
config-schema := schema_dir / actup_config

# List available recipes
default:
    @just --list --unsorted

# Install

# Install all binaries to ~/.cargo/bin
[group('install')]
inst-all: install-actup install-actup-lsp install-actup-mcp

# Install actup CLI
[group('install')]
install-actup:
    cargo +stable install-actup

# Install actup-lsp language server
[group('install')]
install-actup-lsp:
    cargo +stable install-lsp

# Install actup-mcp server (stub)
[group('install')]
install-actup-mcp:
    -cargo +stable install-mcp

# Uninstall binary from ~/.cargo/bin
[group('install')]
uninstall binary="actup":
    cargo uninstall {{ binary }}

# Build

# Build all packages (debug)
[group('build')]
build:
    cargo build --all

# Build all packages (release)
[group('build')]
build-release:
    cargo build --release --all

# Build only the CLI (release)
[group('build')]
cli:
    cargo build --package actup --release

# Check all packages compile
[group('build')]
check:
    cargo check --all

# Clean build artifacts
[group('build')]
clean:
    cargo clean

# Test

# Run all tests
[group('test')]
test:
    cargo test --all

# Run all tests with nextest
[group('test')]
nextest:
    cargo nextest run --all

# Run tests with output
[group('test')]
test-verbose:
    cargo test --all -- --nocapture

# Run tests for a specific package
[group('test')]
test-pkg pkg:
    cargo test --package {{ pkg }}

# Build actup-wasm for Node.js and run its crate node tests
[group('test')]
wasm-node-test:
    wasm-pack build crates/wasm --target nodejs --out-dir ../../target/wasm-pkg
    node crates/wasm/tests/node_parse.mjs
    node crates/wasm/tests/node_check.mjs

# Test the GitHub Action install and run scripts
[group('test')]
action-test:
    shellcheck action/*.sh
    action/test.sh

# Test the crates.io publish script
[group('test')]
crates-publish-test:
    shellcheck .github/actions/crates-publish/*.sh
    .github/actions/crates-publish/test.sh

# Test the release version script
[group('test')]
release-version-test:
    shellcheck .github/actions/release-version/*.sh
    .github/actions/release-version/test.sh

# Test the example-workflow pin script
[group('test')]
examples-test:
    shellcheck examples/*.sh
    examples/pin.test.sh

# Pin the example workflows to the commit of release tag `tag`
[group('docs')]
examples-pin tag:
    examples/pin.sh {{ tag }}

# Run tests with coverage (llvm-cov)
[group('test')]
cov:
    cargo llvm-cov nextest --all

# Run tests with coverage and generate HTML report
[group('test')]
cov-html:
    cargo llvm-cov nextest --all --html --output-path {{ coverage-html }}
    @echo "Coverage report: {{ coverage-html }}"

# Run tests with coverage and generate lcov report
[group('test')]
cov-lcov:
    cargo llvm-cov nextest --all --lcov --output-path {{ coverage-lcov }}

# Lint

# Run clippy on all packages
[group('lint')]
clippy:
    cargo clippy --all-targets --all-features --workspace -- -D clippy::all

# Run all nightly lints
[env("RUSTFLAGS", "-Zmacro-backtrace")]
[group('lint')]
clippy-nightly:
    cargo +nightly clippy --all-targets --all-features --workspace -- -D clippy::all

# Run clippy strict (fail on warnings)
[group('lint')]
clippy-strict:
    cargo clippy --all-targets --all-features -- -D warnings

# Run clippy strict on actup-wasm for wasm32
[group('lint')]
clippy-wasm:
    cargo clippy -p actup-wasm --target wasm32-unknown-unknown -- -D warnings

# Format code
[group('lint')]
fmt:
    dprint fmt

# Check formatting without changes
[group('lint')]
fmt-check:
    dprint check

# Run all lints (fmt + clippy strict)
[group('lint')]
lint: fmt-check clippy clippy-strict clippy-wasm

# Run

# Run the CLI (debug build)
[group('run')]
run *args:
    cargo run --bin actup -- {{ args }}

# Run the CLI (release build)
[group('run')]
run-release *args:
    cargo run --bin actup --release -- {{ args }}

# Run actup binary directly
[group('run')]
actup *args:
    cargo run --bin actup -- {{ args }}

# Watch for changes and check
[group('run')]
watch:
    cargo watch -c -x check

# Cache

# Show cache info
[group('cache')]
cache-info:
    cargo run --bin actup -- cache info

# Clear the cache
[group('cache')]
cache-clear:
    cargo run --bin actup -- cache clear

# Print cache path
[group('cache')]
cache-path:
    @cargo run --bin actup -- cache path

# Docs

# Generate docs
[group('docs')]
doc:
    cargo doc --all --no-deps

# Generate and open docs
[group('docs')]
doc-open:
    cargo doc --all --no-deps --open

# Generate JSON schema for config
[group('docs')]
schema:
    @mkdir -p {{ schema_dir }}
    cargo run --bin actup -- schema > {{ config-schema }}

# Regenerate the `actup_core::yaml` simple uppercase table from a UnicodeData.txt 15.0.0 path
[group('docs')]
simple-upper ucd:
    cargo run --quiet -p actup-core --example gen_simple_upper -- {{ ucd }} crates/core/src/yaml/simple_upper.rs

# CI

# Run full CI checks locally
[group('ci')]
ci: fmt-check clippy-strict clippy-wasm test wasm-node-test action-test examples-test crates-publish-test release-version-test
    @echo "All CI checks passed!"

# Run actup on this repo
[group('ci')]
self-check:
    cargo run --release --package actup -- check

# Fail when a workflow or action ref in this repo is not pinned to a commit SHA
[group('ci')]
self-check-pins:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo run --package actup -- check --format json . \
        | jq -e 'map(select(.reference | test("^[0-9a-f]{40}$") | not)) | if length == 0 then true else map("\(.file):\(.line): \(.action) is not pinned to a commit SHA") | join("\n") + "\n" | halt_error(1) end'

# Run actup on this repo (verbose)
[group('ci')]
self-check-verbose:
    cargo run --release --package actup -- check --verbose

# Dependencies

# Update dependencies
[group('deps')]
update:
    cargo update

# Show outdated dependencies
[group('deps')]
outdated:
    cargo outdated -R

# Count lines of code
[group('deps')]
loc:
    @tokei crates/

# Extensions

# Check all editor extensions
[group('extensions')]
ext-check: zed::clippy vscode::lint vscode::typecheck vscode::test

# Verify the Rust→tsify→TS→consumer chain end-to-end (wasm build, wasm tests, then VS Code typecheck against fresh `.d.ts`)
[group('extensions')]
verify-wasm: wasm::build wasm::test vscode::typecheck

# Run full CI checks: ci, then wasm/TS shape verification, then extensions
[group('ci')]
ci-full: ci wasm::build wasm::test ext-check

# Branding

# Regenerate canonical icon PNGs from SVG sources (gitignored output, ad-hoc usage)
[group('branding')]
icons:
    bun branding/generate-icon.ts -s 128 -p 20 -r 18 "#0d1117" branding/icon.svg branding/icon-128.png
    bun branding/generate-icon.ts -s 256 -p 20 -r 36 "#0d1117" branding/icon.svg branding/icon-256.png
    bun branding/generate-icon.ts -s 512 -p 20 -r 72 "#0d1117" branding/icon.svg branding/icon-512.png
