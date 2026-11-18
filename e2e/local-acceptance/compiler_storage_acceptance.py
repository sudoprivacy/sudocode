"""Exercise real cache eviction without placing coordination in its data tree."""

import ctypes
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

import psutil


SOURCE = Path(__file__).resolve().parents[2]
CAPACITY = 2 * 1024**3
LEGACY_REFUSAL = '''        # The backend owns every file under its data root, including eviction.
        # Never let it evict the lock or retained server-incarnation record.
        if any((self.cache / name).exists() for name in (".server.lock", ".server-owner.json")):
            self.disabled = True
            print("COMPILER_CACHE: legacy control files are inside the managed data root; preserving that store and using original Cargo.", flush=True)
            return False
'''
WORKER = '''import importlib.util,json,sys
from pathlib import Path
data=json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
spec=importlib.util.spec_from_file_location("storage_control_executor",Path(data["source"])/".github/scripts/local_acceptance.py")
helper=importlib.util.module_from_spec(spec);spec.loader.exec_module(helper)
helper.ROOT=Path(data["source"])
helper.run_command(data["command"],Path(data["cwd"]),Path(data["log"]))
'''


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


class Runner:
    """Bound only private controls, reusing the original executor and cleanup."""

    def __init__(self, directory):
        self.directory = directory
        self.commands = []
        self.guard = load("storage_control_cleanup", SOURCE / ".github/local-acceptance/parallel_guard.py")

    def execute(self, command, cwd, log, env=None):
        log = Path(log)
        log.parent.mkdir(parents=True, exist_ok=True)
        config = log.with_suffix(".input.json")
        config.write_text(json.dumps({"source": str(SOURCE), "command": list(map(str, command)),
                                      "cwd": str(cwd), "log": str(log)}), encoding="utf-8")
        environment = {**os.environ, **(env or {}), "CARGO_BUILD_JOBS": "1", "SCODE_ACCEPTANCE_BUILD_JOBS": "1"}
        if len(command) > 1 and command[1] == "--start-server":
            environment["SCCACHE_ERROR_LOG"] = str(self.directory / "server-stderr.log")
        environment = {key: value for key, value in environment.items()
                       if key.upper() not in {"GH_TOKEN", "GITHUB_TOKEN"}}
        started = time.monotonic()
        observed = {}
        timed_out = False
        with log.with_suffix(".worker.log").open("wb") as stream:
            process = subprocess.Popen([sys.executable, "-c", WORKER, str(config)], cwd=SOURCE,
                                       env=environment, stdout=stream, stderr=subprocess.STDOUT)
            while process.poll() is None:
                try:
                    for child in psutil.Process(process.pid).children(recursive=True):
                        observed[child.pid, child.create_time()] = child
                except psutil.NoSuchProcess:
                    pass
                if time.monotonic() - started > 40:
                    timed_out = True
                    self.guard.terminate_workers({"private-control": process}, observed)
                    break
                time.sleep(.05)
        diagnostic = {"command": list(map(str, command)), "elapsed_seconds": time.monotonic() - started,
                      "timed_out": timed_out, "worker_exit": process.returncode, "log": str(log)}
        self.commands.append(diagnostic)
        log.with_suffix(".diagnostic.json").write_text(json.dumps(diagnostic, indent=2) + "\n", encoding="utf-8")
        if timed_out or process.returncode:
            raise RuntimeError(f"Private storage control failed; original logs: {log}")
        result = json.loads(log.with_suffix(".command.json").read_text(encoding="utf-8"))
        assert result["exit_code"] == 0 and result["log_sha256"] == digest(log)
        return result


def seed_sparse(data, control):
    """Use file metadata to exercise the actual unchanged 2 GiB cache limit."""
    data.mkdir(parents=True)
    control.mkdir(parents=True, exist_ok=True)
    lock = control / ".server.lock"
    lock.write_bytes(b"\0")
    os.utime(lock, (946684800, 946684800))
    filler = data / "capacity-fixture.sparse"
    with filler.open("w+b") as stream:
        if os.name == "nt":
            from ctypes import wintypes
            import msvcrt

            kernel = ctypes.WinDLL("kernel32", use_last_error=True)
            kernel.DeviceIoControl.argtypes = [wintypes.HANDLE, wintypes.DWORD, wintypes.LPVOID, wintypes.DWORD,
                                              wintypes.LPVOID, wintypes.DWORD, ctypes.POINTER(wintypes.DWORD), wintypes.LPVOID]
            kernel.DeviceIoControl.restype = wintypes.BOOL
            returned = wintypes.DWORD()
            handle = msvcrt.get_osfhandle(stream.fileno())
            assert kernel.DeviceIoControl(handle, 0x900C4, None, 0, None, 0, ctypes.byref(returned), None), ctypes.get_last_error()
            kernel.SetFilePointerEx.argtypes = [wintypes.HANDLE, ctypes.c_longlong, ctypes.POINTER(ctypes.c_longlong), wintypes.DWORD]
            kernel.SetFilePointerEx.restype = wintypes.BOOL
            kernel.SetEndOfFile.argtypes = [wintypes.HANDLE]
            kernel.SetEndOfFile.restype = wintypes.BOOL
            position = ctypes.c_longlong()
            assert kernel.SetFilePointerEx(handle, CAPACITY - 1, ctypes.byref(position), 0), ctypes.get_last_error()
            assert kernel.SetEndOfFile(handle), ctypes.get_last_error()
        else:
            stream.truncate(CAPACITY - 1)
    os.utime(filler, (978307200, 978307200))
    if os.name == "nt":
        kernel.GetCompressedFileSizeW.argtypes = [wintypes.LPCWSTR, ctypes.POINTER(wintypes.DWORD)]
        kernel.GetCompressedFileSizeW.restype = wintypes.DWORD
        high = wintypes.DWORD()
        low = kernel.GetCompressedFileSizeW(str(filler), ctypes.byref(high))
        allocation = high.value << 32 | low
    else:
        allocation = filler.stat().st_blocks * 512
    assert allocation < 1024**2, ("Sparse fixture must not write 2 GiB", allocation)
    return {"logical_bytes": filler.stat().st_size, "allocated_bytes": allocation}


