# Architecture

This document describes how wasm-dap debugs WebAssembly. It is aimed at contributors.

## Overview

```text
 VS Code / DAP client
        │  DAP over stdio or TCP
        ▼
 ┌──────────────────────────────┐      commands       ┌───────────────────────────────┐
 │ server (adapter thread)      │ ──────────────────▶ │ debuggee thread               │
 │  - request handling          │                     │  - Wasmtime Store + instance  │
 │  - BreakpointManager         │ ◀────────────────── │  - DebugHandler               │
 │  - disassembly, sources      │   events (stopped,  │  - pause loop / inspection    │
 └──────────────────────────────┘   output, exited)   └───────────────────────────────┘
        │ shared, immutable                                   │
        ▼                                                     ▼
 ProgramInfo: WasmModule (static analysis), DebugInfo (DWARF), SourceMap
```

## Wasmtime guest debugging

Modules are compiled with `Config::guest_debug(true)`. The instrumentation keeps every frame's
WebAssembly locals and operand stack in known slots, records a frame table for each program
point, and emits patchable breakpoint sites at every instruction. Signals-based traps are
disabled, so every trap goes through a libcall where a debug event can be raised with the stack
intact.

The debuggee thread installs a `DebugHandler`. Wasmtime invokes it for:

| Event | Use |
|-------|-----|
| `Breakpoint` | User breakpoints, single steps and temporary step breakpoints |
| `EpochYield` | Pause requests and live breakpoint updates (`Engine::increment_epoch`) |
| `Trap` | Stopping on traps (unreachable, out of bounds, division by zero, ...) |
| `HostcallError` | Stopping on failing imports; WASI `proc_exit` is recognised and ignored |

While paused, the handler blocks the debuggee thread and serves commands (stack trace, scopes,
variables, evaluation, memory access, resume). Frames are read through `FrameHandle`; globals,
memories and functions that are not exported are reached with the `debug_*` accessors.

Because a debug handler requires async entry points, calls use `call_async` driven by a small
parking executor (`runtime::block_on`). The handler never yields, so the executor is trivial and
nested calls for expression evaluation work from inside the handler.

## Execution control

- **Breakpoints** are resolved by the adapter (`debugger::breakpoints`) to module offsets and
  published as an immutable `BreakpointSet`. The debuggee diffs it against the patched set.
  Updates while running interrupt the debuggee through the epoch counter. Breakpoints on a
  `loop` opcode are moved into the loop body, since back-edges skip the `loop` instruction.
- **Stepping** (`debugger::stepping`) single-steps until execution reaches a statement of a
  different source line. Calls that should not be entered (step over, step out, code without
  line information) run to a temporary breakpoint at the return address instead of being
  single-stepped. Rows from other files at the same frame depth belong to inlined callees and
  are skipped by step over.
- **Pause** sets a flag and increments the epoch. The reported location at an epoch check is
  imprecise, so the debuggee enables single-stepping and stops at the next instruction.
- **Termination** sets a flag that the epoch deadline callback turns into a trap.

## DWARF

`dwarf::DebugInfo` loads the `.debug_*` custom sections with gimli. WebAssembly DWARF addresses
are offsets from the start of the code section payload, and are converted to module offsets
(the address space of Wasmtime's debugging API). The `wat` crate's generated DWARF is relative
to the first function entry instead; `runtime::read_module` records the bias for text input.

- `lines`: merged line table, address-to-line lookup and line-to-address resolution.
- `vars`: variables in scope at a program counter, walking lexical blocks; static variables.
- `types`: lazily parsed type model, including Rust variant parts and template parameters.
- `expr`: an evaluator for the location expression subset compilers emit for WebAssembly,
  since gimli does not evaluate `DW_OP_WASM_location`.

`debugger::values` reads values described by these types from linear memory or from
WebAssembly values and formats them, with special shapes for Rust strings, slices and vectors.

## Server

`server::Server` reads DAP messages on a reader thread and merges them with debuggee events into
one channel, so responses are always written before the events they cause. Requests that need
the paused program are forwarded to the debuggee thread and wait for the reply. The disassembly,
breakpoint locations and source requests are answered from `ProgramInfo` on the adapter thread.

## Tracing

`trace` runs a program with breakpoints at the entry and every exit of each function, reading
arguments from the callee's locals and results from its operand stack. A shadow call stack
infers returns whose exit instruction was not observed. Instruction tracing adds single-stepping.
