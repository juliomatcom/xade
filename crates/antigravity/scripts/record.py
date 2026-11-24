#!/usr/bin/env python3
"""Record Antigravity (`agy acp`) sessions as redacted NDJSON fixtures.

Usage: record.py <scenario> <sandbox-dir> [--agy PATH]
       record.py --rescrub <fixtures-dir>   (re-apply redaction to existing fixtures)
Writes crates/antigravity/fixtures/<scenario>.ndjson. Each line is
{"dir": "in"|"out"|"meta", "frame": {...}}: "in" is what Xade sends, "out" what agy emits, "meta" is
recorder bookkeeping (argv, a `restart` marker between two processes, exit code, stderr tail).
Home paths, the user name, emails and the plan tier are redacted; `available_commands_update` frames are dropped
(huge, and they list the user's private skills). Sandboxes under $HOME are refused.
Every conversation the run created is deleted afterwards (`conversations/<id>.db*`, `brain/<id>`,
`annotations/<id>.pbtxt`, its rows in `conversation_summaries.db`); nothing else under ~/.gemini is touched.
"""
import argparse, json, os, re, select, shutil, sqlite3, subprocess, sys, time, uuid

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.join(os.path.dirname(HERE), "fixtures")
HOME = os.path.expanduser("~")
USER = os.path.basename(HOME)
EMAIL = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")
UUID = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
AGY_DIR = os.path.join(HOME, ".gemini", "antigravity-cli")
MODEL = "gemini-3.8-flash"
TIMEOUT = 180

INIT_PARAMS = {"protocolVersion": 1,
               "clientCapabilities": {"fs": {"readTextFile": False, "writeTextFile": False}, "terminal": False}}


def redact(s: str) -> str:
    s = s.replace(HOME, "/HOME").replace(USER, "USER")
    return EMAIL.sub("<redacted-email>", s)


def scrub(obj):
    """Drop the account's plan tier (`_meta.antigravity.tier`) anywhere in a frame."""
    if isinstance(obj, dict):
        return {k: ("<redacted>" if k == "tier" else scrub(v)) for k, v in obj.items()}
    if isinstance(obj, list):
        return [scrub(v) for v in obj]
    return obj


def rescrub(fixtures_dir):
    """Re-apply scrub/redact to existing fixtures in place."""
    for name in sorted(os.listdir(fixtures_dir)):
        if name.endswith(".ndjson"):
            path = os.path.join(fixtures_dir, name)
            lines = []
            for line in open(path):
                d = json.loads(line)
                d["frame"] = scrub(d["frame"])
                lines.append(redact(json.dumps(d)))
            with open(path, "w") as f:
                f.write("\n".join(lines) + "\n")


def dropped(frame) -> bool:
    return (frame.get("method") == "session/update"
            and frame.get("params", {}).get("update", {}).get("sessionUpdate") == "available_commands_update")


def delete_conversation(sid: str):
    """Delete one conversation this recorder created. Refuses anything but a plain UUID.

    Besides `conversations/<id>.db*` and `brain/<id>`, agy leaves `annotations/<id>.pbtxt` and rows
    for the id in `conversation_summaries.db` (which the TUI lists); only those are removed.
    """
    if not UUID.match(sid):
        return
    for suffix in ("", "-wal", "-shm"):
        try:
            os.remove(os.path.join(AGY_DIR, "conversations", sid + ".db" + suffix))
        except FileNotFoundError:
            pass
    try:
        os.remove(os.path.join(AGY_DIR, "annotations", sid + ".pbtxt"))
    except FileNotFoundError:
        pass
    shutil.rmtree(os.path.join(AGY_DIR, "brain", sid), ignore_errors=True)
    db = os.path.join(AGY_DIR, "conversation_summaries.db")
    if os.path.exists(db):
        try:
            con = sqlite3.connect(db, timeout=10)
            with con:
                for table, col in (("conversation_summaries", "conversation_id"),
                                   ("conversations_fts", "conversation_id"),
                                   ("conversation_parents", "conversation_id")):
                    con.execute(f"DELETE FROM {table} WHERE {col} = ?", (sid,))
            con.close()
        except sqlite3.Error as e:
            print(f"warning: could not remove {sid} from conversation_summaries.db: {e}", file=sys.stderr)


