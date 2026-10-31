"""Real acceptance commands shared by the author's hook and opposite-OS CI."""

import argparse
from contextlib import contextmanager
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import platform
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.request
import zipfile


def nextest_binary(output):
    """Use a pinned, checksum-verified tool without replacing a user's installation."""
    version = '0.9.150'
    architecture = platform.machine().lower()
    system = platform.system()
    if system == 'Darwin' and architecture in {'arm64', 'aarch64', 'x86_64', 'amd64'}:
        triplet = 'universal-apple-darwin'
    else:
        architecture = {'amd64': 'x86_64', 'arm64': 'aarch64'}.get(architecture, architecture)
        suffix = {'Windows': 'pc-windows-msvc', 'Linux': 'unknown-linux-gnu'}.get(system)
        if suffix is None:
            raise RuntimeError(f'No verified nextest binary for {system}/{architecture}')
        triplet = f'{architecture}-{suffix}'
    hashes = {
        'x86_64-pc-windows-msvc': '6fd8f9ba48c946a79a5bcdd35bf4f922447b149f7ca6c3ff7f39e12f32ef78db',
        'aarch64-pc-windows-msvc': '812da1f75a1a18911d8aeb74e2b75caa95e652bfd210a46527de7d5f0776f22e',
        'x86_64-unknown-linux-gnu': 'b0959bc64232974224cf9fb22c915845398c0e8668fa7831eb5b43fb1eef781f',
        'aarch64-unknown-linux-gnu': '9164568e9840c6509177086828ac7d3178c2d8feb0658b4d2bff62fa620e98c8',
        'universal-apple-darwin': '82a759b4d218d38359424196ab244c470e8f7c7d149c814fbbeef4d67c3c301a',
    }
    if triplet not in hashes:
        raise RuntimeError(f'No pinned nextest checksum for {triplet}')
    extension = 'zip' if system == 'Windows' else 'tar.gz'
    filename = f'cargo-nextest-{version}-{triplet}.{extension}'
    source = f'https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-{version}/{filename}'
    cache = Path.home() / '.cache/sudocode-local-acceptance/tools/nextest' / version / triplet
    cache.mkdir(parents=True, exist_ok=True)
    archive = cache / filename
    if not archive.exists():
        with tempfile.NamedTemporaryFile(dir=cache, suffix='.download', delete=False) as temporary:
            temporary_path = Path(temporary.name)
        try:
            urllib.request.urlretrieve(source, temporary_path)
            if hashlib.sha256(temporary_path.read_bytes()).hexdigest() != hashes[triplet]:
                raise RuntimeError(f'nextest archive checksum mismatch: {source}')
            os.replace(temporary_path, archive)
        finally:
            temporary_path.unlink(missing_ok=True)
    contents = archive.read_bytes()
    if hashlib.sha256(contents).hexdigest() != hashes[triplet]:
        raise RuntimeError(f'Cached nextest archive checksum mismatch: {archive}')
    name = 'cargo-nextest.exe' if system == 'Windows' else 'cargo-nextest'
    if system == 'Windows':
        with zipfile.ZipFile(io.BytesIO(contents)) as bundle:
            members = [item for item in bundle.infolist() if not item.is_dir() and Path(item.filename).name == name]
            if len(members) != 1:
                raise RuntimeError('Expected exactly one nextest executable in the verified archive')
            executable_bytes = bundle.read(members[0])
    else:
        with tarfile.open(fileobj=io.BytesIO(contents), mode='r:gz') as bundle:
            members = [item for item in bundle.getmembers() if item.isfile() and Path(item.name).name == name]
            if len(members) != 1:
                raise RuntimeError('Expected exactly one nextest executable in the verified archive')
            executable_bytes = bundle.extractfile(members[0]).read()
    executable = cache / name
    if executable.exists():
        if executable.read_bytes() != executable_bytes:
            raise RuntimeError(f'Cached nextest executable differs from the verified archive: {executable}')
    else:
        executable.write_bytes(executable_bytes)
        executable.chmod(0o755)
    (output / 'nextest-source.json').write_text(json.dumps({
        'version': version, 'url': source, 'archive_sha256': hashes[triplet],
        'executable_sha256': hashlib.sha256(executable_bytes).hexdigest(),
    }, indent=2))
    return executable


def nextest_options(output):
    """Keep retry/filter/fail-fast behavior independent of ambient user settings."""
    configuration = output / 'nextest.toml'
    configuration.write_text('[profile.default]\nretries = 0\nfail-fast = { max-fail = 1, terminate = "wait" }\n')
    workers = int(os.environ.get('SCODE_ACCEPTANCE_TEST_THREADS', '4'))
    if not 1 <= workers <= 4:
        raise RuntimeError('SCODE_ACCEPTANCE_TEST_THREADS must be between 1 and 4')
    return ['--config-file', str(configuration), '--profile', 'default', '--ignore-default-filter',
            '--test-threads', str(workers), '--retries', '0', '--max-fail', '1:wait', '--no-tests', 'fail']


def changed_cli_test_targets(root, base):
    """Scope isolated test edits; any shared or production input keeps the full suite."""
    changed = subprocess.check_output(["git", "diff", "--name-only", "--no-renames", "-z", base, "HEAD"],
                                      cwd=root).decode().split("\0")[:-1]
    # External specs/fixtures can affect unchanged tests too. Only known docs
    # may accompany an isolated integration module without widening acceptance.
    inputs = [path for path in changed if path != "README.md"
              and not (path.startswith("docs/") and path.endswith(".md"))]
    pattern = r"rust/crates/rusty-sudocode-cli/tests/([A-Za-z0-9_]+)\.rs"
    if not inputs or any(not re.fullmatch(pattern, path) or not (root / path).is_file() for path in inputs):
        return None
    return sorted({Path(path).stem for path in inputs})


def test_group_source(names):
    """Keep opt-in wrappers declarative so selection cannot hide extra Rust code."""
    return ("//! Opt-in acceptance target; original individual Cargo test targets stay available.\n\n"
            + '#![allow(\n    clippy::duplicate_mod,\n'
              '    reason = "Original test modules keep their own helper namespaces."\n)]\n\n'
            + "\n\n".join(f'#[path = "../{name}.rs"]\nmod {name};' for name in names) + "\n")


