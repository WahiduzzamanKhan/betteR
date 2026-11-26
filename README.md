# betteR

**A modern R console for your terminal. One binary, no R packages.**

betteR (the command is `better`) gives R the console experience that RStudio and Positron users take for granted, in any terminal: syntax highlighting as you type, completion from your live session, a real line editor, `browser()` debugging, plots and help. It doesn't install anything into your R library, so your `renv.lock` stays clean.

![betteR in a terminal: completion of data frame columns, function signatures while typing, multi-line editing, stepping through browser(), and a plot drawn inline](docs/media/betteR_demo.gif)

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
- **Completion as you type, from your live session**: a pop-up appears as you type a name, or straight after `$`, `@` or `::`, with data frame columns, functions you've defined, packages and arguments, each with its package (`{base}`) in a second column. Beside the highlighted suggestion is its help, laid out with line breaks and colour: a function's usage, title, description and arguments from its help page, or its signature if it has none. Press Tab to take a suggestion, or for file paths in strings. When and how it pops up, and which key takes a suggestion, are [settings](#settings).
- **Function signatures** under the line while you type a call, with the current argument in bold: `mean(x, trim = 0, `**`na.rm = FALSE`**`, ...)`.
- **Multi-line editing.** Enter only runs code once R considers it complete (an open `{` continues on the next line). Shift-Enter inserts a newline. Terminals that send Shift-Enter the same as Enter (they don't support the kitty keyboard protocol), such as macOS Terminal and GNOME Terminal, use Alt-Enter (Option-Enter on a Mac) instead; the startup banner says which, and [`editing.newline`](#settings) picks another.
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

`better` is a single, standalone binary: to run it, you need nothing but R. Prebuilt binaries for macOS, Linux and Windows are on [GitHub Releases](https://github.com/WahiduzzamanKhan/betteR/releases), and the install scripts below download the right one for your computer.

### Requirements

- **R 4.2 or newer**, installed normally. `better` finds R through `$R_HOME` if it's set, otherwise the `R` on your `PATH`. Ubuntu 22.04's own R is too old (4.1): install R from [CRAN's Ubuntu repository](https://cran.r-project.org/bin/linux/ubuntu/) instead.
- **One of these platforms:**

  | OS | CPU | Needs |
  |---|---|---|
  | macOS | Apple silicon | macOS 11 or newer |
  | macOS | Intel | macOS 10.12 or newer |
  | Linux | x86-64, ARM64 | glibc 2.28 or newer: RHEL, Rocky and Alma 8 or newer, Debian 11 or newer, Ubuntu 20.04 or newer, Fedora, openSUSE, Arch |
  | Windows | x86-64, ARM64 | Windows 10 or 11 (tested on 11). On Windows on ARM, use R's ARM64 build. |

  Tested with R 4.2, 4.3, 4.5 and 4.6 on macOS, Ubuntu, Debian, Fedora, Rocky Linux, openSUSE, Arch and Windows 11.

### macOS and Linux

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/WahiduzzamanKhan/betteR/releases/latest/download/better-installer.sh | sh
```

### Windows

In PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/WahiduzzamanKhan/betteR/releases/latest/download/better-installer.ps1 | iex"
```

R's Windows installer puts R neither on your `PATH` nor in `R_HOME`, so `better` can't find it yet. Set `R_HOME` to your R folder once, for example:

```powershell
[Environment]::SetEnvironmentVariable('R_HOME', 'C:\Program Files\R\R-4.6.2', 'User')
```

**conda's R** also needs Windows' standard DLL search order: set `BETTER_STANDARD_DLL_SEARCH=1` (see [Environment variables](#environment-variables)).

### Check that it works

Both scripts install `better` into `~/.cargo/bin` (`%USERPROFILE%\.cargo\bin` on Windows) and add that folder to your `PATH`. Open a new terminal, so it picks up the new `PATH`, then run:

```sh
better --version
echo 'R.version.string' | better
```

### Unsigned binaries

The binaries aren't signed yet: there's no Apple notarization and no Windows code signing.

- **macOS:** the install script isn't affected. If you download a `better` archive with a browser instead, Gatekeeper blocks it. Remove the quarantine flag with `xattr -d com.apple.quarantine better`.
- **Windows:** SmartScreen or your antivirus may warn about the download or the first run.

Each release lists the SHA-256 checksum of every archive (`sha256.sum`) if you want to check what you downloaded.

### Updating and uninstalling

To update, run the install command again.

If you use [betteR for VS Code](https://github.com/WahiduzzamanKhan/vs_code_betteR), keep the extension and `better` on the same minor version: extension 0.2.x works with console 0.2.x. While betteR is at 0.x, a new minor version (0.2 → 0.3) can change how the two talk to each other, and patch versions (0.2.0 → 0.2.1) don't. If they don't fit, the extension says which one to update.

To uninstall, delete `better` from `~/.cargo/bin` (`better.exe` from `%USERPROFILE%\.cargo\bin` on Windows), or run `cargo uninstall better` if you built it with `cargo install`.

## Build from source

Building takes a few minutes the first time and gives the same binary as the releases.

### Requirements for building

Git, Rust 1.95 or newer and a C and C++ compiler:

| OS | What to install |
|---|---|
| macOS | [Rust](https://rustup.rs) and the Xcode Command Line Tools (`xcode-select --install`, which includes Git) |
| Debian, Ubuntu | [Rust](https://rustup.rs), and `sudo apt install build-essential git` |
| Fedora | [Rust](https://rustup.rs), and `sudo dnf install gcc gcc-c++ git` |
| Windows | Only Git (`winget install --id Git.Git`). [`install.ps1`](#building-on-windows) installs Rust and the Visual Studio C++ build tools if they're missing. |

### Building on macOS and Linux

```sh
git clone https://github.com/WahiduzzamanKhan/betteR.git
cd betteR
cargo install --path .
```

This builds an optimised `better` (about 31 MB) and installs it into `~/.cargo/bin`. Make sure that folder is on your `PATH`. The official rustup installer sets this up; with Homebrew's `rustup`, add it to `~/.zshrc`:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

To update, pull the latest changes and run `cargo install --path .` again.

### Building on Windows

`install.ps1` in this repository builds `better` from source. It's a different script from `better-installer.ps1` on the releases page, which downloads a prebuilt binary.

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

Open a new terminal afterwards, so it picks up the new `PATH` and `R_HOME`. `-Yes` answers yes to every question. To update, pull the latest changes and run `install.ps1` again.

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
| **Shift-Enter** | Insert a newline without running. In terminals that send Shift-Enter the same as Enter, it's **Alt-Enter** (**Option-Enter** on a Mac) instead. The startup banner names the key, and [`editing.newline`](#settings) changes it; the other keys run the line like Enter |
| **→** (at end of line) | Accept the grey suggestion from history |
| **↑** / **↓** | Previous / next history entry |
| **Ctrl-R** | Search history |
| **Ctrl-C** | Interrupt running code, or clear the current line |
| **Ctrl-D** | Quit (on an empty line) |

With [`completion.accept = "enter"`](#settings), Enter takes the highlighted suggestion while suggestions are showing, and Tab moves to the next one.

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

- **Plots** are drawn inline in terminals with an image protocol: iTerm2, WezTerm, kitty, and Windows Terminal 1.22 or later (as Sixel). Elsewhere, `better` prints a short note instead. To choose the protocol yourself, or turn images off, use the setting [`startup.inline_plots`](#settings). VS Code's integrated terminal can show them with `startup.inline_plots = "iterm"` and the setting `terminal.integrated.enableImages` turned on.
- **Help pages** (`?mean`, `help("lm")`, `??regression`), **htmlwidgets** (plotly, leaflet, DT, …) and **Shiny apps** open in your default browser, or in the program named by `$BROWSER`.

### Settings

`better` reads its settings from `config.toml`:

- macOS and Linux: `~/.config/better/config.toml` (or `$XDG_CONFIG_HOME/better/config.toml`);
- Windows: `%APPDATA%\better\config.toml`;
- or the file given with `--config <path>` or the `BETTER_CONFIG` environment variable.

`better` writes this file, with every setting commented out, the first time it starts and finds none. It never changes an existing file, so new settings in later releases are listed here and in `better --print-config`, not added to your file. Uncomment a line to change a setting:

```toml
[completion]
min_chars = 2
accept = "enter"
```

Settings are read when the console starts, so restart it after a change. A setting with a mistake in it (an unknown name, the wrong type, a value out of range) is reported in one dim line at startup, naming the setting and the file, and keeps its default; the console still starts.

| Setting | Values | Default | What it does |
|---|---|---|---|
| `startup.r_home` | path | `""` | The R installation to use. Empty: `$R_HOME`, else `R RHOME` (the `R` on your `PATH`). |
| `startup.ark` | path | `""` | A separate ark binary (like `--ark`). Empty: the built-in kernel. Not used in VS Code, where the extension's `betteR.arkPath` decides. |
| `startup.banner` | `true`, `false` | `true` | The R version, ark and key-hint lines at startup. |
| `startup.inline_plots` | `"auto"`, `"iterm"`, `"kitty"`, `"sixel"`, `"none"` | `"auto"` | How plots are drawn in the terminal. `auto` detects the terminal; `iterm` also works for VS Code's terminal with `terminal.integrated.enableImages` on, `sixel` for other Sixel terminals (foot, xterm), and `none` turns images off (use it if an older Windows Terminal prints garbage instead of plots). |
| `completion.lsp` | `true`, `false` | `true` | Completion from ark's language server (like `--no-lsp` when `false`). |
| `completion.popup` | `"typing"`, `"tab"` | `"typing"` | Show suggestions as you type, or only when Tab is pressed. |
| `completion.min_chars` | 0–10 | `3` | With `popup = "typing"`: how many characters of a name must be typed before suggestions show. Right after `$`, `@` or `::` they always show. |
| `completion.delay_ms` | 0–2000 | `0` | With `popup = "typing"`: how long typing must pause, in milliseconds, before suggestions show. Each key restarts the wait; once the suggestions show, they follow your typing at once. Tab doesn't wait. |
| `completion.accept` | `"tab"`, `"enter"` | `"tab"` | The key that takes the highlighted suggestion. With `enter`, Enter takes it while suggestions are showing and runs the line when they aren't; Tab opens the suggestions and moves to the next one. |
| `completion.max_suggestions` | 1–50 | `5` | The most suggestions shown at once (the border isn't counted). With more matches, the list scrolls. |
| `editing.newline` | `"auto"`, `"shift_enter"`, `"alt_enter"`, `"opt_enter"` | `"auto"` | The one key that inserts a newline; the others run the line, like Enter. `auto`: Shift-Enter where the terminal can tell it from Enter (the kitty keyboard protocol, Windows, VS Code), else Alt-Enter (Option-Enter on a Mac, which needs "Option as Meta" as below). `opt_enter`: Option-Enter on a Mac, which a terminal sends as Alt-Enter only with "Use Option as Meta key" turned on (Terminal: Settings → Profiles → Keyboard; iTerm2: Profiles → Keys, Left Option key "Esc+"); VS Code needs nothing. On Windows and Linux it's the same as `alt_enter`. `shift_enter` in a terminal that sends Shift-Enter as Enter prints a warning at startup, since Shift-Enter then runs the line. The banner names the key. |
| `history.size` | 0 or more | `10000` | Lines kept in the history file. `0` keeps no history (and leaves the file as it is). |
| `history.suggestions` | `true`, `false` | `true` | Fish-style suggestions: the rest of a matching history line in grey after the cursor; → takes it. |

**Precedence**, lowest to highest: the defaults, `config.toml`, environment variables (`R_HOME` sets `startup.r_home`), then command-line flags: `--set key=value` (repeatable, for example `better --set completion.accept=enter`), `--no-lsp` and `--ark`. In VS Code, the extension's `betteR.console.*` settings are passed as `--set` flags, and only the ones you've set, so `config.toml` serves both the terminal and VS Code.

- `better --print-config` shows the settings in use and where each value came from (`default`, `file`, `env` or `flag`).
- `better --reset-config` replaces the file with the commented-out template, so every setting goes back to its default. It saves the old file as `config.toml.bak` first (replacing an earlier backup) and asks before it does anything, unless you add `--yes`.

### Command-line options

```text
better [OPTIONS]

  -f, --connection-file <PATH>  Attach to a running R session instead of starting one
      --log <PATH>              Keep the R session's log at this path
      --no-lsp                  Turn off completion
      --set <KEY=VALUE>         Change a setting for this run, e.g. --set completion.accept=enter (repeatable)
      --config <PATH>           Read settings from this file instead
      --print-config            Show the settings in use and where each comes from
      --reset-config            Reset the config file to the defaults (keeps a .bak; --yes: don't ask)
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
| `BETTER_HISTORY` | History file (default: `~/Library/Application Support/better/history.txt` on macOS, `~/.local/share/better/history.txt` on Linux, `%APPDATA%\better\history.txt` on Windows) |
| `BROWSER` | Program used to open help pages, htmlwidgets and apps |
| `BETTER_STANDARD_DLL_SEARCH` | Windows only: `1` loads R's libraries with the standard DLL search order (which includes `PATH`), as conda's R needs |
| `R_HOME` | Which R installation to use (default: the `R` on your `PATH`). Overrides `startup.r_home` in `config.toml` |
| `BETTER_CONFIG` | The config file to read (see [Settings](#settings)) |
| `BETTER_IDE_TOKEN` | Internal: the token `--ide` proves itself with, set by the VS Code extension |

**Deprecated.** These still work, at the level of environment variables, but print a line at startup naming the setting that replaces them. A later release removes them; the changelog will say when.

| Variable | Use instead |
|---|---|
| `BETTER_INLINE_IMAGES` (`iterm`, `kitty`, `sixel`, `none`) | `startup.inline_plots` |
| `BETTER_SHIFT_ENTER` (`1`, `0`) | `editing.newline` (`1` is `"shift_enter"`, `0` is `"alt_enter"`) |

## Troubleshooting

- **"R session ended abnormally"** or **"kernel exited during startup"**: R crashed, or couldn't start. The message gives the reason from the session log when the log has one (for example `Unsupported R version 4.1.2. Ark requires R >= 4.2.0`), and the path of the log, which is kept for diagnosis.
- **Plots show as a note instead of an image**: your terminal doesn't support inline images. Use one that does, or set `startup.inline_plots`.
- **"Can't find R"**: make sure `R` runs from the same shell. betteR uses `$R_HOME` if set, otherwise the `R` on your `PATH`. Set `R_HOME`, or the setting `startup.r_home`, to choose a specific R installation. On Windows, R isn't on `PATH` or in `R_HOME` after installing it; see [Windows](#windows).

## Limitations

- If the terminal tab is closed while R is running, R stops correctly but a small folder is left in `$TMPDIR`.

## Development

```sh
cargo build                                        # debug build in target/debug/better
cargo test                                         # unit tests
uv run --with pexpect python tests/pty_smoke.py    # end-to-end tests in a real terminal
```

On Windows, the end-to-end tests run in a ConPTY: `uv run --with pywinpty python tests/pty_smoke.py target/debug/better.exe`.

Commits follow [Conventional Commits](https://www.conventionalcommits.org). For branches and releases, see [CONTRIBUTING.md](CONTRIBUTING.md). For how the console works with ark, see [docs/design.md](docs/design.md).

## License and credits

betteR is released under the [MIT License](LICENSE).


betteR is built on [ark](https://github.com/posit-dev/ark) by Posit Software, PBC (MIT licence), [reedline](https://github.com/nushell/reedline) and [tree-sitter-r](https://github.com/r-lib/tree-sitter-r).
