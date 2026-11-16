#!/usr/bin/env python3
"""End-to-end checks of the terminal surface: the interactive chat and
`unharnessed login`/`logout`, on the real binary in a real pseudo-terminal,
with a real kernel and the faux provider (no network, no credentials).

usage: scripts/unharnessed-cli-e2e.py <out-dir> [case...]

Cases: chat (a turn with a cell, /help, /exit), continue (-c brings the
session back), interrupt (Ctrl+C stops a running cell), paste (pasted lines
are one message), multiline (a trailing backslash continues), queued (a message typed
during a turn runs next), goal (/goal runs until goal.complete()), wake (a
background bash() starts a turn on its own), quit (Ctrl+C twice at the
prompt), first-run (no credentials: the sign-in menu), api-key (login and
logout with a key), print (-p still answers), help (login is listed).

Unix only (it needs a pty). Writes <out-dir>/<case>.out: the command, the
faux script, then everything typed (`>>> `) and everything the terminal
showed. UNHARNESSED_BIN picks the binary (default target/debug/unharnessed).
Exits non-zero when a check fails.
"""
import json
import os
import pty
import re
import select
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = os.environ.get("UNHARNESSED_BIN", str(ROOT / "target/debug/unharnessed"))
OUT = Path(sys.argv[1]).resolve()
CASES = sys.argv[2:] or ["chat", "continue", "interrupt", "paste", "multiline", "queued", "goal", "wake",
                         "quit", "first-run", "api-key", "print", "help"]
OUT.mkdir(parents=True, exist_ok=True)
# A throwaway HOME: no credentials and no provider keys, so the faux
# provider is the only model. The kernel venv is the one already built.
HOME = OUT / "home"
BASE_ENV = {k: v for k, v in os.environ.items()
            if not k.startswith(("UNHARNESSED_", "UH_", "XDG_", "AWS_", "CLOUDSDK_", "GOOGLE_", "AZURE_"))
            and not k.endswith(("_API_KEY", "_TOKEN"))}
KERNEL_VENV = os.environ.get("UNHARNESSED_KERNEL_VENV", str(Path.home() / ".unharnessed/kernel-venv"))
ENV = dict(BASE_ENV, HOME=str(HOME), TERM="xterm-256color", NO_COLOR="1",
           UV_CACHE_DIR=os.environ.get("UV_CACHE_DIR", str(Path.home() / ".cache/uv")),
           UNHARNESSED_KERNEL_VENV=KERNEL_VENV, UNHARNESSED_OFFLINE="1", DO_NOT_TRACK="1")
# The dev binary finds the runtime and the skills in this checkout; an
# installed one (UNHARNESSED_BIN) beside itself, as it ships.
if "UNHARNESSED_BIN" not in os.environ:
    ENV["UNHARNESSED_PACKAGE_DIR"] = str(ROOT)
ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")
FAILED = []


def check(name, ok, detail=""):
    print(("PASS  " if ok else "FAIL  ") + name + ("" if ok else f" ({detail})"), flush=True)
    if not ok:
        FAILED.append(name)


def cell(code):
    return {"content": [{"type": "toolCall", "name": "ipython", "arguments": {"code": code}}]}