def grouped_workspace_test_targets(root, metadata):
    """Replace only validated original integration targets with opt-in wrappers."""
    packages = [package for package in metadata["packages"] if package["id"] in metadata["workspace_members"]]
    groups, covered, targets = {}, set(), []
    for package in packages:
        directory = Path(package["manifest_path"]).parent.resolve()
        group_directory = directory / "tests/acceptance_groups"
        declarations = {Path(target["src_path"]) for target in package["targets"]
                        if target["kind"] == ["test"] and Path(target["src_path"]).parent == group_directory}
        if set(group_directory.glob("*.rs")) != declarations:
            raise RuntimeError(f"Every acceptance wrapper must have an explicit Cargo test target: {group_directory}. "
                               "Register each wrapper with matching name/path and test=false, or remove the obsolete wrapper; "
                               "individual ungrouped tests are selected automatically.")
        original = {target["name"]: target for target in package["targets"]
                    if target["kind"] == ["test"] and target["test"]}
        for target in package["targets"]:
            if target["kind"] != ["test"]:
                continue
            path = Path(target["src_path"])
            if path.parent != group_directory:
                continue
            if target["test"] or target["name"] != path.stem:
                raise RuntimeError(f"Acceptance wrapper must keep its name and test=false: {path}")
            if any((directory / "tests" / candidate).exists()
                   for candidate in [target["name"] + ".rs", target["name"] + "/main.rs"]):
                raise RuntimeError(f"Acceptance wrapper shadows an individual Cargo target: {path}. "
                                   "Choose an unused group name so the original target remains available.")
            if (checkout_link(path) or not path.resolve().is_relative_to(directory)
                    or not directory.is_relative_to(root.resolve())):
                raise RuntimeError(f"Acceptance test wrapper escaped its package: {path}")
            if target.get("required-features"):
                raise RuntimeError(f"Acceptance wrapper must not conditionally omit its modules: {path}")
            source = path.read_text(encoding="utf-8")
            names = re.findall(r'^#\[path = "\.\./([A-Za-z_][A-Za-z0-9_]*)\.rs"\]\nmod \1;$', source, re.MULTILINE)
            if not names or source.strip() != test_group_source(names).strip():
                raise RuntimeError(f"Acceptance wrapper must contain only canonical path modules: {path}")
            members = []
            for name in names:
                identity = (package["id"], name)
                if (name not in original or identity in covered
                        or Path(original[name]["src_path"]).resolve() != directory / "tests" / (name + ".rs")):
                    raise RuntimeError(f"Missing, duplicate or redirected acceptance test module {name}: {path}")
                covered.add(identity)
                members.append(name)
            groups[(package["id"], target["name"])] = members
    if not groups:
        return [], {"mode": "original-workspace", "groups": []}
    for package in packages:
        for target in package["targets"]:
            identity = (package["id"], target["name"])
            if target["kind"] == ["test"] and ((target["test"] and identity not in covered) or identity in groups):
                targets.append(target["name"])
            elif target["test"] and target["kind"] not in [["test"], ["lib"], ["bin"], ["proc-macro"]]:
                raise RuntimeError(f"Unsupported enabled test target kind: {target}")
            elif not target["test"] and target["kind"] in [["lib"], ["bin"], ["proc-macro"]]:
                raise RuntimeError(f"Grouped workspace selection requires an explicit disabled library/bin plan: {target}")
    if len(targets) != len(set(targets)) or any(name in targets for _, name in covered):
        raise RuntimeError("Test target names overlap packages; keep the overlapping module standalone")
    arguments = ["--lib", "--bins"]
    for name in sorted(targets):
        arguments += ["--test", name]
    return arguments, {"mode": "grouped-workspace", "covered_original_targets": len(covered),
                       "selected_integration_targets": len(targets),
                       "groups": [{"package": package, "target": name, "modules": members}
                                  for (package, name), members in sorted(groups.items())]}


