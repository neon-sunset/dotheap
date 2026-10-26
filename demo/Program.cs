using System.Buffers;
using System.Collections.Concurrent;
using System.Net;
using System.Net.Sockets;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using System.Text.RegularExpressions;
using System.Threading.Channels;
using Shop;

// A process with known memory use, for screenshots and tests: a response cache, compiled routes, a request log, user
// sessions, metrics, a buffer pool, garbage left over several rounds, objects waiting for finalization, and native
// memory owned, leaked and held in known ways. Prints "ready <pid>" when done.
var random = new Random(42);
string[] languages = ["en", "de", "fr", "ja"];
string[] agents = ["Mozilla/5.0 (Windows NT 10.0; Win64; x64)", "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5)", "curl/8.5.0", "Go-http-client/2.0"];

// Requests arrive in rounds; each round leaves some objects alive among a lot of garbage.
for (int round = 0; round < 6; round++)
{
    for (int i = 0; i < 12_000; i++)
    {
        var id = round * 12_000 + i;
        var key = $"/api/products/{id}?lang={languages[id % 4]}";
        var body = new byte[512 + random.Next(6_000)];
        random.NextBytes(body.AsSpan(0, 64));
        var temporary = Encoding.UTF8.GetBytes($"GET {key} HTTP/1.1\r\nHost: shop.example\r\nUser-Agent: {agents[id % 4]}\r\n\r\n");
        if (i % 3 != 0) ResponseCache.Entries[key] = new CacheEntry(key, body, DateTime.UtcNow.AddMinutes(id % 60), temporary.Length);
        RequestLog.Rows.Enqueue(new LogRow(DateTime.UtcNow, new string(agents[id % 4]), id % 17 == 0 ? 404 : 200, body.Length));
        if (i % 4 == 0) Sessions.Add(Guid.NewGuid());
    }
    for (int i = 0; i < 20_000; i++) _ = new string('x', 20 + i % 200);
    for (int i = 0; i < 40; i++) _ = new Connection();
    GC.Collect(round % 2);
}

for (int i = 0; i < 5_000; i++) Metrics.Counters[$"http.requests.{languages[i % 4]}.{i}"] = new long[512];
foreach (var pattern in RouteTable.Patterns) RouteTable.Routes.Add(new Route(pattern, new Regex(pattern, RegexOptions.NonBacktracking)));
foreach (var route in RouteTable.Routes)
    for (int i = 0; i < 3_000; i++) route.Pattern.IsMatch($"/api/products/{i}/reviews?page={random.Next(100)}&sort=new");
for (int i = 0; i < 64; i++) BufferPool.Buffers.Add(GC.AllocateArray<byte>(64 * 1024, pinned: true));
for (int i = 0; i < 20_000; i++) Metrics.Samples.Add(i % 100);

// Native memory owned by managed objects, through an IntPtr field and through a SafeHandle.
for (int i = 0; i < 200; i++) NativeState.Buffers.Add(new NativeBuffer(4096));
for (int i = 0; i < 50; i++) NativeState.Handles.Add(new MallocHandle(65536));
// Leaked native memory: nothing points to it.
for (int i = 0; i < 400; i++) { var p = Marshal.AllocHGlobal(8192); unsafe { new Span<byte>((void*)p, 8192).Fill(0x5A); } }
// Native memory that keeps a managed object alive through a GC handle.
var held = new Session(Guid.NewGuid(), "held only by a GC handle stored in native memory");
NativeState.Block = Marshal.AllocHGlobal(256);
Marshal.WriteIntPtr(NativeState.Block, 8, GCHandle.ToIntPtr(GCHandle.Alloc(held)));
held = null;
// A native list: block A points to block B.
var head = Marshal.AllocHGlobal(512);
Marshal.WriteIntPtr(head, 0, Marshal.AllocHGlobal(100_000));
NativeState.Owner = new NativeOwner(head);
NativeState.Layouts = new NativeLayouts();
// Crypto objects with native state; one is not disposed.
NativeState.Key = RSA.Create(2048);
_ = RSA.Create(2048);

