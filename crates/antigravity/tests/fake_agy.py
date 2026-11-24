#!/usr/bin/env python3
"""Fake `agy acp` for tests/fake.rs. Never shipped.

Run through a symlink named `agy` in a per-test directory holding a `fixture` file (the path of the
NDJSON to replay); the client's lines are appended to `in.log` there. The script is never written
at test time, so exec cannot hit "text file busy".

Walks a fixture in order. An `in` entry blocks until the client sends a line (appended to LOG_FILE);
an `out` entry is written to stdout, and a response's `id` is rewritten to the id the client really
used for the matching `in` request. EOF on stdin ends the process with 0, as the real agy does.
`meta` entries: {"sleep": s}, {"stderr": text}, {"exit": code}, {"hang": true} (ignore stdin EOF).
Anything else in `meta` (argv, restart, exit_code) is ignored.
"""
import json
import os
import sys
import time

here = os.path.dirname(sys.argv[0])
log = os.path.join(here, "in.log")
fixture = open(os.path.join(here, "fixture")).read().strip()
idmap = {}


def read():
    line = sys.stdin.readline()
    if not line:
        return None
    with open(log, "a") as f:
        f.write(line)
    return json.loads(line)


for raw in open(fixture):
    if not raw.strip():
        continue
    entry = json.loads(raw)
    d, frame = entry["dir"], entry["frame"]
    if d == "meta":
        if "sleep" in frame:
            time.sleep(frame["sleep"])
        if "stderr" in frame:
            sys.stderr.write(frame["stderr"] + "\n")
            sys.stderr.flush()
        if "exit" in frame:
            sys.exit(frame["exit"])
        if frame.get("hang"):
            while True:
                time.sleep(1)
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

while read() is not None:
    pass
