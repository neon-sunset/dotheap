# Swift and Rust interop

The Viewer links `heap-bridge` statically. `scripts/generate-bridge-models.py` generates a C ABI
and immutable Swift projections from the bridge API and its Rust models. Both sides are built
together; the generated ABI is internal to Dotheap.

## Ownership

A Rust result transfers a pointer and its destructor to Swift. A `Box<T>` transfers its allocation;
an `Arc<T>` transfers one strong reference to the existing allocation. The Swift `RustAllocation`
is noncopyable and calls that destructor in `deinit`. Destruction reconstructs the original Rust
owner, drops nested fields and releases the allocation with its original layout. Calling `free`
directly would not satisfy this contract, even when both languages use the same system allocator.

Copyable Swift records and collections retain a shared `RustOwner`. A nested record or array slice
can outlive the snapshot or query result from which it came. The allocation is released when its
last Swift view is released. Reading a record creates a small value containing a pointer and an
owner reference; it does not allocate or copy the Rust record. Each transferred result needs one
Swift owner allocation, and copying a view can retain that owner.

The reverse direction follows the same rule. Swift transfers one retain of a progress callback
owner to Rust. Rust invokes the supplied Swift release callback on destruction, including error
and panic paths. Strings and records passed as arguments are borrowed for the duration of the
call unless the Rust API explicitly takes ownership.

## Data access

- Scalar heap and graph columns expose their original contiguous buffers through `RustArray`,
  a read-only `RandomAccessCollection`. Indexing reads the buffer directly.
- Scalar-only records use explicit Rust `repr(C)` layouts and matching C declarations. Swift
  reads their fields directly.
- Other records use generated typed field readers. Rust interprets its own `Vec`, `String`,
  `Option`, enum and map layouts; Swift does not assume those layouts are stable.
- Nested arrays expose a pointer, length and element stride. They retain the same owner as the
  containing record. Numeric maps use direct lookups in the Rust map.
- Cached query results transfer an `Arc` reference. Repeated calls share the cached record and
  array storage.

Equality checks recognize shared storage before comparing values. SwiftUI can compare views
frequently; comparing a view with itself must not walk the heap or graph columns.

Keep queries near the data they inspect. Native-root membership and type-name lookup run in Rust
with borrowed arguments. File RSS is aggregated once in the cached file report; table comparisons
read that value. Native groups retain their original chunk lists, and selecting a group requests
only its largest 500 chunks. A field getter must not hide a full-report copy, scan or sort.

Swift strings are constructed from borrowed UTF-8 when a text property is read. Enum tags become
Swift enum values. The small handle-count dictionary and fragmentation region-index sets are
materialized as Swift collections. Sorting and filtering can also create Swift arrays of views.
These operations do not serialize a result or copy the underlying Rust object graph.

Shared data is immutable. The bridge does not expose mutable pointers, and Rust must not resize
or mutate storage while Swift views exist. API changes that need mutation must specify a separate
ownership contract. Swift `Span` is suitable for a scoped borrow; the stored views used by SwiftUI
need ownership that can outlive the call.

## Viewer navigation

The current section uses one SwiftUI hosting view. Switching sections replaces its content and
releases the previous page's controls and local state. Analysis results remain cached per snapshot;
selection and navigation history belong to the session. Keeping every visited page attached, even
when hidden, makes window resizing traverse all of those view trees.

The selection highlight is committed before constructing the destination page, so its layout
cannot hold up that selection update. A later selection or snapshot change cancels obsolete work.
A link to a row clears a filter that would hide that row.

The split view supplies page dimensions. The page container accepts those dimensions directly;
asking AppKit to derive a size from page content adds layout work to navigation. Query benchmarks
do not measure this cost. Profile the main thread during navigation, window resizing and sidebar
changes as well as the data preparation that precedes them.

## Build and validation

Run `scripts/build-bridge.sh debug` after changing a Rust model or bridge method, then `swift test`.
Commit the generated Rust, C and Swift files with the source change. CI regenerates them and checks
for drift.

Rust tests verify nested destruction, over-aligned allocations, `Arc` transfer and error cleanup.
Swift tests verify pointer identity, escaped column and nested-slice lifetimes, cached report
sharing, borrowed inputs, callback release and analysis results. Viewer tests check session and
appearance updates, resizing, keyboard focus, and release of inactive content and the previous snapshot.
Real-dump comparisons and navigation profiling complement these tests.