class Term:
    """The binary in a pseudo-terminal, as a person would run it."""

    def __init__(self, name, args, work, script=None, agent_dir=None):
        self.work = work
        work.mkdir(parents=True, exist_ok=True)
        env = dict(ENV)
        agent_dir = agent_dir or (work / "agent")
        env["UNHARNESSED_CODING_AGENT_DIR"] = str(agent_dir)
        env["UNHARNESSED_AGENT_DIR"] = str(agent_dir)
        if script is not None:
            env["UNHARNESSED_FAUX_SCRIPT"] = json.dumps(script)
        self.log = open(OUT / f"{name}.out", "a")
        self.log.write(f"$ cd {work} && {BIN} {' '.join(args)}\n")
        if script is not None:
            self.log.write(f"UNHARNESSED_FAUX_SCRIPT={json.dumps(script)}\n")
        self.log.write("---\n")
        master, slave = pty.openpty()
        self.master = master
        self.child = subprocess.Popen([BIN, *args], cwd=work, env=env, stdin=slave, stdout=slave,
                                      stderr=slave, start_new_session=True, close_fds=True,
                                      preexec_fn=lambda: __import__("fcntl").ioctl(0, __import__("termios").TIOCSCTTY, 0))
        os.close(slave)
        self.screen = ""

    def pump(self, timeout):
        ready, _, _ = select.select([self.master], [], [], timeout)
        if not ready:
            return False
        try:
            data = os.read(self.master, 65536)
        except OSError:
            return False
        if not data:
            return False
        text = ANSI.sub("", data.decode("utf-8", "replace")).replace("\r\n", "\n")
        self.screen += text
        self.log.write(text)
        self.log.flush()
        return True

    def expect(self, pattern, timeout=60, since=0):
        """Wait until `pattern` (a regex) shows after offset `since`."""
        end = time.time() + timeout
        while time.time() < end:
            match = re.search(pattern, self.screen[since:])
            if match:
                return True
            self.pump(min(0.2, max(0.01, end - time.time())))
        return bool(re.search(pattern, self.screen[since:]))

    def type(self, text):
        self.log.write(f"\n>>> {text!r}\n")
        os.write(self.master, text.encode())

    def ctrl_c(self):
        # The terminal sends SIGINT to the foreground group on ^C.
        self.log.write("\n>>> ^C\n")
        os.write(self.master, b"\x03")

    def mark(self):
        return len(self.screen)

    def wait(self, timeout=60):
        end = time.time() + timeout
        while time.time() < end and self.child.poll() is None:
            self.pump(0.2)
        while self.pump(0.1):
            pass
        if self.child.poll() is None:
            self.child.kill()
            self.child.wait()
            return None
        self.log.write(f"\n[exit {self.child.returncode}]\n")
        return self.child.returncode


def fresh(name):
    work = OUT / name
    shutil.rmtree(work, ignore_errors=True)
    (OUT / f"{name}.out").unlink(missing_ok=True)
    return work / "project"


def session_messages(work):
    files = sorted((work / "sessions").rglob("*.jsonl")) if (work / "sessions").exists() else []
    files = files or sorted((work / "agent" / "sessions").rglob("*.jsonl"))
    rows = []
    for file in files:
        for line in file.read_text().splitlines():
            row = json.loads(line)
            if row.get("type") == "message":
                rows.append(row["message"])
    return rows


def user_texts(work):
    texts = []
    for message in session_messages(work):
        if message.get("role") != "user":
            continue
        content = message.get("content")
        if isinstance(content, str):
            texts.append(content)
        else:
            texts.append("".join(part.get("text", "") for part in content if part.get("type") == "text"))
    return texts


def case_chat():
    work = fresh("chat")
    term = Term("chat", [], work, {"responses": [cell("print(6 * 7)"), "The answer is 42.", "spare"]})
    check("chat: banner names the model", term.expect(r"faux/faux-1"), term.screen[-400:])
    check("chat: banner warns about permissions", "without asking first" in term.screen)
    term.expect(r"> ")
    term.type("What is 6 times 7?\n")
    check("chat: the cell's code shows", term.expect(r"ipython\s*\n\s*│ print\(6 \* 7\)"), term.screen[-600:])
    check("chat: the cell's output shows", term.expect(r"└ 42"), term.screen[-600:])
    check("chat: the answer streams", term.expect(r"The answer is 42\."), term.screen[-600:])
    mark = term.mark()
    term.expect(r"> ", since=mark - 5)
    term.type("/help\n")
    check("chat: /help lists the commands", term.expect(r"/compact \[focus\]", since=mark), term.screen[mark:])
    mark = term.mark()
    term.type("/model\n")
    check("chat: a panel-only command says so", term.expect(r"not available in the terminal", since=mark),
          term.screen[mark:])
    term.type("/exit\n")
    code = term.wait()
    check("chat: /exit exits 0", code == 0, f"exit {code}")
    check("chat: says how to continue", "unharnessed -c" in term.screen)
    texts = user_texts(work)
    check("chat: the prompt is in the session file", "What is 6 times 7?" in texts, texts)


