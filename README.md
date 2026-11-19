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
- **Multi-line editing.** Enter only runs code once R considers it complete (an open `{` continues on the next line). Alt-Enter inserts a newline.
- **History** with fish-style suggestions as you type, Ctrl-R reverse search and bracketed paste.
- **`browser()` and `debug()`** with a `Browse[1]>` prompt. Enter steps, as in R.
- **`readline()` and `menu()`** prompts.
- **Ctrl-C** stops running code without losing your session.
- **Plots drawn inline** in terminals that support images (iTerm2, WezTerm, kitty).
- **Help pages, htmlwidgets (plotly, leaflet, DT…) and Shiny apps** open in your browser.
- **Scripting:** pipe code in (`echo 'summary(mtcars)' | better`), with exit code 1 on an R error.
- **Nothing installed into R.** No `languageserver`, `httpgd` or other helper packages.

betteR also powers **betteR for VS Code** ([vs_code_betteR](https://github.com/WahiduzzamanKhan/vs_code_betteR)). The extension runs the same console in VS Code's terminal and shares its R session with the editor, debugger, plot pane and help panel.

## How it works

betteR is built on [ark](https://github.com/posit-dev/ark), Posit's open-source R kernel (MIT), which is also the engine behind Positron. ark is compiled into the `better` binary, so the only thing you need besides `better` is R. Design notes are in [docs/design.md](docs/design.md).

## Requirements

- **R**, installed normally. Tested with R 4.3 and 4.6.
- **macOS or Linux.** Windows is not supported yet. betteR has been tested on macOS.
- **To build from source:** Rust 1.95 or newer (from [rustup](https://rustup.rs)), Git, and a C compiler (on macOS, the Xcode Command Line Tools: `xcode-select --install`).

## Installation

There are no prebuilt binaries yet, so install from source:

```sh
git clone https://github.com/WahiduzzamanKhan/betteR.git
cd betteR
cargo install --path .
```

This builds an optimised `better` (about 31 MB) and installs it into `~/.cargo/bin`. A first build downloads and compiles all dependencies, including ark, and takes a few minutes.

Make sure `~/.cargo/bin` is on your `PATH`. The official rustup installer sets this up; with Homebrew's `rustup`, add it to `~/.zshrc`:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

Check that it works:

```sh
better --version
echo 'R.version.string' | better
```

To update later, pull the latest changes and run `cargo install --path .` again. To uninstall, run `cargo uninstall better`.

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
| **Alt-Enter** | Insert a newline without running |
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
plot(1:10)               # drawn inline in iTerm2, WezTerm or kitty
?mean                    # opens the help page in your browser
```

### Plots, help and interactive output

- **Plots** are drawn inline in terminals with an image protocol: iTerm2, WezTerm and kitty. Elsewhere, `better` prints a short note instead. To force a protocol, set `BETTER_INLINE_IMAGES=iterm` or `kitty`, or `none` to turn images off. VS Code's integrated terminal can show them with `BETTER_INLINE_IMAGES=iterm` and the setting `terminal.integrated.enableImages` turned on.
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
| `BETTER_INLINE_IMAGES` | `iterm`, `kitty` or `none`: overrides inline-image detection |
| `BETTER_HISTORY` | History file (default: `~/Library/Application Support/better/history.txt` on macOS, `~/.local/share/better/history.txt` on Linux) |
| `BROWSER` | Program used to open help pages, htmlwidgets and apps |
| `BETTER_STANDARD_DLL_SEARCH` | Windows only: `1` loads R's libraries with the standard DLL search order (which includes `PATH`), as conda's R needs |
| `R_HOME` | Which R installation to use (default: the `R` on your `PATH`) |

## Troubleshooting

- **"R session ended abnormally"**: R crashed. The message gives the path of the session log, which is kept for diagnosis.
- **Plots show as a note instead of an image**: your terminal doesn't support inline images. Use one that does, or set `BETTER_INLINE_IMAGES`.
- **"Can't find R"**: make sure `R` runs from the same shell. betteR uses `$R_HOME` if set, otherwise the `R` on your `PATH`. Set `R_HOME` to choose a specific R installation.

## Limitations

- If the terminal tab is closed while R is running, R stops correctly but a small folder is left in `$TMPDIR`.

## Development

```sh
cargo build                                        # debug build in target/debug/better
cargo test                                         # unit tests
uv run --with pexpect python tests/pty_smoke.py    # end-to-end tests in a real terminal
```

Commits follow [Conventional Commits](https://www.conventionalcommits.org). For how the console works with ark, see [docs/design.md](docs/design.md).

## License and credits

betteR is released under the [MIT License](LICENSE).


betteR is built on [ark](https://github.com/posit-dev/ark) by Posit Software, PBC (MIT licence), [reedline](https://github.com/nushell/reedline) and [tree-sitter-r](https://github.com/r-lib/tree-sitter-r).