def run(name, root, output, base, execute):
    output.mkdir(parents=True, exist_ok=True)
    results = []
    rust = root / "rust"
    target = Path(os.environ.get("CARGO_TARGET_DIR", rust / "target")).resolve()
    suffix = ".exe" if os.name == "nt" else ""
    env = {"SCODE_TEST_BACKEND": "mock", "CARGO_TARGET_DIR": str(target)}

    def command(*args, cwd=root, extra=None):
        results.append(execute(args, cwd, output / f"{len(results):03d}.log", {**env, **(extra or {})}))
        return output / f"{len(results)-1:03d}.log"

    def cargo(*args, **kwargs):
        return command("cargo", *args, cwd=rust, **kwargs)

    def native_tests(*args):
        return command(str(nextest_binary(output)), "nextest", "run",
                       *nextest_options(output), *args, cwd=rust)

    if name == "preflight":
        command(sys.executable, ".github/scripts/check_doc_source_of_truth.py")
        entry = stage_profile_executor(root, execution_policy_ref(), name, output / "history-policy")
        print("History preflight uses the protected policy checker; the author branch stays unchanged.", flush=True)
        command(sys.executable, str(entry), base, "HEAD")
    elif name == "guard-contract":
        print("Local contract validation: full private Docker matrix, with Git/signature checks before Cargo; "
              "then author-host native recovery, nextest, release, grouping and example guards. "
              "No product build or benchmark lease.", flush=True)
        if not shutil.which("docker"):
            raise RuntimeError("Contract validation requires local Docker. Install/start Docker Desktop, "
                               "then retry this push or the printed acceptance command.")
        try:
            command("docker", "info", "--format", "{{.ServerVersion}}")
        except (RuntimeError, OSError) as error:
            raise RuntimeError("Contract validation cannot reach local Docker. Start Docker Desktop or correct "
                               "this shell's DOCKER_HOST/context, then retry; the daemon error is in the "
                               f"guard log above. Native smoke and matrix have not started: {error}") from error
        policy_ref = execution_policy_ref()
        entry = stage_profile_executor(root, policy_ref, name, output / "native-guard-policy")
        contract_container(root, output, command)
        command(sys.executable, str(entry), "--repository", str(root),
                "--output", str(output / "native-guard"),
                "--executable", str(nextest_binary(output)),
                "--source-receipt", str(output / "nextest-source.json"),
                "--options", json.dumps(nextest_options(output)))
    elif name == "native-sdk":
        sdk = root / "sdk/typescript"
        command("node", "--version", cwd=sdk)
        command("npm", "ci", cwd=sdk)
        command("npm", "test", cwd=sdk)
        command("npm", "run", "pack:runtime", cwd=sdk)
    elif name == "native-workspace":
        if os.name == "nt" and not shutil.which("pwsh.exe"):
            raise RuntimeError("Windows native acceptance requires PowerShell 7 (pwsh.exe) on PATH. "
                               "Add your existing PowerShell 7 directory to PATH, or install it with "
                               "`winget install --id Microsoft.PowerShell --exact --source winget`, "
                               "then rerun the same acceptance command. No workspace compilation started.")
        cargo("fmt", "--all", "--check")
        tests = changed_cli_test_targets(root, base)
        (output / "test-scope.json").write_text(json.dumps({
            "mode": "changed-cli-targets" if tests is not None else "full-workspace",
            "targets": tests,
        }, indent=2))
        if tests is not None:
            arguments = ["--locked", "-p", "rusty-sudocode-cli", "--features", "engine-acp/mailbox"]
            for test in tests:
                arguments += ["--test", test]
            native_tests(*arguments)
        else:
            command(sys.executable, "scripts/generate_cache_metrics.py", "--check")
            command(sys.executable, "scripts/check_cache_usage_fixtures.py")
            command(sys.executable, "e2e/build-metadata/run.py",
                    extra={"BUILD_METADATA_EVIDENCE": str(output / "build-metadata")})
            cargo("test", "--locked", "--manifest-path", "vendor/codex-mermaid/Cargo.toml", "--target-dir", str(target))
            cargo("check", "--workspace", "--benches", "--locked")
            metadata_log = cargo("metadata", "--no-deps", "--locked", "--format-version=1")
            targets, selection = grouped_workspace_test_targets(root, json.loads(metadata_log.read_text(encoding="utf-8")))
            (output / "test-scope.json").write_text(json.dumps(selection, indent=2))
            print("Native workspace test selection: " + json.dumps(selection), flush=True)
            native_tests("--workspace", "--locked", "--features", "engine-acp/mailbox", *targets)
            # Stable nextest excludes doctests; retain Cargo's separate path.
            cargo("test", "--workspace", "--locked", "--features", "engine-acp/mailbox", "--doc")
            cargo("clippy", "--workspace", "--locked")
    elif name == "native-examples":
        print("Compile-only examples: building the normal example binaries with the workspace mailbox feature graph. "
              "This profile never runs example entry points.", flush=True)
        cargo("build", "--workspace", "--locked", "--features", "engine-acp/mailbox", "--examples")
    elif name == "native-resize":
        if os.name == "nt":
            terminal = Path.home() / ".cache/sudocode-terminal-host"
            command(sys.executable, "e2e/terminal-resize/setup_windows.py", str(terminal))
            command(sys.executable, "e2e/terminal-resize/check_windows_setup.py", str(terminal / "host.zip"))
            env.update({"SCODE_TERMINAL_HOST": str(terminal / "Code.exe"),
                        "SCODE_TERMINAL_MODULES": str(terminal / "07f806f999/resources/app/node_modules.asar")})
        else:
            command("npm", "ci", "--prefix", "e2e/terminal-resize")
        env["SCODE_TERMINAL_LOG_DIR"] = str(output / "terminal")
        tests = ["pty_chrome_resize", "pty_prose_preview", "pty_review_scrollback"]
        if os.name != "nt":
            tests.append("pty_codex_colors")
        for test in tests:
            cargo("test", "--locked", "-p", "rusty-sudocode-cli", "--test", test,
                  "--", "--ignored", "--nocapture")
    elif name == "native-release":
        cargo("build", "--release", "--locked", "-p", "rusty-sudocode-cli", "--bin", "scode")
        binary = output / ("scode" + suffix)
        shutil.copy2(target / "release" / ("scode" + suffix), binary)
        env["SCODE_TEST_BIN"] = str(binary)
        # Reuse the native workspace's test graph. Only the driver is debug;
        # SCODE_TEST_BIN still launches the copied normal release executable.
        print("Release acceptance: the native test driver launches the copied release scode; "
              "reusing workspace test artifacts instead of rebuilding release dev-dependencies.", flush=True)
        cargo("test", "--locked", "-p", "rusty-sudocode-cli", "--features", "engine-acp/mailbox", "--test", "pty_model_stream_close",
              "raw_stream_capture_keeps_streaming_and_resume_responsive", "--", "--exact", "--nocapture")
        command(sys.executable, "e2e/startup/run.py", "--bin", str(binary), "--budget-ms", "100",
                "--report", str(output / "startup.json"))
    elif name == "native-services":
        cargo("build", "--locked", "-p", "rusty-sudocode-cli", "--bin", "scode")
        binary = target / "debug" / ("scode" + suffix)
        command(sys.executable, "e2e/shared-services/run.py", "--scode", str(binary),
                "--artifacts", str(output / "services"))
        # The existing harness resolves the pinned native grpcurl for each OS.
        bash = shutil.which("bash")
        if not bash:
            raise RuntimeError("Git Bash is required for shared-service credential acceptance")
        command(bash, "-c", 'source e2e/nexus-a2a/lib.sh; GRPCURL="$(grpcurl_bin)"; '
                'python e2e/shared-services/existing_box.py --scode "$1" --grpcurl "$(native_path "$GRPCURL")" --artifacts "$2"',
                "scode-services", str(binary), str(output / "existing-box"))
    elif name == "native-cohost":
        # raft-proto rejects protoc >=4. Pin the same tool on macOS/Windows as
        # the release workflow, and leave the developer's global PATH intact.
        if platform.system() in {"Windows", "Darwin"}:
            architecture = "aarch_64" if platform.machine().lower() == "arm64" else "x86_64"
            archive_name = "win64" if os.name == "nt" else f"osx-{architecture}"
            folder = Path.home() / ".cache/sudocode-protoc-3.20.2" / archive_name
            executable = folder / "bin" / ("protoc" + suffix)
            if not executable.exists():
                folder.mkdir(parents=True, exist_ok=True)
                archive = folder / "protoc.zip"
                urllib.request.urlretrieve(f"https://github.com/protocolbuffers/protobuf/releases/download/v3.20.2/protoc-3.20.2-{archive_name}.zip", archive)
                with zipfile.ZipFile(archive) as stream:
                    stream.extractall(folder)
                executable.chmod(0o755)
            env["PROTOC"] = str(executable)
        profile = "cohost-release-windows" if os.name == "nt" else "cohost-release"
        version = "local-" + subprocess.check_output(["git", "rev-parse", "--short=12", "HEAD"], cwd=root).decode().strip()
        env["NEXUSD_BUILD_VERSION"] = version
        cargo("build", "--locked", "--profile", profile, "-p", "nexusd-cohost", "--features",
              "daemon,driver-ai,egress-gate-presidio", "--bin", "nexusd-cohost")
        binary = target / profile / ("nexusd-cohost" + suffix)
        path = command(str(binary), "--version")
        if version not in path.read_text(errors="replace"):
            raise RuntimeError("Co-host build version stamp was lost")
        limits = {"Windows": 42, "Darwin": 45 if platform.machine().lower() == "arm64" else 46, "Linux": 55}
        if binary.stat().st_size > limits[platform.system()] * 1024 * 1024:
            raise RuntimeError("Co-host exceeds the existing platform release size budget")
    elif name in {"rendering", "browser"}:
        if os.environ.get("SCODE_ACCEPTANCE_CONTAINER") == "1":
            benchmark(name, root, output, base, command, target)
        else:
            try:
                policy_ref, candidate_ref = run.execution_refs
            except AttributeError:
                policy_ref = candidate_ref = None
            container(name, root, output, base, command, execute,
                      policy_ref=policy_ref, candidate_ref=candidate_ref)
    else:
        raise RuntimeError(f"Unknown mandatory acceptance check: {name}")
    if not results:
        raise RuntimeError(f"No command ran for required check: {name}")
    return results


