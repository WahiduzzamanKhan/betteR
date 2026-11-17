"""Phase 1 interactive smoke test: drives the real `better` binary in a PTY.

Usage: uv run --with pexpect python tests/pty_smoke.py [path/to/better]
"""

import json
import os
import re
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import pexpect

ROOT = Path(__file__).resolve().parents[1]
BIN = sys.argv[1] if len(sys.argv) > 1 else str(ROOT / "target/debug/better")
ANSI = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b[()][A-Z0-9]|\r")

# Reedline enables bracketed paste each time it starts reading a line.
READY = r"\x1b\[\?2004h"
TOP = r"\x1b\[34m> "
BROWSE = (READY, r"Browse\[1\]> ")

results = {}

# Records what the console asks the browser to open, instead of opening it.
OPENED = os.path.join(tempfile.mkdtemp(), "opened.txt")
FAKE_BROWSER = os.path.join(os.path.dirname(OPENED), "browser")
with open(FAKE_BROWSER, "w") as f:
    f.write(f'#!/bin/sh\necho "$1" >> {OPENED}\n')
os.chmod(FAKE_BROWSER, 0o755)


class Term:
    """pexpect wrapper that answers cursor-position queries (reedline sends them)."""

    def __init__(self, args=(), cwd=None):
        env = dict(os.environ, BETTER_HISTORY=os.path.join(tempfile.mkdtemp(), "history.txt"),
                   BETTER_INLINE_IMAGES="iterm", BROWSER=FAKE_BROWSER)
        self.p = pexpect.spawn(BIN, list(args), dimensions=(40, 120), encoding="utf-8", timeout=30, env=env, cwd=cwd)
        self.raw = ""

    def expect(self, *patterns, timeout=30):
        """Matches `patterns` in order (a DSR query may sit between them)."""
        deadline = time.time() + timeout
        return all(self._expect_one(p, deadline) for p in patterns)

    def _expect_one(self, pattern, deadline):
        while time.time() < deadline:
            i = self.p.expect([pattern, r"\x1b\[6n", pexpect.TIMEOUT, pexpect.EOF], timeout=deadline - time.time())
            self.raw += self.p.before + (self.p.after if isinstance(self.p.after, str) else "")
            if i == 0:
                return True
            if i == 1:
                self.p.send("\x1b[1;1R")
                continue
            return False
        return False

    def run(self, line, until=None, timeout=30):
        """Sends a line; by default waits until reedline is reading the next top-level line."""
        self.raw = ""
        self.p.send(line + "\r")
        ok = self.expect(*(until or (READY, TOP)), timeout=timeout)
        return ok, ANSI.sub("", self.raw)


def check(name, ok, detail=""):
    results[name] = ok
    print(f"{'PASS' if ok else 'FAIL'}  {name}" + (f"  [{detail!r}]" if not ok else ""), flush=True)


t = Term()
check("startup banner + prompt", t.expect("Ctrl-D quits", READY, TOP, timeout=60))

t.run("x <- 42")
ok, out = t.run("x * 2")
check("execute", ok and "[1] 84" in out, out)

t.raw = ""
t.p.send("if (TRUE) print('hi') # c")
t.expect(r"# c", timeout=5)
check("syntax highlighting emits colors", "\x1b[" in t.raw and "if" in ANSI.sub("", t.raw), t.raw[-200:])
t.p.send("\x03")  # Ctrl-C at the prompt clears the line
t.expect(READY, timeout=5)

t.raw = ""
t.p.send("f <- function(a) {\r")
check("multiline continuation prompt", t.expect(r"\+ ", timeout=10), ANSI.sub("", t.raw))
t.p.send("a + 1\r")
t.expect(r"\+ ", timeout=10)
t.run("}")
ok, out = t.run("f(1)")
check("multiline function works", ok and "[1] 2" in out, out)

t.run("y <- mtcars")
t.raw = ""
t.p.send("y$cy\t")
time.sleep(1.5)
ok, out = t.run("")
check("tab completes live data frame column (y$cy -> y$cyl)", "y$cyl" in out and "[1] 6 6 4" in out, out)

t.run("print_me_please <- function(a, b = 2) a + b")
t.raw = ""
t.p.send("print_me\t")
time.sleep(1.5)
ok, out = t.run("1)")
check("tab completes session-defined function", ok and "[1] 3" in out, out)

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

t.raw = ""
t.p.send("Sys.sleep(30); print('not interrupted')\r")
time.sleep(1.5)
start = time.time()
t.p.send("\x03")
ok = t.expect(READY, timeout=10)
check("Ctrl-C interrupts running R code", ok and time.time() - start < 5 and "not interrupted" not in t.raw,
      ANSI.sub("", t.raw))

ok, out = t.run("x")
check("session survives interrupt", ok and "[1] 42" in out, out)

t.p.sendcontrol("d")
t.p.expect(pexpect.EOF, timeout=15)
t.p.close()
check("Ctrl-D exits cleanly", t.p.exitstatus == 0, t.p.exitstatus)

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
t.p.sendcontrol("d")
t.p.expect(pexpect.EOF, timeout=15)


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
t.p.sendcontrol("d")
t.p.expect(pexpect.EOF, timeout=15)
kernel.kill()
kernel.wait()

print()
failed = [k for k, v in results.items() if not v]
print(f"{len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
