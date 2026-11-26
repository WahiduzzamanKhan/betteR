"""Phase 1 interactive smoke test: drives the real `better` binary in a PTY.

Usage: uv run --with pexpect python tests/pty_smoke.py [path/to/better]
On Windows (a ConPTY, as Windows Terminal uses):
       uv run --with pywinpty python tests/pty_smoke.py [path\\to\\better.exe]
"""

import json
import os
import queue
import re
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

WINDOWS = sys.platform == "win32"
if WINDOWS:
    from winpty import PtyProcess
else:
    import pexpect

ROOT = Path(__file__).resolve().parents[1]
BIN = sys.argv[1] if len(sys.argv) > 1 else str(ROOT / ("target/debug/better.exe" if WINDOWS else "target/debug/better"))
# Failure details include what the console printed; the Windows CI console is cp1252.
sys.stdout.reconfigure(encoding="utf-8", errors="backslashreplace")
ANSI = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b[()][A-Z0-9]|\r")

# Reedline enables bracketed paste each time it starts reading a line.
READY = r"\x1b\[\?2004h"
TOP = r"\x1b\[34m> "
BROWSE = (READY, r"Browse\[1\]> ")

results = {}

# No run may read or write the user's own config file: each console gets a
# config path in a fresh temp dir (where its first start writes the template),
# and `XDG_CONFIG_HOME` points elsewhere too.
def isolated_env(extra=None):
    config_home = tempfile.mkdtemp()
    return {**os.environ, "XDG_CONFIG_HOME": config_home,
            "BETTER_CONFIG": os.path.join(config_home, "better", "config.toml"), **(extra or {})}


# What the tests below assume unless they say otherwise.
SETTINGS = {"startup.inline_plots": "iterm"}

# Records what the console asks the browser to open, instead of opening it.
OPENED = os.path.join(tempfile.mkdtemp(), "opened.txt")
if WINDOWS:
    FAKE_BROWSER = os.path.join(os.path.dirname(OPENED), "browser.cmd")
    with open(FAKE_BROWSER, "w") as f:
        f.write(f'@echo off\r\n>>"{OPENED}" echo %~1\r\n')
else:
    FAKE_BROWSER = os.path.join(os.path.dirname(OPENED), "browser")
    with open(FAKE_BROWSER, "w") as f:
        f.write(f'#!/bin/sh\necho "$1" >> {OPENED}\n')
    os.chmod(FAKE_BROWSER, 0o755)


class Pty:
    """The console in a pseudo-terminal: a Unix PTY, or a Windows ConPTY."""

    def __init__(self, args, cwd, env):
        if WINDOWS:
            self.p = PtyProcess.spawn([BIN, *args], cwd=cwd, env=env, dimensions=(40, 120))
            # ConPTY reads block, so a thread feeds a queue that can be polled.
            self.chunks = queue.Queue()
            threading.Thread(target=self._pump, daemon=True).start()
        else:
            self.p = pexpect.spawn(BIN, list(args), dimensions=(40, 120), encoding="utf-8", env=env, cwd=cwd)
        self.pid = self.p.pid
        self.eof = False

    def _pump(self):
        while True:
            try:
                self.chunks.put(self.p.read(65536))
            except EOFError:
                self.chunks.put(None)
                return

    def send(self, text):
        if WINDOWS:
            self.p.write(text)
        else:
            self.p.send(text)

    def read(self, timeout):
        """What arrived within `timeout` seconds; "" if nothing did, None at EOF."""
        if self.eof:
            return None
        if WINDOWS:
            try:
                chunk = self.chunks.get(timeout=timeout)
            except queue.Empty:
                return ""
        else:
            try:
                chunk = self.p.read_nonblocking(65536, timeout=timeout)
            except pexpect.TIMEOUT:
                return ""
            except pexpect.EOF:
                chunk = None
        if chunk is None:
            self.eof = True
        return chunk

    def wait(self, timeout=15):
        """Waits for the console to exit; its exit status, or None if it didn't."""
        deadline = time.time() + timeout
        while time.time() < deadline and self.read(0.1) is not None:
            pass
        if WINDOWS:
            while self.p.isalive() and time.time() < deadline:
                time.sleep(0.1)
            return None if self.p.isalive() else self.p.exitstatus
        if self.p.isalive():
            return None
        self.p.close()
        return self.p.exitstatus