def execution_policy_ref():
    """Use the dispatcher's frozen policy even for an older author checkout."""
    try:
        policy_ref, _ = run.execution_refs
    except AttributeError:
        policy_ref = None
    return policy_ref or "origin/main"


def native_guard_files():
    """Bind external tool bytes through the existing executable fingerprint format."""
    return {
        "e2e/local-acceptance/examples_acceptance.py": "2dd117fb90141a8b5242f87c3fa56e00f0ba366e6b25cd460455614817b469cc",
        "e2e/local-acceptance/policy_guard_acceptance.py": "6ac93a84fcf01f07e9d201948566cef15f06f4f0f0a03a4909cc2915ac6459e2",
        "e2e/local-acceptance/profile_compat_acceptance.py": "3823cd9ef712daaea3166593f8686173f65ae66305c4e31b7b86903db41a67b2",
        "e2e/local-acceptance/grouped_targets_acceptance.py": "d86cace0ae23e2b706ccbe7f4dfbf17f4f2eacb1e1910ed41c93406a5f114df1",
        "e2e/local-acceptance/dependency_scope_acceptance.py": "62793773de094c8691ae078ded6cef2097aa433ae6943b79a3e84410da0529a3",
        ".github/scripts/check_pr_history.py": "1c0117d2045f6ac6c44ea327b08f9c08b30bad0b734381592458d39259f0dab7",
        ".github/local-acceptance/native_guard.py": "73bb0489909a9a99607b9430a12585349d17df72895e03355096dab406c6e7c2",
        ".github/local-acceptance/nextest_guard.py": "a59a5f82e5185a2d7617c57a3db24e337b0ca22c2a821205dbec8fa356592477",
        ".github/scripts/local_acceptance.py": "3fc6533bdf3b3040aae46896bc6de1fffeb4966edcf2d8c66f0c6995f0086b3b",
        "e2e/local-acceptance/run.py": "73713e7e051e29846c98de2ad312c76a6dd8470bf032db8bc642b03b7de86307",
        "e2e/local-acceptance/resume_acceptance.py": "5144633ecffa714d7f56dc991826cdccad5a338a5e828e75e0540254aa249ccd",
        "e2e/local-acceptance/release_acceptance.py": "c239ee4fd1ad442e0a9aad54c55de5c6245eced3f4fd6c0cecaf6e06a86bd804",
        "e2e/startup/run.py": "4fb4ac6b52331b8278459c18f54355bf8f8209d1ea0e87bca01a54c0ba3f77e4",
        "e2e/startup/reference.rs": "327e5a6fadd9dc707d8bbbd6268ecaa8f27df4945029a6cb51c41b9fc8162578",
    }


def stage_profile_executor(root, policy_ref, name, policy_root):
    """Materialize one hash-bound native tool set without changing the candidate."""
    files = native_guard_files()
    if name == "preflight":
        files = {".github/scripts/check_pr_history.py": files[".github/scripts/check_pr_history.py"]}
    elif name != "guard-contract":
        raise RuntimeError(f"No policy-owned native executor for {name}")
    for relative, expected in files.items():
        destination = (policy_root / relative).resolve()
        if not destination.is_relative_to(policy_root.resolve()):
            raise RuntimeError("Executor input escaped its policy snapshot")
        contents = subprocess.check_output(["git", "show", f"{policy_ref}:{relative}"], cwd=root)
        actual = hashlib.sha256(contents).hexdigest()
        if actual != expected:
            raise RuntimeError(f"Native guard input {relative} changed: SHA256 {actual}. "
                               "Update its protected pin before running acceptance.")
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(contents)
    if name == "preflight":
        return policy_root / ".github/scripts/check_pr_history.py"
    # The grouping fixture needs actual protected selection rules. Bind only
    # those fields: approval maps contain these recipe hashes and cannot be
    # included in their own input digest. Signer data is not used by the planner.
    policy = json.loads(subprocess.check_output(
        ["git", "show", f"{policy_ref}:spec/local_acceptance.json"], cwd=root))
    selection = {key: policy[key] for key in ["version", "repository", "checks"]}
    data = json.dumps(selection, sort_keys=True, separators=(",", ":")).encode()
    actual_scope = hashlib.sha256(data).hexdigest()
    if actual_scope != "524bc892fcd10dbf73663e67ef363367f904b2904ae89b188d1d2c9e85d92dd9":
        raise RuntimeError(f"Native grouping scope policy changed: SHA256 {actual_scope}. "
                           "Update the selection digest in .github/local-acceptance/checks.py stage_profile_executor, "
                           "then rerun contract-update through the installed hook. No grouping guard has started.")
    policy_file = policy_root / "spec/local_acceptance.json"
    policy_file.parent.mkdir(parents=True, exist_ok=True)
    policy_file.write_bytes(data)
    # The release fixture imports this module. Bind the exact branch it executes
    # without a cyclic byte hash of a file containing its own execution pins.
    spec = importlib.util.spec_from_file_location("guard_authority", policy_root / ".github/scripts/local_acceptance.py")
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    helper.ROOT = root
    expected = "137fad247c36f8eab6d05350ac037f275de2df0283638a136c251aa3b765b220"
    if helper.reachable_profile_commands_hashes(policy_ref, ["native-release"])["native-release"] != expected:
        raise RuntimeError("Native release guard recipe changed; update its protected execution pin")
    (policy_root / ".github/local-acceptance/checks.py").write_bytes(subprocess.check_output(
        ["git", "show", f"{policy_ref}:.github/local-acceptance/checks.py"], cwd=root))
    return policy_root / ".github/local-acceptance/native_guard.py"


