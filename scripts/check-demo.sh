#!/bin/sh
# Checks dotheap's results on a dump of the demo app (demo/) against what the demo allocates.
# usage: scripts/check-demo.sh <dotheap binary> <dump> [CoreCLR|Satori]   (the GC the dump must be of)
set -eu
bin=$1; dump=$2; export DEMO_GC=${3:-}
python=$(command -v python3 || command -v python)
"$bin" instances "$dump" 'System.Collections.Concurrent.ConcurrentDictionary`2<System.String, Shop.CacheEntry>' --json > cache.json
"$bin" instances "$dump" Shop.CacheEntry --json > entries.json
"$bin" instances "$dump" Shop.Session --text "held only by a GC handle" --json > held.json
"$python" - <<'PY'
import json
cache, entries, held = (json.load(open(f)) for f in ("cache.json", "entries.json", "held.json"))
def check(ok, what):
    print(("ok    " if ok else "FAIL  ") + what)
    if not ok: raise SystemExit(1)
retained = cache["instances"][0]["retained"]
check(retained > 150 << 20, f"the response cache retains more than 150 MB ({retained >> 20} MB)")
check(entries["objects"] == 48000, f"48,000 cache entries ({entries['objects']})")
check(held["objects"] == 1 and held["instances"][0]["state"] == "live", "the session held by a GC handle in native memory is live")
PY
for check in async arraypool events cancellation timers blocked locks exceptions connections instance-counts; do "$bin" "$check" "$dump" --json --limit 0 > "check-$check.json"; done
"$bin" tasks "$dump" --json --limit 0 > tasks.json
"$bin" network "$dump" --json --limit 0 > network.json
"$bin" files "$dump" --json --limit 0 > files.json
"$bin" summary "$dump" --json > summary.json
"$bin" finalizable "$dump" --json --limit 0 > finalizable.json
batch=$("$python" -c 'import json; print(next(g["example"] for g in json.load(open("tasks.json"))["groups"] if g["outermostCall"].startswith("Jobs.RunBatch")))')
"$bin" tasks "$dump" "$batch" --json --limit 0 > tasks-batch.json
"$python" - <<'PY'
import json, re
r = {c: json.load(open(f"check-{c}.json")) for c in ["async", "arraypool", "events", "cancellation", "timers", "blocked", "locks", "exceptions", "connections", "instance-counts"]}
def check(ok, what):
    print(("ok    " if ok else "FAIL  ") + what)
    if not ok: raise SystemExit(1)
calls = r["async"]["tasksByAsyncStack"]
def call(rows, text):
    return next((c for c in rows if c["call"].startswith(text)), None)
never = next((c for c in calls if c["call"].startswith("Waiter.Outer at await 0") and "Task, held only by the calls above" in c["call"]), None)
# A stale stack word can still point to one of the last tasks (the stack is scanned conservatively), so allow a few.
check(never and never["count"] >= 290, f"{never['count'] if never else 0} of 300 async calls wait forever on a task only they hold")
batch = call(calls, "Jobs.RunBatch at await 0")
batch = batch and call(batch.get("children", []), "Jobs.RunBatch at await 0")
fetch = batch and call(batch.get("children", []), "Task.WhenAll") and call(call(batch["children"], "Task.WhenAll").get("children", []), "Jobs.Fetch at await 0")
check(batch and batch["count"] == 10 and fetch and fetch["count"] == 30, "10 batches wait in Task.WhenAll for 30 jobs, in one graph")
graph = json.load(open("tasks-batch.json"))
def node(rows, text):
    return next((n for n in rows if n["call"].startswith(text)), None)
