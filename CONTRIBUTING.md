# Contributing to wasm-dap

Thank you for helping improve wasm-dap. This document explains how to build, test and submit
changes.

## Prerequisites

- Rust 1.95 or newer with the WebAssembly targets used by the test suite:

  ```sh
  rustup target add wasm32-unknown-unknown wasm32-wasip1
  ```

- Node.js 18 or newer, only for working on the VS Code extension.

## Building and testing

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

The tests exercise real behavior. Fixtures in `crates/wasm-dap-testkit/fixtures` are compiled
with `rustc` on first use and cached under `target/wasm-dap-fixtures`; WAT fixtures are
assembled in the tests. Lines are located with `// BP: <name>` markers, so editing a fixture
does not break the tests that use it.

| Suite | What it covers |
|-------|----------------|
| `crates/wasm-dap-core/src/**` unit tests | DWARF paths, line tables, location expressions, value formatting, parsing |
| `crates/wasm-dap-core/tests/dwarf.rs` | DWARF parsing of rustc output |
| `crates/wasm-dap-core/tests/engine.rs` | The debugging engine without DAP |
| `crates/wasm-dap-core/tests/dap.rs` | Complete DAP sessions against the in-process server |
| `crates/wasm-dap-cli/tests/e2e.rs` | The `wasm-dap` binary: DAP over stdio and TCP, `info`, `disasm`, `trace`, and a Cargo-built example |
| `crates/wasm-dap-cli/tests/toolchains.rs` | C and Zig programs built with Zig's toolchain; runs when `WASM_DAP_ZIG` points to a `zig` executable |
| `editors/vscode/test` | A debugging session inside VS Code through the extension |

To run the VS Code integration test:

```sh
cargo build -p wasm-dap-cli
cd editors/vscode
npm ci
WASM_DAP_BINARY=../../target/debug/wasm-dap npm test
```

Set `VSCODE_EXECUTABLE` to use an installed VS Code instead of downloading one.

## Architecture

[docs/architecture.md](docs/architecture.md) describes how the pieces fit together. In short:
`wasm-dap-protocol` holds DAP types and framing, `wasm-dap-core` holds the engine (`debugger`),
DWARF support (`dwarf`), module analysis (`module`), Wasmtime integration (`runtime`), the
tracer (`trace`) and the DAP server (`server`), and `wasm-dap-cli` wraps it in a command-line
tool.

## Guidelines

- Keep changes focused and covered by tests that exercise real modules.
- Code and identifiers are written in English; comments explain intent, not mechanics.
- Run `cargo fmt` and `cargo clippy` before submitting.
- Describe user-visible changes in `CHANGELOG.md` under an "Unreleased" heading.

## Releasing

1. Update the version in `Cargo.toml` (workspace) and `editors/vscode/package.json`, and
   move the changelog entries under the new version.
2. Tag the commit as `vX.Y.Z` and push the tag. The release workflow builds binaries for all
   platforms, packages a VS Code extension per platform with the adapter bundled, and drafts a
   GitHub release with the artifacts.