def contract_container(root, output, command):
    """Keep the full decision matrix local without serial Windows Git startup costs."""
    try:
        policy_ref, candidate_ref = run.execution_refs
    except AttributeError:
        policy_ref = candidate_ref = None
    def revision(ref):
        return subprocess.check_output(["git", "rev-parse", "--verify", f"{ref}^{{commit}}"], cwd=root).decode().strip()
    protected = revision(policy_ref or "origin/main")
    candidate = revision(candidate_ref or "HEAD")
    context = output / "guard-image"
    context.mkdir()
    inputs_hash = hashlib.sha256()
    # Keep image bindings inside the existing executable fingerprint format.
    # A changed image/entrypoint must update its pin before any Docker work.
    pins = {
        "Dockerfile.guard": "7ac3c2e7507240482a89938560d1fc7786575b72866b54280e4a11feca85fa9b",
        "guard.py": "aa5b4f97745372efeef46c6502980fd31427617ef0b7f7418b907f213839a308",
    }
    for name, destination in [("Dockerfile.guard", "Dockerfile"), ("guard.py", "guard.py")]:
        contents = subprocess.check_output(["git", "show", f"{protected}:.github/local-acceptance/{name}"], cwd=root)
        actual = hashlib.sha256(contents).hexdigest()
        if actual != pins[name]:
            raise RuntimeError(f"Guard input {name} changed: SHA256 {actual}. Update its contract_container pin "
                               "with the reviewed input, then rerun acceptance; no image build started.")
        (context / destination).write_bytes(contents)
        inputs_hash.update(name.encode() + b"\0" + contents + b"\0")
    image_receipt = output / "guard-image-id.txt"
    command("docker", "build", "--iidfile", str(image_receipt), "--tag",
            "sudocode-contract-guard:" + inputs_hash.hexdigest()[:24], str(context),
            extra={"DOCKER_BUILDKIT": "1"})
    image = image_receipt.read_text().strip()
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", image):
        raise RuntimeError("Contract image build did not return an immutable image ID")
    inputs = output / "guard-inputs"
    snapshot(root, candidate, inputs)
    (output / "guard-provenance.json").write_text(json.dumps({
        "candidate": candidate, "policy": protected, "image_id": image,
        "context_sha256": inputs_hash.hexdigest()}, indent=2))
    # This suite uses private temporary Git/Cargo fixtures, not benchmark state.
    command("docker", "run", "--rm", "--init", "--cpus", "2", "--memory", "2g", "--pids-limit", "256",
            "--mount", f"type=bind,source={inputs},target=/inputs,readonly", image)


def snapshot(root, ref, destination):
    destination.mkdir(parents=True, exist_ok=False)
    # Extract only Git-tracked inputs; never send target/, private settings or
    # untracked API credentials to Docker. The archive includes no .git data.
    with tempfile.TemporaryFile() as archive:
        subprocess.run(["git", "archive", "--format=tar", ref], cwd=root, stdout=archive, check=True)
        archive.seek(0)
        with tarfile.open(fileobj=archive) as stream:
            stream.extractall(destination, filter="data")


def container_storage():
    return Path.home() / ".cache/sudocode-local-acceptance/linux", "sudocode-acceptance-benchmark-lease"


def container_profile_output(evidence, name):
    """Map the mounted evidence root to the container entrypoint's profile output."""
    return evidence / "container" / name