def command_line(pid):
    """The process's full command line, as other users of the machine can see it."""
    if WINDOWS:
        query = f"(Get-CimInstance Win32_Process -Filter 'ProcessId={pid}').CommandLine"
        return subprocess.run(["powershell", "-NoProfile", "-Command", query], capture_output=True, text=True).stdout
    return subprocess.run(["ps", "-o", "command=", "-p", str(pid)], capture_output=True, text=True).stdout


class Term:
    """Drives the console, answering cursor-position queries as a terminal does
    (reedline sends them on Unix). At startup the console asks (on Unix) whether
    the terminal supports the kitty keyboard protocol: the primary device
    attributes query is always answered, the kitty one only with `kitty`."""

    def __init__(self, args=(), cwd=None, env=None, kitty=False, settings=SETTINGS):
        self.kitty = kitty
        env = isolated_env({"BETTER_HISTORY": os.path.join(tempfile.mkdtemp(), "history.txt"),
                            "BROWSER": FAKE_BROWSER, **(env or {})})
        args = [a for k, v in settings.items() for a in ("--set", f"{k}={v}")] + list(args)
        self.p = Pty(args, cwd, env)
        self.raw = ""
        self.buf = ""  # read, but not yet matched by `expect`
        # The cursor row reported to cursor-position queries: always 1, or,
        # once set, moved down by each newline printed since.
        self.row = None

    def _take(self, chunk):
        parts = chunk.split("\x1b[6n")
        for i, part in enumerate(parts):
            if self.row is not None:
                self.row += part.count("\n")
            if i < len(parts) - 1:
                self.p.send(f"\x1b[{self.row or 1};1R")
        if WINDOWS:
            return
        if self.kitty:
            for _ in range(chunk.count("\x1b[?u")):
                self.p.send("\x1b[?0u")
        for _ in range(chunk.count("\x1b[c")):
            self.p.send("\x1b[?62;22c")

    def expect(self, *patterns, timeout=30):
        """Matches `patterns` in order."""
        deadline = time.time() + timeout
        return all(self._expect_one(p, deadline) for p in patterns)

    def _expect_one(self, pattern, deadline):
        while True:
            m = re.search(pattern, self.buf)
            if m:
                self.raw += self.buf[: m.end()]
                self.buf = self.buf[m.end() :]
                return True
            left = deadline - time.time()
            if left <= 0:
                return False
            chunk = self.p.read(min(left, 0.1))
            if chunk is None:
                return False
            self._take(chunk)
            self.buf += chunk

    def idle(self, seconds):
        """Waits like a user would, collecting what the console prints."""
        self.raw += self.buf
        self.buf = ""
        end = time.time() + seconds
        while time.time() < end:
            chunk = self.p.read(0.05)
            if chunk is None:
                return
            self._take(chunk)
            self.raw += chunk

    def send(self, text):
        self.p.send(text)

    def quit(self):
        """Ctrl-D; the console's exit status, or None if it didn't exit (then it's killed)."""
        self.send("\x04")
        status = self.p.wait()
        if status is None:
            self.p.p.terminate(force=True)
        return status

    def clear_line(self):
        """Closes any completion menu and empties the input line."""
        self.send("\x1b")
        self.send("\x15")
        self.idle(0.5)

    def run(self, line, until=None, timeout=30):
        """Sends a line; by default waits until reedline is reading the next top-level line."""
        self.raw = ""
        self.send(line + "\r")
        ok = self.expect(*(until or (READY, TOP)), timeout=timeout)
        return ok, ANSI.sub("", self.raw)


ROW_COUNTS = {}


def menu_rows(raw):
    """The `print.*` suggestion rows of the last menu drawn (each draw starts
    with a cursor save)."""
    frames = [f for f in raw.split("\x1b7") if "╭" in f]
    return re.findall(r"│(print\.[\w.]*) +\{base\} *│", ANSI.sub("", frames[-1])) if frames else []


def check(name, ok, detail=""):
    results[name] = ok
    print(f"{'PASS' if ok else 'FAIL'}  {name}" + (f"  [{detail!r}]" if not ok else ""), flush=True)


t = Term()
check("startup banner + prompt", t.expect("Ctrl-D quits", READY, TOP, timeout=60))
# Without the kitty protocol, Unix terminals send Shift-Enter as plain Enter;
# Windows reports it through the console API.
NEWLINE_KEY = "Shift-Enter" if WINDOWS else "Option-Enter" if sys.platform == "darwin" else "Alt-Enter"
check(f"banner advertises {NEWLINE_KEY} for a newline", f"{NEWLINE_KEY} newline" in ANSI.sub("", t.raw),
      ANSI.sub("", t.raw))

