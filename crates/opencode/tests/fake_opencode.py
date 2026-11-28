#!/usr/bin/env python3
"""Fake `opencode` for tests/fake.rs. Never shipped.

Run through a symlink named `opencode` in a per-test directory holding a `fixture` file (the path
of the NDJSON to replay). The script is never written at test time, so exec cannot hit "text file
busy". Each run appends its arguments and working folder to `runs.log`.

`acp`: writes `OPENCODE_CONFIG_CONTENT` (or null) to `env.json`, then walks the fixture in order.
An `in` entry blocks until the client sends a line (appended to `in.log`); an `out` entry is written
to stdout, and a response's `id` is rewritten to the id the client really used for the matching
`in` request. EOF on stdin ends the process with 0, as the real one does.

Any other command (`models --standalone`, `session delete --standalone <id>`) only runs the
fixture's `meta` entries.

`meta` entries: {"sleep": s}, {"stdout": text}, {"stderr": text}, {"exit": code},
{"signal": name}, {"hang": true} (ignore stdin EOF), {"spawn_bg": true} (start a long `sleep` in
this process group and write its pid to `bg.pid`). Anything else in `meta` (argv, restart,
exit_code) is ignored.
"""
import json
import os
import signal
import subprocess
import sys
import time

here = os.path.dirname(sys.argv[0])
log = os.path.join(here, "in.log")
fixture = open(os.path.join(here, "fixture")).read().strip()
acp = sys.argv[1:] == ["acp"]
idmap = {}

runs_path = os.path.join(here, "runs.log")
run_index = 0
if os.path.exists(runs_path):
    with open(runs_path) as f:
        run_index = sum(1 for l in f if l.strip())

with open(runs_path, "a") as f:
    f.write(json.dumps({"argv": sys.argv[1:], "cwd": os.getcwd()}) + "\n")
if acp:
    with open(os.path.join(here, "env.json"), "w") as f:
        json.dump(os.environ.get("OPENCODE_CONFIG_CONTENT"), f)


def read():
    line = sys.stdin.readline()
    if not line:
        return None
    with open(log, "a") as f:
        f.write(line)
    return json.loads(line)


def meta(frame):
    if "sleep" in frame:
        time.sleep(frame["sleep"])
    if "stdout" in frame:
        sys.stdout.write(frame["stdout"] + "\n")
        sys.stdout.flush()
    if "stderr" in frame and "exit_code" not in frame:
        sys.stderr.write(frame["stderr"] + "\n")
        sys.stderr.flush()
    if frame.get("spawn_bg"):
        bg = subprocess.Popen(
            ["sleep", "300"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        tmp = os.path.join(here, "bg.pid.tmp")
        with open(tmp, "w") as f:
            f.write(str(bg.pid))
        os.replace(tmp, os.path.join(here, "bg.pid"))
    if "exit" in frame:
        sys.exit(frame["exit"])
    if "signal" in frame:
        os.kill(os.getpid(), getattr(signal, frame["signal"]))
    if frame.get("hang"):
        while True:
            time.sleep(1)


segments = [[]]
for raw in open(fixture):
    if not raw.strip():
        continue
    entry = json.loads(raw)
    if entry["dir"] == "meta" and entry["frame"].get("restart"):
        segments.append([])
    else:
        segments[-1].append(entry)

entries = segments[run_index] if run_index < len(segments) else segments[-1]
for entry in entries:
    d, frame = entry["dir"], entry["frame"]
    if d == "meta":
        meta(frame)
    elif not acp:
        continue
    elif d == "in":
        got = read()
        if got is None:
            sys.exit(0)
        if "method" in frame and "id" in frame and "id" in got:
            idmap[frame["id"]] = got["id"]
    else:
        if "method" not in frame and frame.get("id") in idmap:
            frame = {**frame, "id": idmap[frame["id"]]}
        sys.stdout.write(json.dumps(frame) + "\n")
        sys.stdout.flush()

if acp:
    while read() is not None:
        pass
