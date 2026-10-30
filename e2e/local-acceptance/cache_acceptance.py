"""Exercise real Git snapshots and Cargo output through the production copier."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def prepare_snapshot_cache(directory):
    checks = load("acceptance_profiles", SOURCE / ".github/local-acceptance/checks.py")
    repository = directory / "cache-repository"
    repository.mkdir()
    inputs = directory / "cache-inputs"
    inputs.mkdir()

    def git(*args, date=None):
        environment = os.environ.copy()
        if date:
            environment.update(GIT_AUTHOR_DATE=date, GIT_COMMITTER_DATE=date)
        return subprocess.check_output(["git", *args], cwd=repository, env=environment).decode().strip()

    git("init", "--initial-branch=main")
    git("config", "user.name", "Acceptance Fixture")
    git("config", "user.email", "acceptance@example.invalid")
    (repository / "Cargo.toml").write_text('[package]\nname="acceptance-cache-probe"\nversion="0.0.1"\nedition="2021"\n\n[workspace]\n')
    (repository / "Cargo.lock").write_text('version = 4\n\n[[package]]\nname = "acceptance-cache-probe"\nversion = "0.0.1"\n')
    (repository / "src").mkdir()
    executable = repository / "fixture.sh"
    executable.write_text("#!/bin/sh\nprintf 'executable fixture\\n'\n")
    executable.chmod(0o755)
    for marker, date in [("one", "2020-01-01T00:00:00Z"), ("two", "2021-01-01T00:00:00Z"),
                         ("foreign", "2022-01-01T00:00:00Z")]:
        (repository / "src/main.rs").write_text(f'fn main() {{ println!("{marker}"); }}\n')
        git("add", ".")
        git("update-index", "--chmod=+x", "fixture.sh")
        git("commit", "-m", marker, date=date)
        archived = inputs / marker
        checks.snapshot(repository, "HEAD", archived)
    return inputs


def verify_snapshot_replacements(directory, inputs):
    checks = load("acceptance_profiles", SOURCE / ".github/local-acceptance/checks.py")
    executor = load("acceptance_executor", SOURCE / ".github/scripts/local_acceptance.py")
    work, targets = checks.benchmark_storage("cache-acceptance")
    state = directory / "state"
    candidate = state / work.relative_to("/state") / "candidate"
    target = state / targets.relative_to("/state") / "candidate"
    legacy = state / "cache-acceptance/candidate"
    logs = directory / "cache-logs"
    logs.mkdir()

    def build_and_run():
        log = logs / f"{len(list(logs.glob('*.log'))):03d}.log"
        executor.run_command(["cargo", "build", "--locked", "--manifest-path", str(candidate / "Cargo.toml"),
                              "--target-dir", str(target), "--message-format=json"], directory, log)
        messages = [json.loads(line) for line in log.read_text().splitlines() if line.startswith("{")]
        binaries = [row for row in messages if row.get("reason") == "compiler-artifact" and row.get("executable")]
        assert len(binaries) == 1, binaries
        binary = target / "debug" / ("acceptance-cache-probe.exe" if os.name == "nt" else "acceptance-cache-probe")
        return subprocess.check_output([str(binary)], cwd=candidate).decode().strip(), binaries[0]["fresh"]

    for marker in ["one", "two"]:
        archived = inputs / marker
        checks.replace_checkout(archived, candidate)
        # Reproduce the exact harmful interleaving: an older executor replaces
        # its pre-lease checkout after our copy, before Cargo reads sources.
        checks.replace_checkout(inputs / "foreign", legacy)
        actual, fresh = build_and_run()
        assert actual == marker, f"Current Git snapshot says {marker}, cached executable says {actual}"
        assert not fresh, "Changed archive bytes must rebuild even with old source mtimes"
        if os.name != "nt":
            assert subprocess.check_output([str(candidate / "fixture.sh")]).decode().strip() == "executable fixture"
    # The second run intentionally shares the exact manifest and target paths,
    # package version and old archive mtimes. Only source content has changed.
    print("PASS: current Git snapshot executes through reused Cargo targets despite legacy checkout replacement; executable modes survive")

    main = candidate / "src/main.rs"
    before = main.stat()
    checks.replace_checkout(inputs / "two", candidate)
    assert main.stat().st_mtime_ns == before.st_mtime_ns
    assert build_and_run() == ("two", True), "Identical inputs must reuse their actual Cargo artifact"
    # Prime a repeated comparison, then change both bytes and restore all stat
    # fields that a cached comparison might use. Output alone would miss this:
    # the stale already-built executable still says 'two'. Require recompilation.
    before = main.stat()
    main.write_bytes(main.read_bytes().replace(b"two", b"bad"))
    assert main.stat().st_size == before.st_size
    os.utime(main, ns=(before.st_atime_ns, before.st_mtime_ns))
    checks.replace_checkout(inputs / "two", candidate)
    assert build_and_run() == ("two", False), "Same-size/mtime cached tampering must be repaired and rebuilt"

    poison = candidate / "src/bin/injected.rs"
    poison.parent.mkdir()
    poison.write_text('compile_error!("Untracked cached source must not enter acceptance"); fn main() {}\n')
    try:
        executor.run_command(["cargo", "build", "--locked", "--manifest-path", str(candidate / "Cargo.toml"),
                              "--target-dir", str(target)], directory, logs / "negative-injected.log")
    except RuntimeError:
        pass
    else:
        raise AssertionError("The injected-source negative control unexpectedly compiled")
    checks.replace_checkout(inputs / "two", candidate)
    assert not poison.exists()
    assert build_and_run()[0] == "two"
    if os.name != "nt":
        executable = candidate / "fixture.sh"
        executable.chmod(0o644)
        checks.replace_checkout(inputs / "two", candidate)
        assert subprocess.check_output([str(executable)]).decode().strip() == "executable fixture"
        outside = directory / "outside"
        outside.mkdir()
        guard = outside / "guard.txt"
        guard.write_text("outside must survive")
        (candidate / "extra-link").symlink_to(outside, target_is_directory=True)
        os.mkfifo(candidate / "stale.pipe")
        main.unlink()
        main.symlink_to(guard)
        checks.replace_checkout(inputs / "two", candidate)
        assert not (candidate / "extra-link").exists()
        assert not (candidate / "stale.pipe").exists()
        assert guard.read_text() == "outside must survive"
        assert not main.is_symlink()
        assert build_and_run()[0] == "two"
    print("PASS: identical inputs stay Cargo-fresh; same-size/mtime tampering, untracked code and cached links are repaired")


def verify_cache_lease(directory):
    if os.name == "nt":
        return  # Production flock lives in the Linux container, not its Windows bind mount.
    import sys
    lock_directory = directory / "locks"
    lock_directory.mkdir()
    ready, release, acquired = (directory / name for name in ["ready", "release", "acquired"])
    prefix = ("import importlib.util,time; from pathlib import Path; "
              f"spec=importlib.util.spec_from_file_location('checks',{str(SOURCE / '.github/local-acceptance/checks.py')!r}); "
              "checks=importlib.util.module_from_spec(spec); spec.loader.exec_module(checks)\n")
    holder_code = prefix + (f"with checks.exclusive_benchmark_state(Path({str(lock_directory)!r})):\n"
                           f" Path({str(ready)!r}).write_text('held')\n"
                           " deadline=time.monotonic()+30\n"
                           f" while not Path({str(release)!r}).exists():\n"
                           "  assert time.monotonic()<deadline,'fixture release timed out'\n  time.sleep(0.02)\n")
    waiter_code = prefix + (f"with checks.exclusive_benchmark_state(Path({str(lock_directory)!r})):\n"
                           f" Path({str(acquired)!r}).write_text('acquired')\n")
    processes = []
    with (directory / "holder.log").open("wb") as holder_log, (directory / "waiter.log").open("wb") as waiter_log:
        try:
            holder = subprocess.Popen([sys.executable, "-c", holder_code], stdout=holder_log, stderr=subprocess.STDOUT)
            processes.append(holder)
            deadline = time.monotonic() + 10
            while not ready.exists():
                assert holder.poll() is None and time.monotonic() < deadline, "Holder did not acquire the real lock"
                time.sleep(0.02)
            waiter = subprocess.Popen([sys.executable, "-c", waiter_code], stdout=waiter_log, stderr=subprocess.STDOUT)
            processes.append(waiter)
            deadline = time.monotonic() + 10
            while "Waiting for the local benchmark cache lease" not in (directory / "waiter.log").read_text():
                assert waiter.poll() is None and time.monotonic() < deadline, "Blocked caller did not explain its wait"
                time.sleep(0.02)
            assert not acquired.exists(), "Second process entered the leased checkout concurrently"
            release.write_text("release")
            assert holder.wait(timeout=10) == 0
            assert waiter.wait(timeout=10) == 0
            assert acquired.read_text() == "acquired"
        finally:
            release.write_text("release")
            for process in processes:
                if process.poll() is None:
                    process.terminate()
                    process.wait(timeout=10)
    print("PASS: real competing processes retain exclusive state and explain the blocked acquisition")


def verify_snapshot_cache(directory):
    inputs = prepare_snapshot_cache(directory)
    verify_snapshot_replacements(directory, inputs)
    verify_cache_lease(directory)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="scode-cache-acceptance-") as temporary:
        verify_snapshot_cache(Path(temporary))
