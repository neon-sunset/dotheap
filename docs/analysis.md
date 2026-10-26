# Analysis reference

This document explains how to interpret Dotheap's results and which runtime structures they depend
on. See the README for [commands](../README.md#commands),
[supported dumps and versions](../README.md#compatibility), and [symbols](../README.md#symbols-and-metadata).

## Managed objects and liveness

The GC descriptor before each MethodTable identifies managed reference slots, including reference
arrays and repeated reference runs in value-type arrays. Both live and dead object graphs use
these slots. A missing or invalid descriptor stops analysis with an error that names the type.

Roots include strong and pinned GC handles, thread statics, objects in the NonGC heap (Satori's
immortal regions), and possible object pointers in registers and stacks above the stack pointer.
Stack and register scanning is conservative: stale pointers can overestimate live counts. Weak
handles and stacks of exited threads are excluded. Dependent handles add an edge from the primary
object to the secondary.

Native code keeps managed objects alive through GC handles. An ordinary native word equal to an
object address is not a managed reference. `--native-roots` optionally treats words in writable
memory outside the managed heap that point to object starts as additional roots.

An unreachable object registered for finalization, or already in the finalizer queue, is pending
finalization unless its finalizer is suppressed. Objects reachable only through it are also kept
pending. Registrations come from stock CoreCLR's `CFinalize` array or Satori's per-region
`m_finalizableTrackers`; ready queues come from the end of `CFinalize` or `SatoriFinalizationQueue`.
The header bit `BIT_SBLK_FINALIZER_RUN` records suppression or a completed finalizer run. An
unreachable suppressed object is dead.

`finalizable` separates reachable registered objects, live suppressed or immortal objects,
resurrected objects, pending finalization and dead objects. `GC.ReRegisterForFinalize` can leave
an object pending after its finalizer ran; the snapshot cannot reconstruct that history.

## Managed and native ownership

The combined graph contains live managed objects, malloc chunks and anonymous mappings in use,
and root sources. A thread's stack and registers supply roots for both managed and native memory.
Edges follow managed references, managed native-pointer fields, references between native
allocations, and strong GC handles stored in native allocations. Such a handle belongs to its
containing allocation instead of appearing as a separate handle root.

Lengauer-Tarjan dominators identify the managed and native bytes reachable only through each node.
These are its retained bytes. Paths to roots can cross managed and native allocations. Retained
size describes this detected graph; it does not predict an immediate reduction in RSS.

The Viewer uses the same inspector for managed objects and native allocations. Links in Dominators,
Roots and the inspector select either kind without changing the current page. Back and Forward
restore the page and the inspected object. Dominators groups native children by identified type or
referrer across sizes, with the library and attribution method kept distinct.

Dominators shows the combined retained size. Parentheses show retained native memory for a managed
object, or retained managed memory for a native allocation. Types also includes native memory in
retained totals and instance ordering. Nested instances of the same type count their retained memory
once in the type total.

Settings controls how type names fit the available space: truncate, abbreviate namespaces, or wrap onto
two lines. Names stay expanded when they fit; wrapping uses namespace and type boundaries. Full names
remain available on hover and in object details.

Roots lists direct references from the combined graph, including native stacks, registers, library
data and mappings. Its managed and native byte counts are retained exclusively by each source.
An allocation referenced by several sources can appear in several reference lists, while its retained
bytes belong to their common dominator. Strong GC handles stored in native allocations remain attached
to those allocations; the GC handles tab groups handles by kind and target type.

### Native pointer references

Managed-to-native references come from field layouts: `nint`/`nuint` (`IntPtr`/`UIntPtr`), pointer
and function-pointer fields, including inherited fields, nested structs and array elements.
Integers, floating-point values, managed references and padding are excluded. Native words in
stacks, registers, library data, anonymous mappings and malloc chunks are scanned conservatively.

Freed chunks in tcache and fastbin lists are excluded. Tcache entries are found through each
thread's `tcache_perthread_struct` and the process-wide `tcache_key`. Each chunk in use is classified
by the references that reach it:

| Group | Meaning |
|---|---|
| Native roots | Reachable from native roots, even if managed objects also reference it. |
| Live objects | Held by live managed objects, with no detected path from native roots. |
| Pending finalization | Held only by managed objects pending finalization; their finalizers may release it. |
| Dead objects | Referenced only by dead managed objects; no pending finalizer remains for those owners. |
| Unreferenced | No reference was found; a leak candidate. |

The native allocation view groups chunks across sizes by identified type or reference source,
library and reachability. Each chunk appears in one group. **Identified by** distinguishes a C++
type found through its vtable (`_ZTV` symbols) from a referring C++ class, managed type and field,
or native root. A reference source does not identify the allocation site or establish ownership.
C++ referrer attribution stops at direct native roots and managed pointer fields. These provide
a separate reference source, so a C++ type does not propagate through them into other allocations.
Each distinct native root source contributes an edge, including sources that reference the same
allocation. Shared allocations belong to their common dominator. A nearby stack frame does not
identify an allocation's type or owner.
Native matches can be stale or unrelated, and encoded pointers or missing layouts can hide references.

Select a group to see its exact chunk sizes, counts and bytes. Select a size to filter the allocation
list, or **All sizes** to clear the filter. The Viewer lists up to 500 chunks, largest first; a
uniform-size list states the size shared by every chunk. Sizes include malloc headers and padding,
so they can exceed the bytes originally requested by the application.

Native Memory opens with Statistics, including malloc heaps and memory mappings. Allocations provides
the groups and size distribution; its tables scroll within their panes. Selecting an allocation opens
its details in the shared inspector. Following a reference leaves the group and size filter in place.

In the CLI, `native <core>` lists group IDs. Use `native <core> --group <id>` for the size distribution
and allocation addresses; add `--size <bytes>` to filter by size or `--limit 0` to list all rows.
The chunk inspector and `chunk <core> <address>` show reference evidence, including pointers to
chunk starts or interiors and any detected dominator. A dominator lies on every detected path
from roots to that chunk. Native pointer matches remain conservative in either case.

The inspector and CLI `possibleManagedReferences` also list raw matches to managed object addresses.
These matches do not add retention edges to the graph.

`finalizer-backlog` reports native memory reachable only through objects awaiting finalization.
Their finalizers may release it; inspect whether earlier disposal could have released the resources.
`native-held-by-dead` groups chunks by referring type and field. A hit can mean a native leak, or
that malloc reused an allocation while the dead object retained a stale pointer.

### Allocator and mapping analysis

Dump segments are classified as managed heaps, GC handle tables, thread stacks, JIT code, assembly
images, native libraries and their data, glibc heaps, the macOS dyld shared cache, or other memory.
Individual malloc chunks are analyzed only for Linux/glibc dumps.

Glibc heaps are walked chunk by chunk; older heaps of an arena end in a 16-byte fencepost. Free and
top-chunk bytes count pages with data in the dump, which may be resident or swapped out. The trim
estimate counts whole pages inside free chunks and the main arena's top chunk, following glibc's
`mtrim` rules. It is not a prediction of the RSS change in a running process.

The reader supports these glibc layouts:

- `malloc_state`: fastbins at +16, top chunk at +96 and arena ring at +2160 (glibc 2.26 and later).
- `heap_info` at 64 MiB heap boundaries.
- `tcache_perthread_struct` with 16-bit counts (2.30 and later) or 8-bit counts (2.26–2.29).
- List links with or without pointer mangling (introduced in 2.32).
- The tcache key in an entry's second word (2.29 and later).

The report reads the glibc version from libc's banner. Analysis has been verified on glibc 2.36
(Debian 12) and 2.39 (Ubuntu 24.04).

Createdump can merge adjacent mappings with the same access into one segment. Dotheap splits them
at thread-stack bounds and glibc heap headers. Stack bounds and exited-thread detection use
`struct pthread`; its offsets are inferred from live threads and their thread pointers. This keeps
an adjacent malloc heap separate from a stack and excludes stacks glibc retained after thread exit.

## Garbage and fragmentation

`garbage` follows references between dead objects. It identifies dead objects with no dead referrer
and uses dominators over the dead graph to report the garbage each retains.

`fragmentation` groups dead objects and unused committed space by the GC rule that keeps the space.
For Satori these include gen2 regions more than half live, pinned objects, large regions, reuse
for allocation, and regions outside any generation. Stock CoreCLR groups include LOH, POH, gen2
and ephemeral segments. Space is attributed to surviving objects in each region in proportion to
their sizes, then grouped by type and owner. The owner is the root source or shared object that
dominates them. This is an estimate of attribution, not proof that an allocation caused the waste.
GC storage, objects pending finalization and unparsed bytes are excluded.

`large-objects` uses 85,000 bytes for stock CoreCLR and 32 KiB for Satori. It reports live and dead
objects by type. Stock results include dead LOH objects awaiting gen2 collection and reusable gaps.
Satori results include huge objects that need their own region and memory committed beyond their end.

## Async calls, threads and locks

`async` reads state-machine boxes, task flags (`m_stateFlags`), continuations
(`m_continuationObject`), state (`<>1__state`), awaiter fields, and channel and semaphore waiter
lists. It follows `WhenAll`, `WhenAny`, `Task.Run`, `ContinueWith` and `WaitAsync`, including active
pooled ValueTask state machines. Calls are grouped by async stack and awaited object: many tasks
sharing one wait differ from tasks each waiting on their own object.

A state machine at state -1 is classified as running; its box in a thread-pool queue is queued to
resume. A matching thread stack identifies the executing thread when available. Cycles show
calls waiting on each other. When only waiting calls retain an awaited operation, no other detected
owner can complete it. Unreachable waiting calls have been abandoned. A dump has no wait timestamps;
compare counts and ownership, not inferred ages. Portable PDB async stepping data and sequence
points provide await source lines. This analysis uses the .NET 10 state-machine model, not .NET 11
runtime async.

`blocked` groups synchronous waits (`Task.Wait`, `GetResult`, `Monitor.Wait`) by their first
application frame. It reads global and worker-local thread-pool queues and counts busy, blocked
and idle workers. Queued work with no idle workers and some blocked workers can indicate starvation.

`locks` reads thin-lock headers, sync blocks (`AwareLock`) and `System.Threading.Lock`. A likely
waiter has a lock-enter frame and a stack word pointing to a lock another thread holds. Wait
chains can expose deadlocks, but frames and waiters are inferred; inspect the evidence before
concluding that a deadlock is present.

### Frames and source locations

Thread names come from managed `Thread` objects in the runtime's thread list. Frames are estimated
by scanning stacks for code addresses immediately after call instructions. Earlier calls can leave
stale addresses behind.

Managed addresses are named through the runtime's code map: the range section map, JIT nibble map
and code header, or ReadyToRun runtime functions and entry-point map, then the MethodDesc and its
metadata token. Native addresses use exported symbols or matching debug files. A thread without
managed frames is grouped by its named native frames. A stack root names the frame that holds it.
See [Symbols and metadata](../README.md#symbols-and-metadata) for supplying debug files and PDBs.

## Heap integrity

`dotheap verify <dump>` validates the heap without building the reference graph. Use it when
normal analysis fails or to inspect the diagnostics listed under **Findings > Heap integrity**.
The Viewer also provides **Analysis > Verify Heap…**, including after a dump fails to open.

Each diagnostic gives the structure, field address, observed value and reason. Object addresses
link to the inspector when the object could be parsed. Verification covers:

- Object sizes and used-memory boundaries, including array lengths.
- MethodTable and EEClass links, including canonical types shared by generic instances and arrays.
- GC descriptor counts, offsets, sizes and reference slots. Native pointers and integer fields
  are not treated as managed references.
- Runtime thread links, CoreCLR segment boundaries and linked lists, and finalization queue boundaries.
- Satori region links, queue membership and finalization tracker chains and counts.

Missing dump pages are separate from invalid values. Zeroed ranges and uncertain object boundaries
are reported as unverified. EEClass back-pointers and thread links require the layout
contract stored in the runtime; the report identifies unavailable validation. Unreachable objects
can contain stale references, and a capture during GC or allocation can contain temporary
inconsistencies. Compare the addresses and runtime state before concluding that memory was overwritten.
Sync blocks, thin locks, card tables and assembly metadata tables are not verified.

`verify` reads the dump directly. Findings reuse verification results in the analysis cache.
If normal analysis fails, CLI `findings` returns heap diagnostics and the analysis error; other
reports are unavailable. Diagnostic totals are exact, with details retained for the first 1,000
in each category.
Use `--limit 0` to print all retained details. Exit status 0 means the report completed, including
reports with diagnostics; status 1 means the command failed. JSON separates `invalid`,
`missingMemory` and `unverified` counts.

## Other findings

### Array pools

`arraypool` reads shared ArrayPool partitions and thread-local buckets. An array in two slots
indicates a double return. A pooled array still referenced by a live object outside the pool can
be used after return, when another renter receives it.

Dead arrays of pooled element types with bucket lengths (16 × 2ⁿ) are missed-return candidates.
Arrays from exited thread slots are excluded. Directly allocated arrays of those sizes look the
same; compare with dead arrays of other lengths and live arrays outside the pool.

### Callbacks, timers and instance counts

`cancellation` walks live `CancellationTokenSource` callback lists. Undisposed registrations or
linked sources can retain callback targets and state. `events` reads multicast delegate lists
and groups subscriptions by holding object, field and subscriber type. A long-lived publisher
can retain otherwise unused subscribers.

`timers` reads callbacks, state, due time, period and start ticks from `TimerQueueTimer`. It
identifies `Timer`, `System.Timers.Timer`, `PeriodicTimer`, `Task.Delay`, `Task.WaitAsync` and
`CancelAfter`. Results include callbacks or awaiting calls, PDB source lines when available, and
the holding object and field. Owner paths skip timer queues. Within a group, labels use the part
of the path that distinguishes timers, such as `Worker[][11] → Worker._timer`. Ages are relative
to the latest timer start in the dump, not to capture time.

`instance-counts` reports live counts of `HttpClient`, handlers, `JsonSerializerOptions`, `Regex`,
service providers and other commonly shared or disposable types. Each type has a finding threshold;
instances are grouped by owner. A high count prompts inspection of lifetime and reuse; it does
not prove a leak.

### Exceptions and I/O

`exceptions` reads heap exceptions and saved `_stackTrace` data, plus `TaskExceptionHolder`
objects. It groups live and dead exceptions by type and throw site, and faulted tasks by exception
and observation state. Unthrown or preallocated exceptions can lack a saved stack. Many dead
exceptions from one site can indicate frequent throwing; live ones still have an owner. Finalizing
an unobserved faulted task raises `TaskScheduler.UnobservedTaskException`.

`connections` reads Kestrel request stages and HTTP/2 streams, HttpClient requests and pool queues,
and `SocketAsyncEngine` queues. It reports incoming requests by stage, outgoing requests awaiting
responses or connections, and socket completions queued for workers. `network` also lists sockets,
endpoints, `SafeSocketHandle` descriptors and pending `SocketAsyncContext` operations. Pools belong
to one handler, host, connection kind and proxy. Separate handlers can create separate pools for
the same host; queued requests can mean connections are opening, busy or at a limit.

`files` reads managed `SafeFileHandle` and `FileStream` state. Native paths come from strings in
C++ objects; descriptors are estimated from the integer after a path. A dump has no kernel file
table, so this is not a complete inventory of open descriptors. Extra handle references can come
from active I/O or `DangerousAddRef`, including file mappings.

## Runtime structures and metadata

Dotheap finds libcoreclr by name and reads its build ID, `@(#)Version` string and cDAC contract
descriptor (`DotNetRuntimeContractDescriptor`). The descriptor supplies runtime type layouts and
globals, but no Satori page or region layout.

For stock CoreCLR, `GcDacVars` supplies heaps, generation tables and field offsets for `gc_heap`
and `generation`. Each generation's `start_segment` list supplies regions, or segments on macOS.
The macOS ephemeral segment is split at gen1 and gen0 allocation starts. Unused thread and
generation allocation contexts are skipped; thread contexts come from the cDAC thread list.
Object counts have been compared with SOS `dumpheap` on test dumps.

A MethodTable identifies its module and typedef row. Assembly metadata supplies names and field
signatures; generic arguments come from the per-instantiation dictionary. Fields use EEClass
FieldDescs and the metadata Field table. Struct fields and arrays of structs are expanded through
field signatures and the module's typedef-to-MethodTable map. Supported signatures include TypeDef,
TypeRef, class type parameters and the one known instantiation of a generic struct.

For Linux dumps, the ELF `NT_FILE` note identifies mapped assembly files. If their images are
missing from the dump, Dotheap needs matching files supplied through `--dlls` to read metadata.

### Satori layouts and region contents

[satori_heap.rs](../crates/heap-core/src/satori_heap.rs) defines these layouts for
[Satori](https://github.com/VSadov/Satori):

| Layout | Change |
|---|---|
| Since 9df7b3d86e4 (2026-10-15) | Allocation-zeroing and gen0 fields moved the used, committed, finalization, queue, occupancy, object-count and pinned fields. |
| 3ed9d3c8680 through dc9a20118d2 | `m_nonEmptyFreeLists` shifted the queue, occupancy, object-count and pinned fields by eight bytes. |
| Before 3ed9d3c8680 | Original supported region layout. |

Each layout is checked against every region header. `m_prev` and `m_next` must be null or region
starts; queue pointers must be null or have a known kind; `m_used`, `m_committed` and `m_occupancy`
must fit the region. The selected layout must pass at least half the checks and more than any
other layout. A tie or insufficient matches stops analysis. A partially updated header can fail
without disqualifying an otherwise matching layout. Runtime versions, build IDs and commit strings
do not select the layout. The summary reports the choice and check count.

The reader also depends on 1 GiB page alignment; page fields `m_end` at +8 and `m_firstRegion` at
+24; the region map at +56, with one byte per 2 MiB granule; region fields from `m_ownerThreadTag`
through `m_hasPinnedObjects`; the first object at +36,888; queue-kind ordering; and the runtime's
free-object MethodTable. `m_used` bounds the area searched for objects. Changes to these structures
require a reader update; collector changes that preserve them do not require a new layout.

Regions in generation -1 can be free regions, GC data, or huge objects not yet published through
`SatoriGC::PublishObject`. Allocator queues identify free regions. Work-chunk lists and the
finalization queue identify GC storage; its committed space, including unused capacity, is
reported separately from free space and unparsed bytes. A region with no containing queue is
not necessarily free or lost. Data whose purpose cannot be established remains unparsed.

The heap map labels a single-granule region `large objects` when it contains an object of at least
32 KiB; this is a Dotheap label, not an exclusive Satori allocation category. A huge region spans
multiple granules for one object that does not fit in 2 MiB. Satori's `AllocHuge` creates these
regions, and its `IsLarge` test refers to multi-granule regions.

## Residency and storage

[Resident memory](../README.md#resident-memory) explains how to supply smaps or vmmap data.
For Linux smaps, matching requires at least 95% of dump mapping bytes to have matching ranges.
Adjacent mappings are combined because createdump can merge them. At most 5% of anonymous pages
with data may be absent from both resident and swapped memory in smaps. These checks reject
obvious mismatches; they do not establish that both files describe the same instant.

Without matching residency data, the lower heap estimate excludes zero-filled pages. The upper
estimate includes them within fully committed 2 MiB granules: zeroing writes or transparent huge
pages can make these pages resident. Satori requests huge pages with `DOTNET_gcTHP` by default;
stock CoreCLR can receive them under Linux's `always` setting. Partially committed granules, such
as a huge region's tail, cannot use a full 2 MiB page.

Object arrays, reference graphs and ownership data use file-backed storage. Working memory depends
on object and reference counts and on the analysis being built. It is not bounded by compressed
dump size. Cached forward references avoid repeated full scans.

Compressed input has two storage paths:

- Frames of at most 16 MiB with known sizes are read from the original dump. The cache stores a
  frame index. Dotheap dumps include a seek table; other small-frame dumps need a pass over frame
  headers to build the index.
- Larger or unsized frames, including ordinary `zstd` and systemd-coredump output, are decompressed
  once and stored as independently compressed 256 KiB blocks in the cache.

Sequential scans can use reusable decompression buffers. Random reads retain only the requested
4 KiB pages in a temporary sparse file sized to the expanded dump. A bounded set of decoded blocks
serves nearby reads. Nonzero pages are written on demand and remain available
for the lifetime of the open dump; there is no fixed bound on the disk space they can occupy.
See [Cache](../README.md#cache) for space requirements and cleanup commands.

Findings run up to four independent checks concurrently. Checks share their analysis results;
heap scans and async task reads can also use the Rayon pool. Cached async findings load separately
from the detailed task graph.