t.run("x <- 42")
ok, out = t.run("x * 2")
check("execute", ok and "[1] 84" in out, out)

t.run('cat("hist_marker_text\\n")')
t.raw = ""
t.send('cat("hist_m')
t.idle(1)
check("history suggestion shown in grey", "arker_text" in ANSI.sub("", t.raw), ANSI.sub("", t.raw))
t.clear_line()

t.raw = ""
t.send("if (TRUE) print('hi') # c")
t.expect(r"# c", timeout=5)
check("syntax highlighting emits colors", "\x1b[" in t.raw and "if" in ANSI.sub("", t.raw), t.raw[-200:])
t.send("\x03")  # Ctrl-C at the prompt clears the line
t.expect(READY, timeout=5)

t.raw = ""
t.send("f <- function(a) {\r")
check("multiline continuation prompt", t.expect(r"\+ ", timeout=10), ANSI.sub("", t.raw))
t.send("a + 1\r")
t.expect(r"\+ ", timeout=10)
t.run("}")
ok, out = t.run("f(1)")
check("multiline function works", ok and "[1] 2" in out, out)

if not WINDOWS:
    # Alt-Enter (ESC CR, also what the VS Code extension sends for Shift-Enter
    # where the terminal can't report it): a newline, so nothing runs until Enter.
    t.raw = ""
    t.send("11 + 1\x1b\r")
    t.idle(1)
    ran_early = "[1] 12" in ANSI.sub("", t.raw)
    ok, out = t.run("22 + 1")
    check("Alt-Enter inserts a newline", not ran_early and ok and "[1] 12" in out and "[1] 23" in out, out)

t.run("y <- mtcars")
t.raw = ""
t.send("y$cy\t")
t.idle(1.5)
ok, out = t.run("")
check("tab completes live data frame column (y$cy -> y$cyl)", "y$cyl" in out and "[1] 6 6 4" in out, out)

t.run("print_me_please <- function(a, b = 2) a + b")
t.raw = ""
t.send("print_me\t")
t.idle(1.5)
ok, out = t.run("1)")
check("tab completes session-defined function", ok and "[1] 3" in out, out)

# Completion as you type: a popup after three characters of a name (or right
# after `$`, `@`, `::`), with what each suggestion is; Tab takes one, Enter
# always runs the line as typed.
MENU = "╭"
t.raw = ""
t.send("print_m")
t.idle(1.5)
out = ANSI.sub("", t.raw)
check("completion pops up as you type", MENU in t.raw and "print_me_please" in out, out)
# A function without a help page is described by its signature.
check("suggestion without help shows its signature", "print_me_please(a, b = 2)" in out, out)
t.clear_line()

# Beside a suggestion: its help, fetched as it's selected.
t.raw = ""
t.send("base::mea")
t.idle(2)
out = ANSI.sub("", t.raw)
check("suggestions list their package in a second column", re.search(r"│mean +\{base\}│", out) is not None, out[-600:])
check("suggestion shows its help page", "mean(x, trim = 0, na.rm = FALSE, ...)" in out and "Arithmetic Mean" in out, out[-600:])
check(
    "help keeps its line breaks and colours",
    re.search(r"│ mean\(x, \.\.\.\) +│", out) is not None
    and re.search(r"│ Arithmetic Mean +│", out) is not None
    and "\x1b[36mmean(x, ...)" in t.raw,
    out[-800:],
)
t.raw = ""
t.send("\x1b[B")
t.idle(1.5)
check("next suggestion shows its own help", "Date Class" in ANSI.sub("", t.raw), ANSI.sub("", t.raw)[-600:])
t.clear_line()

# `print.` has dozens of matches in base: 5 rows show (max_suggestions), and
# moving past the last one scrolls.
t.raw = ""
t.send("print.")
t.idle(2)
first = menu_rows(t.raw)
t.raw = ""
for _ in range(6):
    t.send("\x1b[B")
    t.idle(0.4)
scrolled = menu_rows(t.raw)
ROW_COUNTS["default"] = (len(first), len(scrolled))
check("max_suggestions default: 5 suggestion rows with more matches", len(first) == 5, first)
check("the menu scrolls past its last row", len(scrolled) == 5 and scrolled != first, (first, scrolled))
t.clear_line()

