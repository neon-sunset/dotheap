#!/usr/bin/env python3
"""Record analysis results and the commands needed to reproduce them with another build."""
import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path

args = sys.argv[1:]
extra = []
if "--" in args:
    extra = args[args.index("--") + 1:]
    args = args[:args.index("--")]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("binary", type=Path)
parser.add_argument("dump", type=Path)
parser.add_argument("out", type=Path)
parser.add_argument("--limit", type=int, default=200)
parser.add_argument("--cases", type=Path, help="Replay commands.json from an earlier recording")
options = parser.parse_args(args)
if options.limit < 0:
    parser.error("--limit must be zero or greater")
binary = str(options.binary.resolve(strict=True))
dump = str(options.dump.resolve(strict=True))
out = options.out
limit = str(options.limit)
os.makedirs(out, exist_ok=True)
times = {}
commands = []


def run(name, *command):
    commands.append({"name": name, "args": ["{core}" if a == dump else a for a in command]})
    started = time.monotonic()
    p = subprocess.run([binary, *command, "--json", "--limit", limit, *extra], capture_output=True, text=True,
                       env={**os.environ, "SWIFT_DETERMINISTIC_HASHING": "1"})
    times[name] = round(time.monotonic() - started, 3)
    unsupported = "dotheap: malloc analysis needs a Linux core of a process that uses glibc malloc"
    if command[0] in ("native", "chunk") and p.returncode == 1 and p.stderr.splitlines()[-1:] == [unsupported]:
        with open(os.path.join(out, name + ".json"), "w") as f:
            json.dump({"exitCode": 1, "error": unsupported}, f)
            f.write("\n")
        return None
    if p.returncode != 0:
        raise RuntimeError(f"{name}: exit {p.returncode}\n{p.stderr}")
    value = json.loads(p.stdout)
    with open(os.path.join(out, name + ".json"), "w") as f:
        f.write(p.stdout)
    return value


def first(value, *keys):
    """The first value under any of the keys, searching lists and records depth first."""
    if isinstance(value, dict):
        for k in keys:
            if k in value and value[k] not in (None, ""):
                return value[k]
        for v in value.values():
            found = first(v, *keys)
            if found is not None:
                return found
    elif isinstance(value, list):
        for v in value:
            found = first(v, *keys)
            if found is not None:
                return found
    return None


def save_manifest():
    with (out / "commands.json").open("w") as f:
        json.dump({"version": 1, "limit": int(limit), "commands": commands}, f, indent=2)
        f.write("\n")
    with (out / "times.txt").open("w") as f:
        f.write("\n".join(f"{k} {v}" for k, v in times.items()) + "\n")


if options.cases:
    cases = json.loads(options.cases.read_text())
    if cases["version"] != 1:
        parser.error("unsupported command manifest version")
    limit = str(cases["limit"])
    for case in cases["commands"]:
        run(case["name"], *(dump if a == "{core}" else a for a in case["args"]))
    save_manifest()
    sys.exit(0)


run("summary", "summary", dump)
types = run("types", "types", dump)
for sort in ["live", "dead", "count"]:
    run("types-" + sort, "types", dump, "--sort", sort)
run("handles", "handles", dump)
regions = run("regions", "regions", dump)
for by in ["queue", "size"]:
    run("regions-" + by, "regions", dump, "--by", by)
threads = run("threads", "threads", dump)
native = run("native", "native", dump)
for reach in ["native", "live", "pending", "dead", "unreferenced"]:
    run("native-" + reach, "native", dump, "--reach", reach)
for name in ["memory", "garbage", "fragmentation", "strings", "collections", "finalizable", "zero-arrays", "findings", "async",
             "arraypool", "large-objects", "cancellation", "events", "timers", "locks", "blocked", "exceptions", "connections", "stacks", "network", "files", "instance-counts",
             "finalizer-backlog", "native-held-by-dead", "malloc-arenas", "boxed"]:
    run(name, name, dump)
run("retained", "retained", dump)
tasks = run("tasks", "tasks", dump)
run("search", "search", dump, "Exception")

type_name = first(types, "type")
if type_name:
    instances = run("instances", "instances", dump, type_name)
    for option in [["--live"], ["--dead"], ["--sort", "size"]]:
        run("instances" + "-".join([""] + [o.strip("-") for o in option]), "instances", dump, type_name, *option)
    address = first(instances, "address")
    if address:
        run("object", "object", dump, address)
        run("retained-object", "retained", dump, address)
        run("bytes", "bytes", dump, address)
        run("find", "find", dump, address)
        run("region", "regions", dump, address)
tid = first(threads, "tid")
if tid is not None:
    run("thread", "threads", dump, str(tid))
chunk = first(native, "largest")
if chunk:
    run("chunk", "chunk", dump, chunk)
task = first(tasks, "example")
if task:
    run("tasks-graph", "tasks", dump, task)
save_manifest()
