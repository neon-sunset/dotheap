import HeapCore
import SwiftUI

/// Plain names and explanations for Satori and analyzer terms. Descriptions follow the Satori fork sources.
enum Glossary {
    struct Entry: Identifiable {
        var id: String { term + (gc.map { "/" + $0.rawValue } ?? "") }
        let term: String
        let text: String
        /// The GC the term belongs to; nil for terms of both.
        var gc: HeapKind.GC? = nil
    }

    /// Display name and explanation of a Satori queue (QueueKind).
    static func queue(_ raw: String) -> (name: String, help: String) {
        switch raw {
        case "Allocator": ("Free", "Free regions available for allocation, outside every generation. After gen2 collections, the trimmer merges adjacent free regions and decommits memory. With DOTNET_gcTHP enabled, normal trimming keeps the first 2 MB of each free range committed.")
        case "RecyclerTenured": ("Gen2", "Regions promoted to gen2, plus huge objects that do not fit in a 2 MB region and start in gen2.")
        case "RecyclerTenuredFinalizationTracking": ("Gen2, finalizable", "Gen2 regions with finalizable objects, kept in a separate queue for finalization tracking.")
        case "RecyclerEphemeral": ("Gen1", "Ephemeral regions: generation 1.")
        case "RecyclerEphemeralFinalizationTracking": ("Gen1, finalizable", "Gen1 regions that hold objects with finalizers.")
        case "RecyclerNursery": ("Gen0 nursery", "This queue kind is unused. Nursery regions are attached to allocation contexts and appear under In no queue.")
        case "RecyclerReusable": ("Reusable", "Gen1 regions with more than 64 KB of reusable space, retained up to the gcReusableTarget budget. Threads use them for small or large objects before taking free regions. A blocking collection returns them to the gen1 queues.")
        case "RecyclerDemoted": ("Demoted", "Gen2 regions returned to gen1 to reuse their free space. Their older objects remain tracked and count as live during gen1 collections.")
        case "RecyclerDeferredSweep": ("Waiting for sweep", "Marked regions that a concurrent sweep has not reached yet.")
        case "RecyclerStaying": ("Staying", "Regions a collection decided not to relocate.")
        case "RecyclerRelocating", "RecyclerRelocated", "RecyclerRelocatedToHigherGen", "RecyclerRelocationTarget":
            ("Relocation", "Regions in the middle of a compaction: sources, targets or already moved.")
        case "RecyclerFinalizationPending", "RecyclerFinalizationScanComplete": ("Finalization scan", "Regions held while the collector processes finalizable objects.")
        case "WorkChunk": ("GC work memory", "This queue kind is unused. Collector work chunks occupy regions outside every queue and generation.")
        case "(in no queue)": ("In no queue", "m_containingQueue is null. This is common for regions attached to a thread or shared allocation context, immortal regions, and GC data such as work chunks and the finalization queue. Shared contexts include small, large, pinned and immortal allocations. It does not mean the region is free or unowned.")
        default: (raw, "")
        }
    }

    static func generation(_ name: String, gc: HeapKind.GC = .satori) -> String {
        if gc == .coreClr {
            switch name {
            case "gen0": return "Generation 0: new small objects. A gen0 GC promotes the survivors to gen1 and compacts or sweeps."
            case "gen1": return "Generation 1: objects that survived one collection."
            case "gen2": return "Generation 2: long-lived objects, collected by full (background or blocking) collections."
            case "LOH": return "Large object heap: objects normally 85,000 bytes or larger, collected with gen2. Compaction can be requested with GCSettings.LargeObjectHeapCompactionMode and can also occur near the heap hard limit, with GCConserveMemory, or during an aggressive GC."
            case "POH": return "Pinned object heap: arrays created with GC.AllocateArray or GC.AllocateUninitializedArray and pinned: true, plus runtime arrays of static references. Collected with gen2; never compacted."
            case "NonGC heap": return "The runtime's frozen object heap (CorDebug_NonGC in the DAC). Holds string literals, RuntimeType objects and other objects that are never collected."
            case "SOH": return "Small object heap segment (segments GC)."
            default: return ""
            }
        }
        return switch name {
        case "gen0": "Generation 0: one thread's allocation region, with escape tracking. The thread can collect it without stopping other threads. The region moves to gen1 when more than 512 KB of objects are referenced from outside it, or when the thread detaches it."
        case "gen1": "Generation 1: detached gen0 regions, shared, large-object and pinned regions allocated without escape tracking, reused gen1 regions, and demoted gen2 regions."
        case "gen2": "Generation 2: long-lived objects, collected by full collections."
        case "immortal": "Generation 3: immortal regions, equivalent to the stock GC's NonGC heap. Satori places frozen objects such as string literals and RuntimeType objects here. They are never collected."
        default: "Outside every generation: free regions, collector data such as work chunks and the finalization queue, or huge objects not yet published with a MethodTable."
        }
    }

