# Crash Recovery

## What counts as a crash

NCL distinguishes two kinds of fault:

**Lisp errors** — conditions signalled by `ERROR`, wrong-arity calls, unbound variables, type errors. These are caught by the REPL's top-level `handler-case` and printed as error messages. The session continues normally.

**Hard crashes** — Windows Structured Exception Handling (SEH) faults (access violations, stack overflows, illegal instructions) or Rust panics that propagate past all handlers in the Lisp worker thread. These produce a crash dump.

## The crash dump

When a hard crash occurs on the Lisp worker thread, NCL's last-resort exception filter fires. It writes a structured dump to `stderr` containing:

- The exception code and faulting address
- A register snapshot: `RAX`, `RCX`, `RDX`, `RBX`, `RSP`, `RBP`, `RSI`, `RDI`, `R8`–`R15`, `RIP`
- A stack walk showing return addresses, resolved to Lisp function names where possible (JIT symbols are registered at compile time)

Example dump fragment:

```text
[NCL crash] exception 0xC0000005 (access violation) at 0x00007FF6A3B21C40
  RAX=0000000000000000  RCX=00007FF6A3B21C40  RDX=0000000000000008
  RBX=00007FF799A04080  RSP=000000A4F5CFDC30  RBP=000000A4F5CFDE00
  RIP=00007FF6A3B21C40
stack:
  0x00007FF6A3B21C40  [FACT]
  0x00007FF6A3B20FF0  [FACT]
  0x00007FF6A3A10048  [ncl-repl-loop]
```

The dump is written directly to the OS stderr handle using `WriteFile` — no Rust runtime, no heap allocation. It is safe to read even when the heap is in a bad state.

## Where to find the dump

The dump goes to `stderr`. When running from a terminal, it appears inline. When running under the GUI (`ncl --windows`), stderr is typically redirected to `ncl-stderr.txt` in the working directory. Check that file after an unexpected exit.

## The UI thread stays alive

The crash filter only intercepts faults on the **Lisp worker thread**. The UI thread (the Win32 message pump) is unaffected. After a worker crash, the iGui frame remains open and responsive:

- ledit stays editable
- The log view stays readable
- You can read the dump from `ncl-stderr.txt`

To recover, close the frame and restart `ncl --windows`. Any Lisp definitions from the crashed session are lost — source files are the only persistence.

## Common causes

| Symptom | Likely cause |
|---------|-------------|
| Access violation in JIT'd code | Return from `DEFASM` with wrong tag; bad pointer from FFI |
| Stack overflow | Deep recursion without tail-call optimization in a non-tail position |
| Rust panic | Internal assertion failure; usually a compiler bug — file an issue |
| Illegal instruction | `DEFASM` body with an unsupported opcode or wrong operand encoding |

## Debugging a crash

1. Run with `ncl --windows 2>crash.txt` to capture stderr to a file.
2. Reproduce the crash.
3. Open `crash.txt` and look for the `[NCL crash]` block.
4. The `RIP` value is the instruction that faulted.
5. JIT symbols in the stack trace name the Lisp function that was running.
6. Use `(disassemble 'function-name)` in a fresh session to inspect the generated code.

## The log view as a pre-crash record

The log view (`Ctrl+Shift+L`) is backed by a process-wide ring buffer in the Rust runtime. It survives a worker-thread crash. If you were writing diagnostic output with `LOG-FORMAT` before the crash, the last few thousand lines remain readable in the log view after the worker goes down.

## See also

- [Editor (ledit)](editor.md) — stays alive after a worker crash
- [Windows surface](windows-surface.md) — the iGui frame and UI thread model
- [Getting started](getting-started.md) — restarting NCL