top = node(graph["tree"], "Jobs.RunBatch at await 0")
fetch = top and node(top.get("children", []), "Task.WhenAll") and node(node(top["children"], "Task.WhenAll").get("children", []), "Jobs.Fetch at await 0")
check(graph["calls"] == 50 and fetch and fetch["count"] == 30, f"the tasks command shows the batch graph: 10 batches, 10 WhenAll, 30 jobs ({graph['calls']} calls)")
shared = r["async"]["objectsThatMoreThanOneCallWaitsOn"]
check(any(o["object"].startswith("SemaphoreSlim") and o["heldBy"] == "static Shop.Jobs.Gate" and o["calls"] == 30 for o in shared), "30 jobs wait on one SemaphoreSlim")
check(any(o["object"] == "UnboundedChannel`1<Int32>" and o["heldBy"] == "static Shop.Jobs.Queue" and o["calls"] == 8 for o in shared), "8 readers wait on one channel")
check(any(c["call"].startswith("Jobs.Tick at await 0") and ", in Task.Run" in c["call"] for c in calls), "a call that Task.Run started")
cycle = r["async"]["waitingOnEachOther"]
check(sum(c["count"] for c in cycle) == 3, f"3 pairs of calls wait on each other ({sum(c['count'] for c in cycle)})")
dropped = [m for m in r["async"]["byMethodAndAwait"] if m["methodAndAwait"].startswith("Shop.Waiter.WaitForever at await 0")]
check(dropped and dropped[0]["dead"] >= 300, f"dropped waiting async methods are found ({dropped[0]['dead'] if dropped else 0} of 400)")
check(len(r["arraypool"]["returnedTwice"]) >= 1, "an array returned to the ArrayPool twice")
check(any("Shop.Leaky" in u["heldBy"] for u in r["arraypool"]["stillReferencedAfterReturn"]), "an array held in a static field after it was returned")
check(any(e["heldBy"] == "static Shop.Bus.Changed" and e["subscribers"] == 2000 for e in r["events"]["delegatesWith16OrMoreSubscribers"]), "an event with 2,000 subscribers")
check(r["cancellation"]["sourcesWithRegistrations"][0]["registrations"] == 1500, "a token source with 1,500 registrations")
timers = r["timers"]["byWhatMadeThem"]
check(any(t["kind"] == "Timer" and t["calls"].startswith("Shop.Waiter.OnTimer (Program.cs:") and t["heldBy"] == "static Shop.Bus.Timers" and t["timers"] == 1200 for t in timers), "1,200 timers calling Shop.Waiter.OnTimer, held by a static list")
check(any(t["kind"] == "PeriodicTimer" and t["awaitedBy"].startswith("Clock.Loop (await 0, Program.cs:") and t["heldBy"] == "static Shop.Clock.Ticker" and t["period"] == "every 5 min" for t in timers), "a PeriodicTimer that a loop awaits")
check(any(t["kind"] == "System.Timers.Timer" and t["calls"].startswith("Shop.Clock.OnHeartbeat (Program.cs:") and t["heldBy"] == "static Shop.Clock.Heartbeat" for t in timers), "a System.Timers.Timer and its Elapsed handler")
check(any(t["kind"] == "CancelAfter" and t["heldBy"] == "static Shop.Clock.Deadline" for t in timers), "a token source that cancels itself after an hour")
check(any(t["kind"] == "Task.Delay" and t["awaitedBy"].startswith("Jobs.Tick (await 0") for t in timers), "a Task.Delay that a call awaits")
check(any(b["threadPool"] >= 4 and b["waitFirstApplicationFrame"].startswith("Task.Wait") for b in r["blocked"]["byWait"]), "4 thread pool threads blocked in Task.Wait")
queued = {q["workItem"]: q["items"] for q in r["blocked"]["queuedWorkItems"]}
check(queued.get("Shop.Pool.Block", 0) >= 20, f"20 work items wait in a starved thread pool ({queued.get('Shop.Pool.Block', 0)})")
check(queued.get("Shop.Pool.Resume", 0) == 5, f"5 async calls wait in the queue to resume ({queued.get('Shop.Pool.Resume', 0)})")
check(any(g["state"] == "queued" and g["outermostCall"].startswith("Pool.Resume") and g["tasks"] == 5 for g in json.load(open("tasks.json"))["groups"]), "the tasks command shows the 5 calls as queued")
check(any(g["outermostCall"].startswith("Jobs.RunBatch (await 0, Program.cs:") for g in json.load(open("tasks.json"))["groups"]), "await source lines from the PDB embedded in Demo.dll")
owner = next((t for t in r["locks"]["heldLocks"] if "(cache owner) holds Monitor on Object, held by static Shop.Locks.Cache" in t["thread"]), None)
check(owner and sum(w["threads"] for w in owner.get("children", [])) == 3, "3 threads wait for the lock that the cache owner holds")
check(any(l["lock"] == "Lock, held by static Shop.Locks.Registry" and l["waiters"] == 1 for l in r["locks"]["allHeldLocks"]), "a thread waits for a System.Threading.Lock")
check(any(d["threads"] == 2 and "first then second" in d["thread"] and "second then first" in d["thread"] for d in r["locks"]["deadlocks"]), "2 threads wait for each other's locks")
check(any(f["exception"] == "System.InvalidOperationException: demo failure" and f["unobserved"] >= 60 for f in r["exceptions"]["faultedTasks"]), "60 faulted tasks nobody observed")
check(any(e["type"] == "System.InvalidOperationException" and e["thrownIn"].startswith("Program") and e["live"] >= 60 and e["message"] == "demo failure" for e in r["exceptions"]["byTypeAndThrowingMethod"]), "60 live exceptions thrown in the demo's lambda")
check(any(p["type"] == "System.Net.Http.HttpClient" and p["live"] >= 25 for p in r["instance-counts"]["byType"]), "25 HttpClient instances")
orders = sum(q["requests"] for q in r["connections"]["outgoingRequests"] if q["host"].startswith("127.0.0.1:") and q["method"] == "GET" and q["path"].startswith("/orders/"))
check(orders == 5, f"5 outgoing requests wait for a response or for a connection ({orders})")
network = json.load(open("network.json"))
sockets = network["sockets"]
waiting = sum(k["pendingReceives"] for k in sockets if k["kind"] == "outgoing" and k["owner"] == "HttpClient" and k["otherSide"].startswith("127.0.0.1:"))
check(waiting >= 5 and any(k["kind"] == "listening" and k["owner"] == "static Shop.Net.Listener" for k in sockets), f"their sockets wait to receive ({waiting}), and the listener is there")
check(any(k["kind"] == "incoming" and k["owner"] == "static Shop.Net.Accepted" and k["sockets"] == 5 for k in sockets), "the server holds the 5 connections it accepted")
check(any(p["host"].startswith("127.0.0.1:") and p["http11"] == 5 and p["active"] == 5 and p["kind"] == "http" and p["handlerHeldBy"] == "static Shop.Net.Client"
          for p in network["httpClientPools"]), "an HttpClient pool with 5 connections, each with a request, of the handler that Shop.Net.Client holds")
