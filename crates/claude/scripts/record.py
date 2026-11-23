#!/usr/bin/env python3
"""Record Claude CLI stream-json sessions as redacted NDJSON fixtures.

Usage: record.py <scenario> <cwd> [--claude PATH] [--resume ID]
       record.py --rescrub <fixtures-dir>   (re-apply redaction to existing fixtures)
Writes crates/claude/fixtures/<scenario>.ndjson. Each line is {"dir": "in"|"out", "frame": {...}}
so tests can see both what we sent and what the CLI emitted. Home paths, the user name and
email addresses are redacted before writing.
"""
import argparse, json, os, re, select, shlex, subprocess, sys, time, uuid

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.join(os.path.dirname(HERE), "fixtures")
HOME = os.path.expanduser("~")
USER = os.path.basename(HOME)
EMAIL = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")

BASE_ARGS = ["-p", "--input-format", "stream-json", "--output-format", "stream-json",
             "--verbose", "--include-partial-messages",
             "--permission-prompt-tool", "stdio", "--setting-sources", "project"]

# Scenario = (extra CLI args, steps). Step kinds:
#   ("init",)                       send initialize control_request, wait for its control_response
#   ("send", text, policy)          send user message, run turn until `result`; policy in allow|deny
#   ("send_interrupt", text)        send user message, interrupt after first text delta, wait `result`
SCENARIOS = {
    "simple_turn": (["--model", "haiku"], [("send", "Reply with exactly: hello fixture", "allow")]),
    "two_turns": (["--model", "haiku"], [
        ("send", "Remember the number 41. Reply with just OK.", "allow"),
        ("send", "What number plus one? Reply with just the number.", "allow")]),
    "init_first": (["--model", "haiku"], [("init",), ("send", "Reply with exactly: init ok", "allow")]),
    "thinking": (["--model", "sonnet", "--effort", "high"], [
        ("send", "Think briefly, then answer: what is 17*23? Reply with the number only.", "allow")]),
    "tool_allow": (["--model", "haiku"], [
        ("send", "Use the Bash tool to run `ls`, then use the Write tool to create notes.txt containing 'hi'. "
                 "Then use Edit to change 'hi' to 'hello'. Then reply DONE.", "allow")]),
    "tool_deny": (["--model", "haiku"], [
        ("send", "Use the Write tool to create denied.txt containing 'x'. If it is denied, reply DENIED.", "deny")]),
    "edit_allow": (["--model", "haiku"], [
        ("send", "Use the Edit tool to change the word 'sample' to 'example' in readme.txt. Do not read it first "
                 "unless required. Then reply DONE.", "allow")]),
    "thinking_summaries": (["--model", "haiku", "--settings", '{"showThinkingSummaries": true}'], [
        ("send", "Think step by step about whether 221 is prime, then answer yes or no.", "allow")]),
    "interrupt": (["--model", "haiku"], [
        ("send_interrupt", "Write a 600-word story about a lighthouse keeper."),
        ("send", "Reply with exactly: still alive", "allow")]),
}


SENSITIVE_KEYS = {"account", "email", "orgId", "orgName", "organization", "organizationUuid",
                  "organizationName", "accountUuid", "emailAddress"}


# Local setup the recorder's machine leaks (connectors, plugins, skills, paths); fixtures are public.
SETUP_KEYS = {"mcp_servers", "plugins", "skills", "slash_commands", "terminal_slash_commands", "agents",
              "commands", "memory_paths", "scratchpad_path", "messaging_socket_path", "output_style"}


def scrub(obj):
    """Drop identity and local-setup keys anywhere in a frame; keep only built-in tool names."""
    if isinstance(obj, dict):
        out = {}
        for k, v in obj.items():
            if k in SENSITIVE_KEYS:
                out[k] = "<redacted>"
            elif k in SETUP_KEYS:
                out[k] = [] if isinstance(v, list) else "<redacted>"
            elif k == "tools" and isinstance(v, list):
                out[k] = [t for t in v if not (isinstance(t, str) and t.startswith("mcp__"))]
            else:
                out[k] = scrub(v)
        return out
    if isinstance(obj, list):
        return [scrub(v) for v in obj]
    return obj


REPO_PARENT = os.path.basename(os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(HERE)))))


def redact(s: str) -> str:
    s = s.replace(HOME, "/HOME").replace(USER, "USER")
    if REPO_PARENT and REPO_PARENT != USER:
        s = s.replace(REPO_PARENT, "work")
    return EMAIL.sub("<redacted-email>", s)


def rescrub(fixtures_dir):
    """Re-apply scrub/redact to existing fixture files in place."""
    for name in sorted(os.listdir(fixtures_dir)):
        if not name.endswith(".ndjson"):
            continue
        path = os.path.join(fixtures_dir, name)
        lines = []
        for line in open(path):
            d = json.loads(line)
            d["frame"] = scrub(d["frame"])
            lines.append(redact(json.dumps(d)))
        with open(path, "w") as f:
            f.write("\n".join(lines) + "\n")