t.raw = ""
t.send("pr")
t.idle(1)
check("no popup for a one- or two-letter name", MENU not in t.raw, ANSI.sub("", t.raw))
t.clear_line()

t.raw = ""
# `c` is too short for a popup of its own; ConPTY delivers keys one at a time,
# so a longer name would legitimately show one before the string starts.
t.send('c("print_m')
t.idle(1)
check("no popup inside a string", MENU not in t.raw, ANSI.sub("", t.raw))
t.clear_line()

# Reedline hands the completer the line only up to the cursor, so the
# string is still open; file paths must be offered anyway.
paths = tempfile.mkdtemp().replace("\\", "/")
open(os.path.join(paths, "unique_name.csv"), "w").close()
t.raw = ""
t.send(f'nchar("{paths}/uniq')
t.idle(1)
t.send("\t")
t.idle(1.5)
t.send("\t")
t.idle(1)
ok, out = t.run('")')
check("Tab completes a file path in an unclosed string", ok and "unique_name.csv" in out, out)

t.raw = ""
t.send("y$")
t.idle(1.5)
check("popup right after $", MENU in t.raw and "mpg" in ANSI.sub("", t.raw), ANSI.sub("", t.raw))
t.clear_line()

t.raw = ""
t.send("print_m")
t.idle(1.5)
ok, out = t.run("")
check("Enter runs the line, not the suggestion", ok and "print_m" in out and "not found" in out, out)

# The signature of the call around the cursor, below the input.
t.raw = ""
t.send("print_me_please(1, ")
t.idle(1.5)
out = ANSI.sub("", t.raw)
check("signature shown under the input", "print_me_please(a, b = 2)" in out, out)
check("current argument highlighted", "\x1b[1mb = 2" in t.raw, t.raw[-300:])
t.clear_line()

# Ctrl-R: typed letters go to the search term, Enter takes the match.
t.raw = ""
t.send("\x12")
t.send("x * ")
t.idle(1)
t.send("\r")
t.idle(0.5)
ok, out = t.run("")
check("Ctrl-R finds a history entry and Enter runs it", ok and "[1] 84" in out, out)

ok, out = t.run('name <- readline("Name? ")', until=(r"Name\? ",))
check("readline() input_request prompt", ok, out)
t.run("Ada")
ok, out = t.run("name")
check("readline() value returned", ok and '"Ada"' in out, out)

t.run("g <- function() { z <- 5; browser(); z * 2 }")
ok, out = t.run("g()", until=BROWSE)
check("browser() switches prompt to Browse[1]>", ok, out)
ok, out = t.run("z", until=BROWSE)
check("browser() evaluates in frame", ok and "[1] 5" in out, out)
ok, out = t.run("c")
check("browser() continue returns to top level", ok and "[1] 10" in out, out)

ok, out = t.run("?mean")
opened = open(OPENED).read() if os.path.exists(OPENED) else ""
check("?mean opens the R help page in the browser", ok and "/library/base/html/mean.html" in opened, opened)

ok, out = t.run("plot(1:3)")
check("plot drawn inline (iTerm2 image protocol)", ok and "\x1b]1337;File=inline=1" in t.raw, out[-200:])

ok, out = t.run("View(mtcars)")
check("View() says it needs the VS Code extension", ok and "only works in VS Code" in out, out[-200:])

t.raw = ""
t.send("Sys.sleep(30); print('not interrupted')\r")
t.idle(1.5)
t.raw = ""
start = time.time()
t.send("\x03")
ok = t.expect(READY, timeout=10)
check("Ctrl-C interrupts running R code", ok and time.time() - start < 5 and '[1] "not interrupted"' not in t.raw,
      ANSI.sub("", t.raw))

ok, out = t.run("x")
check("session survives interrupt", ok and "[1] 42" in out, out)

status = t.quit()
check("Ctrl-D exits cleanly", status == 0, status)

# Shift-Enter: a newline even though `11 + 1` is complete, so nothing runs
# until Enter. As the kitty keyboard protocol reports it, or on Windows as
# ConPTY's win32-input-mode key event (Enter key down, with Shift held). In a
# session of its own: once ConPTY has seen one win32-input-mode key, it reads
# later control characters (Ctrl-U) as plain letters.
SHIFT_ENTER = "\x1b[13;28;13;1;16;1_" if WINDOWS else "\x1b[13;2u"
KITTY_ALT = "\x1b[13;3u"
ALT_ENTER = "\x1b\r"