def case_continue():
    work = fresh("continue")
    first = Term("continue", [], work, {"responses": [cell("secret = 1234"), "Stored.", "spare"]})
    first.expect(r"> ")
    first.type("store a secret\n")
    first.expect(r"Stored\.")
    first.type("/exit\n")
    first.wait()
    again = Term("continue", ["-c"], work, {"responses": [cell("print(secret)"), "It is 1234.", "spare"]})
    check("continue: -c says it continues", again.expect(r"Continuing a session of \d+ messages"),
          again.screen[-400:])
    again.expect(r"> ")
    again.type("what is the secret?\n")
    check("continue: the kernel variable came back", again.expect(r"└ 1234"), again.screen[-600:])
    again.expect(r"It is 1234\.")
    mark = again.mark()
    again.expect(r"> ", since=mark - 5)
    again.type("\x04")  # Ctrl+D
    code = again.wait()
    check("continue: Ctrl+D exits 0", code == 0, f"exit {code}")


def case_interrupt():
    work = fresh("interrupt")
    term = Term("interrupt", [], work, {"responses": [cell("import time\ntime.sleep(120)"),
                                                     "after the stop", "spare"]})
    term.expect(r"> ")
    term.type("sleep for a while\n")
    check("interrupt: the cell starts", term.expect(r"time\.sleep\(120\)"), term.screen[-400:])
    time.sleep(2)
    started = time.time()
    term.ctrl_c()
    check("interrupt: Ctrl+C says it stopped", term.expect(r"Stopped\.", timeout=15), term.screen[-600:])
    mark = term.mark()
    check("interrupt: the prompt comes back", term.expect(r"\n> ", timeout=30, since=mark - 10),
          term.screen[-600:])
    check("interrupt: within 30 seconds", time.time() - started < 30, f"{time.time() - started:.1f}s")
    check("interrupt: the process is still running", term.child.poll() is None)
    term.type("/exit\n")
    code = term.wait()
    check("interrupt: exits 0 afterwards", code == 0, f"exit {code}")


def case_paste():
    work = fresh("paste")
    term = Term("paste", [], work, {"responses": ["Got both lines.", "spare"]})
    term.expect(r"> ")
    term.type("line one\nline two\nline three\n")
    check("paste: one answer", term.expect(r"Got both lines\."), term.screen[-400:])
    mark = term.mark()
    term.expect(r"> ", since=mark - 5)
    term.type("/exit\n")
    term.wait()
    texts = user_texts(work)
    check("paste: one message with the three lines", texts == ["line one\nline two\nline three"], texts)


def case_multiline():
    work = fresh("multiline")
    term = Term("multiline", [], work, {"responses": ["Joined.", "spare"]})
    term.expect(r"> ")
    term.type("first part \\\n")
    check("multiline: asks for more", term.expect(r"\n\. $|\. $", timeout=5), term.screen[-200:])
    time.sleep(0.3)
    term.type("second part\n")
    term.expect(r"Joined\.")
    mark = term.mark()
    term.expect(r"> ", since=mark - 5)
    term.type("/exit\n")
    term.wait()
    texts = user_texts(work)
    check("multiline: one message over two lines", texts == ["first part \nsecond part"], texts)


def case_queued():
    work = fresh("queued")
    term = Term("queued", [], work, {"responses": [cell("import time\ntime.sleep(4)"), "First done.",
                                                  "Second answer.", "spare"]})
    term.expect(r"> ")
    term.type("start\n")
    term.expect(r"time\.sleep\(4\)")
    term.type("a follow-up typed while it works\n")
    check("queued: the first turn ends", term.expect(r"First done\.", timeout=60), term.screen[-400:])
    check("queued: the typed message runs next", term.expect(r"Second answer\.", timeout=60),
          term.screen[-400:])
    mark = term.mark()
    term.expect(r"> ", since=mark - 5)
    term.type("/exit\n")
    term.wait()
    texts = user_texts(work)
    check("queued: both messages, in order", texts == ["start", "a follow-up typed while it works"], texts)


def case_goal():
    work = fresh("goal")
    term = Term("goal", [], work, {"responses": [cell("await goal.complete()"), "Goal reached.", "spare"],
                                   "repeatLastResponse": True})
    term.expect(r"> ")
    term.type("/goal write the word done\n")
    check("goal: the goal's turn runs the cell", term.expect(r"goal\.complete\(\)", timeout=60),
          term.screen[-600:])
    check("goal: and answers", term.expect(r"Goal reached\.", timeout=60), term.screen[-600:])
    mark = term.mark()
    term.expect(r"> ", since=mark - 5)
    term.type("/goal\n")
    check("goal: /goal shows it complete", term.expect(r"(?i)complete", timeout=30, since=mark),
          term.screen[mark:])
    term.type("/exit\n")
    code = term.wait()
    check("goal: exits 0", code == 0, f"exit {code}")


