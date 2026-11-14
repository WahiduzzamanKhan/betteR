# betterr — terminal R console (Phase 1)

A Rust R console that runs in any terminal and drives a single **ark** R session over Jupyter. Highlighting comes from tree-sitter-r. Completion comes from ark's LSP, the same engine the VS Code editor will use in Phase 2. Nothing is installed into the R library.

## Build, run, test

```sh
export PATH=/opt/homebrew/opt/rustup/bin:$PATH     # Homebrew rustup is keg-only
cd console && cargo build
./target/debug/betterr                              # start a new ark session
./target/debug/betterr -f /path/to/kernel.json      # attach to a running kernel (Phase 2)
./target/debug/betterr --lsp-stdio -f kernel.json   # bridge ark's LSP to stdio (used by the VS Code extension)
echo 'summary(mtcars)' | ./target/debug/betterr     # piped: run and exit (exit 1 on R error)

cargo test                                          # unit tests
../.venv/bin/python tests/pty_smoke.py              # 15 end-to-end checks in a real PTY
```

`betterr` looks for ark in this order: `--ark <path>`, then `$ARK_PATH`, then `ark` on `PATH`, then `../vendor/ark/target/{release,debug}/ark`.

## What works

| Feature | How |
|---|---|
| Syntax highlighting while typing | tree-sitter-r `HIGHLIGHTS_QUERY`. For overlapping matches, the innermost node wins; for the same node, the later pattern wins. Parse errors are not painted. |
| Tab completion from the live session (`y$` columns, session-defined functions, packages, files) | Ark LSP `textDocument/completion` against a virtual `untitled:` document that mirrors the input buffer. Functions insert `name(`. |
| Multi-line input with `+ ` continuation | reedline `Validator` → Jupyter `is_complete_request` (R's own parser decides) |
| `readline()` / `menu()` | Jupyter `input_request` on stdin. Shell and stdin share one ZeroMQ identity so ark routes input requests to us. |
| `browser()` / `debug()` | The prompt switches to `Browse[N]>`. An empty line steps, as in R. |
| Ctrl-C interrupts running code | `interrupt_request` on control. The rest of the input is not run (see the ark findings below). |
| stdout / stderr / errors / warnings | stderr is shown in red; error tracebacks are shown when they have more than one frame |
| History, fish-style hints, Alt-Enter newline, bracketed paste, reverse-i-search | reedline (history in `~/Library/Application Support/betterr/history.txt`, or `$BETTERR_HISTORY`) |
| `options(width)` follows the terminal width | Set at startup and updated on resize, through the UI comm |
| Attach mode (`-f`) | Shares state with the other client in both directions. Detaching leaves the kernel running; a session the console spawned is shut down on exit. |
| Crash handling | Heartbeat plus child-process monitoring. On a crash it reports the error, exits 1 and keeps the ark log. |

## Ark protocol findings from Phase 1

1. **Jupyter `complete_request` is a stub** (as found in Phase 0), so completion goes through the LSP. **Two LSP clients on one kernel work.** Ark keeps one "console inputs" channel, and the newest LSP connection takes it over, but completion queries the live session directly. Tested: both clients see new objects, including after one disconnects. The console and the VS Code extension can therefore each open their own `lsp` comm.
2. **The prompt is only reported through the `positron.ui` comm** (`prompt_state` event). The console opens this comm only to track `Browse[N]>` and to set the console width. It's an ordinary Jupyter comm and needs no Positron UI.
3. **Frontend RPCs block R.** With the UI comm open, R functions that call the frontend (for example `View()` and `rstudioapi` shims) send `rpc_request` on stdin and wait for an answer. The console replies with a JSON-RPC "method not found" error so R never hangs.
4. **Interrupts don't cancel queued expressions.** If you Ctrl-C `Sys.sleep(30); save()`, ark still runs `save()`. The console works around this by splitting input into top-level expressions with tree-sitter and sending them one at a time, stopping on interrupt or error, like R's own console. Input that doesn't parse is sent whole so R reports the syntax error. This deserves an upstream ark fix.
5. **Plots** arrive as `display_data` `image/png` even without Positron's plot comm, which is a good starting point for Phase 4. For now the console prints a placeholder.

## Known limitations / next steps

- No signature or help popups in the console yet (ark offers `signatureHelp` and `hover`).
- The completion menu has no descriptions. They would force a single column; an `IdeMenu` popup could add them back.
- Password `input_request`s use a minimal no-echo reader.
- If the terminal is hung up (tab closed), ark exits with it, but the temp dir (`$TMPDIR/betterr-<pid>`) is left behind.
- Ark log: removed after a clean exit, kept after a crash, and kept whenever `--log` is passed.