    static let entries: [Entry] = [
        Entry(term: "Page", text: "A reservation of 1 GB or more that holds regions. Its header contains a card table and a region map.", gc: .satori),
        Entry(term: "Region", text: "The unit Satori manages: 2 MB, or a multiple of 2 MB for a huge object or free range. Each has a generation and a 36 KB header for marking, indexing and bookkeeping. Regions attached to allocation contexts have no queue.", gc: .satori),
        Entry(term: "Region", text: "A stock GC memory range on Linux and Windows. Small-object regions default to 4 MB, or 1–2 MB under small heap hard limits. LOH and POH regions are eight times larger; a very large object can need more. Each generation has a list of regions (start_segment); heap_segment bookkeeping is stored separately.", gc: .coreClr),
        Entry(term: "Segment", text: "A stock GC memory range on macOS, which uses segments rather than regions. The ephemeral segment holds gen2 at its start, followed by gen1 and gen0 at their allocation starts.", gc: .coreClr),
        Entry(term: "Granule", text: "2 MB of address space, the size of a normal region and of one cell in the heap map.", gc: .satori),
        Entry(term: "Heap map cell", text: "2 MB of address space in the heap map; a region covers one or more cells, and 1 MB regions share a cell.", gc: .coreClr),
        Entry(term: "Committed", text: "Region memory committed by the OS: m_committed in Satori, heap_segment.committed in CoreCLR. It is not necessarily resident; supply smaps or vmmap data to measure RSS."),
        Entry(term: "Used", text: "Memory up to m_used, the highest address handed out for objects. It includes live objects, dead objects and free gaps. Committed memory above it has not held objects.", gc: .satori),
        Entry(term: "Allocated", text: "Memory up to heap_segment.allocated, or alloc_allocated in the active gen0 region. Committed memory beyond that limit holds no objects.", gc: .coreClr),
        Entry(term: "Server GC heap", text: "Each server GC heap has its own generations and regions. DATAS, enabled by default since .NET 9, starts with one heap and adjusts the count toward a target share of time in GC (2% by default).", gc: .coreClr),
        Entry(term: "Allocation context", text: "A range a thread uses to allocate without locking. The space between its allocation pointer and limit holds no objects and is skipped by the walk as Unparsed.", gc: .coreClr),
        Entry(term: "Live", text: "Objects reachable from detected roots. Stack and register words that point inside an object count as roots, so stale pointers can overestimate liveness."),
        Entry(term: "Dead", text: "Unreachable objects that still occupy memory until a collection covers their region. Gen1 collections cover gen0 and gen1; gen2 collections cover all collectible generations.", gc: .satori),
        Entry(term: "Dead", text: "Objects that are no longer reachable but still occupy memory until the next collection of their generation.", gc: .coreClr),
        Entry(term: "Free gaps", text: "Swept dead objects and unused allocation space (m_allocStart to m_allocEnd). Gen0 and gen1 reuse it for allocation. Gen2 can receive relocated objects, or sparse regions can be demoted to gen1 for reuse. Other gaps remain until compaction or until the region empties.", gc: .satori),
        Entry(term: "Free gaps", text: "Swept space represented by free objects on a generation's free list. Gen2 uses it for promoted objects; the LOH uses it for new large objects. Compaction removes gaps where the GC permits it.", gc: .coreClr),
        Entry(term: "GC storage", text: "Committed space reserved for GC work chunks and finalization queue entries, including unused capacity. It is unavailable for managed object allocation. Its regions need no containing queue: work chunks are held in lists, and the finalization queue points to its storage directly.", gc: .satori),
        Entry(term: "Unparsed", text: "Used space that the walk could not classify, including active allocation ranges and zero-filled ends. Unidentified GC data and allocations without a MethodTable can appear here. Recognized GC storage is reported separately.", gc: .satori),
        Entry(term: "Unparsed", text: "Used space that the walk could not read as objects, such as an active allocation range or zero-filled memory at the end.", gc: .coreClr),
        Entry(term: "Fragmentation", text: "(dead + free gaps) / (all objects + free gaps): the share of object space without live data."),
        Entry(term: "Satori occupancy", text: "m_occupancy: bytes Satori counts as occupied after collection, plus ranges reserved for allocation since then. An active range counts in full until allocation stops and its unused part is returned. Objects that died since collection remain counted until collected. This is not current liveness.", gc: .satori),
        Entry(term: "Relocation (compaction)", text: "Moves live objects out of sparse regions so those regions can be freed. Candidates must be at most half full and movable. Normal collections require enough estimated reclaim: about 1/6 of condemned space for gen2 or 1/2 for gen1. Explicit compaction can bypass that reclaim test in runtimes that support it. Relocation and pointer updates stop all threads; low-latency mode normally avoids relocation.", gc: .satori),
        Entry(term: "Compaction", text: "Blocking collections choose between sweeping into free lists and moving objects to remove gaps. Background collections only sweep. LOH compaction is conditional: an explicit request, heap hard limit, GCConserveMemory or aggressive GC. The POH never compacts.", gc: .coreClr),
        Entry(term: "Trimmer", text: "Merges adjacent free regions and decommits memory. Normal background passes follow gen2 collections and are throttled to one page scan per millisecond and one region per 10 ms. With DOTNET_gcTHP enabled, normal trimming keeps the first 2 MB of each free range committed. Aggressive collection or memory-pressure trimming can change this behavior, depending on the runtime.", gc: .satori),
        Entry(term: "Thread-local GC (TLGC)", text: "A thread can collect and compact its escape-tracked gen0 region while other threads run. Objects referenced from outside the region are marked as escaped and stay live during TLGC. The thread also keeps objects reachable from its own stack. Escaped objects can need a global collection even when they are now unreachable. Collection runs on allocation; an idle thread does not periodically collect its region.", gc: .satori),
        Entry(term: "Finalization queue", text: "CFinalize records objects registered for finalization by generation, followed by objects ready for the finalizer thread. Only the ready objects act as roots; registration alone does not make an object live.", gc: .coreClr),
        Entry(term: "Retained size", text: "Bytes reachable only through an object or root source in the detected graph, including the object itself: its dominator subtree. Includes managed and native memory. It does not predict an immediate RSS reduction."),
        Entry(term: "Dominator", text: "An object or root source through which every path from the roots to another object passes."),
        Entry(term: "Root source", text: "Where reachability starts: a thread stack above its stack pointer, registers, strong GC handles, statics, immortal or frozen objects, or the finalizer queue. Writable native memory also roots native allocations. It roots managed objects directly only with Native memory roots enabled."),
        Entry(term: "GC handles", text: "Runtime handle-table entries. Strong and pinned handles are roots; weak handles are not. A dependent handle keeps its secondary object alive only while its primary is alive."),
        Entry(term: "Unreferenced dead object", text: "A dead object with no references from other dead objects. It can be the start of an abandoned data structure; its retained garbage consists of dead objects reachable only through it."),
        Entry(term: "Free and dead space (fragmentation)", text: "Dead objects and unused committed space, grouped by the GC rule that keeps it. Each region's space is attributed to its survivors in proportion to their sizes. GC storage, objects pending finalization and unparsed bytes are excluded."),
        Entry(term: "malloc arena", text: "A glibc allocator with one or more heaps: the main arena uses the brk heap; other arenas use heaps aligned to 64 MB. Chunk headers supply in-use and free sizes. The top chunk is the unused end of a heap."),
        Entry(term: "Top chunk", text: "The free end of a glibc heap; freeing memory at that end enlarges it. The trim threshold controls when glibc can return it to the OS. Dynamic thresholds can grow after large allocations are freed; MALLOC_TRIM_THRESHOLD_ fixes the threshold. malloc_trim trims only the main arena's top chunk."),
        Entry(term: "malloc_trim(0)", text: "Releases whole pages inside free chunks of every arena with MADV_DONTNEED, and trims the main arena's top chunk. The Native Memory page estimates the releasable bytes; other arenas' top chunks remain."),
        Entry(term: "Arena", text: "glibc assigns threads to malloc arenas, up to eight arenas per CPU core by default on 64-bit systems. MALLOC_ARENA_MAX sets the limit. Free memory in one arena cannot serve allocations in another."),
        Entry(term: "Frame (estimated)", text: "The function inferred from the nearest return address below a stack slot: a code address just after a call instruction. An earlier, deeper call can leave an address behind, so the inferred frame can be wrong."),
        Entry(term: "Leak candidate", text: "An allocated malloc chunk with no detected pointer from stacks, registers, library data, other native memory, chunks or managed objects. Freed tcache and fastbin chunks are excluded. Encoded pointers and unsupported allocator structures can hide owners."),
        Entry(term: "Held by managed objects", text: "Malloc memory reachable only through managed objects, directly through SafeHandle.handle or IntPtr fields, or through other chunks. It can remain allocated until Dispose or a finalizer releases it. Dead owners with no pending finalizer need separate investigation."),
        Entry(term: "Background GC", text: "A gen2 collection that marks and sweeps while application threads run. It never compacts; free lists hold space for reuse and empty regions can be freed. Enabled by default with System.GC.Concurrent. Induced collections, allocation failure, heap hard limits, GCConserveMemory or high memory load can require a blocking gen2 collection.", gc: .coreClr),
        Entry(term: "DATAS", text: "Dynamic adaptation to application sizes, enabled by default for server GC since .NET 9. Adjusts heap count by time spent in GC, sets the gen0 budget from live size and sets GCConserveMemory to 5. Small processes therefore need not keep one heap and allocation budget per core.", gc: .coreClr),
        Entry(term: "Free region", text: "A region with no objects, kept for reuse with its memory still committed. After GC, regions unlikely to be reused or free for too many collections move to a decommit list. Server GC decommits gradually; workstation GC does part during collection and defers the rest.", gc: .coreClr),
        Entry(term: "Transparent huge pages", text: "Linux can back eligible anonymous memory with 2 MB pages. DOTNET_gcTHP, enabled by default on Linux, has Satori request MADV_HUGEPAGE and commit memory in whole 2 MB steps. Decommitted ranges lose that advice. smaps reports actual huge-page use.", gc: .satori),
    ]
}