check(len(network["socketEngines"]) >= 1 and sum(e["registeredSockets"] for e in network["socketEngines"]) >= 6, "socket engines, with the listener and the 5 HttpClient sockets registered")
files = json.load(open("files.json"))
check(any(f["path"].endswith(".log") and "dotheap-demo-" in f["path"] and f["access"] == "write" and f["unwritten"] == "100 of 4096" and f["heldBy"] == "static Shop.Files.Log" for f in files["openFiles"]), "a file with 100 bytes waiting in its FileStream buffer")
finalizable = {t["type"]: t for t in json.load(open("finalizable.json"))["types"]}
handles, noop = finalizable.get("Shop.MallocHandle", {}), finalizable.get("Shop.NoopHandle", {})
check(handles.get("liveRegistered") == 50 and 27 <= handles.get("pendingFinalization", 0) <= 30,
      f"50 live MallocHandles are registered for finalization and the 30 undisposed ones are pending finalization ({handles.get('liveRegistered')}, {handles.get('pendingFinalization')})")
check(noop.get("pendingFinalization", 0) == 0 and noop.get("liveRegistered", 0) == 0,
      f"no disposed NoopHandle is pending finalization ({noop.get('pendingFinalization', 0)} pending, {noop.get('dead', 0)} dead)")