class Rec:
    def __init__(self, cmd, cwd, out_path):
        self.p = subprocess.Popen(cmd, cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, bufsize=0)
        self.out = open(out_path, "w")
        self.buf = b""
        self.session_id = ""

    def log(self, direction, frame):
        self.out.write(redact(json.dumps({"dir": direction, "frame": scrub(frame)})) + "\n")
        self.out.flush()

    def send(self, frame):
        self.log("in", frame)
        self.p.stdin.write((json.dumps(frame) + "\n").encode())
        self.p.stdin.flush()

    def frames(self, timeout=180):
        deadline = time.time() + timeout
        while time.time() < deadline:
            while b"\n" in self.buf:
                line, self.buf = self.buf.split(b"\n", 1)
                if line.strip():
                    try:
                        f = json.loads(line)
                    except json.JSONDecodeError:
                        f = {"_unparsed": line.decode(errors="replace")}
                    self.log("out", f)
                    if f.get("type") == "system" and f.get("session_id"):
                        self.session_id = f["session_id"]
                    yield f
            r, _, _ = select.select([self.p.stdout], [], [], 1.0)
            if r:
                chunk = os.read(self.p.stdout.fileno(), 65536)
                if not chunk:
                    return
                self.buf += chunk
            elif self.p.poll() is not None:
                return
        raise TimeoutError("no result frame in time")

    def user(self, text):
        self.send({"type": "user", "session_id": self.session_id, "parent_tool_use_id": None,
                   "message": {"role": "user", "content": [{"type": "text", "text": text}]}})

    def answer(self, f, policy):
        req = f.get("request", {})
        if policy == "allow":
            resp = {"behavior": "allow", "updatedInput": req.get("input", {})}
        else:
            resp = {"behavior": "deny", "message": "Denied by user"}
        self.send({"type": "control_response",
                   "response": {"subtype": "success", "request_id": f.get("request_id"), "response": resp}})

    def run_turn(self, policy, interrupt_after_delta=False):
        interrupted = False
        for f in self.frames():
            t = f.get("type")
            if t == "control_request" and f.get("request", {}).get("subtype") == "can_use_tool":
                self.answer(f, policy)
            elif interrupt_after_delta and not interrupted and t == "stream_event" \
                    and f.get("event", {}).get("delta", {}).get("type") == "text_delta":
                self.send({"type": "control_request", "request_id": "req_" + uuid.uuid4().hex[:8],
                           "request": {"subtype": "interrupt"}})
                interrupted = True
            elif t == "result":
                return

    def init(self):
        rid = "req_" + uuid.uuid4().hex[:8]
        self.send({"type": "control_request", "request_id": rid, "request": {"subtype": "initialize"}})
        for f in self.frames(timeout=60):
            if f.get("type") == "control_response":
                return

    def close(self):
        self.p.stdin.close()
        for _ in self.frames(timeout=15):
            pass
        try:
            code = self.p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.p.terminate()
            code = self.p.wait(timeout=5)
        err = self.p.stderr.read().decode(errors="replace")
        self.log("meta", {"exit_code": code, "stderr_tail": redact(err[-2000:])})
        self.out.close()
        return code


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "--rescrub":
        return rescrub(sys.argv[2])
    ap = argparse.ArgumentParser()
    ap.add_argument("scenario")
    ap.add_argument("cwd")
    ap.add_argument("--claude", default="claude")
    ap.add_argument("--resume", default=None, help="provider session id to resume (scenario 'resume')")
    a = ap.parse_args()
    if a.scenario == "resume":
        extra, steps = ["--model", "haiku", "--resume", a.resume], [("send", "What did I ask you to reply before?", "allow")]
    else:
        extra, steps = SCENARIOS[a.scenario]
    os.makedirs(FIXTURES, exist_ok=True)
    out = os.path.join(FIXTURES, a.scenario + ".ndjson")
    rec = Rec([a.claude] + BASE_ARGS + extra, a.cwd, out)
    rec.log("meta", {"argv": BASE_ARGS + [x if x != a.resume else "<session-id>" for x in extra]})
    for step in steps:
        if step[0] == "init":
            rec.init()
        elif step[0] == "send":
            rec.user(step[1]); rec.run_turn(step[2])
        elif step[0] == "send_interrupt":
            rec.user(step[1]); rec.run_turn("allow", interrupt_after_delta=True)
    code = rec.close()
    # Streamed chunks can split the user name, defeating plain replacement: refuse to keep such a fixture.
    if USER[:5].lower() in open(out).read().lower():
        os.remove(out)
        sys.exit(f"fixture removed: user name fragment found; record in a cwd whose path has no user name (e.g. mktemp -d)")
    print(json.dumps({"scenario": a.scenario, "exit_code": code, "session_id": rec.session_id, "file": out}))


if __name__ == "__main__":
    sys.exit(main())
