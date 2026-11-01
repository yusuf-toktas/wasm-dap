# Changelog

## 0.1.0

- Debug WebAssembly modules through the wasm-dap adapter: breakpoints (line, function,
  instruction, conditional, hit count, logpoints), stepping by line or instruction, pause,
  variables, watches, memory and disassembly views.
- Launch configuration schema, snippets and a default configuration provider.
- **Debug WebAssembly File** command that asks which export to call for modules without `_start`.
- WebAssembly text (`.wat`) language with syntax highlighting.
- Adapter discovery through `wasm-dap.adapterPath`, a bundled binary, `PATH`, or a wasm-dap
  checkout; in untrusted workspaces the adapter is never taken from the workspace.