summary = json.load(open("summary.json"))
check(re.match(r"^\d+\.\d+\.\d+", summary["dump"].get("netCore") or ""), f"the version of the .NET libraries ({summary['dump'].get('netCore')})")
import os
if os.environ["DEMO_GC"]: check(summary["dump"]["gc"] == os.environ["DEMO_GC"], f"the dump is of the {os.environ['DEMO_GC']} GC ({summary['dump']['gc']})")
PY
platform=$("$bin" summary "$dump" --json | "$python" -c 'import json, sys; print(json.load(sys.stdin)["dump"]["platform"])')
case "$platform" in Linux*)
    "$bin" native "$dump" --json --limit 0 > native.json
    "$bin" finalizer-backlog "$dump" --json --limit 0 > backlog.json
    "$bin" native-held-by-dead "$dump" --json --limit 0 > held-by-dead.json
    "$python" - <<'PY'
import json
native = json.load(open("native.json"))
backlog = json.load(open("backlog.json"))
heldByDead = json.load(open("held-by-dead.json"))
def check(ok, what):
    print(("ok    " if ok else "FAIL  ") + what)
    if not ok: raise SystemExit(1)
# A stale word in native memory (a field of a later allocation that reuses freed memory) can reach a leaked block.
leaked = sum(g["chunks"] for g in native["groups"] if g["heldBy"].startswith("nothing") and g["label"].startswith("8208-byte"))
check(leaked >= 395, f"the 400 leaked 8 KB blocks are unreferenced, apart from a few that a stale word reaches ({leaked})")
# Chunks that objects own through the field, whatever else holds them: stale words on stacks and in library data can
# also point to a block, which then counts as held by native roots.
def owned(field, held=""):
    return sum(g["chunks"] for g in native["groups"] if g["label"].startswith("referenced by " + field) and g["heldBy"].startswith(held))
handles, pending = owned("Shop.MallocHandle.handle"), owned("Shop.MallocHandle.handle", "managed objects pending finalization")
check(handles == 80 and handles - pending >= 50, f"80 MallocHandles own a 64 KB chunk each, 50 of them live ({handles}, {pending} held only by ones pending finalization)")
check(owned("Shop.NativeBuffer.Data") == 200, f"200 NativeBuffers own a 4 KB chunk each ({owned('Shop.NativeBuffer.Data')})")
check(owned("Shop.NativeLayouts.Nested.Value.Pointer") == 1, "a pointer in nested structs refers to its native chunk")
check(owned("Shop.PointerValue[]") == 2, "pointers in struct array elements refer to their native chunks")
check(owned("System.IntPtr[]") == 4, "pointers in vector and multidimensional arrays refer to their native chunks")
check(owned("System.Int64[]") == 0, "integer array values are not native references")
raw = sum(r["chunks"] for r in heldByDead["byTypeAndFieldOfTheDeadObject"] if r["type"] == "Shop.RawBuffer" and r["field"] == "Data")
check(raw >= 45 and any("no finalizer will free it" in f for f in heldByDead["findings"]),
      f"the native memory of 50 dead RawBuffers is held only by them, and no finalizer will free it ({raw})")
waiting = sum(r["retained"] for r in backlog["byTypeOfTheObjectPendingFinalization"] if r["type"] == "Shop.MallocHandle")
check(waiting >= 0.9 * 30 * 65536, f"native memory of 30 undisposed MallocHandles waits for the finalizer ({waiting} bytes)")
PY
    ;;
esac
