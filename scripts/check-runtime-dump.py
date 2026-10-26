#!/usr/bin/env python3
"""Check runtime-capture output handling with a local diagnostics IPC stub."""
import os
from pathlib import Path
import select
import socket
import struct
import subprocess
import sys
import tempfile
import threading


def no_runtime_fallback(binary):
    with tempfile.TemporaryDirectory(prefix="dh-ipc-", dir="/tmp") as folder:
        output = Path(folder) / "capture.dmp.zst"
        with socket.socket(socket.AF_UNIX) as server:
            server.bind(str(Path(folder) / f"dotnet-diagnostic-{os.getpid()}-test-socket"))
            server.listen(1)
            for command in (["dump", str(os.getpid())], ["summary", "--pid", str(os.getpid())]):
                result = subprocess.run([binary, *command, "--out", str(output), "--json"],
                                        env={**os.environ, "TMPDIR": folder}, capture_output=True,
                                        text=True, timeout=90)
                # Direct capture depends on host permissions. Neither outcome may use the diagnostics socket.
                assert result.returncode in (0, 1), result.stderr
                if result.returncode == 0:
                    assert output.is_file() and output.stat().st_size > 0
                    output.unlink()
                else:
                    assert "--offline" in result.stderr, result.stderr
                    assert not result.stdout, result.stdout
                    assert not output.exists()
                assert not select.select([server], [], [], 0)[0], "runtime capture started without --offline"
        print("no-runtime-fallback: passed")


def check(binary, case):
    with tempfile.TemporaryDirectory(prefix="dh-ipc-", dir="/tmp") as folder:
        folder = Path(folder)
        output = folder / ("capture.dmp" if case in ("raw", "existing-raw", "raw-race") else "capture.dmp.zst")
        existing = case.startswith("existing-")
        if existing:
            output.write_bytes(b"existing output")
        neighbors = [folder / "capture.dmp", folder / "capture.dmp.zst.partial"] if case == "neighbors" else []
        for path in neighbors:
            path.write_bytes(b"existing neighbor")
        raw_paths, failures = [], []
        with socket.socket(socket.AF_UNIX) as server:
            server.bind(str(folder / f"dotnet-diagnostic-{os.getpid()}-test-socket"))
            server.listen(1)
            server.settimeout(10)

            def serve():
                try:
                    with server.accept()[0] as connection:
                        connection.settimeout(10)
                        request = bytearray()
                        while len(request) < 20 or len(request) < struct.unpack_from("<H", request, 14)[0]:
                            chunk = connection.recv(4096)
                            assert chunk, "incomplete diagnostics request"
                            request += chunk
                        assert request[:14] == b"DOTNET_IPC_V1\0"
                        units, = struct.unpack_from("<I", request, 20)
                        raw = Path(request[24:24 + (units - 1) * 2].decode("utf-16-le"))
                        raw_paths.append(raw)
                        assert raw != output and raw not in neighbors
                        raw.write_bytes(b"captured memory")
                        if case.endswith("race"):
                            output.write_bytes(b"concurrent output")
                        connection.sendall(b"DOTNET_IPC_V1\0" + struct.pack("<HBBHI", 24, 0xff, 0, 0, 0))
                except Exception as error:
                    failures.append(error)

            thread = threading.Thread(target=serve, daemon=True)
            if not existing:
                thread.start()
            result = subprocess.run([binary, "dump", str(os.getpid()), "--offline", "--out", str(output)],
                                    env={**os.environ, "TMPDIR": str(folder)}, capture_output=True, text=True, timeout=15)
            if not existing:
                thread.join(timeout=15)
                assert not thread.is_alive(), "runtime stub did not finish"
                assert not failures, failures
            if existing or case.endswith("race"):
                assert result.returncode == 1, result.stderr
                assert output.read_bytes() == (b"existing output" if existing else b"concurrent output")
                if not existing:
                    assert raw_paths[0].read_bytes() == b"captured memory"
                    assert str(raw_paths[0]) in result.stderr
            else:
                assert result.returncode == 0, result.stderr
                data = subprocess.check_output(["zstd", "-dc", str(output)]) if output.suffix == ".zst" else output.read_bytes()
                assert data == b"captured memory"
                assert output.stat().st_mode & 0o777 == 0o600
                assert not raw_paths[0].exists()
            for path in neighbors:
                assert path.read_bytes() == b"existing neighbor"
            assert not list(folder.glob(output.name + ".*.partial"))
        print(f"runtime-{case}: passed")


if __name__ == "__main__":
    binary = str(Path(sys.argv[1]).resolve())
    no_runtime_fallback(binary)
    for case in ("raw", "compressed", "existing-raw", "existing-compressed", "neighbors", "raw-race", "compressed-race"):
        check(binary, case)
