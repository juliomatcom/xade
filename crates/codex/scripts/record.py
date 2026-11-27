#!/usr/bin/env python3
"""Record Codex (`codex app-server`) sessions as redacted NDJSON fixtures.

Usage: record.py <scenario|all> <sandbox-dir> [--codex PATH] [--model gpt-6-luna]
Writes crates/codex/fixtures/<scenario>.ndjson. Each line is
{"dir": "in"|"out"|"meta", "frame": {...}}: "in" is what Xade sends, "out" what codex emits, "meta" is
recorder bookkeeping (argv, a `restart` marker between two processes, exit code, stderr tail).
Home paths, the user name, emails, user/account ids are redacted. Every thread the run created is
deleted afterwards with `thread/delete`. Sandboxes under $HOME are refused. Runs cost API tokens.
"""
import argparse, json, os, re, select, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.join(os.path.dirname(HERE), "fixtures")
HOME = os.path.expanduser("~")
USER = os.path.basename(HOME)
EMAIL = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")
SECRET_KEYS = {"userId", "accountId", "email", "installationId", "serverName", "workspaceName"}
TIMEOUT = 180
DROP = {"remoteControl/status/changed", "skills/changed", "mcpServer/startupStatus/updated", "app/list/updated"}


def redact(s):
    s = s.replace(HOME, "/HOME").replace(USER, "USER")
    return EMAIL.sub("<redacted-email>", s)


def scrub(o):
    if isinstance(o, dict):
        return {k: ("<redacted>" if k in SECRET_KEYS and v is not None else scrub(v)) for k, v in o.items()}
    if isinstance(o, list):
        return [scrub(v) for v in o]
    return o


class Rec:
    def __init__(self, codex, cwd, out):
        self.codex, self.cwd, self.out, self.next_id = codex, cwd, out, 1
        self.threads = set()
        self.proc = None

    def log(self, d, frame):
        self.out.write(redact(json.dumps({"dir": d, "frame": scrub(frame)})) + "\n")
        self.out.flush()

    def start(self):
        argv = [self.codex, "app-server"]
        self.proc = subprocess.Popen(argv, cwd=self.cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE)
        self.buf = b""
        self.log("meta", {"argv": argv[1:]})
        self.req("initialize", {"clientInfo": {"name": "xade", "title": "Xade", "version": "0.0.0"}})
        self.send({"jsonrpc": "2.0", "method": "initialized"})

    def send(self, frame):
        self.log("in", frame)
        self.proc.stdin.write((json.dumps(frame) + "\n").encode())
        self.proc.stdin.flush()

    def read(self, timeout=TIMEOUT):
        fd = self.proc.stdout.fileno()
        deadline = time.time() + timeout
        while b"\n" not in self.buf:
            r, _, _ = select.select([fd], [], [], max(0, deadline - time.time()))
            if not r:
                raise TimeoutError("no frame")
            chunk = os.read(fd, 65536)
            if not chunk:
                raise EOFError("codex closed stdout")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        if not line.strip():
            return self.read(timeout)
        frame = json.loads(line)
        if frame.get("method") in DROP:
            return self.read(timeout)
        self.log("out", frame)
        return frame

    def req(self, method, params, on=None):
        """Send a request and read until its response; `on(frame)` sees every other frame."""
        rid = self.next_id
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        while True:
            f = self.read()
            if f.get("id") == rid and "method" not in f:
                if "result" in f and isinstance(f["result"], dict) and "thread" in f["result"]:
                    self.threads.add(f["result"]["thread"]["id"])
                return f
            if on:
                on(f)

    def until(self, pred, on=None):
        while True:
            f = self.read()
            if on:
                on(f)
            if pred(f):
                return f

    def reply(self, rid, result):
        self.send({"jsonrpc": "2.0", "id": rid, "result": result})

    def stop(self):
        self.proc.stdin.close()
        try:
            code = self.proc.wait(timeout=20)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            code = self.proc.wait()
        err = self.proc.stderr.read().decode(errors="replace")[-2000:]
        self.log("meta", {"exit_code": code, "stderr_tail": err})

    def cleanup(self):
        """Delete every thread this run created; waits for each response (closing stdin early skips them)."""
        if not self.threads:
            return
        r = Rec(self.codex, self.cwd, open(os.devnull, "w"))
        try:
            r.start()
            for t in sorted(self.threads):
                r.req("thread/delete", {"threadId": t})
        except Exception as e:
            print("cleanup failed:", repr(e), file=sys.stderr)
        finally:
            if r.proc and r.proc.poll() is None:
                r.proc.stdin.close()
                r.proc.wait(timeout=20)


def thread_params(r, auto=False, **kw):
    p = {"cwd": r.cwd, "model": r.model, "approvalPolicy": "never" if auto else "untrusted",
         "sandbox": "workspace-write"}
    p.update(kw)
    return p


def turn(r, tid, text, effort="low", **kw):
    p = {"threadId": tid, "input": [{"type": "text", "text": text}], "effort": effort}
    p.update(kw)
    return r.req("turn/start", p)


def is_done(f):
    return f.get("method") == "turn/completed"


def new_thread(r, **kw):
    return r.req("thread/start", thread_params(r, **kw))["result"]["thread"]["id"]


def approve(decision):
    def on(r, f):
        if "id" in f and f.get("method", "").endswith("requestApproval"):
            r.reply(f["id"], {"decision": decision})
    return on


# ---- scenarios ----

def sc_simple_turn(r):
    r.start()
    tid = new_thread(r)
    turn(r, tid, "Reply with exactly: pong")
    r.until(is_done)
    r.stop()