for (int i = 0; i < 4; i++) new Thread(() => Thread.Sleep(Timeout.Infinite)) { Name = $"worker {i}", IsBackground = true }.Start();

// Cases for the checks.
// ArrayPool: one array returned twice, one kept in a field after it was returned.
var twice = ArrayPool<byte>.Shared.Rent(4096);
ArrayPool<byte>.Shared.Return(twice);
ArrayPool<byte>.Shared.Return(twice);
Leaky.Buffer = ArrayPool<byte>.Shared.Rent(8192);
ArrayPool<byte>.Shared.Return(Leaky.Buffer);
// An event that subscribers join and never leave.
for (int i = 0; i < 2_000; i++) Bus.Changed += new Subscriber(i).OnChanged;
// A long-lived token source with registrations nobody disposes.
for (int i = 0; i < 1_500; i++) Bus.Shutdown.Token.Register(static s => GC.KeepAlive(s), new Subscriber(i));
// Timers created per operation and never disposed.
for (int i = 0; i < 1_200; i++) Bus.Timers.Add(new Timer(Waiter.OnTimer, new Subscriber(i), TimeSpan.FromHours(1), Timeout.InfiniteTimeSpan));
// Async methods that wait for a TaskCompletionSource nothing else holds: they never finish.
for (int i = 0; i < 300; i++) Waiter.Pending.Add(Waiter.Outer());
// Batches that wait for all their jobs, jobs that wait on a gate nobody opens, readers of one channel, a call that
// Task.Run started, and pairs of calls that wait on each other.
for (int i = 0; i < 10; i++) Waiter.Pending.Add(Jobs.RunBatch());
for (int i = 0; i < 8; i++) Waiter.Pending.Add(Jobs.ReadAll());
Waiter.Pending.Add(Task.Run(Jobs.Tick));
for (int i = 0; i < 3; i++) Jobs.StartPair();
Jobs.StartRunning();
// Threads that hold locks: one holds a lock that three others wait for, two wait for each other's locks, and one
// holds a System.Threading.Lock that another waits for.
Locks.Start();
// Thread pool threads blocked on a task (sync over async).
var never = new TaskCompletionSource();
for (int i = 0; i < 4; i++) _ = Task.Run(() => never.Task.Wait());
// Timers of other kinds: a PeriodicTimer that a loop awaits, a token source that cancels itself after an hour, and a
// System.Timers.Timer with an Elapsed handler.
Clock.Start();
// A file written through a FileStream: the bytes wait in its buffer.
Files.Start();
// HttpClient instances created per use.
for (int i = 0; i < 25; i++) Bus.Clients.Add(new HttpClient());
// Faulted tasks nobody observes.
for (int i = 0; i < 60; i++) Bus.Faulted.Add(Task.Run(() => throw new InvalidOperationException("demo failure")));
while (Bus.Faulted.Any(t => !t.IsCompleted)) Thread.Sleep(10);
// HttpClient requests to a server that accepts their connections and never answers.
Net.Start();
for (int i = 0; i < 1000 && Net.Accepted.Count < 5; i++) Thread.Sleep(10);
if (Net.Accepted.Count < 5) Console.Error.WriteLine($"demo: the server accepted {Net.Accepted.Count} of the 5 connections in 10 s; the network checks will fail");
// A starved thread pool: 20 more work items than the pool may have threads, each blocked on a gate nobody opens, so at
// least 20 wait in the queue. Five async calls are then ready to resume, and their continuations wait in the queue too.
var resume = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
for (int i = 0; i < 5; i++) Waiter.Pending.Add(Pool.Resume(resume.Task));
ThreadPool.GetMaxThreads(out _, out int completionPorts);
int maxWorkers = Math.Max(Environment.ProcessorCount, ThreadPool.ThreadCount) + 4;
ThreadPool.SetMaxThreads(maxWorkers, completionPorts);
for (int i = 0; i < maxWorkers + 20; i++) ThreadPool.QueueUserWorkItem(Pool.Block);
for (int i = 0; i < 1000 && ThreadPool.PendingWorkItemCount < 20; i++) Thread.Sleep(10);
resume.SetResult();
if (Environment.GetEnvironmentVariable("DEMO_WRITES") == "1") Writes.Start();