def container(name, root, output, base, command, execute, policy_ref, candidate_ref):
    import hashlib

    state, lease_volume = container_storage()
    state.mkdir(parents=True, exist_ok=True)
    # Keep active Cargo/npm state on Docker's Linux filesystem. The previous
    # host cache stays read-only and seeds the volume under the same lease.
    def revision(ref):
        return subprocess.check_output(["git", "rev-parse", "--verify", f"{ref}^{{commit}}"], cwd=root).decode().strip()

    protected = revision(policy_ref or "origin/main")
    candidate = revision(candidate_ref or "HEAD")
    policy = json.loads(subprocess.check_output(["git", "show", f"{protected}:e2e/terminal-resize/performance-policy.json"], cwd=root))
    reference = revision(policy["baseline_commit"] if name == "rendering" else base)
    sources = output / "inputs"
    sources.mkdir()
    for label, ref in [("candidate", candidate), ("baseline", reference), ("trusted", protected)]:
        snapshot(root, ref, sources / label)
    context = output / "image-context"
    context_hash = hashlib.sha256()
    for source, destination in [
        (".github/local-acceptance/Dockerfile", "Dockerfile"),
        ("e2e/terminal-resize/package.json", "e2e/terminal-resize/package.json"),
        ("e2e/terminal-resize/package-lock.json", "e2e/terminal-resize/package-lock.json"),
    ]:
        path = context / destination
        path.parent.mkdir(parents=True, exist_ok=True)
        contents = subprocess.check_output(["git", "show", f"{protected}:{source}"], cwd=root)
        path.write_bytes(contents)
        context_hash.update(destination.encode() + b"\0" + contents + b"\0")
    image = f"sudocode-local-acceptance:{context_hash.hexdigest()[:24]}"
    image_receipt = output / "image-id.txt"
    # This map exists only in the current trusted runner process. Never infer
    # provenance from a previous output directory or a shared mutable cache.
    try:
        invocations = container.invocations
    except AttributeError:
        invocations = {}
    key = (str(output.parent.resolve()), candidate, protected, context_hash.hexdigest())
    previous = invocations.get(key) if name == "browser" else None
    if previous is None:
        command("docker", "build", "--iidfile", str(image_receipt), "--tag", image, str(context),
                extra={"DOCKER_BUILDKIT": "1"})
    else:
        image_receipt.write_text(previous["image_id"] + "\n", encoding="utf-8")
        print("Reusing this invocation's immutable rendering image for browser acceptance: "
              f"{previous['image_id']}", flush=True)
    image_id = image_receipt.read_text().strip()
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", image_id):
        raise RuntimeError(f"Docker build did not produce an immutable image ID: {image_receipt}")
    identity = {
        "candidate": candidate, "baseline": reference, "policy": protected,
        "image_id": image_id, "image_tag": image, "context_sha256": context_hash.hexdigest(),
    }
    (output / "container-provenance.json").write_text(json.dumps(identity, indent=2))
    (sources / "identity.json").write_text(json.dumps(identity, indent=2))
    if previous is not None:
        reused = sources / "candidate-release"
        reused.mkdir()
        shutil.copy2(previous["binary"], reused / "scode")
        (reused / "identity.json").write_text(json.dumps({
            **identity, "version": 1, "sha256": previous["sha256"],
        }, indent=2))
        verified_candidate_release(sources)
    runner = sources / "runner/.github"
    (runner / "local-acceptance").mkdir(parents=True)
    (runner / "scripts").mkdir()
    shutil.copyfile(__file__, runner / "local-acceptance/checks.py")
    shutil.copyfile(execute.__code__.co_filename, runner / "scripts/local_acceptance.py")
    command("docker", "run", "--rm", "--init", "--cpus", "2", "--memory", "8g", "--pids-limit", "2048",
            "--mount", f"type=bind,source={sources},target=/inputs,readonly",
            "--mount", f"type=bind,source={output},target=/evidence",
            "--mount", f"type=volume,source={lease_volume}-cache-v1,target=/state",
            "--mount", f"type=bind,source={state},target=/legacy-state,readonly",
            "--mount", f"type=volume,source={lease_volume},target=/locks",
            "--env", "SCODE_ACCEPTANCE_CONTAINER=1", "--env", "CARGO_HOME=/state/cargo", image_id,
            "/inputs/runner/.github/local-acceptance/checks.py", "--container-check", name,
            "--base", base)
    if name == "rendering":
        profile_output = container_profile_output(output, name)
        binary = profile_output / "scode-candidate"
        report = json.loads((profile_output / "measurements/report.json").read_text())
        digest = hashlib.sha256(binary.read_bytes()).hexdigest()
        if report["status"] != "passed" or report["candidate_sha256"] != digest or checkout_link(binary):
            raise RuntimeError("Rendering candidate binary does not match its completed measurements")
        invocations[key] = {"binary": binary, "sha256": digest, "image_id": image_id}
        container.invocations = invocations


def verified_candidate_release(inputs):
    """Accept only the same invocation's read-only, hash-bound normal release."""
    directory = inputs / "candidate-release"
    if not directory.exists():
        return None
    binary, receipt = directory / "scode", directory / "identity.json"
    if any(checkout_link(path) for path in [directory, binary, receipt]) or not binary.is_file():
        raise RuntimeError("Reused benchmark release must contain regular files")
    expected = json.loads((inputs / "identity.json").read_text())
    observed = json.loads(receipt.read_text())
    if observed.get("version") != 1 or any(observed.get(name) != expected[name]
            for name in ["candidate", "policy", "image_id", "context_sha256"]):
        raise RuntimeError("Reused benchmark release belongs to different source, policy or image")
    if hashlib.sha256(binary.read_bytes()).hexdigest() != observed.get("sha256"):
        raise RuntimeError("Reused benchmark release bytes changed after rendering")
    return binary


def copy_checkout_file(source, destination):
    """Keep identical inputs stable and give changed bytes fresh Cargo mtimes."""
    source, destination = Path(source), Path(destination)
    if checkout_link(destination):
        raise RuntimeError(f"Expected a regular benchmark input, found a link: {destination}")
    if (destination.is_file() and source.stat().st_size == destination.stat().st_size
            and stat.S_IMODE(source.stat().st_mode) == stat.S_IMODE(destination.stat().st_mode)):
        # Compare bytes on every invocation. filecmp caches same-size/mtime
        # comparisons and can miss a changed cached file with restored times.
        with source.open("rb") as expected, destination.open("rb") as cached:
            while True:
                first, second = expected.read(1024 * 1024), cached.read(1024 * 1024)
                if first != second:
                    break
                if not first:
                    return destination
    shutil.copyfile(source, destination)
    shutil.copymode(source, destination)
    return destination


def checkout_link(path):
    return path.is_symlink() or (hasattr(path, "is_junction") and path.is_junction())


def remove_checkout_entry(path, root):
    """Remove an entry inside the leased checkout without traversing cache links."""
    if not path.parent.resolve().is_relative_to(root.resolve()):
        raise RuntimeError(f"Benchmark cache entry escaped its checkout: {path}")
    if path.is_symlink() or not path.is_dir():
        path.unlink()
    elif hasattr(path, "is_junction") and path.is_junction():
        os.rmdir(path)
    else:
        shutil.rmtree(path)


def replace_checkout(source, destination, *, root=None):
    """Synchronize the exact snapshot while preserving unchanged compiler inputs."""
    source, destination = Path(source), Path(destination)
    if checkout_link(destination):
        raise RuntimeError(f"Benchmark checkout root must be a real directory: {destination}")
    root = destination if root is None else root
    destination.mkdir(parents=True, exist_ok=True)
    for cached in destination.iterdir():
        original = source / cached.name
        if not original.exists() and not original.is_symlink():
            remove_checkout_entry(cached, root)
    for original in source.iterdir():
        cached = destination / original.name
        if original.is_dir():
            if checkout_link(cached) or (cached.exists() and not cached.is_dir()):
                remove_checkout_entry(cached, root)
            replace_checkout(original, cached, root=root)
            shutil.copymode(original, cached)
        else:
            if checkout_link(cached) or (cached.exists() and not cached.is_file()):
                remove_checkout_entry(cached, root)
            copy_checkout_file(original, cached)


@contextmanager
def exclusive_benchmark_state(directory=Path("/locks")):
    """Serialize the Linux container's mutable sources, npm trees and targets."""
    import fcntl

    # Docker Desktop bind mounts do not provide reliable flock exclusion.
    # The shared named volume keeps this inode on the Linux filesystem.
    with (directory / "benchmark.lock").open("a") as lease:
        started = time.monotonic()
        try:
            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print(f"Waiting for the local benchmark cache lease: {directory / 'benchmark.lock'}. "
                  "Another local acceptance owns the shared checkout and compiler outputs.", flush=True)
            if directory == Path("/locks"):
                print("Inspect contenders on the host: docker ps --filter volume=sudocode-acceptance-benchmark-lease", flush=True)
            fcntl.flock(lease, fcntl.LOCK_EX)
        print(f"Acquired local benchmark cache lease after {time.monotonic() - started:.3f}s", flush=True)
        yield


