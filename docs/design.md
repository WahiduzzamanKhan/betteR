# betteR console: design notes

How the console is built on ark, and what was learned about ark's protocols along the way. For installation and usage, see the [README](../README.md).

`better` runs its R session by launching itself in kernel mode (`better --kernel`), which runs ark's code built into the same binary. ark is pinned to one upstream commit in `Cargo.toml`; `tree-sitter` and `zmq` are kept at the exact versions ark uses, because both link native libraries and only one copy of each can be linked.

## How each feature works

| Feature | How |
|---|---|
| Syntax highlighting while typing | tree-sitter-r `HIGHLIGHTS_QUERY`. For overlapping matches, the innermost node wins; for the same node, the later pattern wins. Parse errors are not painted. |
| Tab completion from the live session (`y$` columns, session-defined functions, packages, files) | Ark LSP `textDocument/completion` against a virtual `untitled:` document that mirrors the input buffer. Functions insert `name(`. |
| Multi-line input with `+ ` continuation | reedline `Validator` → Jupyter `is_complete_request` (R's own parser decides) |
| `readline()` / `menu()` | Jupyter `input_request` on stdin. Shell and stdin share one ZeroMQ identity so ark routes input requests to us. |
| `browser()` / `debug()` | The prompt switches to `Browse[N]>`. An empty line steps, as in R. |
| Ctrl-C interrupts running code | `interrupt_request` on control. The rest of the input is not run (see the ark findings below). |
| stdout / stderr / errors / warnings | stderr is shown in red; error tracebacks are shown when they have more than one frame |
| History, fish-style hints, Alt-Enter newline, bracketed paste, reverse-i-search | reedline (history in `~/Library/Application Support/better/history.txt`, or `$BETTER_HISTORY`) |
| `options(width)` follows the terminal width | Set at startup and updated on resize, through the UI comm |
| Attach mode (`-f`) | Shares state with the other client in both directions. Detaching leaves the kernel running; a session the console spawned is shut down on exit. |
| Plots | Drawn inline in terminals with an image protocol: iTerm2 and WezTerm (OSC 1337), and kitty. Override detection with `BETTER_INLINE_IMAGES=iterm|kitty|none`; VS Code's terminal works with `iterm` if `terminal.integrated.enableImages` is on. Otherwise a one-line hint is printed. When hosted by the extension, plots go to its *R Plots* pane instead. |
| Interactive output (htmlwidgets such as plotly, leaflet and DT; Shiny apps; `viewer` output) and help pages (`?topic`, `??search`) | Opened in `$BROWSER`, or your default browser. When hosted by the extension, shown in its *R Plots* / *R Viewer* / *R Help* panels instead. |
| Crash handling | Heartbeat plus child-process monitoring. On a crash it reports the error, exits 1 and keeps the ark log. |

## Ark protocol findings from Phase 1

1. **Jupyter `complete_request` is a stub** (as found in Phase 0), so completion goes through the LSP. **Two LSP clients on one kernel work for completion.** Ark keeps one "console inputs" channel, and the newest LSP connection takes it over, but completion queries the live session directly. Tested: both clients see new objects, including after one disconnects. When hosted by the extension (Phase 3), the console still uses the extension's LSP client so the editor gets the console channel; see below.
2. **The prompt is only reported through the `positron.ui` comm** (`prompt_state` event). The console opens this comm only to track `Browse[N]>` and to set the console width. It's an ordinary Jupyter comm and needs no Positron UI.
3. **Frontend RPCs block R.** With the UI comm open, R functions that call the frontend (for example `View()` and `rstudioapi` shims) send `rpc_request` on stdin and wait for an answer. The console replies with a JSON-RPC "method not found" error so R never hangs.
4. **Interrupts don't cancel queued expressions.** If you Ctrl-C `Sys.sleep(30); save()`, ark still runs `save()`. The console works around this by splitting input into top-level expressions with tree-sitter and sending them one at a time, stopping on interrupt or error, like R's own console. Input that doesn't parse is sent whole so R reports the syntax error. This deserves an upstream ark fix.
5. **Plots.** Without the UI comm, ark sends static `display_data` PNGs. With it, which is how the console runs, every plot is a `positron.plot` comm: a `comm_open` with a 640×400 PNG `pre_render`, then `update` events (for example after `lines()`), and `render` requests at any size and format. `dev.off()` doesn't close plot comms, so history survives. Update payloads come as unpadded base64; the console pads them before passing them on.

## Hosted mode and ark debugger findings (Phase 3)

With `--ide-socket`, the console connects to its host IDE over JSON lines on a Unix socket, and:

- **It opens ark's `ark_dap` comm and reports the DAP port.** The comm holder is the frontend ark asks to run debugger commands. A Step clicked in the IDE arrives as a comm `execute {command: "n"}`. A background thread watches IOPub while the line editor waits, and raises reedline's break signal so the command is echoed (`Browse[1]> n`) and run as if typed.
- **It runs IDE `execute` requests** with `positron.code_location`. Each top-level expression gets its own location shifted into the file, using tree-sitter byte positions, and is echoed at its own prompt. Half-typed input survives because reedline keeps its buffer across a break.
- **It forwards ark's `start_debug` / `stop_debug` hints**, which the extension uses to attach or detach VS Code's debugger.
- **It completes through the IDE's language client** instead of its own LSP, so ark keeps a single LSP client. Ark only sends console scopes and `ark:` virtual documents to its newest LSP connection.

Ark findings:
1. **Every step emits `stop_debug` then `start_debug`.** Treating `stop_debug` as "done" and disconnecting makes ark inject `Q`.
2. **Attaching a DAP client while R is already paused doesn't produce a `stopped` event.**
3. **Functions without a file srcref are served as `ark:ark-<pid>/debug/...` virtual documents**, through the LSP request `ark/internal/virtualDocument {path}`.

Watching IOPub from a reedline *idle callback* would make reedline re-query the cursor position on every repaint, and leave raw mode on while it waits for the answer. That's why it's a separate thread that only `try_lock`s the client.

## Known limitations / next steps

- No signature or help popups in the console yet (ark offers `signatureHelp` and `hover`).
- The completion menu has no descriptions. They would force a single column; an `IdeMenu` popup could add them back.
- Password `input_request`s use a minimal no-echo reader.
- If the terminal is hung up (tab closed), ark exits with it, but the temp dir (`$TMPDIR/better-<pid>`) is left behind.
- Ark log: removed after a clean exit, kept after a crash, and kept whenever `--log` is passed.
