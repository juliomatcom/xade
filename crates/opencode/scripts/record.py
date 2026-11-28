#!/usr/bin/env python3
"""Record OpenCode (`opencode acp`) sessions as redacted NDJSON fixtures.

Usage: record.py <scenario> <sandbox-dir> [--opencode PATH]
Writes crates/opencode/fixtures/<scenario>.ndjson. Each line is
{"dir": "in"|"out"|"meta", "frame": {...}}: "in" is what Xade sends, "out" what OpenCode emits,
"meta" is recorder bookkeeping (argv, a `restart` marker between two processes, exit code, stderr
tail). The run uses an empty `XDG_CONFIG_HOME` (no user config, plugins or skills; the login stays in
the data folder). Permission scenarios set the same `OPENCODE_CONFIG_CONTENT` Xade sets with Auto
OFF. Home paths, the user name and emails are redacted, `available_commands_update` frames are
dropped, and the model option list is trimmed. Sandboxes under $HOME are refused.
Every session the run created (subagent sessions included) is deleted afterwards with
`opencode session delete --standalone`. Only the recorder's own child process is ever signalled.
"""
import argparse, json, os, re, select, shutil, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.join(os.path.dirname(HERE), "fixtures")
HOME = os.path.expanduser("~")
USER = os.path.basename(HOME)
EMAIL = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")
SESSION_ID = re.compile(r"^ses_[A-Za-z0-9]+$")
MODEL = "opencode-go/glm-5.3-flash"
PATCH_MODEL = "opencode-go/gpt-6-luna"
KEPT_MODELS = {MODEL, PATCH_MODEL, "opencode-go/claude-haiku-5-5", "opencode/big-pickle"}
TIMEOUT = 180

INIT_PARAMS = {"protocolVersion": 1,
               "clientCapabilities": {"fs": {"readTextFile": False, "writeTextFile": False}, "terminal": False},
               "clientInfo": {"name": "xade", "version": "0.6.0"}}

# Mirrors the Auto OFF environment built by the adapter (src/process.rs).
ASK_ACTIONS = ["shell", "edit", "write", "patch", "webfetch", "websearch", "execute", "generate_image"]
ASK_AGENTS = ["build", "explore", "general"]


def auto_off_config() -> str:
    rules = [{"action": a, "resource": "*", "effect": "ask"} for a in ASK_ACTIONS]
    return json.dumps({"permissions": rules, "agents": {n: {"permissions": rules} for n in ASK_AGENTS}})


def redact(s: str) -> str:
    s = s.replace(HOME, "/redacted-home").replace(USER, "USER")
    return EMAIL.sub("<redacted-email>", s)


def scrub(obj):
    """Trim the model option list (long, and it depends on the account) wherever it appears."""
    if isinstance(obj, dict):
        out = {k: scrub(v) for k, v in obj.items()}
        if out.get("id") == "model" and isinstance(out.get("options"), list):
            keep = KEPT_MODELS | {out.get("currentValue")}
            out["options"] = [{"value": o.get("value"), "name": o.get("value")}
                              for o in out["options"] if o.get("value") in keep]
        return out
    if isinstance(obj, list):
        return [scrub(v) for v in obj]
    return obj


def update_of(frame):
    if frame.get("method") != "session/update":
        return {}
    return frame.get("params", {}).get("update", {})


def dropped(frame) -> bool:
    return update_of(frame).get("sessionUpdate") == "available_commands_update"


def child_session(frame):
    """The subagent session behind an update or a permission request, if any."""
    source = frame.get("params", {}).get("toolCall") or update_of(frame)
    return ((source.get("_meta") or {}).get("opencode/child-session") or {}).get("id")