def newline_case(t, key):
    """Types `11 + 1` and `key`: "newline" if that inserted a newline (then
    runs it with `22 + 1`), "runs" if it ran the line."""
    t.raw = ""
    t.send("11 + 1" + key)
    t.idle(1)
    if "[1] 12" in ANSI.sub("", t.raw):
        t.expect(READY, TOP, timeout=5)
        return "runs"
    ok, out = t.run("22 + 1")
    return "newline" if ok and "[1] 12" in out and "[1] 23" in out else "neither: " + out

# Also a session in Windows Terminal (`WT_SESSION`), with nothing else that
# would pick an image protocol, so plots must come out as Sixel.
t = Term(env={"WT_SESSION": "pty-smoke", "BETTER_INLINE_IMAGES": "", "TERM_PROGRAM": "", "LC_TERMINAL": "",
              "KITTY_WINDOW_ID": ""}, kitty=True, settings={**SETTINGS, "startup.inline_plots": "auto"})
ok = t.expect("Ctrl-D quits", READY, TOP, timeout=60)
check("banner advertises Shift-Enter where the terminal reports it", ok and "Shift-Enter newline" in ANSI.sub("", t.raw),
      ANSI.sub("", t.raw))
t.raw = ""
t.send("11 + 1" + SHIFT_ENTER)
t.idle(1)
ran_early = "[1] 12" in ANSI.sub("", t.raw)
ok, out = t.run("22 + 1")
check("Shift-Enter inserts a newline", not ran_early and ok and "[1] 12" in out and "[1] 23" in out, out)
if not WINDOWS:
    check("auto with the kitty protocol: Alt-Enter runs the line", newline_case(t, KITTY_ALT) == "runs")

t.raw = ""
ok, out = t.run("plot(1:3)")
sixel = re.search(r"\x1bP[0-9;]*q\"1;1;(\d+);(\d+)", t.raw)
check("plot drawn as Sixel in Windows Terminal", ok and sixel and "\x1b\\" in t.raw[sixel.end():], out[-200:])
t.quit()

# `.Rprofile` output (e.g. renv's "Project loaded"). A console that starts its
# own kernel sees it live; one attaching to a running kernel (as the VS Code
# extension's does) gets it replayed from what the kernel recorded at startup.
project = tempfile.mkdtemp()
with open(os.path.join(project, ".Rprofile"), "w") as f:
    f.write('cat("profile stdout\\n")\nmessage("profile message")\n')

t = Term(cwd=project)
ok = t.expect("Ctrl-D quits", READY, TOP, timeout=60)
out = ANSI.sub("", t.raw)
check(".Rprofile output shown once (own kernel)",
      ok and out.count("profile stdout") == 1 and out.count("profile message") == 1, out)
t.quit()


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


session = tempfile.mkdtemp(prefix="better-smoke-")
conn = os.path.join(session, "kernel.json")
with open(conn, "w") as f:
    json.dump({**{p: free_port() for p in ("shell_port", "iopub_port", "stdin_port", "control_port", "hb_port")},
               "ip": "127.0.0.1", "key": "smoke", "transport": "tcp", "signature_scheme": "hmac-sha256"}, f)
kernel = subprocess.Popen([BIN, "--kernel", "--connection_file", conn, "--log", os.path.join(session, "ark.log")],
                          cwd=project, env=isolated_env())
time.sleep(8)  # well past startup, like the extension's console
t = Term(["--connection-file", conn])
ok = t.expect("Ctrl-D quits", "profile stdout", "profile message", READY, TOP, timeout=60)
check(".Rprofile output replayed when attaching later", ok, ANSI.sub("", t.raw))
t.quit()

