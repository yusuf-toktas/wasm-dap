# wasm-dap

**A source-level debugger, execution tracer and module inspector for WebAssembly, built on
[Wasmtime](https://wasmtime.dev) and the [Debug Adapter Protocol](https://microsoft.github.io/debug-adapter-protocol/).**

wasm-dap runs your module inside Wasmtime with guest debugging enabled. It pauses real
execution at breakpoints, steps through your Rust, C, C++, Zig or WAT source, and shows the
live state of the program: source variables decoded from DWARF, WebAssembly locals, the
operand stack, globals and linear memory. Visual Studio Code is supported out of the box;
any DAP client can use the same adapter.

```text
$ wasm-dap trace arith.wasm --invoke fib 3
→ arith::fib(3)  [/fixtures/arith.rs:25]
  → arith::fib(2)  [/fixtures/arith.rs:25]
    → arith::fib(1)  [/fixtures/arith.rs:25]
    ← arith::fib = 1
    → arith::fib(0)  [/fixtures/arith.rs:25]
    ← arith::fib = 0
  ← arith::fib = 1
  → arith::fib(1)  [/fixtures/arith.rs:25]
  ← arith::fib = 1
← arith::fib = 2

returned 2 | 5 calls, max depth 3, 10 events, 2 ms
```

## Features

**Debugging**

- Line breakpoints in source files (from DWARF), in `.wat` files, and in a generated WebAssembly
  text view for modules without debug information.
- Function breakpoints by source, mangled or export name; instruction breakpoints from the
  disassembly view.
- Conditional breakpoints, hit counts (`3`, `== 3`, `> 3`, `% 3`) and logpoints (`n = {n}`).
- Step in, step over and step out by source line or by instruction, continue, and pause (also
  for code stuck in an infinite loop).
- Pauses on WebAssembly traps and on failing host calls, with the full stack still inspectable.
- Breakpoints can be added or removed while the program is running.

**Inspection**

- Real call stacks with source locations, including recursion.
- Source variables with their types: integers, floats, booleans, characters, pointers (with C
  string previews), structs, unions, arrays, enums, Rust enums (`Some(7)`,
  `Rect { w: 3, h: 4 }`), `&str`, slices, `String` and `Vec<T>`.
- WebAssembly locals, operand stack, globals and memories of every frame.
- Watch and hover expressions with C-like syntax, member access, indexing, pointer arithmetic,
  `len()`/`is_empty()` and calls to the module's own functions.
- Modifying variables, globals and memory; VS Code's memory view (provided by the
  [Hex Editor](https://marketplace.visualstudio.com/items?itemName=ms-vscode.hexeditor)
  extension); disassembly view with source correlation.

**Running programs**

- WASI preview 1 programs: arguments, environment, preopened directories, a stdin file, exit
  codes, and stdout/stderr in the debug console.
- Libraries and modules without `_start`: call any exported function with typed arguments.
- Imports that nobody provides are stubbed; calling one stops with a clear error.

**Command line**

- `wasm-dap info`: imports, exports, functions with signatures and source locations, memories,
  globals, tables, custom sections and debug information, as text or JSON.
- `wasm-dap disasm`: function disassembly annotated with source lines.
- `wasm-dap trace`: runs a program and records every call with its arguments, every return with
  its values, and traps with backtraces (optionally every instruction), as text or JSON.
- `wasm-dap serve`: the Debug Adapter Protocol server, over stdio or TCP.

## Installation

wasm-dap needs Rust 1.95 or newer to build.

```sh
cargo install --git https://github.com/yusuf-toktas/wasm-dap wasm-dap-cli
```

This installs the `wasm-dap` binary. For every version tag, the release workflow publishes
prebuilt binaries for Linux, macOS and Windows and VS Code extension packages with the adapter
bundled on the [releases page](https://github.com/yusuf-toktas/wasm-dap/releases).

### Visual Studio Code

Install the extension from a release (`code --install-extension wasm-dap-<platform>.vsix`) or
build it from [`editors/vscode`](editors/vscode). The extension uses the path in the
`wasm-dap.adapterPath` setting, the bundled adapter, or the `wasm-dap` binary on `PATH`. In
untrusted workspaces (VS Code's Restricted Mode) it never runs an adapter taken from the
workspace.

## Quick start

### Debug a Rust program in VS Code

```sh
rustup target add wasm32-wasip1
cd examples/rust-wasi
code .
```

Open `src/main.rs`, click in the gutter to set a breakpoint and press <kbd>F5</kbd>. The
example's `.vscode/launch.json` builds the program with
`cargo build --target wasm32-wasip1` and starts it under the debugger:

```json
{
  "type": "wasm-dap",
  "request": "launch",
  "name": "Debug word-count",
  "program": "${workspaceFolder}/target/wasm32-wasip1/debug/word-count.wasm",
  "args": ["the", "cat", "sat", "on", "the", "mat"],
  "preLaunchTask": "build word-count"
}
```

To debug a function of a library module, name it with `invoke`:

```json
{
  "type": "wasm-dap",
  "request": "launch",
  "name": "Debug fib",
  "program": "${workspaceFolder}/target/wasm32-unknown-unknown/debug/mylib.wasm",
  "invoke": "fib",
  "invokeArgs": [10]
}
```

You can also right-click a `.wasm` or `.wat` file in the explorer and choose
**Debug WebAssembly File**; for modules without `_start` the extension asks which exported
function to call.

### Use the command line

```sh
wasm-dap info app.wasm               # structure and debug information
wasm-dap info app.wasm --json
wasm-dap disasm app.wasm --function main
wasm-dap trace app.wasm -- arg1 arg2 # WASI program with arguments
wasm-dap trace lib.wasm --invoke add 2 3
wasm-dap trace app.wasm --json --env NAME=value --dir .
wasm-dap serve                       # DAP over stdio (what editors use)
wasm-dap serve --port 4711           # DAP over TCP, e.g. for "debugServer": 4711
```

The TCP server only accepts clients that send its secret as `serverToken` in the launch
configuration, because any local user can connect to a loopback port. It prints its address
and a random token at startup; set `WASM_DAP_TOKEN` to choose the token (`--token` works too,
but other local users can read command lines), and pass `--port 0` to let the system pick a
free port. In VS Code, combine `"debugServer": 4711` with `"serverToken": "<token>"`.

## Preparing your module

wasm-dap reads standard DWARF debug information embedded in the module. Without DWARF you can
still debug at the WebAssembly level (function names come from the `name` section when present).

| Language | Build with debug information |
|----------|------------------------------|
| Rust | `cargo build --target wasm32-wasip1` (or `wasm32-unknown-unknown`); debug builds include DWARF. For release builds set `debug = true` in `[profile.release]`. |
| C / C++ | `clang --target=wasm32-wasi -g -O0 ...` with [wasi-sdk](https://github.com/WebAssembly/wasi-sdk), or `zig cc -target wasm32-wasi -g -O0 ...` |
| Zig | `zig build-exe -target wasm32-wasi -O Debug ...` |
| TinyGo | `tinygo build -target=wasi -no-debug=false ...` |

The test suite debugs programs built with rustc (DWARF 4 and 5), clang through `zig cc`, the Zig
compiler and WAT text. TinyGo emits the same kind of DWARF but is not covered by the tests.
| WebAssembly text | Pass the `.wat` file itself as `program`; wasm-dap assembles it with line information pointing back to the text. |

Avoid tools that strip custom sections (`wasm-opt --strip-debug`, `wasm-strip`) for debug builds.
If the binary was built on another machine or with `--remap-path-prefix`, map the recorded paths
with `sourceMap`. Sources of the Rust standard library are found automatically when the
`rust-src` rustup component is installed.

## Launch configuration

| Attribute | Type | Description |
|-----------|------|-------------|
| `program` | string | **Required.** The `.wasm` binary or `.wat` text file to debug. |
| `args` | string[] | Arguments passed to a WASI program after `argv[0]`. |
| `env` | object | Environment variables for the program. |
| `inheritEnv` | boolean | Also expose the adapter's environment. Default `false`. |
| `cwd` | string | Directory that relative paths are resolved against. |
| `dirs` | string[] | Preopened directories, `HOST` or `HOST::GUEST` (as in `wasmtime --dir`). |
| `stdin` | string | File provided as standard input (empty otherwise). |
| `invoke` | string | Exported function to call instead of `_start`. |
| `invokeArgs` | array | Arguments for `invoke`, converted to the parameter types (numbers or strings such as `"0x10"`, `"nan"`). |
| `stopOnEntry` | boolean | Pause before the first instruction runs. |
| `sourceMap` | object | Maps DWARF path prefixes to local paths, e.g. `{ "/build": "${workspaceFolder}" }`. |
| `noDebug` | boolean | Set by "Run Without Debugging": breakpoints and exception stops are disabled. |
| `serverToken` | string | Secret of a `wasm-dap serve --port` server; only needed with `debugServer`. |

Without `invoke`, wasm-dap calls `_start` if the module exports it, otherwise it only
instantiates the module, which runs its start function.

The **Breakpoints** view offers two exception filters, both enabled by default:
*WebAssembly traps* and *Host function errors* (failing imports, including imports nobody
provided).

## Expressions

Watch, hover, Debug Console and breakpoint conditions accept the same expressions:

| Form | Example |
|------|---------|
| Source variables (innermost scope wins) and statics | `total`, `points`, `COUNTER`, `app::CONFIG` |
| Member access, indexing, dereference, address-of | `p.x`, `points[2].y`, `node->next`, `*ptr`, `&value` |
| Arithmetic, comparison, logic, bitwise | `a + b * 2 == 10 && (flags & 4) != 0` |
| Lengths | `word.len() > 7`, `items.is_empty()` |
| WebAssembly state | `$local0`, `$param1`, `$n` (named local), `$global0`, `__stack_pointer`, `$stack0`, `$pc` |
| Linear memory | `$u32(0x1000)`, `$i8(p)`, `$f64(addr)`, `$str(ptr)`, `$str(ptr, len)` |
| Function calls | `fib(10)`, `checksum(buffer, 64)` (not run on hover; limited to 5 seconds) |
| Literals | `42`, `-7`, `0xff`, `0b1010`, `2.5`, `'a'`, `"text"`, `true` |

Logpoint messages interpolate expressions in braces: `fib({n}) called, depth {$global0}`.
Use `{{` and `}}` for literal braces. An expression may be up to 1024 bytes long and nest up
to 100 levels deep.

## How it works

wasm-dap compiles the module with Wasmtime's guest debugging instrumentation (`guest_debug`),
which lets the debugger patch breakpoints into compiled code, single-step, and read every
frame's WebAssembly locals and operand stack. The debuggee runs on its own thread; Wasmtime
calls the debug handler on that thread for breakpoints, steps, epoch interruptions and traps,
and while the program is paused the handler serves inspection requests from the adapter thread.
DWARF is parsed with [gimli](https://github.com/gimli-rs/gimli) to map instructions to source
lines and to locate and decode variables. See [docs/architecture.md](docs/architecture.md).

The workspace consists of:

| Crate | Purpose |
|-------|---------|
| [`wasm-dap-protocol`](crates/wasm-dap-protocol) | DAP messages, request arguments, data types and the stdio framing. |
| [`wasm-dap-core`](crates/wasm-dap-core) | The debugging engine, DWARF support, module analysis, tracer and DAP server. |
| [`wasm-dap-cli`](crates/wasm-dap-cli) | The `wasm-dap` command-line tool. |
| [`editors/vscode`](editors/vscode) | The Visual Studio Code extension. |

## Limitations

- Core WebAssembly modules only; the component model and WASI preview 2 components are not
  supported. WASI programs must target preview 1 (`wasm32-wasip1`).
- One module, one thread: shared-memory threads (`wasi-threads`) are not supported.
- WebAssembly locals and operand stack values are read-only, because Wasmtime's debugging API
  cannot modify them. Source variables stored in linear memory (the usual case in debug builds)
  can be changed.
- Inlined functions are not shown as separate frames. In optimized code some variables are
  reported as optimized out.
- Pause takes effect at the next function call or loop iteration; code blocked inside a host
  call, such as a WASI read, pauses when the call returns.
- Standard input is not interactive; use the `stdin` attribute to provide a file.

## Development

```sh
rustup target add wasm32-unknown-unknown wasm32-wasip1
cargo test --workspace
```

The tests compile real Rust programs to WebAssembly with `rustc` and debug them through the
engine, the in-process DAP server and the `wasm-dap` binary. Set `WASM_DAP_ZIG` to a
[Zig](https://ziglang.org) executable to also build and debug the C and Zig test programs. The VS Code extension has an
integration test that drives a debugging session inside VS Code; see
[editors/vscode/README.md](editors/vscode/README.md). Contributions are welcome; please read
[CONTRIBUTING.md](CONTRIBUTING.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