def benchmark(name, root, output, base, command, target):
    with exclusive_benchmark_state():
        migrate_benchmark_cache(Path("/legacy-state"), Path("/state"))
        benchmark_locked(name, root, output, base, command, target)


def migrate_benchmark_cache(legacy, state):
    """Seed the Linux volume without changing canonical paths or Cargo mtimes."""
    marker = state / ".host-cache-migration-v1.json"
    if checkout_link(marker):
        raise RuntimeError(f"Benchmark cache migration receipt must be a regular file: {marker}")
    if marker.is_file():
        receipt = json.loads(marker.read_text(encoding="utf-8"))
        if receipt.get("version") != 1 or receipt.get("status") != "complete":
            raise RuntimeError(f"Invalid benchmark cache migration receipt: {marker}")
        return
    if not legacy.is_dir() or checkout_link(legacy) or checkout_link(state):
        raise RuntimeError("Benchmark cache migration requires real legacy/state directories")
    started = time.monotonic()
    copied = []
    print("Preparing Linux benchmark cache under the shared lease. "
          "Existing host cache stays read-only; source/target paths and timestamps are preserved. "
          "This one-time copy counts toward local acceptance time.", flush=True)
    # Ignore pre-lease disposable checkouts. Both profiles already share these
    # canonical paths; preserving them keeps existing Cargo dep-info reusable.
    for name in ["cargo", "leased-benchmarks-v1"]:
        source = legacy / name
        if checkout_link(source):
            raise RuntimeError(f"Unexpected benchmark cache root: {source}")
        if not source.exists():
            continue
        destination = state / name
        if checkout_link(source) or checkout_link(destination) or not source.is_dir():
            raise RuntimeError(f"Unexpected benchmark cache root: {source}")
        destination.mkdir(parents=True, exist_ok=True)
        print(f"Copying existing {name} cache to Linux storage; no compiler runs during migration.", flush=True)
        # cp -a retains nanosecond mtimes, modes and internal Cargo links. A
        # failed/interrupted copy leaves no completion marker and is retried.
        subprocess.run(["nice", "-n", "10", "cp", "-a", "--", str(source) + "/.", str(destination)], check=True)
        copied.append(name)
    receipt = {"version": 1, "status": "complete", "copied": copied,
               "elapsed_seconds": round(time.monotonic() - started, 3)}
    temporary = marker.with_suffix(".tmp")
    temporary.write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
    temporary.replace(marker)
    print(f"Linux benchmark cache ready after {receipt['elapsed_seconds']:.3f}s; "
          f"reused directories: {copied}. Full command logs remain in the host evidence directory.", flush=True)


def benchmark_storage(name):
    """Keep leased state disjoint from still-running pre-lease executors."""
    state = Path("/state/leased-benchmarks-v1")
    return state / name, state / "target" / name


