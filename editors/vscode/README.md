# wasm-dap for Visual Studio Code

Debug WebAssembly in VS Code with source-level breakpoints, stepping, variables, memory and
disassembly views. The extension connects VS Code to the
[wasm-dap](https://github.com/yusuf-toktas/wasm-dap) debug adapter, which runs your module in
[Wasmtime](https://wasmtime.dev) with guest debugging enabled.

## Features

- Breakpoints in Rust, C, C++, Zig and WebAssembly text (`.wat`) files, with conditions, hit
  counts and logpoints; function and instruction breakpoints.
- Step in, over and out by line or by instruction (from the Disassembly view), and pause.
- Variables decoded from DWARF: structs, arrays, pointers, enums, Rust enums, `&str`, `String`
  and `Vec<T>`; plus WebAssembly locals, the operand stack, globals and memories.
- Watch and hover expressions, the Debug Console, and VS Code's memory view.
- Pauses on WebAssembly traps and failing imports with the stack intact.
- WASI programs with arguments, environment, preopened directories and exit codes; libraries
  without `_start` by calling an exported function.
- Modules without debug information are shown as WebAssembly text.
- Syntax highlighting for `.wat` files.

## Requirements

The extension needs the `wasm-dap` executable. Release packages of the extension contain it.
Otherwise install it with

```sh
cargo install --git https://github.com/yusuf-toktas/wasm-dap wasm-dap-cli
```

or point the `wasm-dap.adapterPath` setting to a binary. The extension looks for the adapter in
this order: `wasm-dap.adapterPath`, the bundled binary, `wasm-dap` on `PATH`, and
`target/release` or `target/debug` of an open wasm-dap checkout. In untrusted workspaces
(Restricted Mode) `wasm-dap.adapterPath` is only read from user settings and binaries inside
the workspace are never run.

VS Code's memory view, opened from a variable's context menu, is provided by the
[Hex Editor](https://marketplace.visualstudio.com/items?itemName=ms-vscode.hexeditor) extension.

## Getting started

Build your program with debug information, for example a Rust program for WASI:

```sh
cargo build --target wasm32-wasip1
```

Add a configuration to `.vscode/launch.json` (type `wasm-dap` and pick a snippet):

```json
{
  "type": "wasm-dap",
  "request": "launch",
  "name": "Debug WebAssembly",
  "program": "${workspaceFolder}/target/wasm32-wasip1/debug/app.wasm",
  "args": [],
  "cwd": "${workspaceFolder}"
}
```

Set breakpoints and press <kbd>F5</kbd>. For a library, call an exported function:

```json
{
  "type": "wasm-dap",
  "request": "launch",
  "name": "Debug add",
  "program": "${workspaceFolder}/lib.wasm",
  "invoke": "add",
  "invokeArgs": [2, 3]
}
```

You can also right-click a `.wasm` or `.wat` file and choose **Debug WebAssembly File**, or
press <kbd>F5</kbd> with a `.wat` file open and no `launch.json`.

## Launch configuration

| Attribute | Description |
|-----------|-------------|
| `program` | The `.wasm` or `.wat` file to debug (required). |
| `args` | Arguments for a WASI program. |
| `env` | Environment variables. `inheritEnv: true` also passes VS Code's environment. |
| `cwd` | Base directory for relative paths. |
| `dirs` | Preopened directories: `"."`, `"data::/data"`. |
| `stdin` | File used as standard input. |
| `invoke`, `invokeArgs` | Exported function to call instead of `_start`, and its arguments. |
| `stopOnEntry` | Pause before the first instruction. |
| `sourceMap` | Map DWARF path prefixes to local paths. |
| `serverToken` | Secret printed by `wasm-dap serve --port`, when connecting with `debugServer`. |

## Settings

| Setting | Description |
|---------|-------------|
| `wasm-dap.adapterPath` | Path to the `wasm-dap` executable. |
| `wasm-dap.logLevel` | Adapter log level (`error` to `trace`), written to its stderr. |

## Expressions

Use source variables (`point.x`, `items[2]`, `*ptr`, `name.len()`), WebAssembly state
(`$local0`, `$global1`, `$stack0`, `$pc`), memory reads (`$u32(0x1000)`, `$str(ptr)`) and calls
to the module's functions (`fib(10)`) in watches, the Debug Console and breakpoint conditions.

## Development

```sh
cargo build -p wasm-dap-cli        # from the repository root
cd editors/vscode
npm ci
```

Open `editors/vscode` in VS Code and run **Run Extension** to start a development host. The
integration test drives a debugging session inside VS Code:

```sh
WASM_DAP_BINARY=../../target/debug/wasm-dap npm test
```

Set `VSCODE_EXECUTABLE` to use an installed VS Code instead of downloading one. `npm run
package` creates a `.vsix`; copy a `wasm-dap` binary into `bin/` first to bundle it.

## License

MIT OR Apache-2.0
