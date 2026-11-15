"""Pinned, local compiler objects with private per-invocation server ownership."""

from contextlib import contextmanager
import errno
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import socket
import stat
import tarfile
import time
import uuid
import zipfile


VERSION = "0.19.0"
ARCHIVES = {
    ("windows", "x86_64"): ("x86_64-pc-windows-msvc", "zip", "28be8c61f6b2c89b9eb2869f6af7f78e6f992e78a8abae0ce855fcc129cbeac1"),
    ("windows", "aarch64"): ("aarch64-pc-windows-msvc", "zip", "66d20f1ea5e29f4f309136323635b864e1d8f0a8b587abbc22271bc22d439a5a"),
    ("linux", "x86_64"): ("x86_64-unknown-linux-musl", "tar.gz", "aa73c7270c997894038c0d8f0d297865f57569e1687883322bc431713d4bb9dd"),
    ("linux", "aarch64"): ("aarch64-unknown-linux-musl", "tar.gz", "3e47fc4a71b1ddd1b4294a0de9d34498922fdf41e3cd0bda5df3954e3d6deb9d"),
    ("darwin", "x86_64"): ("x86_64-apple-darwin", "tar.gz", "2db7344bb96de350df79cd9f0124dd4e9baa47b3de2fac1e63e1346b66b20684"),
    ("darwin", "aarch64"): ("aarch64-apple-darwin", "tar.gz", "a35a0b7d622618774bc1bb8102d72562dfb21e619f88b153b370d7f89bc286af"),
}

# Cargo's rustc wrapper during a session. Session.start compiles it with the
# host's rustc and builds in the verified sccache path.
WRAPPER_SOURCE = r"""//! Cargo's RUSTC_WRAPPER for the local compiler-object cache.
//!
//! Cargo passes `<compiler> <arguments>`, or `<compiler> @<file>` once the
//! operating system refused the arguments inline. sccache 0.17 and later reads
//! that file and starts rustc with every argument inline again, which Windows
//! refuses past 32,767 characters (mozilla/sccache#2787). Arguments that do not
//! fit inline therefore reach the compiler in a file, uncached; every other
//! unit goes through sccache.
use std::env::{args_os, current_exe};
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::PathBuf;
use std::process::{exit, Command, ExitStatus};
use std::time::{SystemTime, UNIX_EPOCH};

/// The sccache executable this session verified.
const CACHE: &str = include_str!("cache-tool");

fn main() {
    let mut arguments: Vec<OsString> = args_os().skip(1).collect();
    if arguments.is_empty() {
        fail("Cargo passes the compiler as the first argument");
    }
    let compiler = arguments.remove(0);
    if arguments.iter().any(|argument| argument.as_encoded_bytes().starts_with(b"@")) {
        record("cargo");
        finish(&compiler, Command::new(&compiler).args(&arguments).status());
    }
    match Command::new(CACHE).arg(&compiler).args(&arguments).status() {
        Err(error) if too_long(&error) => direct(&compiler, &arguments, &error),
        result => finish(CACHE.as_ref(), result),
    }
}

/// ERROR_FILENAME_EXCED_RANGE on Windows and E2BIG elsewhere, the errors on
/// which Cargo retries with an argument file.
fn too_long(error: &io::Error) -> bool {
    error.raw_os_error() == Some(if cfg!(windows) { 206 } else { 7 })
}

/// Start the compiler as Cargo would: programs inline, arguments in a file.
fn direct(compiler: &OsStr, arguments: &[OsString], error: &io::Error) -> ! {
    let programs = arguments
        .iter()
        .take_while(|argument| !argument.as_encoded_bytes().starts_with(b"-"))
        .count();
    let mut contents = String::new();
    for argument in &arguments[programs..] {
        match argument.to_str() {
            Some(text) if !text.contains('\n') => {
                contents.push_str(text);
                contents.push('\n');
            }
            _ => fail(&format!("cannot start {CACHE}: {error}; an argument has no argument-file form")),
        }
    }
    let file = directory().join(format!("{}.args", unique()));
    if let Err(write) = std::fs::write(&file, contents) {
        fail(&format!("cannot write {}: {write}", file.display()));
    }
    record("wrapper");
    let mut reference = OsString::from("@");
    reference.push(&file);
    let result = Command::new(compiler)
        .args(&arguments[..programs])
        .arg(reference)
        .status();
    let _ = std::fs::remove_file(&file);
    finish(compiler, result)
}

/// The session counts the units that went to the compiler in a file.
fn record(reason: &str) {
    let path = directory().join(format!("{}.{reason}", unique()));
    if let Err(error) = std::fs::write(&path, b"") {
        fail(&format!("cannot record {}: {error}", path.display()));
    }
}

fn directory() -> PathBuf {
    match current_exe() {
        Ok(executable) => executable.with_file_name("argument-files"),
        Err(error) => fail(&format!("cannot locate this wrapper: {error}")),
    }
}

fn unique() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("{}-{nanos}", std::process::id())
}

fn finish(program: &OsStr, result: io::Result<ExitStatus>) -> ! {
    match result {
        Ok(status) => exit(status.code().unwrap_or(101)),
        Err(error) => fail(&format!("cannot start {}: {error}", program.to_string_lossy())),
    }
}

fn fail(message: &str) -> ! {
    eprintln!("compiler-cache rustc wrapper: {message}");
    exit(101)
}
"""