def rendering_cases(terminal, arguments, output, command):
    """Finish and judge each complete scenario before admitting the next one."""
    policy_path = terminal / "performance-policy.json"
    policy = json.loads(policy_path.read_text(encoding="utf-8"))
    cases = policy["profiles"]["core"]
    if not cases or len(cases) != len(set(cases)) or any(
            not re.fullmatch(r"[a-zA-Z0-9_-]+", case) for case in cases):
        raise RuntimeError("Rendering policy must declare distinct, safe scenario names")
    output.mkdir(parents=True, exist_ok=False)
    report = {"version": 1, "profile": "core", "preview": "0", "policy": policy,
              "policy_sha256": hashlib.sha256(policy_path.read_bytes()).hexdigest(),
              "status": "running", "samples": [], "metrics": [], "calibration": [],
              "completed_cases": [], "remaining_cases": list(cases), "case_policy_sha256": {}}
    metadata = None

    def save():
        (output / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    for case in cases:
        # Only partition the selected scenarios. Workloads, sample counts,
        # A/A calibration, alternating A/B pairs and every budget stay intact.
        selected = dict(policy, profiles={**policy["profiles"], "core": [case]})
        selected_path = output / f"{case}-policy.json"
        selected_path.write_text(json.dumps(selected, indent=2) + "\n", encoding="utf-8")
        report["case_policy_sha256"][case] = hashlib.sha256(selected_path.read_bytes()).hexdigest()
        report["active_case"] = case
        save()
        print(f"Rendering scenario {len(report['completed_cases']) + 1}/{len(cases)}: {case}. "
              f"Complete paired result: {output / case / 'report.json'}. "
              "A failed scenario stops this matrix and releases the benchmark lease.", flush=True)
        succeeded = False
        try:
            command(*arguments, "--output", str(output / case), "--policy", str(selected_path), "--profile", "core")
            succeeded = True
        finally:
            child_path = output / case / "report.json"
            try:
                if child_path.exists():
                    child = json.loads(child_path.read_text(encoding="utf-8"))
                    if child["policy"] != selected or child["policy_sha256"] != report["case_policy_sha256"][case]:
                        raise RuntimeError(f"Rendering scenario {case} used a different policy")
                    for key in ["base_sha256", "candidate_sha256", "driver_sha256", "harness_sha256"]:
                        if key in report and report[key] != child[key]:
                            raise RuntimeError(f"Rendering {key} changed between scenarios")
                        report[key] = child[key]
                    for sample in child["samples"]:
                        observed = sample["data"]["metadata"]
                        if metadata is not None and metadata != observed:
                            raise RuntimeError("Rendering terminal observer/platform changed between scenarios")
                        metadata = observed
                        report["samples"].append({**sample, "path": f"{case}/{sample['path']}"})
                    for key in ["metrics", "calibration"]:
                        report[key].extend(child.get(key, []))
                    report["failures"] = child.get("failures", [])
                    if succeeded and child["status"] == "passed":
                        report["completed_cases"].append(case)
                        report["remaining_cases"].remove(case)
                    else:
                        report["status"] = child["status"] if child["status"] in {"failed", "noisy", "invalid"} else "invalid"
                else:
                    report.update(status="invalid", failures=[f"{case}: no completed scenario report; see command log"])
            except (KeyError, ValueError, TypeError, OSError, RuntimeError) as error:
                report.update(status="invalid", failures=[str(error)])
                raise
            finally:
                save()
        if not succeeded or report["status"] != "running":
            raise RuntimeError(f"Rendering scenario {case} did not pass; scenarios without a pass: "
                               f"{report['remaining_cases']}. See {output / 'report.json'}")
    report.update(status="passed", active_case=None)
    save()


def benchmark_locked(name, root, output, base, command, target):
    # Reuse the fixed existing workload, A/A calibration, paired A/B rounds,
    # budget and sample assertions. Changing execution host changes none of them.
    inputs = root.parent
    work, targets = benchmark_storage(name)
    # Both profiles compile the same candidate. Keep its canonical source path
    # and Cargo outputs together, under the existing shared exclusion lease.
    candidate_work, candidate_targets = benchmark_storage("rendering")
    checkouts = {label: (candidate_work if label == "candidate" else work) / label
                 for label in ["candidate", "baseline", "trusted"]}
    build_targets = {"baseline": targets / "baseline", "candidate": candidate_targets / "candidate"}
    work.mkdir(parents=True, exist_ok=True)
    for label in ["candidate", "baseline", "trusted"]:
        destination = checkouts[label]
        # Sources are disposable copies from this invocation's exact Git archive.
        replace_checkout(inputs / label, destination)
    candidate, baseline, trusted = (checkouts[label] for label in ["candidate", "baseline", "trusted"])
    driver_source = trusted if name == "rendering" else candidate
    terminal = driver_source / "e2e/terminal-resize"
    # Dependencies must match the checkout's lock; the image supplies the exact
    # Chromium and its system libraries, not a different JS dependency graph.
    command("npm", "ci", "--prefix", str(terminal))
    driver_name = "pty_render_performance" if name == "rendering" else "pty_hyperlinks"
    reused = verified_candidate_release(inputs) if name == "browser" else None
    for label, source in [("baseline", baseline), ("candidate", candidate)]:
        if label == "candidate" and reused is not None:
            shutil.copy2(reused, output / "scode-candidate")
            print("Browser acceptance reuses the measured normal release from rendering; "
                  "source, protected policy, immutable image and binary SHA256 match. "
                  "Browser drivers and every observer still run.", flush=True)
            continue
        command("cargo", "build", "--locked", "--release", "--manifest-path", str(source / "rust/Cargo.toml"),
                "--target-dir", str(build_targets[label]), "-p", "rusty-sudocode-cli", "--bin", "scode")
        shutil.copy2(build_targets[label] / "release/scode", output / f"scode-{label}")
    # Share artifacts only with the same canonical source checkout. The copied
    # production binaries above stay fixed when dev features build the drivers.
    driver_target = targets / "driver" if name == "rendering" else build_targets["candidate"]
    log = command("cargo", "test", "--locked", "--release", "--manifest-path", str(driver_source / "rust/Cargo.toml"),
                  "--target-dir", str(driver_target), "-p", "rusty-sudocode-cli", "--test", driver_name,
                  "--no-run", "--message-format=json-render-diagnostics")
    messages = []
    for line in log.read_text(errors="replace").splitlines():
        if line.startswith("{"):
            messages.append(json.loads(line))
    drivers = [row["executable"] for row in messages if row.get("reason") == "compiler-artifact"
               and row.get("target", {}).get("name") == driver_name and row.get("executable")]
    if len(drivers) != 1:
        raise RuntimeError(f"Expected exactly one compiled {driver_name} driver: {drivers}")
    script = terminal / ("benchmark.py" if name == "rendering" else "gui-benchmark.py")
    args = [sys.executable, str(script), "--driver", drivers[0], "--base", str(output / "scode-baseline"),
            "--candidate", str(output / "scode-candidate")]
    if name == "rendering":
        rendering_cases(terminal, args, output / "measurements", command)
    else:
        command(*args, "--output", str(output / "measurements"))
    if name == "browser":
        # Move the existing viewport gates too; deleting the old release job
        # must not silently drop its headless/browser CPU and painting checks.
        manifest = str(candidate / "rust/Cargo.toml")
        viewport_target = str(build_targets["candidate"])
        viewport_env = {"SCODE_TEST_BIN": str(output / "scode-candidate"),
                        "SCODE_VIEWPORT_MEASURE_DIR": str(output / "viewport-paint")}
        log = command("cargo", "test", "--release", "--locked", "--manifest-path", manifest,
                      "--target-dir", viewport_target, "-p", "rusty-sudocode-cli", "--test", "pty_prompt_viewport",
                      "--no-run", "--message-format=json-render-diagnostics", extra=viewport_env)
        records = [json.loads(line) for line in log.read_text(errors="replace").splitlines() if line.startswith("{")]
        drivers = [row["executable"] for row in records if row.get("reason") == "compiler-artifact"
                   and row.get("target", {}).get("name") == "pty_prompt_viewport" and row.get("executable")]
        if len(drivers) != 1:
            raise RuntimeError("Missing viewport measurement driver")
        command("cargo", "test", "--release", "--locked", "--manifest-path", manifest, "--target-dir", viewport_target,
                "-p", "rusty-sudocode-cli", "--test", "pty_prompt_viewport", "viewport_browser_interaction",
                "--", "--ignored", "--exact", "--nocapture", extra=viewport_env)
        for observer in ["headless", "browser"]:
            arguments = [sys.executable, str(candidate / "scripts/benchmark_prompt_viewport.py"),
                         "--driver", drivers[0], "--binary", str(output / "scode-candidate"),
                         "--output", str(output / ("viewport-" + observer))]
            if observer == "browser":
                arguments += ["--observer", "browser"]
            command(*arguments, extra=viewport_env)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--container-check", choices=["rendering", "browser"])
    parser.add_argument("--native-checks", nargs="*")
    parser.add_argument("--base", required=True)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    runner = Path(__file__).resolve().parents[2]
    root = Path("/inputs/candidate") if args.container_check else runner
    spec = importlib.util.spec_from_file_location("local_acceptance", runner / ".github/scripts/local_acceptance.py")
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    names = [args.container_check] if args.container_check else args.native_checks
    for name in names:
        output = args.output / name if args.output is not None else container_profile_output(Path("/evidence"), name)
        results = run(name, root, output, args.base, helper.run_command)
        (output / "commands.json").write_text(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