def retire_owned_server(active, tool):
    """Avoid RPC into the deliberately poisoned legacy daemon."""
    if active.server is None:
        return
    for process in active.owned_servers():
        if process.pid == active.server["pid"] and process.create_time() == active.server["birth"]:
            assert Path(process.exe()).resolve() == Path(tool["executable"]).resolve()
            process.terminate()
            process.wait(timeout=5)


def run(output, cache, tool):
    output.mkdir(parents=True, exist_ok=False)
    original = Path(cache.__file__).read_text(encoding="utf-8")
    location = 'self.control = self.cache.with_name(self.cache.name + ".control")'
    assert original.count(location) == 1 and original.count(LEGACY_REFUSAL) == 1
    restored = original.replace(location, "self.control = self.cache", 1).replace(LEGACY_REFUSAL, "", 1)
    mutant_file = output / "restored-inside-layout.py"
    mutant_file.write_text(restored, encoding="utf-8")
    mutant = load("restored_inside_storage_layout", mutant_file)
    cases = []
    for name, module in [("restored-inside-layout", mutant), ("current-outside-layout", cache)]:
        case = output / name
        case.mkdir()
        runner = Runner(case)
        workspace = case / "source"
        (workspace / "src").mkdir(parents=True)
        (workspace / "Cargo.toml").write_text(
            '[workspace]\n[package]\nname="storage-boundary"\nversion="0.0.0"\nedition="2021"\n', encoding="utf-8")
        (workspace / "src/lib.rs").write_text("pub fn answer() -> u32 {42}\n", encoding="utf-8")
        (workspace / "src/main.rs").write_text('fn main(){println!("{}",storage_boundary::answer());}\n', encoding="utf-8")
        runner.execute(["cargo", "generate-lockfile", "--offline"], workspace, case / "lock.log")
        active = module.Session(tool, case / "server", runner.execute, case / "data")
        seed = seed_sparse(active.cache, active.control)
        target = case / "target"
        row = {"variant": name, "seed": seed}
        failed = False
        try:
            assert active.start()
            inode = (active.control / ".server.lock").stat().st_ino
            try:
                active.execute(["cargo", "build", "--offline", "--locked"], workspace, case / "build.log",
                               {"CARGO_TARGET_DIR": str(target)})
            except RuntimeError:
                failed = True
            stable = ((active.control / ".server.lock").exists()
                      and (active.control / ".server.lock").stat().st_ino == inode)
            row.update(build_failed=failed, control_lock_stable=stable)
            if name == "restored-inside-layout":
                retire_owned_server(active, tool)
                text = (case / "server-stderr.log").read_text(encoding="utf-8", errors="replace")
                if os.name == "nt":
                    assert failed and ".server.lock" in text and "Error removing file from cache" in text, text
                else:
                    assert not stable, "The restored control must expose cache eviction of its live coordination lock"
                row["expected_old_layout_failure"] = True
            else:
                assert not failed and stable
                assert not (active.cache / ".server.lock").exists()
                assert not (active.cache / ".server-owner.json").exists()
                binary = target / "debug" / ("storage-boundary.exe" if os.name == "nt" else "storage-boundary")
                first = case / ("retained-first" + binary.suffix)
                shutil.copy2(binary, first)
                runner.execute([str(binary)], workspace, case / "program.log")
                assert (case / "program.log").read_text().strip() == "42"
                assert target.resolve().is_relative_to(output.resolve())
                runner.execute(["cargo", "clean", "--package", "storage-boundary", "--target-dir", str(target)],
                               workspace, case / "private-retirement.log")
                active.execute(["cargo", "build", "--offline", "--locked"], workspace, case / "cache-hit.log",
                               {"CARGO_TARGET_DIR": str(target)})
                stats = json.loads(active.tool_command("capacity-stats", "--show-stats", "--stats-format", "json"))
                assert stats["max_cache_size"] == CAPACITY
                row["actual_cache_hits"] = sum(stats["stats"]["cache_hits"]["counts"].values())
                assert row["actual_cache_hits"] > 0
                row["retained_first_program_sha256"] = digest(first)
        finally:
            if failed:
                retire_owned_server(active, tool)
            active.close()
        row["commands"] = runner.commands
        cases.append(row)
    # Explicit old stores stay intact, even when no old writer remains.
    legacy = output / "explicit-legacy-store"
    legacy.mkdir()
    fence = legacy / ".server.lock"
    fence.write_bytes(b"preserve legacy")
    runner = Runner(output)
    with cache.session(tool, output / "legacy-refusal", runner.execute, legacy) as refused:
        assert not refused.start() and refused.disabled and refused.server is None
    assert fence.read_bytes() == b"preserve legacy"
    result = {"status": "PASS", "cases": cases, "legacy_store_preserved_and_refused": True,
              "scope": "Actual pinned compiler/cache and sparse 2 GiB capacity control; restored-location mutant fails, current compilation/program/cache hit and stable external coordination pass. No product coverage credit."}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result), flush=True)