class Rec:
    def __init__(self, binary, cwd, out, env):
        self.binary, self.cwd, self.env, self.out = binary, cwd, env, out
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
        self.p = subprocess.Popen([self.binary, "acp"], cwd=self.cwd, env=self.env, stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0,
                                  start_new_session=True)
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

    def handle(self, f, hook):
        if not dropped(f):
            self.log("out", f)
        child = child_session(f)
        if child and child not in self.created:
            self.created.append(child)
        if "method" in f and "id" in f:
            if f["method"] == "session/request_permission":
                if hook:
                    hook(f)
                if self.policy != "manual":
                    self.answer_permission(f)
            else:
                self.respond(f["id"], error={"code": -32601, "message": "Method not found"})
            return
        if hook:
            hook(f)
        if "id" in f and "method" not in f:
            self.responses[f["id"]] = f

    def pump(self, until, hook=None, timeout=TIMEOUT):
        """Read frames until responses for every id in `until` arrived. `hook(frame)` may react."""
        for f in self.lines(timeout):
            self.handle(f, hook)
            if all(i in self.responses for i in until):
                return
        if not all(i in self.responses for i in until):
            raise TimeoutError(f"no response for {sorted(set(until) - set(self.responses))}")

    def listen(self, seconds, hook=None):
        """Keep reading (and answering) frames for a while, with nothing of ours outstanding."""
        for f in self.lines(seconds):
            self.handle(f, hook)

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
        r = self.call("session/resume", {"sessionId": sid, "cwd": self.cwd, "mcpServers": []}, timeout=60)
        if "result" in r:
            self.session_id = sid
        return r

    def config(self, config_id, value):
        return self.call("session/set_config_option",
                         {"sessionId": self.session_id, "configId": config_id, "value": value}, timeout=60)

    def prompt_params(self, text):
        return {"sessionId": self.session_id, "prompt": [{"type": "text", "text": text}]}

    def prompt_req(self, text):
        return self.request("session/prompt", self.prompt_params(text))

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
    r.config("effort", "high")
    r.prompt("Think step by step about whether 221 is prime, then answer yes or no.")


def s_tool_allow(r):
    start(r)
    r.prompt("Use the shell tool to run `echo hello-fixture`, then reply DONE.", "allow")


def s_tool_deny(r):
    start(r)
    r.prompt("Use the shell tool to run `touch denied.txt`. If it is denied, reply DENIED.", "deny")


def s_edit(r):
    start(r)
    r.prompt("Do exactly these steps in order, without the shell: 1) use the write tool to create notes.txt "
             "containing the single word alpha. 2) use the read tool on notes.txt. 3) use the write tool to "
             "overwrite notes.txt with the single word beta. 4) use the edit tool to replace beta with gamma "
             "in notes.txt. Then reply DONE.", "allow")


def s_edit_auto(r):
    start(r)
    r.prompt("Do exactly these steps in order, without the shell: 1) use the write tool to create notes.txt "
             "containing the single word alpha. 2) use the write tool to overwrite notes.txt with the single "
             "word beta. 3) use the edit tool to replace beta with gamma in notes.txt. Then reply DONE.", "allow")


def s_edit_deny(r):
    start(r)
    r.prompt("Use the write tool (not the shell) to create denied.txt containing the word no. "
             "If it is denied, reply DENIED.", "deny")


def s_patch(r):
    start(r, PATCH_MODEL)
    r.prompt("Use only your patch tool, never the shell. Make four separate patch calls, in order: "
             "1) add solo.txt containing the line one. 2) in one patch add a.txt containing the line hello "
             "and add b.txt containing the line bye. 3) in one patch change hello to world in a.txt and "
             "delete b.txt. 4) in one patch rename a.txt to c.txt and change world to earth. Then reply DONE.",
             "allow")


def s_interrupt(r):
    start(r)
    sent = []

    def hook(f):
        if not sent and update_of(f).get("sessionUpdate") == "agent_message_chunk":
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

    r.prompt("Use the shell tool to run `sleep 1`, then reply DONE.", hook=hook)
    r.prompt("Reply with exactly: still alive")


def s_steer(r):
    start(r)
    r.policy = "allow"
    first = r.prompt_req("Use the shell tool to run `sleep 4`, then write one sentence about a lighthouse.")
    ids = [first]
    sent = []

    def hook(f):
        u = update_of(f)
        if not sent and u.get("status") == "in_progress" and "command" in (u.get("rawInput") or {}):
            sent.append(1)
            ids.append(r.request("_session/steering", r.prompt_params("Also end your answer with the word BANANA.")))

    # `ids` grows from the hook, so pump re-checks it after every frame.
    r.pump(ids, hook=hook)
    if len(ids) != 2:
        raise RuntimeError("steer: no running tool seen, nothing was steered")


def s_steer_idle(r):
    start(r)
    r.prompt("Reply with exactly: first")
    r.call("_session/steering", r.prompt_params("Reply with exactly: second"), timeout=60)
    r.listen(15)


def s_resume(r):
    start(r)
    r.config("effort", "low")
    r.prompt("Remember the word kumquat7. Reply with just OK.")
    sid = r.session_id
    r.restart()
    r.initialize()
    r.resume(sid)
    r.prompt("What word did I ask you to remember? Reply with just the word.")


