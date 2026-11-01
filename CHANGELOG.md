# Changelog

All notable changes to this project are documented in this file. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [0.1.0] - 2026-11-01

First public release.

### Added

- Debugging engine built on Wasmtime's guest debugging API: breakpoints patched into compiled
  code, single-stepping, frame inspection and trap interception on a dedicated debuggee thread.
- Source-level debugging from DWARF: line tables, function names, scoped variables, location
  expressions (`DW_OP_WASM_location`, frame-base relative, pieces, location lists) and a type
  model covering base types, pointers, structs, unions, arrays, enums and Rust enums.
- Value rendering for Rust `&str`, slices, `String` and `Vec<T>`, C strings and character
  arrays, and bit fields; DWARF 4 and 5 from rustc, clang and the Zig compiler, including Zig's
  by-reference parameters.
- Debugging of `.wat` files at the source level and of any module through a generated
  WebAssembly text view.
- Line, function and instruction breakpoints with conditions, hit counts and logpoints;
  exception filters for traps and host function errors.
- Step in, over and out by line or instruction; pause via epoch interruption.
- Scopes for source locals, WebAssembly locals, the operand stack, globals and memories.
- Expression evaluator for watch, hover, the Debug Console and breakpoint conditions, including
  member access, indexing, pointer arithmetic, `len()`/`is_empty()`, memory reads and calls to
  module functions.
- Memory read/write, variable and global modification, disassembly with source correlation.
- WASI preview 1 support with arguments, environment, preopened directories, a stdin file,
  captured stdout/stderr and exit codes; stubs for unresolved imports.
- Automatic source mapping for the Rust standard library when `rust-src` is installed.
- `wasm-dap info`, `disasm`, `trace` (execution tracing with arguments, return values, traps
  and optional instruction tracing; text or JSON) and `serve` (stdio or TCP).
- Visual Studio Code extension with launch schema, snippets, a WAT language definition, a
  "Debug WebAssembly File" command and adapter discovery.

### Changed

- `wasm-dap trace` now executes the program and traces calls; the previous static listing of
  module sections is part of `wasm-dap info`.
- The DAP `initialized` event is sent after a successful `launch`, once the module is loaded.

### Fixed

- DAP capabilities are serialized with the protocol's camelCase field names.
- Diagnostics are written to stderr so they can no longer corrupt the DAP stream on stdout,
  and are colored only when stderr is a terminal.
- `wasm-dap serve --port` keeps listening when accepting a connection fails or stderr is
  closed, instead of exiting, and prints the address it actually listens on, so `--port 0`
  picks a free port.

### Security

- `wasm-dap serve --port` requires clients to present a secret (`serverToken`) before they can
  launch programs or make other requests; the token is random unless set with
  `WASM_DAP_TOKEN` or `--token`, which warns because command lines are visible to other users.
- Text output of `info`, `disasm` and `trace` escapes control characters, so names in a module
  cannot inject terminal escape sequences.
- The `source` request only returns the program and the source files named in its DWARF.
- Malformed debug information, oversized DAP messages and overly complex expressions cannot
  exhaust the adapter's stack or memory: traversals of types, scopes, pointers and expression
  trees are bounded, sizes taken from DWARF are checked, and DAP headers and bodies have size
  limits.
- The VS Code extension supports Workspace Trust: in untrusted workspaces it never runs an
  adapter taken from the workspace (`wasm-dap.adapterPath` is read from user settings only and
  binaries inside the workspace are ignored).