FinalizerBlocker.Start();
// Keep the garbage alive through setup and collection, then remove its references for the dump.
object[] garbage = new object[620];
int garbageIndex = 0;
for (int i = 0; i < 400; i++) garbage[garbageIndex++] = Waiter.WaitForever();
for (int i = 0; i < 100; i++) garbage[garbageIndex++] = ArrayPool<byte>.Shared.Rent(16384);
for (int i = 0; i < 20; i++) garbage[garbageIndex++] = new byte[1 << 20];
for (int i = 0; i < 30; i++) garbage[garbageIndex++] = new MallocHandle(65536);
for (int i = 0; i < 20; i++)
{
    var handle = new NoopHandle();
    handle.Dispose();
    garbage[garbageIndex++] = handle;
}
for (int i = 0; i < 50; i++) garbage[garbageIndex++] = new RawBuffer(32768);

string ready = $"ready {Environment.ProcessId}";
var output = Console.Out;
GC.Collect();
Array.Clear(garbage);
output.WriteLine(ready);
output.Flush();
Thread.Sleep(Timeout.Infinite);

namespace Shop
{
    static class ResponseCache
    {
        public static readonly ConcurrentDictionary<string, CacheEntry> Entries = new();
    }

    record CacheEntry(string Key, byte[] Body, DateTime Expires, int RequestBytes);

    static class RouteTable
    {
        public static readonly string[] Patterns =
        [
            @"^/api/products/(\d+)$", @"^/api/products/(\d+)/reviews$", @"^/api/users/([a-z0-9-]+)/orders$", @"^/static/.+\.(css|js|png|svg)$",
            @"(?i)select\s+.+\s+from\s+\w+", @"(?i)<script[^>]*>", @"^/api/search\?q=[^&]{1,200}(&page=\d+)?$", @"\.\./",
        ];
        public static readonly List<Route> Routes = [];
    }

    record Route(string Template, Regex Pattern);

    static class RequestLog
    {
        public static readonly ConcurrentQueue<LogRow> Rows = new();
    }

    record LogRow(DateTime Time, string UserAgent, int Status, int Bytes);

    static class Sessions
    {
        public static readonly Dictionary<Guid, Session> All = [];
        public static void Add(Guid id) => All[id] = new Session(id, "user-" + id.ToString("N")[..8]);
    }

    class Session(Guid id, string name)
    {
        public Guid Id = id;
        public string Name = name;
        public List<string> Recent = new(64);
    }

    static class Metrics
    {
        public static readonly Dictionary<string, long[]> Counters = [];
        public static readonly List<object> Samples = [];
    }

    static class BufferPool
    {
        public static readonly List<byte[]> Buffers = [];
    }

    class Connection
    {
        ~Connection() => Thread.Sleep(0);
    }

    static class NativeState
    {
        public static readonly List<NativeBuffer> Buffers = [];
        public static readonly List<MallocHandle> Handles = [];
        public static IntPtr Block;
        public static NativeOwner? Owner;
        public static NativeLayouts? Layouts;
        public static RSA? Key;
    }

    unsafe class NativeBuffer
    {
        public IntPtr Data;
        public int Length;

        public NativeBuffer(int length)
        {
            Data = Marshal.AllocHGlobal(length);
            Length = length;
            new Span<byte>((void*)Data, length).Fill(1);
        }
    }

    unsafe class MallocHandle : SafeHandle
    {
        public MallocHandle(int size) : base(IntPtr.Zero, true)
        {
            SetHandle(Marshal.AllocHGlobal(size));
            // Old vtable pointers can change the label used by the native memory checks.
            new Span<byte>((void*)handle, size).Clear();
        }
        public override bool IsInvalid => handle == IntPtr.Zero;
        protected override bool ReleaseHandle() { Marshal.FreeHGlobal(handle); return true; }
    }

    class RawBuffer(int length)
    {
        public IntPtr Data = Marshal.AllocHGlobal(length);
    }

    class NoopHandle() : SafeHandle(IntPtr.Zero, true)
    {
        public override bool IsInvalid => false;
        protected override bool ReleaseHandle() => true;
    }

