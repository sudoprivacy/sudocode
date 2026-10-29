"""Exercise real Git snapshots and Cargo output through the production copier."""

import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile


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
        executor.run_command(["cargo", "build", "--locked", "--manifest-path", str(candidate / "Cargo.toml"),
                              "--target-dir", str(target)], directory, logs / f"{len(list(logs.glob('*.log'))):03d}.log")
        binary = target / "debug" / ("acceptance-cache-probe.exe" if os.name == "nt" else "acceptance-cache-probe")
        return subprocess.check_output([str(binary)], cwd=candidate).decode().strip()

    for marker in ["one", "two"]:
        archived = inputs / marker
        checks.replace_checkout(archived, candidate)
        # Reproduce the exact harmful interleaving: an older executor replaces
        # its pre-lease checkout after our copy, before Cargo reads sources.
        checks.replace_checkout(inputs / "foreign", legacy)
        actual = build_and_run()
        assert actual == marker, f"Current Git snapshot says {marker}, cached executable says {actual}"
        if os.name != "nt":
            assert subprocess.check_output([str(candidate / "fixture.sh")]).decode().strip() == "executable fixture"
    # The second run intentionally shares the exact manifest and target paths,
    # package version and old archive mtimes. Only source content has changed.
    print("PASS: current Git snapshot executes through reused Cargo targets despite legacy checkout replacement; executable modes survive")


def verify_snapshot_cache(directory):
    inputs = prepare_snapshot_cache(directory)
    verify_snapshot_replacements(directory, inputs)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="scode-cache-acceptance-") as temporary:
        verify_snapshot_cache(Path(temporary))