# Hosted by an IDE, as the VS Code extension does: the IDE listens on loopback
# TCP, and the console proves itself with the token from its environment.
token = os.urandom(16).hex()
with socket.socket() as server:
    server.bind(("127.0.0.1", 0))
    server.listen(1)
    server.settimeout(60)
    # Hosted, the extension binds the newline key itself and sends ESC CR.
    t = Term(["--connection-file", conn, "--ide", str(server.getsockname()[1])], env={"BETTER_IDE_TOKEN": token})
    try:
        ide, _ = server.accept()
        ide_lines = ide.makefile("r", encoding="utf-8")
        hello = json.loads(ide_lines.readline())
    except (OSError, ValueError) as e:
        ide, hello = None, repr(e)
    check("hosted console sends hello with its token and protocol first",
          hello == {"type": "hello", "token": token, "protocol": 1}, hello)
    argv = command_line(t.p.pid)
    check("IDE token is not on the console's command line", argv.strip() != "" and token not in argv, argv)
    ok = t.expect("Ctrl-D quits", READY, TOP, timeout=60)
    check("banner advertises Shift-Enter when the host maps it", ok and "Shift-Enter newline" in ANSI.sub("", t.raw),
          ANSI.sub("", t.raw))
    if ide:
        t.raw = ""
        ide.sendall((json.dumps({"type": "execute", "code": 'cat("from the IDE\\n")'}) + "\n").encode())
        ok = ok and t.expect(r"from the IDE\r?\n", READY, TOP)
    check("hosted console runs code the IDE sends", ok, ANSI.sub("", t.raw)[-300:])
    check("hosted: ESC CR inserts a newline", newline_case(t, ALT_ENTER) == "newline")
    if ide and not WINDOWS:
        # Code that prints nothing: its echo is the only line printed, and the
        # next prompt must start below it rather than be drawn over it (#4).
        # The prompt is on row 1 as far as the console knows; from here the
        # terminal reports the cursor's real row.
        t.raw = ""
        t.row = 1
        ide.sendall((json.dumps({"type": "execute", "code": "df <- mtcars"}) + "\n").encode())
        ok = t.expect(r"df \S*<-\S* mtcars\r?\n", READY, TOP)
        # Rows the cursor is moved to after the echo, to draw the prompt.
        rows = set(re.findall(r"\x1b\[(\d+);\d+H", t.raw.split("mtcars", 1)[-1]))
        t.row = None
        check("IDE code that prints nothing stays echoed above the next prompt",
              ok and rows == {"2"}, (rows, t.raw[-300:]))
    t.quit()
    if ide:
        ide.close()

# Hosted, editing.newline comes only from --set (the IDE's own settings): a
# value in config.toml is ignored, or the banner would name a key the IDE
# doesn't send the newline for.
newline_env = isolated_env({"BETTER_IDE_TOKEN": token})
os.makedirs(os.path.dirname(newline_env["BETTER_CONFIG"]))
with open(newline_env["BETTER_CONFIG"], "w") as f:
    f.write('[editing]\nnewline = "alt_enter"\n')
for flags, banner in (({}, "Shift-Enter"), ({"editing.newline": "alt_enter"}, "Alt-Enter")):
    with socket.socket() as server:
        server.bind(("127.0.0.1", 0))
        server.listen(1)
        server.settimeout(60)
        t = Term(["--connection-file", conn, "--ide", str(server.getsockname()[1])], env=newline_env,
                 settings={**SETTINGS, **flags})
        try:
            ide, _ = server.accept()
        except OSError:
            ide = None
        ok = t.expect("Ctrl-D quits", READY, TOP, timeout=60)
        out = ANSI.sub("", t.raw)
        how = "with --set editing.newline=alt_enter" if flags else 'with newline = "alt_enter" only in the file'
        check(f"hosted {how}: banner names {banner}", ok and f"{banner} newline" in out, out)
        t.quit()
        if ide:
            ide.close()

# An IDE that speaks another protocol answers the hello with `incompatible`
# and closes the channel: the console shows why and keeps working on its own.
with socket.socket() as server:
    server.bind(("127.0.0.1", 0))
    server.listen(1)
    server.settimeout(60)
    t = Term(["--connection-file", conn, "--ide", str(server.getsockname()[1])], env={"BETTER_IDE_TOKEN": token})
    try:
        ide, _ = server.accept()
        ide.makefile("r", encoding="utf-8").readline()
        ide.sendall((json.dumps({"type": "incompatible", "message": "smoke: update the extension"}) + "\n").encode())
        ide.close()
    except OSError as e:
        print("incompatible IDE:", repr(e))
    ok = t.expect("smoke: update the extension", READY, TOP, timeout=60)
    check("console shows the IDE's incompatible message", ok, ANSI.sub("", t.raw)[-300:])
    t.idle(1)
    ok, out = t.run("1 + 1")
    check("console keeps working after an incompatible IDE", ok and "[1] 2" in out, out[-300:])
    t.quit()
kernel.kill()
kernel.wait()

# Settings (#6).
TEMPLATE_LINES = ("[completion]", "# min_chars = 3", "# delay_ms = 0", "# accept = \"tab\"", "# size = 10000")


