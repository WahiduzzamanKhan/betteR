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

    def __init__(self, args=(), cwd=None, env=None, kitty=False):
        self.kitty = kitty
        env = {**os.environ, "BETTER_HISTORY": os.path.join(tempfile.mkdtemp(), "history.txt"),
               "BETTER_INLINE_IMAGES": "iterm", "BROWSER": FAKE_BROWSER, **(env or {})}
        self.p = Pty(args, cwd, env)
        self.raw = ""
        self.buf = ""  # read, but not yet matched by `expect`

    def _take(self, chunk):
        for _ in range(chunk.count("\x1b[6n")):
            self.p.send("\x1b[1;1R")
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


def check(name, ok, detail=""):
    results[name] = ok
    print(f"{'PASS' if ok else 'FAIL'}  {name}" + (f"  [{detail!r}]" if not ok else ""), flush=True)


t = Term()
check("startup banner + prompt", t.expect("Ctrl-D quits", READY, TOP, timeout=60))
# Without the kitty protocol, Unix terminals send Shift-Enter as plain Enter;
# Windows reports it through the console API.
NEWLINE_KEY = "Shift-Enter" if WINDOWS else "Alt-Enter"
check(f"banner advertises {NEWLINE_KEY} for a newline", f"{NEWLINE_KEY} newline" in ANSI.sub("", t.raw),
      ANSI.sub("", t.raw))

t.run("x <- 42")
ok, out = t.run("x * 2")
check("execute", ok and "[1] 84" in out, out)

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
# Also a session in Windows Terminal (`WT_SESSION`), with nothing else that
# would pick an image protocol, so plots must come out as Sixel.
t = Term(env={"WT_SESSION": "pty-smoke", "BETTER_INLINE_IMAGES": "", "TERM_PROGRAM": "", "LC_TERMINAL": "",
              "KITTY_WINDOW_ID": ""}, kitty=True)
ok = t.expect("Ctrl-D quits", READY, TOP, timeout=60)
check("banner advertises Shift-Enter where the terminal reports it", ok and "Shift-Enter newline" in ANSI.sub("", t.raw),
      ANSI.sub("", t.raw))
t.raw = ""
t.send("11 + 1" + SHIFT_ENTER)
t.idle(1)
ran_early = "[1] 12" in ANSI.sub("", t.raw)
ok, out = t.run("22 + 1")
check("Shift-Enter inserts a newline", not ran_early and ok and "[1] 12" in out and "[1] 23" in out, out)

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
                          cwd=project)
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
    # BETTER_SHIFT_ENTER: the host maps Shift-Enter itself (as the extension
    # does where VS Code's terminal can't report it).
    t = Term(["--connection-file", conn, "--ide", str(server.getsockname()[1])],
             env={"BETTER_IDE_TOKEN": token, "BETTER_SHIFT_ENTER": "1"})
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

print()
failed = [k for k, v in results.items() if not v]
print(f"{len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