struct GlossaryView: View {
    /// The GC of the open dump; terms of the other GC are left out. Without a dump, terms of both are shown.
    let gc: HeapKind.GC?

    var body: some View {
        List {
            SwiftUI.Section("Terms") {
                ForEach(Glossary.entries.filter { $0.gc == nil || gc == nil || $0.gc == gc }) { e in
                    VStack(alignment: .leading, spacing: 3) {
                        Text(e.term).font(.headline)
                        Text(e.text).foregroundStyle(.secondary).textSelection(.enabled)
                    }
                    .padding(.vertical, 3)
                }
            }
            ForEach(gc.map { [$0] } ?? [.satori, .coreClr], id: \.self) { gc in
                SwiftUI.Section(self.gc == nil ? "Generations, \(gc.rawValue) GC" : "Generations") {
                    ForEach(gc == .satori ? ["gen0", "gen1", "gen2", "immortal", "none"] : ["gen0", "gen1", "gen2", "LOH", "POH", "NonGC heap"], id: \.self) { g in
                        HStack(alignment: .top) {
                            Circle().fill(Palette.generation(g)).frame(width: 10, height: 10).padding(.top, 4)
                            VStack(alignment: .leading) { Text(g).font(.headline); Text(Glossary.generation(g, gc: gc)).foregroundStyle(.secondary) }
                        }
                    }
                }
            }
            if gc != .coreClr {
            SwiftUI.Section("Satori queues") {
                ForEach(["Allocator", "(in no queue)", "RecyclerEphemeral", "RecyclerEphemeralFinalizationTracking", "RecyclerTenured",
                         "RecyclerTenuredFinalizationTracking", "RecyclerReusable", "RecyclerDemoted", "RecyclerDeferredSweep", "RecyclerStaying",
                         "RecyclerRelocating", "RecyclerFinalizationPending"], id: \.self) { q in
                    let g = Glossary.queue(q)
                    VStack(alignment: .leading, spacing: 3) {
                        Text(g.name).font(.headline) + Text("  \(q)").font(.caption.monospaced()).foregroundColor(.secondary)
                        Text(g.help).foregroundStyle(.secondary)
                    }
                    .padding(.vertical, 3)
                }
            }
            }
        }
    }
}

/// A queue name with its plain name and an explanation on hover.
struct QueueLabel: View {
    let raw: String
    var body: some View {
        let q = Glossary.queue(raw)
        Text(q.name).help(q.help.isEmpty ? raw : "\(q.help)\n\nSatori queue: \(raw)")
    }
}
