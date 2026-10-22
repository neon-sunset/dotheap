# Live capture implementation

For commands, permissions, output paths and disk requirements, see
[Capture a process](../README.md#capture-a-process).

## Copying memory

Linux live capture copies memory while the target runs, then stops its threads to read registers
and finish the copy. It reads the mappings again at the final stop to account for changes to the
address space. File and shared mappings are copied again, including memfd mappings such as the
runtime's double-mapped JIT code. Other processes can still write shared memory during this stop.

Private anonymous pages are compared by hash. On x64, soft-dirty bits from `/proc/<pid>/pagemap`
reduce the pages that need comparison. Reading and resetting those bits through
`/proc/<pid>/clear_refs` requires brief stops. Without soft-dirty support, including on arm64,
all resident pages are compared. Up to three additional copies can run before the final stop.

A write within a 2 MiB transparent huge page marks the whole page dirty. Widely distributed writes
can therefore require another read of most of the heap. Pause time depends on the pages that must
be copied at the stop, available memory, memory-read speed and storage speed.

Memory reads use half the available CPU cores, with at least one worker. Copying before the
final pause uses at most eight workers.

## Storage and resource limits

The initial copies go into temporary Zstandard level-1 frames beside the output. The writer uses
256 KiB blocks and starts new frames at mapping boundaries. After the target resumes, it reuses
unchanged frames and compresses changed frames again. The final file includes a seek table so
the reader can locate frames without scanning the whole file.

Consumed temporary ranges are released with hole punching where the filesystem supports it.
This reduces peak disk use while the final file is assembled. Writes are flushed and evicted
from the page cache in 64 MiB batches, with about 128 MiB in flight. Free-space checks stop capture
when space available to non-root users falls below 2 GiB.

Pages copied at the final stop use up to one quarter of available memory, after accounting for
the remaining memory in enclosing cgroups. Additional pages go into a temporary mapped file.
Disk writes during this stop can lengthen the pause. Dotheap requests `oom_score_adj=1000` to make
itself a preferred OOM victim; the kernel can still kill the target or another process.

Temporary files use `O_TMPFILE` where supported. The fallback uses unique names; these files can
remain if Dotheap is forcibly killed.

## Completion and failures

Capture fails if the target exits or replaces its address space before the final copy, or if a
required memory or register read fails at the final stop. Removing a mapped file does not by
itself make its memory unreadable. The Files view identifies deleted file mappings.

Once the target resumes, Dotheap reads `/proc/<pid>/smaps` and saves it beside the dump. This is a
separate Zstandard frame, not a sequence of the dump writer's 256 KiB blocks. If smaps cannot be
saved, capture reports a warning. The dump and smaps describe slightly different instants; see
[Resident memory](../README.md#resident-memory).

The output is published without replacing an existing file. Progress on stderr includes capture
phases, read and write rates, temporary disk use and individual pauses. The final report gives
the pause count, total pause time and longest pause.

Runtime capture requires `--offline`, including on macOS; it is never an automatic fallback.
It uses the .NET diagnostics protocol instead of this copying procedure. The target
stays paused while the runtime writes the raw dump. Compression starts after it resumes and needs
space for both the raw and compressed files. A compression or publication error reports the raw
dump's recovery path.
