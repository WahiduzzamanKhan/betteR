# betteR console: design notes

How the console is built on ark, and what was learned about ark's protocols along the way. For installation and usage, see the [README](../README.md).

`better` runs its R session by launching itself in kernel mode (`better --kernel`), which runs ark's code built into the same binary. ark is pinned to one upstream commit in `Cargo.toml`; `tree-sitter` and `zmq` are kept at the exact versions ark uses, because both link native libraries and only one copy of each can be linked.

## How each feature works

| Feature | How |
|---|---|
| Syntax highlighting while typing | tree-sitter-r `HIGHLIGHTS_QUERY`. For overlapping matches, the innermost node wins; for the same node, the later pattern wins. Parse errors are not painted. |
| Completion as you type, from the live session (`y$` columns, session-defined functions, packages, files) | Ark LSP `textDocument/completion` against a virtual `untitled:` document that mirrors the input buffer. Functions insert `name(`. Shown in reedline's `IdeMenu`, with each item's package as a second column (padded into its display text) and its help in a box `better` draws beside the list; see *Completion as you type* below. |
| Signature of the call around the cursor | Ark LSP `textDocument/signatureHelp`, drawn on reedline's status line below the input, with the active parameter in bold. |
| Multi-line input with `+ ` continuation | reedline `Validator` → Jupyter `is_complete_request` (R's own parser decides) |
| `readline()` / `menu()` | Jupyter `input_request` on stdin. Shell and stdin share one ZeroMQ identity so ark routes input requests to us. |
| `browser()` / `debug()` | The prompt switches to `Browse[N]>`. An empty line steps, as in R. |
| Ctrl-C interrupts running code | `interrupt_request` on control. The rest of the input is not run (see the ark findings below). On Windows, a console Ctrl-C reaches every process attached to the console, and ark doesn't handle it there, so the console starts its kernel in a new process group, which ignores console Ctrl-C. |
| stdout / stderr / errors / warnings | stderr is shown in red; error tracebacks are shown when they have more than one frame |
| History, fish-style hints, Shift-Enter (or Alt-Enter) newline, bracketed paste, reverse-i-search | reedline. History in `history.txt` in the OS's data directory: `~/Library/Application Support/better/` on macOS, `~/.local/share/better/` on Linux, `%APPDATA%\better\` on Windows; or `$BETTER_HISTORY`. Shift-Enter needs the kitty keyboard protocol on Unix: at startup the console asks the terminal (`supports_keyboard_enhancement`, one round trip; up to 2 s only if the terminal answers neither query) and the banner offers Alt-Enter if it doesn't support it. Windows reports Shift-Enter through the console API. A host that maps Shift-Enter itself sets `BETTER_SHIFT_ENTER=1` (the VS Code extension does where its terminal has the protocol off). |
| `options(width)` follows the terminal width | Set at startup and updated on resize, through the UI comm |
| Attach mode (`-f`) | Shares state with the other client in both directions. Detaching leaves the kernel running; a session the console spawned is shut down on exit. |
| Ending a session from outside (`better --shutdown -f kernel.json`) | Sends a Jupyter `shutdown_request` on control and waits up to 5 s for the `shutdown_reply`. Ark interrupts any running code and turns the request into end-of-file at R's prompt, so R quits as at `q()`: it runs `.Last` and finalizers and removes its temp directory. The VS Code extension uses this to close or restart a session, because signals don't end R cleanly: R ignores `SIGTERM` on macOS, and on Windows Node's `kill()` always kills outright. |
| Plots | Drawn inline in terminals with an image protocol: iTerm2 and WezTerm (OSC 1337), kitty, and Windows Terminal 1.22+ (Sixel, detected by `WT_SESSION`; the PNG is decoded and reduced to 256 colours, without dithering). Override detection with `BETTER_INLINE_IMAGES=iterm|kitty|sixel|none`; VS Code's terminal works with `iterm` if `terminal.integrated.enableImages` is on. Otherwise a one-line hint is printed. When hosted by the extension, plots go to its *R Plots* pane instead. |
| Interactive output (htmlwidgets such as plotly, leaflet and DT; Shiny apps; `viewer` output) and help pages (`?topic`, `??search`) | Opened in `$BROWSER`, or your default browser. When hosted by the extension, shown in its *R Plots* / *R Viewer* / *R Help* panels instead. |
| Crash handling | Heartbeat plus child-process monitoring. On a crash it reports the error, with the reason from the ark log (its first panic, else its last `ERROR` line; `src/arklog.rs`), exits 1 and keeps the log. |

## Ark protocol findings from Phase 1

1. **Jupyter `complete_request` is a stub** (as found in Phase 0), so completion goes through the LSP. **Two LSP clients on one kernel work for completion.** Ark keeps one "console inputs" channel, and the newest LSP connection takes it over, but completion queries the live session directly. Tested: both clients see new objects, including after one disconnects. When hosted by the extension (Phase 3), the console still uses the extension's LSP client so the editor gets the console channel; see below.
2. **The prompt is only reported through the `positron.ui` comm** (`prompt_state` event). The console opens this comm only to track `Browse[N]>` and to set the console width. It's an ordinary Jupyter comm and needs no Positron UI.
3. **Frontend RPCs block R.** With the UI comm open, R functions that call the frontend (for example `View()` and `rstudioapi` shims) send `rpc_request` on stdin and wait for an answer. The console replies with a JSON-RPC "method not found" error so R never hangs.
4. **Interrupts don't cancel queued expressions.** If you Ctrl-C `Sys.sleep(30); save()`, ark still runs `save()`. The console works around this by splitting input into top-level expressions with tree-sitter and sending them one at a time, stopping on interrupt or error, like R's own console. Input that doesn't parse is sent whole so R reports the syntax error. This deserves an upstream ark fix.
5. **Plots.** Without the UI comm, ark sends static `display_data` PNGs. With it, which is how the console runs, every plot is a `positron.plot` comm: a `comm_open` with a 640×400 PNG `pre_render`, then `update` events (for example after `lines()`), and `render` requests at any size and format. `dev.off()` doesn't close plot comms, so history survives. Update payloads come as unpadded base64; the console pads them before passing them on.

## Hosted mode and ark debugger findings (Phase 3)

With `--ide <port>`, the console connects to its host IDE over JSON lines on loopback TCP (`127.0.0.1:<port>`, with `TCP_NODELAY`), and:

- **It proves itself first.** Any local user can connect to a loopback port, so the IDE gives the console a random token in the `BETTER_IDE_TOKEN` environment variable. It isn't on the command line, where other users could read it. The console's first line is `{"type":"hello","token":"…","protocol":N}`, and the IDE closes any connection that doesn't start with a matching token. TCP rather than a Unix socket because it works the same on Windows, where Node can't listen on a file path.
- **It says which protocol it speaks.** The console and the extension are installed separately, so they can come from different releases. `protocol` is `ide::PROTOCOL`, the version of the channel's messages; bump it whenever a message, or what it means, changes in a way the other side would get wrong, together with the extension's `PROTOCOL` and `MIN_CONSOLE_VERSION`. An IDE speaking another protocol answers `{"type":"incompatible","message":"…"}` and closes the connection. The console prints the message in red and keeps working on its own; IDE requests (completion, help) then fail at once instead of waiting for their timeout. `incompatible` is the one message that never changes. Versions follow semantic versioning; while at 0.x, a minor bump marks a breaking change.

- **It opens ark's `ark_dap` comm and reports the DAP port.** The comm holder is the frontend ark asks to run debugger commands. A Step clicked in the IDE arrives as a comm `execute {command: "n"}`. A background thread watches IOPub while the line editor waits, and raises reedline's break signal so the command is echoed (`Browse[1]> n`) and run as if typed.
- **It runs IDE `execute` requests** with `positron.code_location`. Each top-level expression gets its own location shifted into the file, using tree-sitter byte positions, and is echoed at its own prompt. Half-typed input survives because reedline keeps its buffer across a break.
- **It forwards ark's `start_debug` / `stop_debug` hints**, which the extension uses to attach or detach VS Code's debugger.
- **It completes through the IDE's language client** instead of its own LSP, so ark keeps a single LSP client. Ark only sends console scopes and `ark:` virtual documents to its newest LSP connection.
- **It feeds the IDE's environment explorer.** It opens ark's `positron.variables` comm and relays its `refresh` / `update` events as `variables_event`. It forwards the UI comm's `prompt_state`, which arrives after every execution. It passes the IDE's `comm_rpc` requests (variables `inspect`, `delete`, `clear`, `list`, and UI `evaluate_code`) to the matching comm and returns each reply as `comm_rpc_reply`. Like plot requests, these are sent from the background IOPub thread, so typing at the prompt isn't interrupted.
- **It relays data explorers.** When ark opens a `positron.dataExplorer` comm (`View(df)`, or the IDE's variables `view` request), the console forwards it as `comm_open`, its events as `comm_event` and its closing as `comm_closed`. It routes the IDE's `comm_rpc` requests to it by `comm_id`, and closes it on the IDE's `comm_close`. Standalone, the console prints a one-line note that the data viewer needs the VS Code extension, and otherwise ignores these comms.
- **It replays the session's startup output.** Ark sources `.Rprofile` (e.g. renv's activation) after it reports ready but before any command, and sends the output on IOPub. A late subscriber never sees it, and the extension opens its console once the session is ready. So `better --kernel` subscribes to its own IOPub before R starts and records stream output, up to the first `execute_input`, in `kernel.startup.jsonl` next to the connection file (owner-only). A console that attaches (`-f`) prints it after its banner. A console that spawns its own kernel passes `--no-startup-record` and sees the output live, which is guaranteed rather than a matter of timing: ark starts R only once its first IOPub subscriber is connected, and without a recorder that subscriber is the console. (With a recorder, the recorder was first and R could run `.Rprofile` before the console had subscribed, which lost the output on slow CI machines.) The console also shows IOPub output that arrives while it's still waiting for the kernel's `kernel_info` reply; `.Rprofile` output has no parent request and used to be dropped there. An attached console still drops it, because it replays the recorded copy instead.

Ark findings:
1. **Every step emits `stop_debug` then `start_debug`.** Treating `stop_debug` as "done" and disconnecting makes ark inject `Q`.
2. **Attaching a DAP client while R is already paused doesn't produce a `stopped` event.**
3. **Functions without a file srcref are served as `ark:ark-<pid>/debug/...` virtual documents**, through the LSP request `ark/internal/virtualDocument {path}`.

Watching IOPub from a reedline *idle callback* would make reedline re-query the cursor position on every repaint, and leave raw mode on while it waits for the answer. That's why it's a separate thread that only `try_lock`s the client.

## Completion as you type

Reedline has no option for completing as you type, so the console assembles it:

- **Trigger.** An edit-mode wrapper (`AutoCompleteMode`) follows every typed name character, `$`, `@` and `:` with a `Menu` event. The completer decides whether there's anything worth showing: a name of three or more characters, or right after `$`, `@` or `::`, and never inside a string or comment. Tab asks explicitly, so shorter prefixes and file paths in strings work. That holds until the menu Tab opened closes. The menu counts its openings so the completer can tell one from the next.
- **Keys.** Tab is `UntilFound([MenuAccept, Menu])`: take the highlighted suggestion, or open the menu. Enter is `Multiple([Esc, Enter])`, so it always runs the line as typed. The menu closes when a typed character can't extend the name (`with_word_chars("._")`).
- **The menu** (`QuietMenu` around `IdeMenu`) closes instead of showing "NO RECORDS FOUND". Reedline's quick completions are on, so typing refreshes an open menu at once and a Tab arriving in the same burst of input takes a current suggestion. Quick completions would also insert a menu's only suggestion as it opens (`mea` would become `mean(`), so `QuietMenu` refuses that check once per opening and lets the menu load its values when drawn.
- **Help beside each suggestion.** Ark sends a suggestion's help only when asked about it (`completionItem/resolve`). Reedline's `IdeMenu` doesn't say which suggestion is selected, so `CompletionMenu` mirrors the selection (one move per draw, wrapping at the ends). When the selected suggestion has no help yet, it fetches it and keeps it by item. The box is drawn by `better`, not reedline: `IdeMenu`'s description box re-wraps text on whitespace and counts escape codes as text, so it could only show one plain paragraph. Instead, `IdeMenu` draws the list alone and `CompletionMenu` joins its own box to the list's rows (`help_box.rs`). The box is placed right of the list, or in the room left of it, up to 76 × 16. The help page is turned from ark's Markdown into styled paragraphs:
  - the function's usage lines (only those for that function, since a page can document several), in colour;
  - the title, in bold;
  - the Description section;
  - the arguments, names in bold.

  It's word-wrapped by display width and ends in "…" when it doesn't fit. A function without a help page (one defined in the session, or an S3 method listed under its generic) gets its signature from signature help instead. Until help arrives, or for things without any, the box says what the suggestion is (`function`, `column`). Fetching blocks drawing: about 0.1 s per item standalone, longer the first time.
- **Speed.** Ark's answer for a word is cached and narrowed locally as the word grows, unless ark marks it incomplete. So typing a name asks once, not once per letter. Completion as you type gives up after 800 ms rather than stall typing.
- **Signatures.** Ark only finds a call that's closed, as it always is in an editor that pairs brackets. So the status line asks about the input with its open brackets (and any open string) closed at the end. It asks again only when the call or the argument changes, and gives up after 500 ms. Hosted, completion, help and signature requests go through the extension's LSP client (`complete`, `resolve` and `signature` on the IDE channel).

## Known limitations / next steps

- While the suggestions are open, every keystroke costs a cursor-position query to the terminal (reedline re-anchors the prompt around any host completer, see reedline #1130). Instant locally; one round trip each over a slow remote link.
- Password `input_request`s use a minimal no-echo reader.
- If the terminal is hung up (tab closed), ark exits with it, but the temp dir (`$TMPDIR/better-<pid>`) is left behind.
- Ark log: removed after a clean exit, kept after a crash, and kept whenever `--log` is passed.