    sealed class FinalizerBlocker
    {
        private static readonly ManualResetEventSlim Entered = new();

        [MethodImpl(MethodImplOptions.NoInlining)]
        private static void Create() => _ = new FinalizerBlocker();

        public static void Start()
        {
            Create();
            GC.Collect();
            if (!Entered.Wait(TimeSpan.FromSeconds(10)))
                throw new InvalidOperationException("The demo finalizer did not start.");
        }

        ~FinalizerBlocker()
        {
            Entered.Set();
            Thread.Sleep(Timeout.Infinite);
        }
    }

    static class Leaky
    {
        public static byte[]? Buffer;
    }

    static class Bus
    {
        public static event EventHandler? Changed;
        public static readonly CancellationTokenSource Shutdown = new();
        public static readonly List<Timer> Timers = [];
        public static readonly List<HttpClient> Clients = [];
        public static readonly List<Task> Faulted = [];
        public static void Raise() => Changed?.Invoke(null, EventArgs.Empty);
    }

    class Subscriber(int id)
    {
        public int Id = id;
        public void OnChanged(object? sender, EventArgs e) => Id++;
    }

    static class Waiter
    {
        public static readonly List<Task> Pending = [];

        public static async Task WaitForever()
        {
            var signal = new TaskCompletionSource();
            await signal.Task;
        }

        public static async Task Outer() => await WaitForever();

        public static void OnTimer(object? state) => GC.KeepAlive(state);
    }

    static class Jobs
    {
        public static readonly SemaphoreSlim Gate = new(0);
        public static readonly Channel<int> Queue = Channel.CreateUnbounded<int>();

        public static async Task RunBatch() => await Task.WhenAll(Fetch(), Fetch(), Fetch());

        static async Task Fetch() => await Gate.WaitAsync();

        public static async Task ReadAll()
        {
            while (await Queue.Reader.WaitToReadAsync()) Queue.Reader.TryRead(out _);
        }

        public static async Task Tick() => await Task.Delay(TimeSpan.FromHours(1));

        public static void StartRunning()
        {
            var resume = new TaskCompletionSource();
            var started = new TaskCompletionSource();
            Waiter.Pending.Add(RunContinuously(resume.Task, started));
            // Resume inline on a dedicated thread, independent of the starved thread pool.
            new Thread(resume.SetResult) { Name = "running task", IsBackground = true }.Start();
            started.Task.GetAwaiter().GetResult();
        }

        static async Task RunContinuously(Task resume, TaskCompletionSource started)
        {
            await resume;
            started.SetResult();
            while (true) Thread.SpinWait(1024);
        }

        public static void StartPair()
        {
            var first = new TaskCompletionSource<Task>();
            var second = new TaskCompletionSource<Task>();
            Task a = WaitFor(first.Task), b = WaitFor(second.Task);
            Waiter.Pending.Add(a);
            Waiter.Pending.Add(b);
            first.SetResult(b);
            second.SetResult(a);
        }

        static async Task WaitFor(Task<Task> other) => await await other;
    }

    static class Net
    {
        static readonly TcpListener Listener = new(IPAddress.Loopback, 0);
        static readonly HttpClient Client = new() { Timeout = Timeout.InfiniteTimeSpan };
        public static readonly List<TcpClient> Accepted = [];
        public static readonly List<Task> Requests = [];

        public static void Start()
        {
            Listener.Start();
            _ = Accept();
            int port = ((IPEndPoint)Listener.LocalEndpoint).Port;
            for (int i = 0; i < 5; i++) Requests.Add(Client.GetAsync($"http://127.0.0.1:{port}/orders/{i}"));
        }

        static async Task Accept()
        {
            while (true)
            {
                var client = await Listener.AcceptTcpClientAsync();
                lock (Accepted) Accepted.Add(client);
            }
        }
    }

    static class Pool
    {
        static readonly ManualResetEventSlim Gate = new();
        public static void Block(object? state) => Gate.Wait();

        public static async Task Resume(Task ready)
        {
            await ready;
            Gate.Wait();
        }
    }