def s_set_config(r):
    start(r)
    r.config("effort", "high")
    r.config("effort", "default")


def s_bad_model(r):
    start(r, model=None)
    r.config("model", "opencode-go/no-such-model")
    r.config("model", MODEL)


def s_bad_resume(r):
    r.initialize()
    r.resume("ses_000000000000000000000000000")


def s_unknown_method(r):
    r.initialize()
    r.call("xade/no_such_method", {}, timeout=30)


def s_subagent(r):
    start(r)
    r.prompt("Use the subagent tool (agent 'general') to run the shell command `echo childmark`. Wait for "
             "the subagent to finish, then reply with its output. Do not run the command yourself.", "allow")
    r.listen(10)


def s_background_subagent(r):
    start(r)
    r.prompt("Start a subagent (agent 'general') in the background whose task is to run the shell command "
             "`sleep 8; echo bgmark`. Do NOT wait for it: immediately reply with the single word started "
             "and end your turn.", "allow")
    r.listen(45)


SCENARIOS = {n[2:]: f for n, f in sorted(globals().items()) if n.startswith("s_")}
# Recorded with the Auto OFF environment, so the permission requests are the production ones.
AUTO_OFF = {"tool_allow", "tool_deny", "edit", "edit_deny", "patch", "interrupt_pending_permission",
            "subagent", "background_subagent"}


def delete_session(binary, sid, cwd, env):
    if not SESSION_ID.match(sid):
        return None
    p = subprocess.run([binary, "session", "delete", "--standalone", sid], cwd=cwd, env=env,
                       capture_output=True, text=True, timeout=60)
    if p.returncode == 0 or "session not found" in (p.stdout + p.stderr).lower():
        return None
    return sid


def listed_sessions(binary, cwd, env):
    p = subprocess.run([binary, "session", "list", "--standalone", "--format", "json"], cwd=cwd, env=env,
                       capture_output=True, text=True, timeout=60)
    if p.returncode != 0:
        raise RuntimeError(f"session list failed: {redact(p.stderr[-500:])}")
    return [s.get("id") for s in json.loads(p.stdout or "[]")]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("scenario", choices=sorted(SCENARIOS))
    ap.add_argument("cwd")
    ap.add_argument("--opencode", default="opencode")
    a = ap.parse_args()
    cwd = os.path.realpath(a.cwd)
    if cwd == HOME or cwd.startswith(HOME + os.sep):
        sys.exit("refusing to record under $HOME: the user name would leak; use `mktemp -d`")
    config_home = tempfile.mkdtemp(prefix="oc-record-config-")
    env = dict(os.environ)
    env.pop("OPENCODE_CONFIG_CONTENT", None)
    env.update({"XDG_CONFIG_HOME": config_home, "OPENCODE_DISABLE_AUTOUPDATE": "1",
                "OPENCODE_DISABLE_CLAUDE_CODE": "1"})
    if a.scenario in AUTO_OFF:
        env["OPENCODE_CONFIG_CONTENT"] = auto_off_config()
    os.makedirs(FIXTURES, exist_ok=True)
    out_path = os.path.join(FIXTURES, a.scenario + ".ndjson")
    out = open(out_path, "w")
    rec = Rec(a.opencode, cwd, out, env)
    rec.log("meta", {"argv": ["acp"], "scenario": a.scenario, "auto": a.scenario not in AUTO_OFF})
    code, failed, leftover = None, [], []
    try:
        SCENARIOS[a.scenario](rec)
        code = rec.close()
    finally:
        if rec.p.poll() is None:
            rec.p.kill()
        out.close()
        try:
            failed = [s for s in (delete_session(a.opencode, s, cwd, env) for s in rec.created) if s]
            leftover = listed_sessions(a.opencode, cwd, env)
        finally:
            shutil.rmtree(config_home, ignore_errors=True)
    text = open(out_path).read()
    if USER.lower() in text.replace("USER", "").lower() or "/home/" in text.lower():
        os.remove(out_path)
        sys.exit("fixture removed: user name or home path found; record in `mktemp -d`")
    print(json.dumps({"scenario": a.scenario, "exit_code": code, "file": out_path,
                      "deleted": len(rec.created) - len(failed), "failed": failed, "leftover": leftover}))
    if failed or leftover:
        sys.exit(f"sessions left behind: {sorted(set(failed + leftover))}")


if __name__ == "__main__":
    sys.exit(main())
