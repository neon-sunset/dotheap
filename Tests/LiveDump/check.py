#!/usr/bin/env python3
"""Exercise Linux live capture against changes made after the initial copy.

Run as root: python3 Tests/LiveDump/check.py <live_dump_checks>
"""
import ctypes
import json
import mmap
import os
from pathlib import Path
import select
import signal
import struct
import subprocess
import sys
import tempfile
import threading


def target(replacement=False):
    libc = ctypes.CDLL(None, use_errno=True)
    page = mmap.PAGESIZE
    fd = os.open("replacement-file" if replacement else "mapped-file", os.O_RDWR | os.O_CREAT | os.O_EXCL, 0o600)
    os.write(fd, b"\x11" * page)
    private = mmap.mmap(fd, page, flags=mmap.MAP_PRIVATE, prot=mmap.PROT_READ | mmap.PROT_WRITE)
    shared = mmap.mmap(fd, page, flags=mmap.MAP_SHARED, prot=mmap.PROT_READ | mmap.PROT_WRITE)
    private_address = ctypes.addressof(ctypes.c_char.from_buffer(private))
    shared_address = ctypes.addressof(ctypes.c_char.from_buffer(shared))
    anonymous = mmap.mmap(-1, page, flags=mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS)
    anonymous[0] = 0x55 if replacement else 0x66
    anonymous_address = ctypes.addressof(ctypes.c_char.from_buffer(anonymous))
    new_mapping = None
    workers = []
    wait = threading.Event()
    # Touch both mappings before capture; each starts with the same file contents.
    assert private[0] == shared[0] == 0x11
    print(json.dumps({"pid": os.getpid(), "private": private_address, "shared": shared_address,
                      "anonymous": anonymous_address}), flush=True)
    for command in sys.stdin:
        if command.strip() == "change":
            private[0] = 0x22
            shared[0] = 0x33
            assert libc.mprotect(ctypes.c_void_p(private_address), page, mmap.PROT_READ) == 0
            assert libc.mprotect(ctypes.c_void_p(shared_address), page, mmap.PROT_READ) == 0
        elif command.strip() in ("delete", "redeploy"):
            os.unlink("mapped-file")
            Path("mapped-file").write_bytes(b"\x44" * page)
            if command.strip() == "redeploy":
                os.kill(os.getpid(), signal.SIGKILL)
        elif command.strip() == "exit":
            return
        elif command.strip() == "exec":
            os.execv(sys.executable, [sys.executable, str(Path(__file__).resolve()), "--replacement"])
        elif command.strip() == "truncate":
            os.ftruncate(fd, 0)
        elif command.strip() == "discard":
            anonymous.madvise(mmap.MADV_DONTNEED)
        elif command.strip() == "remap":
            libc.mmap.restype = ctypes.c_void_p
            libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_long]
            address = libc.mmap(anonymous_address, page, mmap.PROT_READ | mmap.PROT_WRITE,
                                mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS | 0x10, -1, 0)
            assert address == anonymous_address
            ctypes.c_ubyte.from_address(address).value = 0x77
        elif command.strip() == "protect":
            anonymous[0] = 0x88
            assert libc.mprotect(ctypes.c_void_p(anonymous_address), page, mmap.PROT_READ) == 0
        elif command.strip() == "new-mapping":
            new_mapping = mmap.mmap(-1, page, flags=mmap.MAP_PRIVATE | mmap.MAP_ANONYMOUS)
            new_mapping[0] = 0x99
            print(json.dumps({"new": ctypes.addressof(ctypes.c_char.from_buffer(new_mapping))}), flush=True)
            continue
        elif command.strip() == "new-threads":
            workers = [threading.Thread(target=wait.wait, daemon=True) for _ in range(3)]
            for worker in workers:
                worker.start()
            print(json.dumps({"threads": [worker.native_id for worker in workers]}), flush=True)
            continue
        print("ready", flush=True)


def line(process, timeout=30):
    if not select.select([process.stdout], [], [], timeout)[0]:
        raise AssertionError("timed out waiting for test process")
    value = process.stdout.readline()
    if not value:
        raise AssertionError(f"test process exited: {process.poll()}")
    return value.strip()


def read_core(path):
    data = subprocess.check_output(["zstd", "-dc", str(path)], timeout=30)
    assert data[:6] == b"\x7fELF\x02\x01"
    phoff, = struct.unpack_from("<Q", data, 32)
    phnum, = struct.unpack_from("<H", data, 56)
    loads, notes = [], []
    for i in range(phnum):
        kind, _, offset, address, _, size = struct.unpack_from("<IIQQQQ", data, phoff + i * 56)
        if kind == 1:
            loads.append((address, size, offset))
        elif kind == 4:
            end = offset + size
            while offset < end:
                namesz, descsz, kind = struct.unpack_from("<III", data, offset)
                desc = offset + 12 + (namesz + 3) // 4 * 4
                notes.append((kind, data[desc:desc + descsz]))
                offset = desc + (descsz + 3) // 4 * 4

    def byte(address):
        for start, size, offset in loads:
            if start <= address < start + size:
                return data[offset + address - start]
        raise AssertionError(f"address {address:x} is missing")

    return byte, notes