def digest(contents):
    return hashlib.sha256(contents).hexdigest()


def configured(*environments):
    return any(key.upper().startswith("SCCACHE_") or key.upper() in {"RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"}
               for environment in environments for key in environment)


def archive_definition(system=None, machine=None):
    system = (system or platform.system()).lower()
    machine = (machine or platform.machine()).lower()
    machine = {"amd64": "x86_64", "arm64": "aarch64"}.get(machine, machine)
    if (system, machine) not in ARCHIVES:
        return None
    target, suffix, expected = ARCHIVES[system, machine]
    directory = f"sccache-v{VERSION}-{target}"
    return {"version": VERSION, "target": target, "filename": directory + "." + suffix,
            "sha256": expected, "member": directory + "/sccache" + (".exe" if system == "windows" else "")}


def executable_bytes(archive, definition):
    contents = archive.read_bytes()
    if digest(contents) != definition["sha256"]:
        raise RuntimeError("Pinned compiler-cache archive checksum mismatch")
    if definition["filename"].endswith(".zip"):
        with zipfile.ZipFile(io.BytesIO(contents)) as packed:
            entries = [entry for entry in packed.infolist() if entry.filename == definition["member"]]
            if len(entries) != 1 or entries[0].is_dir() or stat.S_ISLNK(entries[0].external_attr >> 16):
                raise RuntimeError("Compiler-cache archive must contain one regular executable")
            return packed.read(entries[0])
    with tarfile.open(fileobj=io.BytesIO(contents), mode="r:gz") as packed:
        entries = [entry for entry in packed.getmembers() if entry.name == definition["member"]]
        if len(entries) != 1 or not entries[0].isfile():
            raise RuntimeError("Compiler-cache archive must contain one regular executable")
        return packed.extractfile(entries[0]).read()


def prepare(download, cache=None):
    """Verify archive/member bytes before executing a cached compiler tool."""
    if configured(os.environ):
        print("COMPILER_CACHE: preserving existing wrapper/cache configuration; no additional tool download.", flush=True)
        return None
    definition = archive_definition()
    if definition is None:
        return None
    cache = Path(cache or Path.home() / ".cache/sudocode-local-acceptance/tools/sccache")
    directory = cache / VERSION / definition["target"]
    directory.mkdir(parents=True, exist_ok=True)
    archive = directory / definition["filename"]
    binary = directory / Path(definition["member"]).name
    if archive.is_symlink() or binary.is_symlink():
        raise RuntimeError("Compiler-cache tool inputs must be regular private cache files")
    if not archive.exists():
        download("mozilla/sccache", "v" + VERSION, definition["filename"], archive)
    expected = executable_bytes(archive, definition)
    if binary.exists():
        if binary.read_bytes() != expected:
            raise RuntimeError("Cached compiler-cache executable differs from its pinned archive")
    else:
        binary.write_bytes(expected)
        binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
    return {**definition, "archive": str(archive.resolve()), "executable": str(binary.resolve()),
            "executable_sha256": digest(expected)}