def piped(code, args=(), env=None):
    """Runs `code` through the console's piped mode: a full start, then exit."""
    return subprocess.run([BIN, *args], input=code, capture_output=True, text=True, timeout=120,
                          env=env or isolated_env(), encoding="utf-8", errors="replace")


# First start: the template is written at the default location, under
# XDG_CONFIG_HOME (no BETTER_CONFIG); a second start leaves an edited file alone.
env = isolated_env()
del env["BETTER_CONFIG"]
config = os.path.join(env["XDG_CONFIG_HOME"], "better", "config.toml")
r = piped("1 + 1", env=env)
written = open(config).read() if os.path.exists(config) else ""
check("first start writes the config template", r.returncode == 0 and all(l in written for l in TEMPLATE_LINES),
      (r.returncode, r.stderr[-300:], written))
with open(config, "w") as f:
    f.write("[completion]\nmin_chars = 3\n")
r = piped("1 + 1", env=env)
check("a second start leaves an edited config alone", open(config).read() == "[completion]\nmin_chars = 3\n",
      open(config).read())

r = subprocess.run([BIN, "--reset-config", "--yes"], capture_output=True, text=True, env=env, timeout=30)
backup = config + ".bak"
check("--reset-config --yes writes the template and keeps a .bak",
      r.returncode == 0 and all(l in open(config).read() for l in TEMPLATE_LINES)
      and open(backup).read() == "[completion]\nmin_chars = 3\n" and backup in r.stdout,
      (r.returncode, r.stdout, r.stderr))

# startup.r_home reaches the kernel the console starts (it can't start R there).
r = piped("1 + 1", ["--set", "startup.r_home=/nonexistent/R"])
check("startup.r_home is used for the kernel", r.returncode == 1 and "/nonexistent/R" in r.stderr, r.stderr[-400:])

r = subprocess.run([BIN, "--print-config", "--set", "completion.popup=tab"], capture_output=True, text=True,
                   env=isolated_env({"BETTER_SHIFT_ENTER": "1"}), timeout=30)
check("deprecated BETTER_SHIFT_ENTER still works, with a warning",
      re.search(r'editing\.newline = "shift_enter" +# env BETTER_SHIFT_ENTER', r.stdout) is not None
      and "BETTER_SHIFT_ENTER is deprecated: use the setting `editing.newline`" in r.stderr,
      (r.stdout, r.stderr))
check("--print-config shows where each value came from",
      re.search(r'completion\.popup = "tab" +# flag --set', r.stdout) is not None
      and re.search(r"history\.size = 10000 +# default", r.stdout) is not None, r.stdout)

# A config file with several settings, and some mistakes.
env = isolated_env()
os.makedirs(os.path.dirname(env["BETTER_CONFIG"]))
with open(env["BETTER_CONFIG"], "w") as f:
    f.write('[startup]\nbanner = false\ninline_plots = "iterm"\n\n[completion]\nmin_chars = 3\naccept = "enter"\n'
            'colour = "red"\ndelay_ms = 5000\nmax_suggestions = 2\n\n[history]\nsuggestions = false\n')
t = Term(env=env, settings={})
ok = t.expect(READY, TOP, timeout=60)
out = ANSI.sub("", t.raw)
check("bad config entries print a warning and the console still starts",
      ok and "unknown setting `completion.colour`" in out and "`completion.delay_ms` must be a whole number from 0 to 2000"
      in out and env["BETTER_CONFIG"] in out, out)
check("startup.banner = false hides the banner", ok and "Ctrl-D quits" not in out and "ark " not in out, out)

t.run("print_me_please <- function(a, b = 2) a + b")
t.raw = ""
t.send("pr")
t.idle(1)
check("min_chars = 3: no menu after 2 characters", MENU not in t.raw, ANSI.sub("", t.raw))
t.raw = ""
t.send("i")
t.idle(1.5)
check("min_chars = 3: a menu after 3", MENU in t.raw and "print" in ANSI.sub("", t.raw), ANSI.sub("", t.raw))
t.clear_line()
# Not ending in a name: with accept = "enter", Enter would take a suggestion.
t.run("df <- mtcars[1:3, ]")
t.raw = ""
t.send("df$")
t.idle(1.5)
check("min_chars = 3: a menu right after df$", MENU in t.raw and "mpg" in ANSI.sub("", t.raw), ANSI.sub("", t.raw))
t.clear_line()
t.raw = ""
t.send("print.")
t.idle(2)
rows = menu_rows(t.raw)
ROW_COUNTS["max_suggestions = 2"] = len(rows)
check("max_suggestions = 2 shows 2 suggestion rows", len(rows) == 2, rows)
t.clear_line()