def check(driver, command):
    with tempfile.TemporaryDirectory(prefix="dotheap-live-check-") as folder:
        target_process = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "--target"],
                                          cwd=folder, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        capture = None
        try:
            info = json.loads(line(target_process))
            dump = Path(folder) / "capture.dmp.zst"
            if command == "existing":
                dump.write_bytes(b"existing dump")
            if command == "stopped":
                os.kill(info["pid"], signal.SIGSTOP)
                os.waitpid(info["pid"], os.WUNTRACED)
            capture_args = [driver, str(info["pid"]), str(dump)]
            if command == "cancel":
                capture_args.append("--cancel-while-stopped")
            capture = subprocess.Popen(capture_args, stdin=subprocess.PIPE,
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            if command == "existing":
                output, error = capture.communicate(timeout=30)
                assert capture.returncode != 0, "capture replaced an existing dump"
                assert dump.read_bytes() == b"existing dump", "capture changed the existing dump"
                return
            assert line(capture) == "before-stop"
            if command == "output-race":
                dump.write_bytes(b"existing dump")
            if command != "stopped":
                target_process.stdin.write(command + "\n")
                target_process.stdin.flush()
                if command in ("exit", "redeploy"):
                    target_process.wait(timeout=10)
                elif command == "exec":
                    info = json.loads(line(target_process))
                elif command in ("new-mapping", "new-threads"):
                    info.update(json.loads(line(target_process)))
                else:
                    assert line(target_process) == "ready"
            output, error = capture.communicate("continue\n", timeout=30)
            if command == "output-race":
                assert capture.returncode == 1, "capture replaced an output created during capture"
                assert dump.read_bytes() == b"existing dump"
                assert target_process.poll() is None
                assert "TracerPid:\t0\n" in Path(f"/proc/{info['pid']}/status").read_text()
                return
            if command in ("exit", "exec", "truncate", "redeploy", "cancel"):
                assert capture.returncode != 0, "capture succeeded without readable target memory"
                assert not dump.exists(), "failed capture left a dump"
                if command not in ("exit", "redeploy"):
                    assert target_process.poll() is None, "capture terminated the target"
                    assert "TracerPid:\t0\n" in Path(f"/proc/{info['pid']}/status").read_text()
                return
            assert capture.returncode == 0, error
            assert output.startswith("complete "), output
            assert dump.stat().st_mode & 0o777 == 0o600
            assert target_process.poll() is None, "capture terminated the target"
            status = Path(f"/proc/{info['pid']}/status").read_text()
            assert "TracerPid:\t0\n" in status, "capture left the target traced"
            if command == "stopped":
                assert "State:\tT (stopped)\n" in status, "capture resumed a stopped target"
            byte, notes = read_core(dump)
            expected = {"discard": 0, "remap": 0x77, "protect": 0x88}.get(command, 0x66)
            assert byte(info["anonymous"]) == expected, "anonymous mapping has stale contents"
            if command == "new-mapping":
                assert byte(info["new"]) == 0x99, "new mapping is missing or has stale contents"
            if command == "new-threads":
                tids = {struct.unpack_from("<i", desc, 32)[0] for kind, desc in notes if kind == 1}
                assert {info["pid"], *info["threads"]} <= tids, "new threads have no registers"
            if command == "change":
                assert byte(info["private"]) == 0x22, "private mapping lost a write before mprotect"
                assert byte(info["shared"]) == 0x33, "read-only shared mapping has stale contents"
            else:
                assert byte(info["private"]) == byte(info["shared"]) == 0x11, "deleted mapping used the replacement file"
            process_info = next(desc for kind, desc in notes if kind == 3)
            assert len(process_info) == 136
            assert struct.unpack_from("<i", process_info, 24)[0] == info["pid"], "NT_PRPSINFO has the wrong PID"
            assert process_info[40:56].split(b"\0")[0], "NT_PRPSINFO has no process name"
            assert process_info[56:136].split(b"\0")[0], "NT_PRPSINFO has no command line"
        finally:
            if capture is not None and capture.poll() is None:
                capture.kill()
                capture.wait(timeout=10)
            if target_process.poll() is None:
                target_process.kill()
                target_process.wait(timeout=10)


if __name__ == "__main__":
    if sys.argv[1] in ("--target", "--replacement"):
        target(replacement=sys.argv[1] == "--replacement")
    else:
        failures = []
        commands = ["change", "delete", "redeploy", "exec", "exit", "truncate", "existing", "output-race", "stopped",
                    "discard", "remap", "protect", "new-mapping", "new-threads"]
        if "--cancellation" in sys.argv[2:]:
            commands.append("cancel")
        for command in commands:
            try:
                check(str(Path(sys.argv[1]).resolve()), command)
                print(f"ok    {command}", flush=True)
            except Exception as error:
                failures.append(command)
                print(f"FAIL  {command}: {error}", flush=True)
        sys.exit(bool(failures))