def case_wake():
    work = fresh("wake")
    term = Term("wake", [], work, {"responses": [cell('job = bash("sleep 3; echo background-done")'),
                                                 "Started it.", "The background command finished.",
                                                 "spare"]})
    term.expect(r"> ")
    term.type("start a background job\n")
    term.expect(r"Started it\.")
    check("wake: a turn starts by itself when bash() ends",
          term.expect(r"The background command finished\.", timeout=60), term.screen[-600:])
    mark = term.mark()
    term.expect(r"> ", since=mark - 5)
    term.type("/exit\n")
    code = term.wait()
    check("wake: exits 0", code == 0, f"exit {code}")


def case_quit():
    work = fresh("quit")
    term = Term("quit", [], work, {"responses": ["spare"]})
    term.expect(r"> ")
    time.sleep(0.5)
    term.ctrl_c()
    check("quit: the first Ctrl+C asks for a second", term.expect(r"Ctrl\+C again to quit"), term.screen[-300:])
    term.ctrl_c()
    code = term.wait(30)
    check("quit: the second one quits with 0", code == 0, f"exit {code}")


def case_first_run():
    work = fresh("first-run")
    term = Term("first-run", [], work)  # no faux script, no credentials
    check("first-run: says no provider is set up", term.expect(r"No model provider is set up"),
          term.screen[-400:])
    check("first-run: lists subscriptions", term.expect(r"Subscriptions \(sign in with your browser\)"))
    check("first-run: lists Claude Pro/Max", term.expect(r"Anthropic \(Claude Pro/Max\)"))
    check("first-run: lists API keys", term.expect(r"API keys"))
    term.expect(r"Number \(1-\d+, Enter to cancel\): $")
    term.type("abc\n")
    check("first-run: a wrong answer asks again", term.expect(r"Type a number from 1 to \d+"))
    term.type("\n")
    code = term.wait()
    check("first-run: Enter cancels with exit 1", code == 1, f"exit {code}")


def case_api_key():
    work = fresh("api-key")
    agent = work / "agent"
    login = Term("api-key", ["login", "openai"], work, agent_dir=agent)
    check("api-key: asks for the key", login.expect(r"API key: $"), login.screen[-300:])
    login.type("sk-e2e-test-key\n")
    code = login.wait()
    check("api-key: login exits 0", code == 0, f"exit {code}")
    check("api-key: the key did not show", "sk-e2e-test-key" not in login.screen, login.screen[-300:])
    auth = json.loads((agent / "auth.json").read_text()) if (agent / "auth.json").exists() else {}
    check("api-key: auth.json holds the key", json.dumps(auth).find("sk-e2e-test-key") >= 0, auth)
    logout = Term("api-key", ["logout", "openai"], work, agent_dir=agent)
    code = logout.wait()
    check("api-key: logout exits 0", code == 0, f"exit {code}")
    auth = json.loads((agent / "auth.json").read_text()) if (agent / "auth.json").exists() else {}
    check("api-key: the key is gone", "sk-e2e-test-key" not in json.dumps(auth), auth)
    bad = Term("api-key", ["login", "no-such-provider"], work, agent_dir=agent)
    code = bad.wait()
    check("api-key: an unknown provider fails", code == 1 and "no provider no-such-provider" in bad.screen,
          bad.screen[-300:])


def case_print():
    work = fresh("print")
    term = Term("print", ["-p", "say hi"], work, {"responses": ["hi there"]})
    code = term.wait()
    check("print: -p answers and exits 0", code == 0 and "hi there" in term.screen, term.screen[-300:])


def case_help():
    work = fresh("help")
    term = Term("help", ["--help"], work)
    code = term.wait()
    check("help: lists login", code == 0 and re.search(r"\n  login\s+Sign in", term.screen) is not None,
          term.screen[-800:])


for case in CASES:
    print(f"== {case}", flush=True)
    globals()["case_" + case.replace("-", "_")]()

print(f"\n{len(FAILED)} failed" if FAILED else "\nall passed")
sys.exit(1 if FAILED else 0)