def sc_two_turns(r):
    r.start()
    tid = new_thread(r)
    turn(r, tid, "Remember the word 'zebra'. Reply with exactly: ok")
    r.until(is_done)
    turn(r, tid, "Which word did I ask you to remember? One word.")
    r.until(is_done)
    r.stop()


def sc_thinking(r):
    r.start()
    tid = new_thread(r)
    turn(r, tid, "What is 17*23? Think briefly, then answer with the number only.", effort="medium",
         summary="concise")
    r.until(is_done)
    r.stop()


def _tool(r, decision, prompt):
    r.start()
    tid = new_thread(r)
    turn(r, tid, prompt)
    h = approve(decision)
    r.until(is_done, on=lambda f: h(r, f))
    r.stop()


def sc_tool_allow(r):
    _tool(r, "accept", "Run the shell command `touch made_by_shell.txt` now. Do not ask me anything.")


def sc_tool_deny(r):
    _tool(r, "decline", "Run the shell command `touch made_by_shell.txt` now. Do not ask me anything.")


def sc_edit(r):
    _tool(r, "accept", "Create a file named hello.txt containing the single line `hi`, using your patch/edit tool.")


def sc_edit_update(r):
    with open(os.path.join(r.cwd, "notes.txt"), "w") as f:
        f.write("alpha\nbeta\ngamma\n")
    _tool(r, "accept", "In notes.txt replace the line `beta` with `BETA`, using your patch/edit tool.")


def sc_auto(r):
    r.start()
    tid = new_thread(r, auto=True)
    turn(r, tid, "Run the shell command `touch auto_made.txt` now, then reply done.")
    r.until(is_done, on=lambda f: approve("accept")(r, f))
    r.stop()


def sc_interrupt(r):
    r.start()
    tid = new_thread(r)
    res = turn(r, tid, "Write a 600-word essay about the history of bridges.")
    turn_id = res["result"]["turn"]["id"]
    r.until(lambda f: f.get("method") == "item/agentMessage/delta")
    r.req("turn/interrupt", {"threadId": tid, "turnId": turn_id})
    r.until(is_done)
    r.stop()


def sc_interrupt_pending_permission(r):
    r.start()
    tid = new_thread(r)
    res = turn(r, tid, "Run the shell command `touch pending.txt` now. Do not ask me anything.")
    turn_id = res["result"]["turn"]["id"]
    ap = r.until(lambda f: "id" in f and f.get("method", "").endswith("requestApproval"))
    r.req("turn/interrupt", {"threadId": tid, "turnId": turn_id})
    r.until(is_done)
    r.stop()


def sc_steer(r):
    r.start()
    tid = new_thread(r)
    res = turn(r, tid, "Count slowly from 1 to 40, one number per line.")
    turn_id = res["result"]["turn"]["id"]
    r.until(lambda f: f.get("method") == "item/agentMessage/delta")
    r.req("turn/steer", {"threadId": tid, "expectedTurnId": turn_id,
                         "input": [{"type": "text", "text": "Stop counting and just reply: steered"}]})
    r.until(is_done)
    r.stop()


def sc_resume(r):
    r.start()
    tid = new_thread(r)
    turn(r, tid, "Remember the word 'walrus'. Reply with exactly: ok")
    r.until(is_done)
    r.stop()
    r.log("meta", {"restart": True})
    r.start()
    r.req("thread/resume", {"threadId": tid, "cwd": r.cwd, "model": r.model, "approvalPolicy": "untrusted",
                            "sandbox": "workspace-write", "excludeTurns": True})
    turn(r, tid, "Which word did I ask you to remember? One word.")
    r.until(is_done)
    r.stop()


def sc_bad_resume(r):
    r.start()
    r.req("thread/resume", {"threadId": "00000000-0000-0000-0000-000000000000", "cwd": r.cwd})
    r.stop()


def sc_bad_model(r):
    r.start()
    tid = new_thread(r)
    turn(r, tid, "Reply with exactly: pong", model="no-such-model")
    r.until(lambda f: is_done(f) or f.get("method") == "error")
    r.stop()


def sc_account(r):
    r.start()
    r.req("account/read", {})
    r.req("account/rateLimits/read", {})
    r.req("model/list", {})
    r.stop()


SCENARIOS = {k[3:]: v for k, v in globals().items() if k.startswith("sc_")}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("scenario")
    ap.add_argument("sandbox")
    ap.add_argument("--codex", default="codex")
    ap.add_argument("--model", default="gpt-6-luna")
    a = ap.parse_args()
    sandbox = os.path.realpath(a.sandbox)
    if sandbox.startswith(HOME + os.sep):
        sys.exit("refusing a sandbox under $HOME")
    names = list(SCENARIOS) if a.scenario == "all" else [a.scenario]
    os.makedirs(FIXTURES, exist_ok=True)
    for name in names:
        cwd = os.path.join(sandbox, name)
        os.makedirs(cwd, exist_ok=True)
        with open(os.path.join(FIXTURES, name + ".ndjson"), "w") as out:
            r = Rec(a.codex, cwd, out)
            r.model = a.model
            out.write(json.dumps({"dir": "meta", "frame": {"scenario": name, "model": a.model}}) + "\n")
            try:
                SCENARIOS[name](r)
            except Exception as e:  # keep what was recorded
                r.log("meta", {"recorder_error": repr(e)})
                if r.proc and r.proc.poll() is None:
                    r.proc.kill()
            finally:
                r.cleanup()
        print(name, "done", file=sys.stderr)


if __name__ == "__main__":
    main()