t.raw = ""
t.send("print_me")
t.idle(1.5)
t.send("\r")
t.idle(1)
ok, out = t.run("1)")
check('completion.accept = "enter" takes a suggestion with Enter', ok and "[1] 3" in out, out)

t.run('cat("hist_marker_text\\n")')
t.raw = ""
t.send('cat("hist_m')
t.idle(1)
check("history.suggestions = false shows no grey suggestion", "arker_text" not in ANSI.sub("", t.raw),
      ANSI.sub("", t.raw))
t.clear_line()
t.quit()

# completion.delay_ms, from the command line.
t = Term(settings={"completion.delay_ms": "300"})
t.expect("Ctrl-D quits", READY, TOP, timeout=60)
t.run("print_me_please <- function(a, b = 2) a + b")
t.raw = ""
for ch in "print_m":
    t.send(ch)
    t.idle(0.06)
typing = t.raw
t.idle(1.5)
check("delay_ms = 300: no menu while typing quickly", MENU not in typing, ANSI.sub("", typing))
check("delay_ms = 300: a menu after the pause", MENU in t.raw and "print_me_please" in ANSI.sub("", t.raw),
      ANSI.sub("", t.raw))
t.clear_line()
# Keys typed around the moment the timer fires.
for i, ch in enumerate("zz_slow_name <- 7"):
    t.send(ch)
    t.idle(0.3 + (-0.03, -0.01, 0.0, 0.01, 0.03)[i % 5])
t.send("\x1b")
t.idle(0.3)
ok, out = t.run("")
ok, out = t.run("zz_slow_name")
check("delay_ms = 300: no key lost", ok and "[1] 7" in out, out)
t.quit()

# editing.newline: only the chosen key inserts a newline; the others run the
# line. Without the kitty protocol, Shift-Enter arrives as Enter, so only
# Alt-Enter (ESC CR, also Option-Enter's bytes) can be sent; with it, both.
if not WINDOWS:
    OPTION = "Option-Enter" if sys.platform == "darwin" else "Alt-Enter"
    cases = [
        # (setting, kitty, banner key, {key sequence name: expected})
        ("shift_enter", False, "Shift-Enter", {"Alt-Enter": "runs"}),
        ("alt_enter", False, "Alt-Enter", {"Alt-Enter": "newline"}),
        ("opt_enter", False, OPTION, {"Alt-Enter": "newline"}),
        ("shift_enter", True, "Shift-Enter", {"Shift-Enter": "newline", "Alt-Enter": "runs"}),
        ("alt_enter", True, "Alt-Enter", {"Alt-Enter": "newline", "Shift-Enter": "runs"}),
        ("opt_enter", True, OPTION, {"Alt-Enter": "newline", "Shift-Enter": "runs"}),
    ]
    keys = {False: {"Alt-Enter": ALT_ENTER}, True: {"Alt-Enter": KITTY_ALT, "Shift-Enter": SHIFT_ENTER}}
    for value, kitty, banner, expected in cases:
        where = "kitty protocol" if kitty else "no kitty protocol"
        t = Term(settings={**SETTINGS, "editing.newline": value}, kitty=kitty)
        ok = t.expect("Ctrl-D quits", READY, TOP, timeout=60)
        out = ANSI.sub("", t.raw)
        check(f"newline = {value} ({where}): banner names {banner}", ok and f"{banner} newline" in out, out)
        warned = "can't tell" in out or "sends Shift-Enter as Enter" in out
        check(f"newline = {value} ({where}): warns only if Shift-Enter can't be told apart",
              warned == (value == "shift_enter" and not kitty), out)
        for name, want in expected.items():
            got = newline_case(t, keys[kitty][name])
            check(f"newline = {value} ({where}): {name} {'inserts a newline' if want == 'newline' else 'runs the line'}",
                  got == want, got)
        ok, out = t.run("33 + 1")
        check(f"newline = {value} ({where}): Enter runs the line", ok and "[1] 34" in out, out)
        t.quit()

print("observed suggestion rows:", ROW_COUNTS)
print()
failed = [k for k, v in results.items() if not v]
print(f"{len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
