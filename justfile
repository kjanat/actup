# actup justfile - https://just.systems
# Run `just` or `just --list` to see available recipes

set unstable := true
set shell := ["bash", "-cu"]

mod zed 'extensions/zed/justfile'
mod vscode 'extensions/vscode/justfile'
mod wasm 'packages/actup-wasm/justfile'

alias b := build
alias t := test
alias r := run
alias ch := check
alias c := clippy
alias l := lint
alias i := install

schema_dir := "schemas"
actup_config := "actup-config.schema.json"

# List available recipes
default:
    @just --list --unsorted

# Install

# Install all binaries to ~/.cargo/bin
[group('install')]
install: install-actup install-actup-lsp install-actup-mcp

# Install actup CLI
[group('install')]
install-actup:
    cargo install --bin=actup --path crates/actup

# Install actup-lsp language server
[group('install')]
install-actup-lsp:
    cargo install --bin=actup-lsp --path crates/lsp

# Install actup-mcp server (stub)
[group('install')]
install-actup-mcp:
    -cargo install --bin=actup-mcp --path crates/mcp

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

# Run tests with coverage (llvm-cov)
[group('test')]
cov:
    cargo llvm-cov nextest --all

# Run tests with coverage and generate HTML report
[group('test')]
cov-html:
    cargo llvm-cov nextest --all --html
    @echo "Coverage report: target/llvm-cov/html/index.html"

# Run tests with coverage and generate lcov report
[group('test')]
cov-lcov:
    cargo llvm-cov nextest --all --lcov --output-path target/lcov.info

# Lint

# Run clippy on all packages
[group('lint')]
clippy:
    cargo clippy --all-targets --all-features

# Run all nightly lints
[group('lint')]
clippy-nightly:
    env RUSTFLAGS="-Zmacro-backtrace" cargo +nightly clippy --all-targets --all-features --workspace -- -D clippy::all

# Run clippy strict (fail on warnings)
[group('lint')]
clippy-strict:
    cargo clippy --all-targets --all-features -- -D warnings

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
lint: fmt-check clippy-strict

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
    cargo run --bin actup -- schema > {{ schema_dir }}/{{ actup_config }}

# CI

# Run full CI checks locally
[group('ci')]
ci: fmt-check clippy-strict test
    @echo "All CI checks passed!"

# Run actup on this repo
[group('ci')]
self-check:
    cargo run --release --package actup -- check

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
ext-check: zed::clippy vscode::lint vscode::typecheck

# Run full CI checks including extensions
[group('ci')]
ci-full: ci ext-check
