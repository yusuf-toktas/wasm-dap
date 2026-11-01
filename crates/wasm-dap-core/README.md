# wasm-dap-core

The engine behind [wasm-dap](https://github.com/yusuf-toktas/wasm-dap): a WebAssembly debugger
built on Wasmtime's guest debugging API.

- `debugger`: breakpoints, stepping, pause, stack frames, variables, expression evaluation and
  memory access for a module running on a dedicated thread.
- `dwarf`: DWARF line tables, scoped variables, types and WebAssembly location expressions.
- `module`: static analysis and text-format disassembly of module binaries.
- `runtime`: Wasmtime engine setup, WASI preview 1 host and import stubs.
- `trace`: execution tracing of calls, returns and traps.
- `server`: a Debug Adapter Protocol server.

See the repository README for usage and `docs/architecture.md` for the design.
