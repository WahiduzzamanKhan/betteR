# betteR

**A modern R console for your terminal. One binary, no R packages.**

betteR (the command is `better`) gives R the console experience that RStudio and Positron users take for granted, in any terminal: syntax highlighting as you type, completion from your live session, a real line editor, `browser()` debugging, plots and help. It doesn't install anything into your R library, so your `renv.lock` stays clean.

```text
> y <- mtcars
> y$c⇥
cyl   carb
> summary(y$mpg)
   Min. 1st Qu.  Median    Mean 3rd Qu.    Max.
  10.40   15.43   19.20   20.09   22.80   33.90
```

## Features

- **Syntax highlighting** while you type.
- **Completion as you type, from your live session**: a pop-up appears once a name has three letters, or straight after `$`, `@` or `::`, with data frame columns, functions you've defined, packages and arguments, each with its package (`{base}`) in a second column. Beside the highlighted suggestion is its help, laid out with line breaks and colour: a function's usage, title, description and arguments from its help page, or its signature if it has none. Press Tab to take a suggestion, or for file paths in strings.
- **Function signatures** under the line while you type a call, with the current argument in bold: `mean(x, trim = 0, `**`na.rm = FALSE`**`, ...)`.
- **Multi-line editing.** Enter only runs code once R considers it complete (an open `{` continues on the next line). Shift-Enter inserts a newline. Terminals that send Shift-Enter the same as Enter (they don't support the kitty keyboard protocol), such as macOS Terminal and GNOME Terminal, need Alt-Enter instead; the startup banner says which to use.
- **History** with fish-style suggestions as you type, Ctrl-R reverse search and bracketed paste.
- **`browser()` and `debug()`** with a `Browse[1]>` prompt. Enter steps, as in R.
- **`readline()` and `menu()`** prompts.
- **Ctrl-C** stops running code without losing your session.
- **Plots drawn inline** in terminals that support images (iTerm2, WezTerm, kitty, Windows Terminal).
- **Help pages, htmlwidgets (plotly, leaflet, DT…) and Shiny apps** open in your browser.
- **Scripting:** pipe code in (`echo 'summary(mtcars)' | better`), with exit code 1 on an R error.
- **Nothing installed into R.** No `languageserver`, `httpgd` or other helper packages.

betteR also powers **betteR for VS Code** ([vs_code_betteR](https://github.com/WahiduzzamanKhan/vs_code_betteR)). The extension runs the same console in VS Code's terminal and shares its R session with the editor, debugger, plot pane and help panel.

## How it works

betteR is built on [ark](https://github.com/posit-dev/ark), Posit's open-source R kernel (MIT), which is also the engine behind Positron. ark is compiled into the `better` binary, so the only thing you need besides `better` is R. Design notes are in [docs/design.md](docs/design.md).

## Installation

`better` is a single, standalone binary: to run it, you need nothing but R. There are no prebuilt binaries yet, so for now you build it from source, which takes a few minutes the first time.

### Requirements

- **To run `better`:** R, installed normally. Tested with R 4.3, 4.5 and 4.6, on macOS (Apple silicon), Linux (Ubuntu x86-64 and ARM64, Fedora) and Windows 11 (x86-64 and ARM64). `better` finds R through `$R_HOME` if it's set, otherwise the `R` on your `PATH`.
- **To build it:** Git, Rust 1.95 or newer and a C and C++ compiler. They're only needed for building:

  | OS | What to install |
  |---|---|
  | macOS | [Rust](https://rustup.rs) and the Xcode Command Line Tools (`xcode-select --install`, which includes Git) |
  | Debian, Ubuntu | [Rust](https://rustup.rs), and `sudo apt install build-essential git` |
  | Fedora | [Rust](https://rustup.rs), and `sudo dnf install gcc gcc-c++ git` |
  | Windows | Only Git (`winget install --id Git.Git`). [`install.ps1`](#windows) installs Rust and the Visual Studio C++ build tools if they're missing. |

### macOS and Linux

```sh
git clone https://github.com/WahiduzzamanKhan/betteR.git
cd betteR
cargo install --path .
```

This builds an optimised `better` (about 31 MB) and installs it into `~/.cargo/bin`. Make sure that folder is on your `PATH`. The official rustup installer sets this up; with Homebrew's `rustup`, add it to `~/.zshrc`:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

Check that it works:

```sh
better --version
echo 'R.version.string' | better
```

### Windows

Install [R](https://cran.r-project.org/bin/windows/base/) (on Windows on ARM, R's ARM64 build) and Git, then run this in PowerShell:

```powershell
git clone https://github.com/WahiduzzamanKhan/betteR.git
cd betteR
powershell -ExecutionPolicy Bypass -File install.ps1
```

`install.ps1` asks before it installs anything, then:

- installs the Visual Studio C++ build tools with winget, or adds what's missing to the Visual Studio you have. Windows asks for administrator rights once. CMake and Rtools are not needed.
- installs Rust with rustup, if it isn't installed;
- builds `better` for your R's architecture (x64 or ARM64), and on Windows on ARM sets up the Clang compiler environment the build needs there;
- installs `better.exe` into `%USERPROFILE%\.cargo\bin`, which rustup puts on your `PATH`, and checks that R runs in it;
- offers to set `R_HOME` if R can only be found in the registry, as after a normal R install, because `better` looks for R through `R_HOME` or `PATH`.

Open a new terminal afterwards, so it picks up the new `PATH` and `R_HOME`. `-Yes` answers yes to every question.

**conda's R** needs Windows' standard DLL search order: set `BETTER_STANDARD_DLL_SEARCH=1` (see [Environment variables](#environment-variables)).

### Updating and uninstalling

To update, pull the latest changes and run the install command again: `cargo install --path .` on macOS and Linux, `install.ps1` on Windows. If you use [betteR for VS Code](https://github.com/WahiduzzamanKhan/vs_code_betteR), update it at the same time: the extension and `better` talk to each other, and versions from different times may not understand each other.

To uninstall, run `cargo uninstall better`.

## Usage

Start the console:

```sh
better
```

It starts a new R session in the current directory. Quit with **Ctrl-D** or `q()`.

### Keys

| Key | Action |
|---|---|
| **Tab** | Take the highlighted suggestion, or show suggestions when none are showing |
| **↑** / **↓** (suggestions showing) | Move through the suggestions |
| **Esc** | Close the suggestions |
| **Enter** | Run the input as typed (even with suggestions showing), or start a new line if it isn't complete yet |
| **Shift-Enter** | Insert a newline without running. Use **Alt-Enter** in terminals where Shift-Enter runs the line (they send it the same as Enter); the startup banner names the key that works |
| **→** (at end of line) | Accept the grey suggestion from history |
| **↑** / **↓** | Previous / next history entry |
| **Ctrl-R** | Search history |
| **Ctrl-C** | Interrupt running code, or clear the current line |
| **Ctrl-D** | Quit (on an empty line) |

### Things to try

```r
y <- mtcars              # then type  y$  : the columns pop up; Tab takes one
mean(1:10,               # the signature shows under the line
f <- function(x) {       # Enter continues on the next line
  x * 2
}
g <- function() { z <- 5; browser(); z * 2 }
g()                      # Browse[1]> prompt: type z, press Enter to step, c to continue
name <- readline("Name? ")
Sys.sleep(30)            # then Ctrl-C
plot(1:10)               # drawn inline in iTerm2, WezTerm, kitty or Windows Terminal
?mean                    # opens the help page in your browser
```

### Plots, help and interactive output

- **Plots** are drawn inline in terminals with an image protocol: iTerm2, WezTerm, kitty, and Windows Terminal 1.22 or later (as Sixel). Elsewhere, `better` prints a short note instead. To force a protocol, set `BETTER_INLINE_IMAGES=iterm`, `kitty` or `sixel` (for other Sixel terminals, such as foot or xterm), or `none` to turn images off; use `none` if an older Windows Terminal prints garbage instead of plots. VS Code's integrated terminal can show them with `BETTER_INLINE_IMAGES=iterm` and the setting `terminal.integrated.enableImages` turned on.
- **Help pages** (`?mean`, `help("lm")`, `??regression`), **htmlwidgets** (plotly, leaflet, DT, …) and **Shiny apps** open in your default browser, or in the program named by `$BROWSER`.

### Command-line options

```text
better [OPTIONS]

  -f, --connection-file <PATH>  Attach to a running R session instead of starting one
      --log <PATH>              Keep the R session's log at this path
      --no-lsp                  Turn off completion
      --ark <PATH>              Use a separate ark build instead of the built-in one
  -h, --help                    Show help
  -V, --version                 Show the version
```

`--kernel`, `--lsp-stdio`, `--ide <PORT>` and `--shutdown` are used internally by the VS Code extension. `--shutdown -f <connection file>` ends that kernel's session the way `q()` would: R runs `.Last` and removes its temporary directory. `--ide` connects to it on `127.0.0.1:<PORT>` and needs the extension's token in the `BETTER_IDE_TOKEN` environment variable.

**Running scripts:** if input is piped in, `better` runs it and exits. It stops at the first error and exits with code 1:

```sh
echo 'summary(mtcars)' | better
better < analysis.R
```

**Attaching to a session:** `better -f kernel.json` connects to an R session that is already running, for example one started by the VS Code extension. Both clients see the same variables. Quitting the console leaves that session running.

### Environment variables

| Variable | Effect |
|---|---|
| `BETTER_INLINE_IMAGES` | `iterm`, `kitty`, `sixel` or `none`: overrides inline-image detection |
| `BETTER_HISTORY` | History file (default: `~/Library/Application Support/better/history.txt` on macOS, `~/.local/share/better/history.txt` on Linux, `%APPDATA%\better\history.txt` on Windows) |
| `BROWSER` | Program used to open help pages, htmlwidgets and apps |
| `BETTER_STANDARD_DLL_SEARCH` | Windows only: `1` loads R's libraries with the standard DLL search order (which includes `PATH`), as conda's R needs |
| `R_HOME` | Which R installation to use (default: the `R` on your `PATH`) |
| `BETTER_IDE_TOKEN` | Internal: the token `--ide` proves itself with, set by the VS Code extension |
| `BETTER_SHIFT_ENTER` | Internal: `1` when the host terminal maps Shift-Enter to a newline itself, so the banner offers Shift-Enter; set by the VS Code extension where VS Code's terminal can't report Shift-Enter |

## Troubleshooting

- **"R session ended abnormally"** or **"kernel exited during startup"**: R crashed, or couldn't start. The message gives the reason from the session log when the log has one (for example `Unsupported R version 4.1.2. Ark requires R >= 4.2.0`), and the path of the log, which is kept for diagnosis.
- **Plots show as a note instead of an image**: your terminal doesn't support inline images. Use one that does, or set `BETTER_INLINE_IMAGES`.
- **"Can't find R"**: make sure `R` runs from the same shell. betteR uses `$R_HOME` if set, otherwise the `R` on your `PATH`. Set `R_HOME` to choose a specific R installation. On Windows, R isn't on `PATH` after installing it; see [Windows](#windows).

## Limitations

- If the terminal tab is closed while R is running, R stops correctly but a small folder is left in `$TMPDIR`.

## Development

```sh
cargo build                                        # debug build in target/debug/better
cargo test                                         # unit tests
uv run --with pexpect python tests/pty_smoke.py    # end-to-end tests in a real terminal
```

On Windows, the end-to-end tests run in a ConPTY: `uv run --with pywinpty python tests/pty_smoke.py target/debug/better.exe`.

Commits follow [Conventional Commits](https://www.conventionalcommits.org). For how the console works with ark, see [docs/design.md](docs/design.md).

## License and credits

betteR is released under the [MIT License](LICENSE).


betteR is built on [ark](https://github.com/posit-dev/ark) by Posit Software, PBC (MIT licence), [reedline](https://github.com/nushell/reedline) and [tree-sitter-r](https://github.com/r-lib/tree-sitter-r).