def compiler_frontend(command):
    command = list(map(str, command))
    stem = Path(command[0]).stem.lower()
    if stem == "cargo-nextest":
        return command[1:2] == ["nextest"] and command[2:3] in [["run"], ["list"], ["archive"]]
    return stem == "cargo" and command[1:2] in [[name] for name in (
        "build", "check", "clippy", "test", "bench", "run", "rustc", "doc", "rustdoc")]


class Session:
    """A finite, lazy compiler server; application/test children retain priority."""

    def __init__(self, tool, directory, execute, cache=None):
        self.tool, self.directory, self.execute_command = tool, Path(directory), execute
        self.cache = Path(cache or Path.home() / ".cache/sudocode-local-acceptance/compiler-objects" / VERSION)
        self.environment, self.server, self.wrapper = None, None, None
        self.tool_commands, self.records, self.priorities = [], [], {}
        self.server_history = []
        self.storage_lock = None
        self.disabled = False

    def tool_command(self, name, *arguments):
        if self.server is not None:
            self.assert_owner()
        log = self.directory / (name + ".log")
        result = self.execute_command([self.tool["executable"], *arguments], self.directory, log, self.environment)
        self.tool_commands.append({"phase": name, "receipt": result, "output": log.read_text(encoding="utf-8")})
        return log.read_text(encoding="utf-8")

    def acquire_storage(self):
        """A local disk cache supports one server; a busy cache adds no wait."""
        self.cache.mkdir(parents=True, exist_ok=True, mode=0o700)
        lock = (self.cache / ".server.lock").open("a+b")
        if lock.tell() == 0:
            lock.write(b"\0")
            lock.flush()
        lock.seek(0)
        try:
            if os.name == "nt":
                import msvcrt

                msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
            else:
                import fcntl

                fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as error:
            lock.close()
            if error.errno not in {errno.EACCES, errno.EAGAIN}:
                raise
            self.disabled = True
            print("COMPILER_CACHE: local storage is busy; preserving the original Cargo execution without waiting.", flush=True)
            return False
        self.storage_lock = lock
        try:
            owner = json.loads((self.cache / ".server-owner.json").read_text(encoding="utf-8"))
        except FileNotFoundError:
            owner = None
        except (ValueError, OSError):
            owner = {"invalid": True}
        if owner is not None:
            try:
                occupied = (owner["phase"] not in {"starting", "active"}
                            or owner["executable"] != self.tool["executable"]
                            or (owner["phase"] == "starting" and time.time() - owner["at"] < 60)
                            or self.retained_process_alive(owner["server"])
                            or bool(self.owned_servers(owner["owner"])))
            except (KeyError, TypeError, ValueError):
                occupied = True
            if occupied:
                lock.close()
                self.storage_lock = None
                self.disabled = True
                print("COMPILER_CACHE: retained server ownership is active or unknown; using original Cargo without waiting.", flush=True)
                return False
        return True

    def retained_process_alive(self, identity):
        import psutil

        if identity is None:
            return False
        try:
            return psutil.Process(identity["pid"]).create_time() == identity["birth"]
        except psutil.NoSuchProcess:
            return False
        except psutil.AccessDenied:
            return True

    def write_owner(self, phase):
        path = self.cache / ".server-owner.json"
        temporary = path.with_suffix(".tmp")
        temporary.write_text(json.dumps({"owner": self.environment["SCODE_COMPILER_CACHE_OWNER"],
                                        "executable": self.tool["executable"], "phase": phase,
                                        "at": time.time(), "server": self.server}) + "\n", encoding="utf-8")
        temporary.replace(path)

    def assert_owner(self):
        import psutil

        process = psutil.Process(self.server["pid"])
        if (process.create_time() != self.server["birth"]
                or Path(process.exe()).resolve() != Path(self.tool["executable"])
                or process.environ().get("SCODE_COMPILER_CACHE_OWNER") != self.environment["SCODE_COMPILER_CACHE_OWNER"]):
            raise RuntimeError("Compiler-cache server ownership changed; do not contact an unowned server")
        return process

    def owned_servers(self, marker=None):
        import psutil

        matching = []
        for process in psutil.process_iter(["pid", "exe"]):
            try:
                if (process.info["exe"] and Path(process.info["exe"]).resolve() == Path(self.tool["executable"])
                        and process.environ().get("SCODE_COMPILER_CACHE_OWNER") == (marker or self.environment["SCODE_COMPILER_CACHE_OWNER"])):
                    matching.append(process)
            except (psutil.NoSuchProcess, psutil.AccessDenied):
                continue
        return matching

    def current_server(self):
        import psutil

        if self.server is not None:
            try:
                process = psutil.Process(self.server["pid"])
                if process.create_time() == self.server["birth"]:
                    process = self.assert_owner()
                    priority = psutil.BELOW_NORMAL_PRIORITY_CLASS if os.name == "nt" else 10
                    if process.nice() != priority:
                        process.nice(priority)
                    return process
            except psutil.NoSuchProcess:
                pass
        matching = self.owned_servers()
        if not matching:
            return None
        if len(matching) != 1:
            raise RuntimeError("Compiler-cache invocation has ambiguous owned processes")
        process = matching[0]
        process.nice(psutil.BELOW_NORMAL_PRIORITY_CLASS if os.name == "nt" else 10)
        identity = {"pid": process.pid, "birth": process.create_time(), "priority": process.nice()}
        if identity != self.server:
            self.server_history.append(identity)
        self.server = identity
        self.write_owner("active")
        return process

    def start(self):
        definition = archive_definition()
        if self.tool["sha256"] != definition["sha256"] or self.tool["target"] != definition["target"]:
            raise RuntimeError("Compiler-cache tool record does not match this pinned platform")
        expected = executable_bytes(Path(self.tool["archive"]), definition)
        if digest(expected) != self.tool["executable_sha256"] or Path(self.tool["executable"]).read_bytes() != expected:
            raise RuntimeError("Prepared compiler-cache tool changed before use")
        if not self.acquire_storage():
            return False
        self.directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        wrapper = self.build_wrapper()
        configuration = self.directory / "local-cache.toml"
        configuration.write_text('[cache.disk]\ndir = ' + json.dumps(str(self.cache.resolve())) + '\nsize = 2147483648\n', encoding="utf-8")
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        self.environment = {"SCCACHE_CONF": str(configuration.resolve()), "SCCACHE_DIR": str(self.cache.resolve()),
                            "SCCACHE_CACHED_CONF": str((self.directory / "cached-config").resolve()),
                            "SCCACHE_CACHE_SIZE": "2G", "SCCACHE_DIRECT": "false",
                            "SCCACHE_SERVER_PORT": str(port), "SCCACHE_IDLE_TIMEOUT": "600",
                            "SCCACHE_CLIENT_SIDE": "1", "RUSTC_WRAPPER": wrapper,
                            "SCODE_COMPILER_CACHE_OWNER": uuid.uuid4().hex}
        if self.tool_command("version", "--version").strip() != "sccache " + VERSION:
            raise RuntimeError("Unexpected pinned compiler-cache version")
        self.write_owner("starting")
        self.tool_command("server-start", "--start-server")
        if self.current_server() is None:
            raise RuntimeError("Compiler-cache server was not uniquely created by this invocation")
        stats = json.loads(self.tool_command("initial-stats", "--show-stats", "--stats-format", "json"))
        if stats["max_cache_size"] != 2 * 1024**3:
            raise RuntimeError("Compiler-cache disk limit differs from the declared 2 GiB")
        return True

    def build_wrapper(self):
        """Compile Cargo's rustc wrapper around this session's verified sccache."""
        source = self.directory / "wrapper"
        source.mkdir(mode=0o700)
        (source / "main.rs").write_bytes(WRAPPER_SOURCE.encode("utf-8"))
        (source / "cache-tool").write_bytes(self.tool["executable"].encode("utf-8"))
        (self.directory / "argument-files").mkdir(mode=0o700)
        executable = self.directory / ("rustc-wrapper" + (".exe" if os.name == "nt" else ""))
        receipt = self.execute_command(["rustc", "--edition=2021", "-C", "opt-level=1", "--crate-name", "rustc_wrapper",
                                        str(source / "main.rs"), "-o", str(executable)],
                                       self.directory, self.directory / "wrapper-build.log")
        self.wrapper = {"source_sha256": digest(WRAPPER_SOURCE.encode("utf-8")), "executable": str(executable.resolve()),
                        "executable_sha256": digest(executable.read_bytes()), "build": receipt}
        return self.wrapper["executable"]

    def argument_file_units(self):
        """Units the wrapper gave the compiler in an argument file: Cargo's, or its own."""
        units = {"cargo": 0, "wrapper": 0}
        for path in (self.directory / "argument-files").iterdir():
            if path.suffix[1:] in units:
                units[path.suffix[1:]] += 1
        return units

    def execute(self, command, cwd, log, env=None):
        frontend = compiler_frontend(command)
        if not frontend or configured(os.environ, env or {}) or self.disabled:
            return self.execute_command(command, cwd, log, env)
        if self.environment is None:
            if not self.start():
                return self.execute_command(command, cwd, log, env)
        if self.current_server() is None:
            self.disabled = True
            print("COMPILER_CACHE: owned server exited; preserving original Cargo for remaining commands.", flush=True)
            return self.execute_command(command, cwd, log, env)
        try:
            result = self.execute_command(command, cwd, log, {**(env or {}), **self.environment})
        except RuntimeError:
            receipt = Path(log).with_suffix(".command.json")
            if receipt.is_file():
                self.records.append((json.loads(receipt.read_text(encoding="utf-8")), Path(log), list(map(str, command))))
            raise
        self.records.append((result, Path(log), list(map(str, command))))
        for name, counts in result.get("compiler_priorities", {}).items():
            session = self.priorities.setdefault(name, {})
            for priority, count in counts.items():
                session[priority] = session.get(priority, 0) + count
        return result

    def stop_server(self):
        if self.environment is None:
            return
        process = self.current_server()
        stats = None
        if process is not None:
            try:
                stats = json.loads(self.tool_command("final-stats", "--show-stats", "--stats-format", "json"))
            finally:
                self.tool_command("server-stop", "--stop-server")
            process.wait(timeout=5)
        owner = self.cache / ".server-owner.json"
        if owner.exists() and json.loads(owner.read_text(encoding="utf-8"))["owner"] == self.environment["SCODE_COMPILER_CACHE_OWNER"]:
            owner.unlink()
        if self.wrapper is not None:
            self.wrapper["argument_file_units"] = self.argument_file_units()
            print("COMPILER_CACHE_ARGUMENT_FILES=" + json.dumps(self.wrapper["argument_file_units"]), flush=True)
        # Every cached command shares this session. Record it once, on the
        # first command, and bind each command to it by SHA-256.
        session = {"version": VERSION, "tool": self.tool, "wrapper": self.wrapper, "server": self.server, "stats": stats,
                   "server_history": self.server_history, "compiler_priorities": self.priorities,
                   "tool_commands": self.tool_commands}
        identity = digest(json.dumps(session, sort_keys=True, separators=(",", ":")).encode())
        for index, (result, log, requested) in enumerate(self.records):
            result["compiler_cache"] = {"requested_command": requested, "session_sha256": identity,
                                        **({"session": session} if index == 0 else {})}
            log.with_suffix(".command.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")

    def close(self):
        try:
            self.stop_server()
        except BaseException:
            # Startup/stat failures must also retire only this invocation's
            # exact executable/owner matches, never a server found by port.
            for process in self.owned_servers() if self.environment is not None else []:
                process.terminate()
                process.wait(timeout=5)
            raise
        finally:
            if self.storage_lock is not None:
                self.storage_lock.close()
                self.storage_lock = None


@contextmanager
def session(tool, directory, execute, cache=None):
    value = Session(tool, directory, execute, cache)
    try:
        yield value
    finally:
        value.close()