class Rec:
    def __init__(self, agy, cwd, out, env=None):
        self.agy, self.cwd, self.env, self.out = agy, cwd, env, out
        self.next_id = 1
        self.responses = {}
        self.created = []       # session ids to delete at the end
        self.session_id = ""
        self.buf = b""
        self.p = None
        self.policy = "allow"
        self.start()

    def log(self, direction, frame):
        self.out.write(redact(json.dumps({"dir": direction, "frame": scrub(frame)})) + "\n")
        self.out.flush()

    def start(self):
        self.p = subprocess.Popen([self.agy, "acp"], cwd=self.cwd, env=self.env, stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
        self.buf = b""
        self.responses = {}

    def write(self, frame):
        self.log("in", frame)
        self.p.stdin.write((json.dumps(frame) + "\n").encode())
        self.p.stdin.flush()

    def request(self, method, params):
        rid = self.next_id
        self.next_id += 1
        self.write({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        return rid

    def notify(self, method, params):
        self.write({"jsonrpc": "2.0", "method": method, "params": params})

    def respond(self, rid, result=None, error=None):
        f = {"jsonrpc": "2.0", "id": rid}
        f["result" if error is None else "error"] = result if error is None else error
        self.write(f)

    def lines(self, timeout):
        deadline = time.time() + timeout
        while time.time() < deadline:
            while b"\n" in self.buf:
                line, self.buf = self.buf.split(b"\n", 1)
                if line.strip():
                    try:
                        yield json.loads(line)
                    except json.JSONDecodeError:
                        yield {"_unparsed": line.decode(errors="replace")}
            r, _, _ = select.select([self.p.stdout], [], [], 0.5)
            if r:
                chunk = os.read(self.p.stdout.fileno(), 1 << 20)
                if not chunk:
                    return
                self.buf += chunk
            elif self.p.poll() is not None and not self.buf:
                return

    def answer_permission(self, f):
        opts = f.get("params", {}).get("options", [])
        pick = {"allow": "allow_once", "deny": "reject_once"}.get(self.policy)
        opt = next((o for o in opts if o.get("kind") == pick), None)
        if opt is None:
            outcome = {"outcome": "cancelled"}
        else:
            outcome = {"outcome": "selected", "optionId": opt["optionId"]}
        self.respond(f["id"], {"outcome": outcome})

    def pump(self, until, hook=None, timeout=TIMEOUT):
        """Read frames until responses for every id in `until` arrived. `hook(frame)` may react."""
        for f in self.lines(timeout):
            if not dropped(f):
                self.log("out", f)
            if "method" in f and "id" in f:
                if f["method"] == "session/request_permission":
                    if hook:
                        hook(f)
                    if self.policy != "manual":
                        self.answer_permission(f)
                else:
                    self.respond(f["id"], error={"code": -32601, "message": "Method not found"})
                continue
            if hook:
                hook(f)
            if "id" in f and "method" not in f:
                self.responses[f["id"]] = f
            if all(i in self.responses for i in until):
                return
        if not all(i in self.responses for i in until):
            raise TimeoutError(f"no response for {sorted(set(until) - set(self.responses))}")

    def call(self, method, params, **kw):
        rid = self.request(method, params)
        self.pump([rid], **kw)
        return self.responses[rid]

    # High-level steps.
    def initialize(self):
        return self.call("initialize", INIT_PARAMS, timeout=60)

    def new_session(self):
        r = self.call("session/new", {"cwd": self.cwd, "mcpServers": []}, timeout=60)
        sid = r.get("result", {}).get("sessionId")
        if sid:
            self.session_id = sid
            self.created.append(sid)
        return r

    def resume(self, sid):
        self.created.append(sid)
        r = self.call("session/resume", {"sessionId": sid, "cwd": self.cwd, "mcpServers": []}, timeout=60)
        if "result" in r:
            self.session_id = sid
        return r

    def config(self, config_id, value):
        return self.call("session/set_config_option",
                         {"sessionId": self.session_id, "configId": config_id, "value": value}, timeout=60)

    def prompt_req(self, text, sid=None):
        return self.request("session/prompt", {"sessionId": sid or self.session_id,
                                               "prompt": [{"type": "text", "text": text}]})

    def prompt(self, text, policy="allow", hook=None):
        self.policy = policy
        rid = self.prompt_req(text)
        self.pump([rid], hook=hook)
        return self.responses[rid]

    def cancel(self):
        self.notify("session/cancel", {"sessionId": self.session_id})

    def restart(self):
        """Close this process and start a new one; the fixture marks the boundary."""
        self.close()
        self.log("meta", {"restart": True})
        self.start()

    def close(self):
        try:
            self.p.stdin.close()
        except OSError:
            pass
        for f in self.lines(15):
            if not dropped(f):
                self.log("out", f)
        try:
            code = self.p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.p.terminate()
            code = self.p.wait(timeout=5)
        err = self.p.stderr.read().decode(errors="replace")
        self.log("meta", {"exit_code": code, "stderr_tail": redact(err[-2000:])})
        return code


def update_kind(f):
    if f.get("method") != "session/update":
        return None
    return f.get("params", {}).get("update", {}).get("sessionUpdate")


# Scenarios. Each takes the recorder, runs its steps and may return after the last response.
def start(r, model=MODEL):
    r.initialize()
    r.new_session()
    if model:
        r.config("model", model)


def s_simple_turn(r):
    start(r)
    r.prompt("Reply with exactly: hello fixture")


def s_two_turns(r):
    start(r)
    r.prompt("Remember the number 41. Reply with just OK.")
    r.prompt("What number plus one? Reply with just the number.")


def s_thinking(r):
    start(r)
    r.config("thought_level", "high")
    r.prompt("Think step by step about whether 221 is prime, then answer yes or no.")


def s_tool_allow(r):
    start(r)
    r.prompt("Use the run_command tool to run `echo hello-fixture`, then reply DONE.", "allow")


def s_tool_deny(r):
    start(r)
    r.prompt("Use the run_command tool to run `touch denied.txt`. If it is denied, reply DENIED.", "deny")


def s_edit(r):
    with open(os.path.join(r.cwd, "readme.txt"), "w") as f:
        f.write("a sample file\n")
    start(r)
    r.prompt("Use the edit_file tool to change the word 'sample' to 'example' in readme.txt. Then reply DONE.",
             "allow")


def s_interrupt(r):
    start(r)
    sent = []

    def hook(f):
        if not sent and update_kind(f) == "agent_message_chunk":
            sent.append(1)
            r.cancel()

    r.prompt("Write a 600-word story about a lighthouse keeper.", hook=hook)
    r.prompt("Reply with exactly: still alive")


def s_interrupt_pending_permission(r):
    start(r)
    sent = []

    def hook(f):
        if f.get("method") == "session/request_permission" and not sent:
            sent.append(1)
            r.policy = "manual"
            r.respond(f["id"], {"outcome": {"outcome": "cancelled"}})
            r.cancel()

    r.prompt("Use the run_command tool to run `sleep 1`, then reply DONE.", hook=hook)


def s_steer(r):
    start(r)
    r.policy = "allow"
    first = r.prompt_req("Use the run_command tool to run `sleep 3`, then write one sentence about a lighthouse.")
    ids = [first]
    sent = []

    def hook(f):
        if not sent and update_kind(f) == "tool_call":
            sent.append(1)
            ids.append(r.prompt_req("Also, end your answer with the word BANANA."))

    # `ids` grows from the hook, so pump re-checks it after every frame.
    r.pump(ids, hook=hook)
    if len(ids) != 2:
        raise RuntimeError("steer: no tool call seen, nothing was steered")


def s_resume(r):
    start(r)
    r.prompt("Remember the word kumquat7. Reply with just OK.")
    sid = r.session_id
    r.restart()
    r.initialize()
    r.resume(sid)
    r.config("model", MODEL)
    r.prompt("What word did I ask you to remember? Reply with just the word.")


def s_set_config(r):
    start(r)
    r.config("thought_level", "low")


def s_bad_model(r):
    start(r, model=None)
    r.config("model", "no-such-model")
    r.config("model", MODEL)


def s_bad_resume(r):
    r.initialize()
    r.resume(str(uuid.uuid4()))


def s_bad_prompt_session(r):
    start(r, model=None)
    rid = r.prompt_req("hello", sid=str(uuid.uuid4()))
    r.pump([rid])


def s_unknown_method(r):
    r.initialize()
    r.call("xade/no_such_method", {}, timeout=30)


SCENARIOS = {n[2:]: f for n, f in sorted(globals().items()) if n.startswith("s_")}


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "--rescrub":
        return rescrub(sys.argv[2])
    ap = argparse.ArgumentParser()
    ap.add_argument("scenario", choices=sorted(SCENARIOS))
    ap.add_argument("cwd")
    ap.add_argument("--agy", default="agy")
    a = ap.parse_args()
    cwd = os.path.realpath(a.cwd)
    if cwd == HOME or cwd.startswith(HOME + os.sep):
        sys.exit("refusing to record under $HOME: the user name would leak; use `mktemp -d`")
    os.makedirs(FIXTURES, exist_ok=True)
    out_path = os.path.join(FIXTURES, a.scenario + ".ndjson")
    before = set(os.listdir(os.path.join(AGY_DIR, "conversations"))) if os.path.isdir(AGY_DIR) else set()
    out = open(out_path, "w")
    rec = Rec(a.agy, cwd, out)
    rec.log("meta", {"argv": ["acp"], "scenario": a.scenario})
    code = None
    try:
        SCENARIOS[a.scenario](rec)
        code = rec.close()
    finally:
        if rec.p.poll() is None:
            rec.p.kill()
        out.close()
        for sid in set(rec.created):
            delete_conversation(sid)
    after = set(os.listdir(os.path.join(AGY_DIR, "conversations"))) if os.path.isdir(AGY_DIR) else set()
    leftover = sorted(after - before)
    text = open(out_path).read()
    if USER[:5].lower() in text.lower() or "/home/" in text:
        os.remove(out_path)
        sys.exit("fixture removed: user name or home path found; record in `mktemp -d`")
    print(json.dumps({"scenario": a.scenario, "exit_code": code, "file": out_path, "leftover": leftover}))
    if leftover:
        sys.exit(f"conversations left behind (not created by this run's ids): {leftover}")


if __name__ == "__main__":
    sys.exit(main())