    static class Locks
    {
        public static readonly object Cache = new();
        public static readonly object First = new(), Second = new();
        public static readonly Lock Registry = new();

        public static void Start()
        {
            using var cacheHeld = new ManualResetEventSlim();
            Run("cache owner", () => { lock (Cache) { cacheHeld.Set(); Thread.Sleep(Timeout.Infinite); } });
            cacheHeld.Wait();
            for (int i = 0; i < 3; i++) Run($"cache reader {i}", () => { lock (Cache) { } });
            var both = new Barrier(2);
            Run("first then second", () => { lock (First) { both.SignalAndWait(); lock (Second) { } } });
            Run("second then first", () => { lock (Second) { both.SignalAndWait(); lock (First) { } } });
            using var registryHeld = new ManualResetEventSlim();
            Run("registry owner", () => { lock (Registry) { registryHeld.Set(); Thread.Sleep(Timeout.Infinite); } });
            registryHeld.Wait();
            Run("registry reader", () => { lock (Registry) { } });
        }

        static void Run(string name, ThreadStart work) => new Thread(work) { Name = name, IsBackground = true }.Start();
    }

    static class Clock
    {
        public static readonly PeriodicTimer Ticker = new(TimeSpan.FromMinutes(5));
        public static readonly CancellationTokenSource Deadline = new(TimeSpan.FromHours(1));
        public static readonly System.Timers.Timer Heartbeat = new(TimeSpan.FromMinutes(10));
        public static int Ticks;

        public static void Start()
        {
            Waiter.Pending.Add(Loop());
            Heartbeat.Elapsed += OnHeartbeat;
            Heartbeat.Start();
        }

        static async Task Loop()
        {
            while (await Ticker.WaitForNextTickAsync()) Ticks++;
        }

        static void OnHeartbeat(object? sender, System.Timers.ElapsedEventArgs e) => Ticks++;
    }

    // For dumps taken while the process runs (scripts/check-while-running.py): a thread writes rounds of increasing
    // numbers into an array without stopping, so in memory of one moment the numbers drop by one at most at one place.
    static class Writes
    {
        public const long Marker = 0x2E70_6165_6874_6F64; // "dotheap."
        public static long[]? Rounds;

        public static void Start()
        {
            var rounds = Rounds = new long[8 << 20];
            rounds[0] = Marker;
            rounds[1] = rounds.Length;
            new Thread(() =>
            {
                for (long round = 1; ; round++)
                    for (int i = 2; i < rounds.Length; i++) Volatile.Write(ref rounds[i], round);
            }) { Name = "writes", IsBackground = true }.Start();
        }
    }

    static class Files
    {
        public static FileStream? Log;

        // The file is deleted at once: the open handle keeps it, and nothing is left on the disk.
        public static void Start()
        {
            string path = Path.Combine(Path.GetTempPath(), $"dotheap-demo-{Environment.ProcessId}.log");
            Log = new FileStream(path, FileMode.Create, FileAccess.Write, FileShare.ReadWrite | FileShare.Delete, 4096);
            Log.Write(new byte[100]);
            File.Delete(path);
        }
    }

    class NativeOwner(IntPtr head)
    {
        public IntPtr Head = head;
    }

    struct PointerValue
    {
        public nint Pointer;
    }

    struct NestedPointer
    {
        public PointerValue Value;
    }

    class NativeLayouts
    {
        public NestedPointer Nested;
        public PointerValue[] Structs = new PointerValue[2];
        public nint[] Pointers = new nint[2];
        public nint[,] Grid = new nint[1, 2];
        public long[] Numbers = new long[2];

        public NativeLayouts()
        {
            Nested.Value.Pointer = Allocate();
            for (int i = 0; i < 2; i++)
            {
                Structs[i].Pointer = Allocate();
                Pointers[i] = Allocate();
                Grid[0, i] = Allocate();
                Numbers[i] = Pointers[i];
            }
        }

        private static unsafe nint Allocate()
        {
            var pointer = Marshal.AllocHGlobal(3072);
            new Span<byte>((void*)pointer, 3072).Clear();
            return pointer;
        }
    }
}
