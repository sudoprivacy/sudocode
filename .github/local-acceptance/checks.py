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


def github_api_request(url, accept):
    """Keep credentials on the original GitHub API request, never redirects."""
    if not url.startswith('https://api.github.com/repos/'):
        raise RuntimeError('Release credentials may only address the GitHub API')
    request = urllib.request.Request(url, headers={'Accept': accept})
    token = os.environ.get('GH_TOKEN') or os.environ.get('GITHUB_TOKEN')
    if token:
        request.add_unredirected_header('Authorization', f'Bearer {token}')
    return request


def github_asset_download(repository, tag, filename, destination):
    """Fetch one named release asset through GitHub's public metadata/binary API."""
    endpoint = f'https://api.github.com/repos/{repository}'
    request = github_api_request(f'{endpoint}/releases/tags/{tag}', 'application/vnd.github+json')
    with urllib.request.urlopen(request, timeout=30) as response:
        release = json.load(response)
    assets = [asset for asset in release['assets'] if asset['name'] == filename]
    if release['tag_name'] != tag or len(assets) != 1:
        raise RuntimeError(f'Expected exactly one pinned GitHub release asset: {repository}/{tag}/{filename}')
    asset_id = assets[0]['id']
    if not isinstance(asset_id, int) or asset_id <= 0:
        raise RuntimeError('The pinned GitHub release returned an invalid asset identity')
    download = f'{endpoint}/releases/assets/{asset_id}'
    print('GITHUB_ASSET_DOWNLOAD=' + json.dumps({
        'repository': repository, 'tag': tag, 'filename': filename, 'api': download,
    }), flush=True)
    request = github_api_request(download, 'application/octet-stream')
    with urllib.request.urlopen(request, timeout=30) as response:
        destination.write_bytes(response.read())


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
            github_asset_download('nextest-rs/nextest', f'cargo-nextest-{version}', filename, temporary_path)
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


def cohost_protoc():
    """Prepare the original platform-pinned compiler without changing global PATH."""
    if platform.system() not in {"Windows", "Darwin"}:
        return None
    architecture = "aarch_64" if platform.machine().lower() == "arm64" else "x86_64"
    archive_name = "win64" if os.name == "nt" else f"osx-{architecture}"
    folder = Path.home() / ".cache/sudocode-protoc-3.20.2" / archive_name
    executable = folder / "bin" / ("protoc.exe" if os.name == "nt" else "protoc")
    if not executable.exists():
        folder.mkdir(parents=True, exist_ok=True)
        archive = folder / "protoc.zip"
        github_asset_download('protocolbuffers/protobuf', 'v3.20.2',
                              f'protoc-3.20.2-{archive_name}.zip', archive)
        with zipfile.ZipFile(archive) as stream:
            stream.extractall(folder)
        executable.chmod(0o755)
    tool_env = {key: value for key, value in os.environ.items()
                if key not in {'GH_TOKEN', 'GITHUB_TOKEN'}}
    version = subprocess.check_output([str(executable), "--version"], text=True, env=tool_env).strip()
    if version != "libprotoc 3.20.2":
        raise RuntimeError(f"Cohost protoc version differs from the pinned 3.20.2: {version}")
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


def docker_engine_probe_command(timeout):
    """Bound the Engine version request without discovering optional CLI plugins."""
    script = """import subprocess,sys
try:
    result = subprocess.run(sys.argv[2:], timeout=float(sys.argv[1]))
except subprocess.TimeoutExpired:
    sys.exit(f"DOCKER_ENGINE_PROBE_TIMEOUT: Docker Engine did not answer within {sys.argv[1]} seconds. Check Docker Desktop and this shell's DOCKER_HOST/context; container/native guards have not started.")
sys.exit(result.returncode)
"""
    return [sys.executable, "-c", script, str(timeout), shutil.which("docker") or "docker",
            "version", "--format", "{{.Server.Version}}"]


def supports_hosted_eight_workers(root):
    """Qualify the measured hosted increase; old or unknown fixtures keep four."""
    path = Path(root) / "rust/crates/rusty-sudocode-cli/tests/pty_bash_compact_results.rs"
    try:
        contents = path.read_bytes().replace(b"\r\n", b"\n")
    except OSError:
        return False
    return hashlib.sha256(contents).hexdigest() == "8c157cf9088b931a7d98ad3434a48a3fe2ab0cf23d1ea61be395da6c45bc7ed6"


def workspace_nextest_options(output, grouped, hosted_source_ready):
    """Raise the qualified grouped pool and preserve existing explicit limits."""
    options = nextest_options(output)
    cpus = os.cpu_count() or 0
    hosted = os.environ.get('GITHUB_ACTIONS') == 'true'
    hosted_minimum = 3 if platform.system() == 'Darwin' else 4
    eligible = cpus >= 8 or (hosted and cpus >= hosted_minimum and hosted_source_ready)
    if grouped and eligible and 'SCODE_ACCEPTANCE_TEST_THREADS' not in os.environ:
        options[options.index('--test-threads') + 1] = '8'
    if grouped and hosted and hosted_minimum <= cpus < 8:
        print('NATIVE_TEST_SOURCE_COMPATIBILITY=' + json.dumps({
            'qualified_resume_source': hosted_source_ready,
            'scope': 'Known measured source enables eight; old/unknown source retains four. '
                     'No source update is required to keep using four.',
        }), flush=True)
    print('NATIVE_TEST_WORKERS: ' + options[options.index('--test-threads') + 1]
          + ' test processes; compiler jobs/priorities, case isolation, retries and deadlines unchanged. '
          'SCODE_ACCEPTANCE_TEST_THREADS=1..4 lowers the pool. Grouped suites retain eight on hosts '
          'with at least eight logical CPUs; qualified hosted sources also use eight from four CPUs, '
          'or three on macOS. Old/unknown hosted sources below eight CPUs, smaller/unknown hosts '
          'and legacy/isolated/other profiles retain four.',
          flush=True)
    return options


def committed_git(root, *arguments):
    """Inspect this checkout independently of caller Git bindings."""
    return subprocess.check_output(
        ["git", *arguments], cwd=root,
        env={key: value for key, value in os.environ.items() if not key.startswith("GIT_")},
        stderr=subprocess.PIPE)


def committed_input_paths(root, base):
    """Read this checkout's committed inputs independently of caller Git bindings."""
    if not base:
        raise ValueError("No committed base")
    return committed_git(root, "diff", "--name-only", "--no-renames", "-z", f"{base}...HEAD").decode().split("\0")[:-1]


def compiler_context_driver_present(root):
    """Permit genuine older source, but refuse removal of an introduced journey."""
    relative = "e2e/compiler-context/run.py"
    if (root / relative).is_file():
        return True
    if committed_git(root, "rev-parse", "--is-shallow-repository").strip() != b"false":
        raise RuntimeError("Cannot prove historical compiler-driver absence from shallow Git history")
    if committed_git(root, "log", "-1", "--format=%H", "HEAD", "--", relative).strip():
        raise RuntimeError("Required compiler context acceptance driver is missing from this source")
    return False


def changed_cli_test_targets(root, base):
    """Scope isolated test edits; any shared or production input keeps the full suite."""
    # Complementary CI can pass newer main; exclude changes outside this branch.
    changed = committed_input_paths(root, base)
    # External specs/fixtures can affect unchanged tests too. Only known docs
    # may accompany an isolated integration module without widening acceptance.
    inputs = [path for path in changed if path != "README.md"
              and not (path.startswith("docs/") and path.endswith(".md"))]
    pattern = r"rust/crates/rusty-sudocode-cli/tests/([A-Za-z0-9_]+)\.rs"
    if not inputs or any(not re.fullmatch(pattern, path) or not (root / path).is_file() for path in inputs):
        return None
    return sorted({Path(path).stem for path in inputs})


def native_cli_test_package_args(root, base):
    """Match the native workspace's package selection, including feature union."""
    try:
        isolated = changed_cli_test_targets(root, base) is not None
    except (OSError, ValueError, subprocess.CalledProcessError):
        isolated = False
    arguments = ["-p", "rusty-sudocode-cli"] if isolated else ["--workspace"]
    print("NATIVE_CLI_TEST_GRAPH=" + json.dumps({"packages": arguments, "isolated_cli_tests": isolated}), flush=True)
    return arguments


def native_resize_test_targets(*, windows):
    """Share the real terminal target inventory with its Cargo graph guard."""
    targets = ["pty_chrome_resize", "pty_prose_preview", "pty_streaming_code", "pty_review_scrollback"]
    if not windows:
        targets.append("pty_codex_colors")
    return targets


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


def workspace_integration_test_targets(targets):
    """Retire grouped unit harnesses; preserve older Cargo default exclusions."""
    if not targets:
        # An explicit glob enables test=false targets and can force inactive
        # required-features. Older sources keep their complete default suite.
        return []
    if targets[:2] != ["--lib", "--bins"]:
        raise RuntimeError("Unexpected grouped workspace target plan; no tests started")
    selected = targets[2:]
    if not selected or len(selected) % 2 or any(selected[index] != "--test" for index in range(0, len(selected), 2)):
        raise RuntimeError("Integration target selection must retain explicit nonempty test names")
    return selected


def performance_low_power_mode():
    """Use Foundation's active mode, including desktops without a pmset setting."""
    import ctypes

    ctypes.CDLL("/System/Library/Frameworks/Foundation.framework/Foundation")
    objc = ctypes.CDLL("/usr/lib/libobjc.A.dylib")
    objc.objc_getClass.argtypes, objc.objc_getClass.restype = [ctypes.c_char_p], ctypes.c_void_p
    objc.sel_registerName.argtypes, objc.sel_registerName.restype = [ctypes.c_char_p], ctypes.c_void_p
    pointer = ctypes.CFUNCTYPE(ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p)(("objc_msgSend", objc))
    boolean = ctypes.CFUNCTYPE(ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p)(("objc_msgSend", objc))
    responds = ctypes.CFUNCTYPE(ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p)(("objc_msgSend", objc))
    info = pointer(objc.objc_getClass(b"NSProcessInfo"), objc.sel_registerName(b"processInfo"))
    selector = objc.sel_registerName(b"isLowPowerModeEnabled")
    if not info or not responds(info, objc.sel_registerName(b"respondsToSelector:"), selector):
        raise RuntimeError("Foundation cannot report the active low-power mode")
    return boolean(info, selector)


def performance_power_state():
    """Read native macOS power settings without changing the user's configuration."""
    source = subprocess.check_output(["/usr/bin/pmset", "-g", "ps"], text=True, timeout=5)
    sources = re.findall(r"^Now drawing from '([^']+)'", source, re.MULTILINE)
    if len(sources) != 1 or sources[0] not in {"AC Power", "Battery Power"}:
        raise RuntimeError("Cannot determine macOS power source; no timing acceptance granted")
    return {"source": sources[0], "low_power_mode": performance_low_power_mode()}


def stable_performance_power(output):
    """Create the timing context only when a profile actually uses it."""
    @contextmanager
    def monitor_window():
        """Refuse unqualified macOS timing windows; keep every observation for diagnosis."""
        if platform.system() != "Darwin":
            yield
            return
        import threading

        evidence = output / "host-power.json"
        observations, failures = [], []
        stopped = threading.Event()

        def sample():
            row = {"at": time.time()}
            try:
                row.update(performance_power_state())
                if row["source"] != "AC Power" or row["low_power_mode"]:
                    failures.append("Timing acceptance requires external AC power with low-power mode disabled")
            except Exception as error:
                row["error"] = str(error)
                failures.append(str(error))
            observations.append(row)
            staged = evidence.with_suffix(".json.stage")
            staged.write_text(json.dumps({
                "kind": "native-performance-power-observations", "poll_interval_seconds": 1,
                "observations": observations, "invalid": bool(failures),
            }, indent=2) + "\n")
            staged.replace(evidence)

        def require_stable():
            if failures:
                raise RuntimeError(f"{failures[0]}. Power evidence: {evidence}. "
                                   "No settings were changed; preserve this attempt and connect AC power before a new run.")

        sample()
        require_stable()
        print(f"Performance host: external AC, low-power mode disabled; monitoring each second: {evidence}", flush=True)

        def monitor():
            while not stopped.wait(1):
                try:
                    sample()
                except Exception as error:
                    failures.append(f"Cannot retain native power observations: {error}")
                    return

        worker = threading.Thread(target=monitor, name="acceptance-power", daemon=True)
        worker.start()
        try:
            yield
        except BaseException:
            # Preserve the original command failure; the sidecar still records any
            # power problem. A failed command can never become a passing profile.
            raise
        else:
            stopped.set()
            worker.join()
            sample()
            require_stable()
        finally:
            stopped.set()
            worker.join()
    return monitor_window()


NATIVE_RUST_PTY_CHECKS = {"native-workspace", "native-resize", "native-release", "native-services"}


def compiler_cache_profiles():
    return NATIVE_RUST_PTY_CHECKS | {"native-context", "native-cohost", "native-examples"}


def prepare_compiler_cache(output):
    """Bind the separate cache implementation before loading or executing it."""
    contents = Path(__file__).with_name("compiler_cache.py").read_bytes().replace(b"\r\n", b"\n")
    if hashlib.sha256(contents).hexdigest() != "35a0c3335d0e5a4e07570cb5fd75edfe60f5d61cd5514738efb35cf744f630e7":
        raise RuntimeError(f"Compiler-cache input {Path(__file__).with_name('compiler_cache.py')} changed: "
                           f"SHA256 {hashlib.sha256(contents).hexdigest()}. Update the reviewed source pin in "
                           "prepare_compiler_cache, then rerun the original supported contract-update command. "
                           "No native build has started.")
    # A fresh path prevents timestamp-based Python bytecode reuse across trees.
    with tempfile.TemporaryDirectory(prefix="scode-compiler-cache-") as private:
        path = Path(private) / "compiler_cache.py"
        path.write_bytes(contents)
        spec = importlib.util.spec_from_file_location("verified_compiler_cache", path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
    tool = module.prepare(github_asset_download)
    output.mkdir(parents=True, exist_ok=True)
    (output / "compiler-cache-source.json").write_text(json.dumps(tool, indent=2) + "\n", encoding="utf-8")
    return module, tool


def native_target_directory(root):
    return Path(os.environ.get("CARGO_TARGET_DIR", root / "rust/target")).resolve()


def windows_terminal_command(root):
    """Share the exact installer invocation with preparation and its receipt."""
    terminal = Path.home() / ".cache/sudocode-terminal-host"
    return [sys.executable, "e2e/terminal-resize/setup_windows.py", str(terminal),
            "--native-target", str(native_target_directory(root))]


def prepare_windows_terminal(root, output, execute):
    """Retain one verified provider for ordinary Rust PTYs and visual journeys."""
    output.mkdir(parents=True, exist_ok=True)
    return execute(windows_terminal_command(root), root, output / "000.log")


def verify_native_terminal(log, target):
    """Refuse successful legacy installers that did not provision the drivers."""
    records = [json.loads(line.removeprefix("NATIVE_CONPTY_HOST="))
               for line in log.read_text(encoding="utf-8").splitlines()
               if line.startswith("NATIVE_CONPTY_HOST=")]
    if len(records) != 1:
        raise RuntimeError("Native terminal preparation did not report exactly one verified provider. "
                           "Include the current terminal installer in this source before qualifying it.")
    identity = records[0]
    directory = target.resolve() / "debug/deps"
    if (Path(identity["driver_directory"]).resolve() != directory
            or identity["architecture"] not in {"x64", "arm64"}
            or not re.fullmatch(r"[0-9a-f]{40}", identity["distribution_commit"])
            or not re.fullmatch(r"[0-9a-f]{64}", identity["archive_sha256"])
            or (os.name == "nt" and identity["dynamic_load_checked"] is not True)):
        raise RuntimeError("Native terminal preparation did not verify this target/provider ABI")
    files = identity["files"]
    if len(files) != 2 or {Path(item["path"]).name for item in files} != {"conpty.dll", "OpenConsole.exe"}:
        raise RuntimeError("Native terminal preparation must report both native sidecars")
    for item in files:
        path = Path(item["path"])
        if (path.resolve() != directory / path.name
                or not re.fullmatch(r"[0-9a-f]{64}", item["sha256"])
                or hashlib.sha256(path.read_bytes()).hexdigest() != item["sha256"]):
            raise RuntimeError(f"Native terminal sidecar differs from the prepared provider: {path}")
    return identity


def terminal_installer_scope(root, base):
    """Retain fault injection for unknown inputs; Rust-only edits keep real setup."""
    try:
        paths = committed_input_paths(root, base)
    except (OSError, ValueError, subprocess.CalledProcessError):
        return {"resolved": False, "fault_injection_required": True, "changed_control_inputs": None}
    controls = [path for path in paths if not path.startswith("rust/")
                and path not in {"README.md", "CONTRIBUTING.md", "sudo-code-roadmap.html"}
                and not (path.startswith("docs/") and path.endswith(".md"))]
    return {"resolved": True, "fault_injection_required": bool(controls), "changed_control_inputs": controls}


def windows_native_shells():
    """Share the existing shell requirements before admission and at execution."""
    if os.name != "nt":
        return []
    system_root = os.environ.get("SystemRoot")
    legacy = Path(system_root) / "System32/WindowsPowerShell/v1.0/powershell.exe" if system_root else None
    pwsh = shutil.which("pwsh.exe")
    shells = [legacy, Path(pwsh) if pwsh else None]
    missing = [name for name, path in zip(["Windows PowerShell", "PowerShell 7 (pwsh.exe)"], shells)
               if path is None or not path.is_file()]
    if missing:
        raise RuntimeError("Windows native acceptance requires " + ", ".join(missing) + ". "
                           "Add the existing PowerShell 7 directory to this process's PATH, or install it with "
                           "`winget install --id Microsoft.PowerShell --exact --source winget`, "
                           "then rerun the same acceptance command. Full acceptance remains required.")
    return shells


def validate_before_host_lease(names, root, output):
    """Reject invalid guard inputs early; execution still stages its own copy."""
    if set(names) & {"guard-contract", "native-workspace"}:
        shells = windows_native_shells()
        if shells:
            print("NATIVE_HOST_PREREQUISITES=" + json.dumps({
                "windows_shells": list(map(str, shells)),
                "stage": "before host queue; execution rechecks these paths",
                "full_acceptance_required": True,
            }), flush=True)
    if "guard-contract" not in names:
        return
    print("GUARD_STATIC_PREFLIGHT: validate protected pins, selection and release recipe before "
          "joining the host queue; no Docker/native field has started. Full guard remains required.", flush=True)
    executor = stage_profile_executor(root, execution_policy_ref(), "guard-contract",
                                      output / "guard-contract" / "prequeue-static-policy")
    spec = importlib.util.spec_from_file_location("prequeue_native_inventory", executor)
    guard = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(guard)
    # These paths only construct argv. Tool preparation and actual execution
    # remain inside the lease; this snapshot grants no completed-check credit.
    pending = argparse.Namespace(executable=Path("not-prepared-nextest"),
                                 source_receipt=Path("not-prepared-source.json"), options="{}")
    fixtures, protocol, native = guard.fixture_inventory(root, output / "guard-contract" / "native", pending)
    print("GUARD_NATIVE_INVENTORY=" + json.dumps({
        "protocol": len(protocol), "native": len(native), "total": len(fixtures),
        "stage": "before host queue; execution rebuilds and rechecks the same inventory",
        "full_acceptance_required": True,
    }), flush=True)
    print("GUARD_STATIC_PREFLIGHT: passed; execution will restage and revalidate the protected Git inputs.",
          flush=True)


def git_for_windows_bash(path=None):
    """Git for Windows' own Bash, located from the git on PATH and never WSL's.

    A git hook runs with Git's mingw64/libexec/git-core first on PATH, three
    levels below the bin/bash.exe of the same installation, so look in every
    ancestor of the resolved git rather than its parent and grandparent only.
    """
    git = shutil.which("git", path=path)
    if not git:
        return None
    for directory in Path(git).resolve().parents:
        for candidate in (directory / "bash.exe", directory / "bin" / "bash.exe"):
            if candidate.is_file():
                return str(candidate)
    return None


def native_services_tool(root, output, execute):
    """Resolve the existing service client before compilation, retaining its bytes."""
    bash = git_for_windows_bash() if os.name == "nt" else shutil.which("bash")
    if not bash:
        raise RuntimeError("Git Bash is required for shared-service credential acceptance")
    probe = '''import hashlib,json,subprocess,sys
from pathlib import Path
binary = Path(sys.argv[1]).resolve(strict=True)
version = subprocess.check_output([str(binary), "--version"], stderr=subprocess.STDOUT,
                                  text=True, timeout=5).strip()
print(json.dumps({"executable": str(binary), "version": version,
                  "sha256": hashlib.sha256(binary.read_bytes()).hexdigest()}))
'''
    # Git for Windows can check shell helpers out as CRLF. Its Bash provides
    # igncr; keep the original helper's location and normal Unix invocation.
    arguments = [bash, *(["-o", "igncr"] if os.name == "nt" else []), "-ec",
                 'source e2e/nexus-a2a/lib.sh; GRPCURL="$(grpcurl_bin)"; '
                 '"$1" -c "$2" "$(native_path "$GRPCURL")"', "scode-services-tool",
                 sys.executable.replace("\\", "/"), probe]
    log = output / "000.log"
    result = execute(arguments, root, log)
    tool = json.loads(log.read_text(encoding="utf-8"))
    return {"root": str(root.resolve()), "command": result, "tool": tool, "log": str(log)}


def prepare_native_checks(names, root, output, execute):
    """Resolve selected native tools before long native compilation."""
    prepared = {}
    if any(name in compiler_cache_profiles() for name in names):
        run.prepared_compiler_cache = prepare_compiler_cache(output / "compiler-cache-tool")
    if "native-services" in names:
        print("NATIVE_SERVICE_TOOL_PREFLIGHT: resolving the existing grpcurl client and running "
              "--version before compilation. A download or executable failure stops here; "
              "both original service journeys remain required.", flush=True)
        run.prepared_services_tool = native_services_tool(root, output / "native-service-tool", execute)
    selected = [name for name in names if name in NATIVE_RUST_PTY_CHECKS]
    if os.name == "nt" and selected:
        print("NATIVE_PREPARATION: installing the selected checksum-pinned Windows terminal host "
              "before compilation; original installer checks and terminal tests remain required.", flush=True)
        first = selected[0]
        directory = output / first
        prepared[first] = [prepare_windows_terminal(root, directory, execute)]
        verify_native_terminal(directory / "000.log", native_target_directory(root))
    return prepared


def native_workspace_lint_arguments(root, base, scope):
    """Keep prepared and direct workspace lint on one argument recipe."""
    if scope["tests"] is not None:
        arguments = ["--locked", *native_cli_test_package_args(root, base),
                     "--features", "engine-acp/mailbox"]
        for test in scope["tests"]:
            arguments += ["--test", test]
        return ["clippy", *arguments, "--", "-D", "warnings"]
    targets = scope["targets"]
    lint_targets = [*targets, "--examples", "--benches"] if targets else ["--all-targets"]
    return ["clippy", "--workspace", *lint_targets, "--locked", "--features", "engine-acp/mailbox",
            "--", "-D", "warnings"]


def native_workspace_format(root, cargo, command):
    """Keep cargo fmt --all's roots, path dependencies and editions in bounded argv."""
    pending = [root / "rust/Cargo.toml"]
    visited = set()
    targets = {}
    while pending:
        manifest = pending.pop().resolve()
        if manifest in visited:
            continue
        metadata_log = cargo("metadata", "--no-deps", "--format-version=1",
                             "--manifest-path", str(manifest))
        packages = json.loads(metadata_log.read_text(encoding="utf-8"))["packages"]
        visited.update(Path(package["manifest_path"]).resolve() for package in packages)
        for package in packages:
            for target in package["targets"]:
                path = str(Path(target["src_path"]).resolve())
                edition = target["edition"]
                if path in targets and targets[path] != edition:
                    raise RuntimeError(f"Conflicting Rust editions for formatting target: {path}")
                targets[path] = edition
            for dependency in package["dependencies"]:
                if dependency.get("path"):
                    local = Path(dependency["path"]) / "Cargo.toml"
                    if local.is_file() and local.resolve() not in visited:
                        pending.append(local)
    if not targets:
        raise RuntimeError("No Cargo formatting targets found; formatting remains required")
    formatter = os.environ.get("RUSTFMT") or shutil.which("rustfmt")
    if not formatter:
        raise RuntimeError("Native acceptance requires the active toolchain's rustfmt")

    def command_units(arguments):
        # CreateProcessW counts UTF-16 units including quoting and the final NUL.
        return len(subprocess.list2cmdline(arguments).encode("utf-16-le")) // 2 + 1

    batches = []
    for edition in sorted(set(targets.values())):
        prefix = [formatter, "--edition", edition, "--check"]
        batch = prefix.copy()
        for path in sorted(path for path, selected in targets.items() if selected == edition):
            if command_units([*prefix, path]) > 24_000:
                raise RuntimeError(f"Formatting target exceeds the native command budget: {path}")
            if command_units([*batch, path]) > 24_000:
                batches.append(batch)
                batch = prefix.copy()
            batch.append(path)
        batches.append(batch)
    print("NATIVE_FORMAT_SCOPE=" + json.dumps({
        "cargo_target_roots": len(targets), "manifests": len(visited), "batches": len(batches),
        "max_command_utf16_units": max(map(command_units, batches)),
        "scope": "All Cargo targets and recursive local path dependencies; normal module/config discovery.",
    }), flush=True)
    for arguments in batches:
        command(*arguments, cwd=root / "rust")


def native_workspace_lint(root, output, base, cargo, command):
    """Run the original lint prefix; a pass alone never completes workspace acceptance."""
    windows_native_shells()
    native_workspace_format(root, cargo, command)
    tests = changed_cli_test_targets(root, base)
    scope = {"tests": tests, "targets": None}
    (output / "test-scope.json").write_text(json.dumps({
        "mode": "changed-cli-targets" if tests is not None else "full-workspace", "targets": tests,
    }, indent=2))
    if tests is not None:
        print("Linting selected CLI targets with warnings denied before compiling/running their tests.", flush=True)
    else:
        command(sys.executable, "scripts/generate_cache_metrics.py", "--check")
        command(sys.executable, "scripts/check_cache_usage_fixtures.py")
        metadata_log = cargo("metadata", "--no-deps", "--locked", "--format-version=1")
        targets, selection = grouped_workspace_test_targets(root, json.loads(metadata_log.read_text(encoding="utf-8")))
        scope["targets"] = targets
        selection["library_and_binary_unit_harnesses_selected"] = not bool(targets)
        (output / "test-scope.json").write_text(json.dumps(selection, indent=2))
        print("Native workspace target selection: " + json.dumps(selection), flush=True)
        print("Workspace lint with warnings denied uses the validated test groups plus library/bin "
              "targets, examples and benches before long builds and PTY execution. "
              "The integrated queue additionally checks all features; QUEUE_CLIPPY_SCOPE reports its target inventory.", flush=True)
        if targets:
            print("NATIVE_TEST_LAYER: every selected integration/PTY target remains required; legacy library/bin "
                  "unit harnesses are not executed. Queue all-feature lint and workspace doctests remain.", flush=True)
        else:
            print("NATIVE_TEST_LAYER: older source without committed groups retains Cargo's default full suite, "
                  "including legacy units and original disabled/feature-gated target exclusions.", flush=True)
    cargo(*native_workspace_lint_arguments(root, base, scope))
    return scope


def workspace_preparation_identity(root, output, base):
    """Bind in-process preparation to the same clean source, output and executor."""
    try:
        refs = run.execution_refs
    except AttributeError:
        refs = None
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root).decode().strip()
    dirty = subprocess.check_output(["git", "status", "--porcelain"], cwd=root)
    if dirty:
        raise RuntimeError("Workspace lint preparation requires the same clean source; unfinished tests remain required")
    return {"root": str(root.resolve()), "output": str(output.resolve()), "base": base,
            "head": head, "execution_refs": refs, "target": str(native_target_directory(root))}


def prepare_native_workspace_lint(root, output, base, execute):
    """Prepare only real lint commands before the contract matrix, under the host lease."""
    directory = output / "native-workspace"
    run.workspace_lint_request = workspace_preparation_identity(root, directory, base)
    print("NATIVE_LINT_PREPARATION: execute the selected workspace lint before guard-contract. "
          "The later workspace profile consumes these original logs once; native tests and the full guard remain required.", flush=True)
    try:
        return run("native-workspace", root, directory, base, execute)
    except RuntimeError as error:
        raise RuntimeError("Early native workspace lint failed before guard-contract started. "
                           f"Workspace acceptance remains incomplete; retain these logs and repair the reported error: {error}") from error
    finally:
        del run.workspace_lint_request


def consume_workspace_lint(preparation, root, output, base, existing):
    """Refuse stale/tampered preparation; it is not a completed signed profile."""
    if preparation["identity"] != workspace_preparation_identity(root, output, base):
        raise RuntimeError("Workspace lint preparation belongs to another source, output, target or executor; "
                           "run the supported check with fresh output and preserve the original evidence")
    commands, scope = preparation["commands"], preparation["scope"]
    if commands[:len(existing)] != existing or scope["tests"] != changed_cli_test_targets(root, base):
        raise RuntimeError("Workspace lint preparation differs from this provider or selected test scope")
    for index, entry in enumerate(commands):
        log = output / f"{index:03d}.log"
        receipt = log.with_suffix(".command.json")
        if (entry["exit_code"] != 0 or json.loads(receipt.read_text()) != entry
                or hashlib.sha256(log.read_bytes()).hexdigest() != entry["log_sha256"]):
            raise RuntimeError(f"Workspace lint preparation evidence changed: {log}; unfinished tests remain required")
    lint = commands[-1]["command"]
    if Path(lint[0]).stem.lower() != "cargo" or lint[1:] != native_workspace_lint_arguments(root, base, scope):
        raise RuntimeError("Prepared workspace lint did not use the current selected arguments with warnings denied")
    print("NATIVE_LINT_PREPARATION: verified and consumed the original lint commands; "
          "running all remaining workspace commands now.", flush=True)
    return scope, commands


def native_compiler_context(root, binary, output, command):
    """Use the same connected context journey for source and release artifacts."""
    if compiler_context_driver_present(root):
        command(sys.executable, "e2e/compiler-context/run.py", "--engine", str(binary),
                "--directory", str(output / "compiler-context"))
    else:
        print("This historical source predates the host-context compiler driver; "
              "its existing native checks remain required.", flush=True)


def native_release_journeys(root, binary, output, base, cargo, command, env):
    """Judge the actual optimized artifact without compiling another CLI for its driver."""
    import tomllib

    env["SCODE_TEST_BIN"] = str(binary)
    manifest = root / "rust/crates/scode-benchmark-drivers/Cargo.toml"
    targets = tomllib.loads(manifest.read_text(encoding="utf-8")).get("test", []) if manifest.is_file() else []
    declared = [target for target in targets if target.get("name") == "release_stream_capture"]
    if declared:
        if (len(declared) != 1 or declared[0].get("test") is not False
                or declared[0].get("path") != "../rusty-sudocode-cli/tests/pty_model_stream_close.rs"
                or declared[0].get("required-features")):
            raise RuntimeError("Release driver must use the original stream/capture workflow without feature exclusions")
        arguments = ["-p", "scode-benchmark-drivers", "--test", "release_stream_capture"]
        print("RELEASE_TEST_DRIVER: standalone original stream/capture workflow; "
              "SCODE_TEST_BIN launches the copied optimized artifact. No unused debug CLI build.", flush=True)
    else:
        arguments = [*native_cli_test_package_args(root, base), "--features", "engine-acp/mailbox",
                     "--test", "pty_model_stream_close"]
        print("RELEASE_TEST_DRIVER: legacy source retains its original native CLI test target.", flush=True)
    cargo("test", "--locked", *arguments, "raw_stream_capture_keeps_streaming_and_resume_responsive",
          "--", "--exact", "--nocapture")
    with stable_performance_power(output):
        command(sys.executable, "e2e/startup/run.py", "--bin", str(binary), "--budget-ms", "100",
                "--report", str(output / "startup.json"))
    native_compiler_context(root, binary, output, command)


def native_compiler_run(name, root, output, base, execute):
    """Keep one finite server around the original profile, including failures."""
    selected = execute

    def command(argv, cwd, log, env=None):
        return selected(argv, cwd, log, env)

    # Re-enter the original dispatcher with a scoped callback. Keeping this
    # branch inside the native cases preserves unrelated profile fingerprints.
    command.compiler_cache_scope = True
    try:
        request = run.workspace_lint_request
    except AttributeError:
        request = None
    try:
        prepared = run.prepared_compiler_cache
    except AttributeError:
        prepared = None
    if request is not None or prepared is None:
        return run(name, root, output, base, command)
    module, tool = prepare_compiler_cache(output / "compiler-cache")
    if tool is None:
        return run(name, root, output, base, command)
    print("COMPILER_CACHE: pinned local objects, 2 GiB maximum; unchanged Cargo output layout and original "
          "source/features/environment keys. Explicit user wrappers/configuration remain authoritative; "
          "busy storage adds no wait. This is not a source-freshness attestation.", flush=True)
    with module.session(tool, output / "compiler-cache", execute) as active:
        selected = active.execute
        return run(name, root, output, base, command)


def run(name, root, output, base, execute):
    if name in {"native-workspace", "native-resize", "native-release", "native-services",
                "native-context", "native-cohost", "native-examples"}:
        try:
            cached = execute.compiler_cache_scope
        except AttributeError:
            cached = False
        if not cached:
            return native_compiler_run(name, root, output, base, execute)
    output.mkdir(parents=True, exist_ok=True)
    results = []
    rust = root / "rust"
    target = native_target_directory(root)
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

    if name in {"native-workspace", "native-resize", "native-services"}:
        print("NATIVE_TOOL_PREFLIGHT: verify the checksum-pinned nextest archive and executable "
              "before compilation; all original tests remain required.", flush=True)
        nextest_binary(output)

    if os.name == "nt" and name in NATIVE_RUST_PTY_CHECKS:
        try:
            prepared = run.prepared_native.get(name)
        except AttributeError:
            prepared = None
        if prepared:
            # These are this invocation's actual early preparation commands.
            if len(prepared) != 1 or prepared[0]["command"] != windows_terminal_command(root):
                raise RuntimeError("Native terminal preparation differs from this profile's target/provider")
            verify_native_terminal(output / "000.log", target)
            results.extend(prepared)

    if name == "preflight":
        command(sys.executable, ".github/scripts/check_doc_source_of_truth.py")
        entry = stage_preflight_executor(root, execution_policy_ref(), output / "history-policy")
        print("History preflight uses the protected policy checker; the author branch stays unchanged.", flush=True)
        command(sys.executable, str(entry), base, "HEAD")
    elif name == "guard-contract":
        print("Local contract validation: full private Docker matrix, with Git/signature checks before Cargo; "
              "independent author-host native recovery, nextest, release, grouping and example guards overlap "
              "after the original Docker/tool preflight. Both full results are required. "
              "No product build or benchmark lease.", flush=True)
        if not shutil.which("docker"):
            raise RuntimeError("Contract validation requires local Docker. Install/start Docker Desktop, "
                               "then retry this push or the printed acceptance command.")
        try:
            print("DOCKER_ENGINE_PROBE: require a server version within 30 seconds before starting "
                  "container/native validation. Optional CLI plugin discovery is outside this probe.",
                  flush=True)
            command(*docker_engine_probe_command(30))
        except (RuntimeError, OSError) as error:
            raise RuntimeError("Contract validation cannot reach local Docker. Start Docker Desktop or correct "
                               "this shell's DOCKER_HOST/context, then retry; the daemon error is in the "
                               f"guard log above. Native smoke and matrix have not started: {error}") from error
        policy_ref = execution_policy_ref()
        entry = stage_profile_executor(root, policy_ref, name, output / "native-guard-policy")
        parallel_contract_guards(root, output, entry, command, results)
    elif name == "native-sdk":
        sdk = root / "sdk/typescript"
        command("node", "--version", cwd=sdk)
        command("npm", "ci", cwd=sdk)
        command("npm", "test", cwd=sdk)
        command("npm", "run", "pack:runtime", cwd=sdk)
    elif name == "native-workspace":
        try:
            request = run.workspace_lint_request
        except AttributeError:
            request = None
        try:
            preparation = run.prepared_workspace
        except AttributeError:
            preparation = None
        if preparation is not None and request is None:
            scope, original = consume_workspace_lint(preparation, root, output, base, results)
            results[:] = original
            del run.prepared_workspace
        else:
            scope = native_workspace_lint(root, output, base, cargo, command)
        if request is not None:
            if request != workspace_preparation_identity(root, output, base):
                raise RuntimeError("Workspace lint source changed during preparation")
            return {"identity": request, "commands": results, "scope": scope}
        tests = scope["tests"]
        if tests is not None:
            arguments = ["--locked", *native_cli_test_package_args(root, base), "--features", "engine-acp/mailbox"]
            for test in tests:
                arguments += ["--test", test]
            native_tests(*arguments)
        else:
            targets = scope["targets"]
            command(sys.executable, "e2e/build-metadata/run.py",
                    extra={"BUILD_METADATA_EVIDENCE": str(output / "build-metadata")})
            cargo("test", "--locked", "--manifest-path", "vendor/codex-mermaid/Cargo.toml", "--target-dir", str(target))
            cargo("check", "--workspace", "--benches", "--locked")
            command(str(nextest_binary(output)), "nextest", "run",
                    *workspace_nextest_options(output, bool(targets),
                                              hosted_source_ready=supports_hosted_eight_workers(root)),
                    "--workspace", "--locked", "--features", "engine-acp/mailbox",
                    *workspace_integration_test_targets(targets), cwd=rust)
            # Stable nextest excludes doctests; retain Cargo's separate path.
            cargo("test", "--workspace", "--locked", "--features", "engine-acp/mailbox", "--doc")
    elif name == "native-examples":
        print("Compile-only examples: building the normal example binaries with the workspace mailbox feature graph. "
              "This profile never runs example entry points.", flush=True)
        cargo("build", "--workspace", "--locked", "--features", "engine-acp/mailbox", "--examples")
    elif name == "native-resize":
        installer_scope = {**terminal_installer_scope(root, base), "windows_host": os.name == "nt"}
        (output / "installer-scope.json").write_text(json.dumps(installer_scope, indent=2), encoding="utf-8")
        print("WINDOWS_TERMINAL_INSTALLER_SCOPE=" + json.dumps(installer_scope), flush=True)
        if os.name == "nt":
            terminal = Path.home() / ".cache/sudocode-terminal-host"
            # The author/CI dispatcher prepares every selected Rust PTY
            # profile. Direct resize guard fixtures retain their existing
            # controlled installer boundary and command inventory.
            if not prepared:
                results.append(prepare_windows_terminal(root, output, execute))
            if installer_scope["fault_injection_required"]:
                command(sys.executable, "e2e/terminal-resize/check_windows_setup.py", str(terminal / "host.zip"))
            else:
                print("Installer code/executor inputs unchanged: skipping artificial HTTP/cache corruption checks; "
                      "actual checksum-pinned setup and every real terminal test remain required.", flush=True)
            env.update({"SCODE_TERMINAL_HOST": str(terminal / "Code.exe"),
                        "SCODE_TERMINAL_MODULES": str(terminal / "07f806f999/resources/app/node_modules.asar")})
        else:
            command("npm", "ci", "--prefix", "e2e/terminal-resize")
        env["SCODE_TERMINAL_LOG_DIR"] = str(output / "terminal")
        tests = native_resize_test_targets(windows=os.name == "nt")
        print("Native resize uses the workspace dev/mailbox graph. Cargo verifies current source; "
              "all selected ignored cases and original budgets remain. The standalone default-feature "
              "debug graph is outside this profile's coverage.", flush=True)
        package_args = native_cli_test_package_args(root, base)
        for test in tests:
            cargo("test", "--locked", *package_args, "--features", "engine-acp/mailbox", "--test", test,
                  "--", "--ignored", "--nocapture")
    elif name == "native-release":
        cargo("build", "--release", "--locked", "-p", "rusty-sudocode-cli", "--bin", "scode")
        binary = output / ("scode" + suffix)
        shutil.copy2(target / "release" / ("scode" + suffix), binary)
        native_release_journeys(root, binary, output, base, cargo, command, env)
    elif name == "native-context":
        print("NATIVE_CONTEXT_SCOPE: connected compiler-context acceptance uses the current workspace "
              "dev/mailbox binary. Optimized stream/resume,100ms startup and compiler context are "
              "mandatory on every newly built release artifact before packaging/publication. "
              "Ordinary source acceptance can miss optimization-specific regressions until release.", flush=True)
        cargo("test", "--no-run", "--locked", *native_cli_test_package_args(root, base),
              "--features", "engine-acp/mailbox", "--test", "pty_model_stream_close")
        native_compiler_context(root, target / "debug" / ("scode" + suffix), output, command)
    elif name == "native-services":
        try:
            preparation = run.prepared_services_tool
        except AttributeError:
            preparation = None
        if preparation is None:
            preparation = native_services_tool(root, output / "service-tool", execute)
        if preparation["root"] != str(root.resolve()):
            raise RuntimeError("Prepared service client belongs to a different source directory")
        tool = preparation["tool"]
        grpcurl = Path(tool["executable"])
        if hashlib.sha256(grpcurl.read_bytes()).hexdigest() != tool["sha256"]:
            raise RuntimeError("Prepared grpcurl bytes changed; repeat native tool preparation")
        results.append(preparation["command"])
        # The receipt lists the probe with this profile's commands, and resume
        # verifies a profile's logs inside its own directory. Keep the probe's
        # original bytes there, at its result index.
        (output / f"{len(results) - 1:03d}.log").write_bytes(Path(preparation["log"]).read_bytes())
        # Cargo's test graph unifies dev-dependency features. A normal build
        # after workspace tests recompiles the CLI and its shared dependencies.
        # Keep Cargo's freshness check, without executing the driver twice.
        print("Native service acceptance: reusing the workspace dev/mailbox feature graph; "
              "Cargo still builds missing/stale inputs. Both original service journeys remain required. "
              "The standalone default-feature debug graph is outside this profile's coverage.", flush=True)
        cargo("test", "--no-run", "--locked", *native_cli_test_package_args(root, base),
              "--features", "engine-acp/mailbox", "--test", "pty_model_stream_close")
        binary = target / "debug" / ("scode" + suffix)
        command(sys.executable, "e2e/shared-services/run.py", "--scode", str(binary),
                "--artifacts", str(output / "services"))
        command(sys.executable, "e2e/shared-services/existing_box.py", "--scode", str(binary),
                "--grpcurl", str(grpcurl), "--artifacts", str(output / "existing-box"))
    elif name == "native-cohost":
        # raft-proto rejects protoc >=4. Pin the same tool on macOS/Windows as
        # the release workflow, and leave the developer's global PATH intact.
        executable = cohost_protoc()
        if executable is not None:
            env["PROTOC"] = str(executable)
        print("Native cohost acceptance uses the workspace dev graph with daemon, model and egress features. "
              "Optimized builds and platform size budgets run on nexusd-cohost release tags.", flush=True)
        version = "local-" + subprocess.check_output(["git", "rev-parse", "--short=12", "HEAD"], cwd=root).decode().strip()
        env["NEXUSD_BUILD_VERSION"] = version
        cargo("build", "--locked", "--profile", "dev", "-p", "nexusd-cohost", "--features",
              "daemon,driver-ai,egress-gate-presidio", "--bin", "nexusd-cohost")
        binary = target / "debug" / ("nexusd-cohost" + suffix)
        path = command(str(binary), "--version")
        if version not in path.read_text(errors="replace"):
            raise RuntimeError("Co-host build version stamp was lost")
    elif name in {"rendering", "browser"}:
        if os.environ.get("SCODE_ACCEPTANCE_CONTAINER") == "1":
            benchmark(name, root, output, base, command, target)
        else:
            try:
                policy_ref, candidate_ref = run.execution_refs
            except AttributeError:
                policy_ref = candidate_ref = None
            if name == "rendering":
                with stable_performance_power(output):
                    container(name, root, output, base, command, execute,
                              policy_ref=policy_ref, candidate_ref=candidate_ref)
            else:
                # The browser smoke checks behaviour, not timing.
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


def preflight_guard_files():
    """Keep the history checker independent of unrelated native fixture pins."""
    return {".github/scripts/check_pr_history.py": "1c0117d2045f6ac6c44ea327b08f9c08b30bad0b734381592458d39259f0dab7"}


def native_guard_files():
    """Bind external tool bytes through the existing executable fingerprint format."""
    return {
        **preflight_guard_files(),
        "e2e/local-acceptance/compiler-objects.Cargo.toml": "03837bb466ab1efb3445df6c8f320676e5e88230d3b00bbdc62b3e150269da02",
        "e2e/local-acceptance/compiler-objects.Cargo.lock": "c88ac7c8fa05c6a2779bc8a9d3399b903807914ef87bc3172ec3d1e9a39c0755",
        "e2e/local-acceptance/compiler_objects_acceptance.py": "af0b59fdf883922f2aa81906d4a869d19339d71e4a988071ff5a27dcf83e05be",
        ".github/local-acceptance/compiler_cache.py": "35a0c3335d0e5a4e07570cb5fd75edfe60f5d61cd5514738efb35cf744f630e7",
        "e2e/nexus-a2a/lib.sh": "93afc06508571b4a4b70615ec3f639e59b486d5c94f696b11f728d90f0d34662",
        "e2e/local-acceptance/github_asset_credentials.py": "c448a1ddaa1b38890092251ab8a2e6a47b654fae89a12f0fefca174b41f58a4d",
        ".github/local-acceptance/prepare_tools.py": "de97e4ded288d9b5f6eb5df668663f9628dcf58f5ab15046ac1146ac2b6ae2fb",
        ".github/actions/release-artifact/action.yml": "1f39a2fdd9bd079d6dd458ddac43b9168e6df70e59976c64217d044d2f4c7a10",
        ".github/actions/release-artifact/msvc.ps1": "d2f1f373582b2dfaced39e5661dce1beb15e0a7086f552cb2ed4762196a4d447",
        ".github/scripts/release_artifact_acceptance.py": "aab9028c35ebed14bcddc18cadb7385e328395617ff478ed4ccd53f810ac01e0",
        ".github/workflows/release.yml": "46a6183ba3a1001b6e755df1bd0cd64879131587cf458ac588f0da2414fb006c",
        "e2e/local-acceptance/release_artifact_acceptance.py": "2566e98db948594841380607640cceed6d350639e624e012af838848fe1f2cf3",
        "rust/crates/rusty-sudocode-cli/build.rs": "6220e21b120f2b38e91f707b902f5e46a455aceb2263e092bbeb16bb4eeb6baa",
        ".github/scripts/ci_input_scope.py": "592e0df76cf8cb43f4d8b723f3cfeac42103997e04a108f277c3864a26caa5ac",
        ".github/workflows/rust-ci.yml": "be3949990d6174a23cecf47836fc040febdb54d34a367ebd65d9e4f86bba9eb8",
        "e2e/terminal-resize/setup_windows.py": "f10420a57c46d3c673c8120316a5a5dc4cc1a234cbbac94c96831fb85ee4bf4e",
        ".github/actions/vfs-search-cache/action.yml": "b1ea2bc54f370d257aac898ce38a139d9e4f84b68c12455f0398e6f665184e39",
        ".github/local-acceptance/cache_layout.py": "a05dddec002f81abe0b3e928bf9ff6bf435bb693fb977ec9982e19acb3cca5a2",
        ".github/local-acceptance/cross_platform.py": "0293d33054ba9a251e151a626a4c5dff18b7380201ad1da01ba8dfd1056deb4e",
        ".github/local-acceptance/gate.py": "d7630a0956482fd93ab45c8b27e8371241f98c5ec64b15f339d39080a87fe412",
        ".github/local-acceptance/native_guard.py": "18b1af6f7b49753d41f9b9551168d1d55d530d5843cb426e33f0d8ebd4d8a726",
        "e2e/local-acceptance/parallel_guards_acceptance.py": "82d58151cae51702e0602852233c730acf6dfa87adcddd8f8fb2da47085afbb6",
        ".github/local-acceptance/parallel_guard.py": "c13f6bcd37333d6f09fe7076530764b5aa668e2ef95bc26fb26fd5adab32c1b2",
        ".github/local-acceptance/nextest_guard.py": "d75834fbb3632c9d75ef6fba5355c63dded13e5c259c234b8ba5ada725fbbb84",
        ".github/local-acceptance/publish.py": "c35807caae9dd5ec1aea1f7af15d6201a9e1df99e50b5d9a771a774e6e3e925d",
        ".github/scripts/local_acceptance.py": "0c83a0040c05ed4836b8af84b799c3fd9c712f9ab98bce67f7b9aef8fda571d5",
        ".github/scripts/vfs_search_cache.py": "7bc2ca0b1dcae653e534f810842824d176f4ad9bc1c5eaec0f60b4b5dbda94b6",
        ".github/workflows/ci-dependency-cache.yml": "9e7626539640a935a38100ce187a6fcb073780e57e37821550e5d713e0312cb4",
        ".github/workflows/local-acceptance-publish.yml": "f787bb8ed1c1e2da68d25d3e2e2c3a9c7d83639c474b4f5388f02c31c1983194",
        ".github/workflows/local-acceptance.yml": "c793582d8df09ee461d250454bd111f4ce39d427ba9cfd691dc682ae95b5f4a5",
        "e2e/local-acceptance/carryover_acceptance.py": "2c2dac64ddfb93633525c7e46817a735acff47fbd0009612b9a49af57a1484c7",
        "e2e/local-acceptance/cohost_acceptance.py": "39cd3a272fdcc7aab3d1434911238924edd208fd247db80ae7cb90e6abb80dc0",
        "e2e/local-acceptance/contract_reads_acceptance.py": "c40da37df440cbad3db58907c488673578b4fe9caab31b4919d2eff30eaf8a37",
        "e2e/local-acceptance/dependency_scope_acceptance.py": "0d7ddf8c693c5530d8949b5e90a58b9fada7d4781447dcff0aca864718b751bd",
        "e2e/local-acceptance/docker_health_acceptance.py": "2d03be855f74aeeabe04db9fd2aceccaf2b68936eb6e7d5c2e212e100397e99a",
        "e2e/local-acceptance/examples_acceptance.py": "0c681b8cde0f60c773cfb0eee82f6b0262dd3f7f79d437591cca0daaaae7f581",
        "e2e/local-acceptance/grouped_targets_acceptance.py": "e6cce07c074efcc1a62293fbfc13aa0581f41114e751a7103f7186046c0de320",
        "e2e/local-acceptance/host_lease_acceptance.py": "6b5f2f0cc9c8653f33ea4d2f43b403ac8e77aaffdb84bccc42782713c6940656",
        "e2e/local-acceptance/inherited_lease_acceptance.py": "cd984f8945d635f0ca7d70f6e0435f84bfb919ab466dc5fc24ca70d98e2d8857",
        "e2e/local-acceptance/native.py": "cbc0c624b225a7ccd35139f677b337081f8c770747945b2133a32d036fcad552",
        "e2e/local-acceptance/native_cache_acceptance.py": "171729bd1351c3d03f89678f29d87eadc3e45305da9e7c5c655ca880977f6d24",
        "e2e/local-acceptance/native_lint_acceptance.py": "4df1f1dd8855d260bba9305cffe34bb9e48652fef03d673bcfd2388d4c4c10e0",
        "e2e/local-acceptance/parallel_acceptance.py": "5cb522ab539da4437b075555d711c26a572017dbad5ea8f255e4014dbb5a07a5",
        "e2e/local-acceptance/policy_guard_acceptance.py": "d0869fbeac4abdd0742d78052feafa5a80e8967ac752d9d985b4e8c97d02900a",
        "e2e/local-acceptance/power_acceptance.py": "47344bcd5a9d4910d091355629f19f3f8263ec40aa806740fba26354b7d1b945",
        "e2e/local-acceptance/profile_compat_acceptance.py": "c85423b23353ac6359e42fe648b2148c6dc68299462b1b92576cf2b7575c36be",
        "e2e/local-acceptance/publication_acceptance.py": "e45db30c079925d8e193d4aea2fe28f10773e6c54922ede15f405b1c9fb97aae",
        "e2e/local-acceptance/release_acceptance.py": "761d53538601b59832c17ad1b71d9691ddb4c83baf1ca8c09128df0bea78d309",
        "e2e/local-acceptance/resource_scheduling_acceptance.py": "cc1ff8e34a12ebcac284330497a26c4ea5af336cd208e1ab2a8531e83180eeb0",
        "e2e/local-acceptance/resume_acceptance.py": "e5f80815740f65075ffd74894b5f5eadfd51e804576c38bbef64730004ffedd5",
        "e2e/local-acceptance/run.py": "453b4bca68e242e4635490e97e45dbb0124195b78c43c2e0a71ac5f4fa24cd66",
        "e2e/local-acceptance/vfs_search_cache_acceptance.py": "fcfa9fc3aaebfa3d5cbb23dedab803f26671f13e58d3d51e1db994640bff6786",
        "e2e/startup/reference.rs": "327e5a6fadd9dc707d8bbbd6268ecaa8f27df4945029a6cb51c41b9fc8162578",
        "e2e/startup/run.py": "4fb4ac6b52331b8278459c18f54355bf8f8209d1ea0e87bca01a54c0ba3f77e4",
    }


def stage_executor_files(root, policy_ref, files, policy_root):
    """Copy the selected protected bytes with one shared path and SHA256 check."""
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


def stage_preflight_executor(root, policy_ref, policy_root):
    """Stage only the protected history checker for metadata preflight."""
    files = preflight_guard_files()
    stage_executor_files(root, policy_ref, files, policy_root)
    return policy_root / next(iter(files))


def stage_profile_executor(root, policy_ref, name, policy_root):
    """Materialize one hash-bound native tool set without changing the candidate."""
    if name == "preflight":
        return stage_preflight_executor(root, policy_ref, policy_root)
    if name != "guard-contract":
        raise RuntimeError(f"No policy-owned native executor for {name}")
    stage_executor_files(root, policy_ref, native_guard_files(), policy_root)
    # The grouping fixture needs actual protected selection rules. Bind only
    # those fields: approval maps contain these recipe hashes and cannot be
    # included in their own input digest. Signer data is not used by the planner.
    policy = json.loads(subprocess.check_output(
        ["git", "show", f"{policy_ref}:spec/local_acceptance.json"], cwd=root))
    selection = {key: policy[key] for key in ["version", "repository", "checks"]}
    data = json.dumps(selection, sort_keys=True, separators=(",", ":")).encode()
    actual_scope = hashlib.sha256(data).hexdigest()
    if actual_scope != "4e7fb701f5ccb3726b4322de85711a2528a8816770065db8e460457d14d6c360":
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
    expected = {'native-release': 'd19f30dd91757fa49d76cc8c49ca81c04961ca223394744c573d5dbf6d98d9d9', 'native-context': '921dee385ac5f2b1605e5120cfedcc05bbb28cbfc115d655d3e6849dce8217cc'}
    observed = helper.reachable_profile_commands_hashes(policy_ref, list(expected))
    for name, digest in expected.items():
        if observed[name] != digest:
            label = "Native release" if name == "native-release" else "Native compiler-context"
            raise RuntimeError(f"{label} guard recipe changed; update its protected execution pin")
    (policy_root / ".github/local-acceptance/checks.py").write_bytes(subprocess.check_output(
        ["git", "show", f"{policy_ref}:.github/local-acceptance/checks.py"], cwd=root))
    return policy_root / ".github/local-acceptance/native_guard.py"


def container_image_tag(repository, digest):
    """Keep each build's image reference owned until its caller finishes using it."""
    import uuid

    return f"{repository}:{digest[:24]}-{uuid.uuid4().hex}"


def guard_tool_source(root, policy_ref):
    """Provision the frozen tool function without duplicating its URL/checksums."""
    import ast

    source = subprocess.check_output(
        ["git", "show", f"{policy_ref}:.github/local-acceptance/checks.py", "--"], cwd=root).decode("utf-8")
    definitions = [node for node in ast.parse(source).body if isinstance(node, ast.FunctionDef)]
    required = {"github_asset_download", "nextest_binary"}
    downloaders = [node for node in definitions if node.name == "github_asset_download"]
    if any(isinstance(node, ast.Call) and isinstance(node.func, ast.Name)
           and node.func.id == "github_api_request" for definition in downloaders for node in ast.walk(definition)):
        required.add("github_api_request")
    functions = [node for node in definitions if node.name in required]
    if sorted(node.name for node in functions) != sorted(required):
        raise RuntimeError("Protected GitHub request/downloader and nextest_binary must each have one definition before building the guard image")
    prefix = ("import hashlib, io, json, os, platform, socket, subprocess, sys, tarfile, tempfile, urllib.request, zipfile\n"
              "from pathlib import Path\n\n")
    main = '''

if __name__ == "__main__":
    socket.setdefaulttimeout(30)
    output = Path("/opt/nextest-provision")
    output.mkdir(parents=True, exist_ok=True)
    executable = nextest_binary(output)
    receipt = json.loads((output / "nextest-source.json").read_text())
    version = subprocess.check_output([str(executable), "--version"], text=True).strip()
    assert " " + receipt["version"] in version, version
    refused = []
    if "--verify-cache" in sys.argv:
        archive = executable.parent / receipt["url"].rsplit("/", 1)[1]
        for label, path, message in [
            ("archive", archive, "Cached nextest archive checksum mismatch"),
            ("executable", executable, "Cached nextest executable differs from the verified archive"),
        ]:
            original = path.read_bytes()
            path.write_bytes(original + b"controlled guard cache corruption")
            try:
                try:
                    nextest_binary(output)
                except RuntimeError as error:
                    assert message in str(error), str(error)
                    refused.append(label)
                else:
                    raise AssertionError("Modified guard tool cache was accepted: " + label)
            finally:
                path.write_bytes(original)
        assert nextest_binary(output) == executable
        assert json.loads((output / "nextest-source.json").read_text()) == receipt
        assert subprocess.check_output([str(executable), "--version"], text=True).strip() == version
    print("GUARD_NEXTEST_PROVISION=" + json.dumps({
        **receipt, "actual_cli": version, "corrupt_cache_refused": refused,
    }), flush=True)
'''
    return (prefix + '\n\n'.join(ast.get_source_segment(source, node) for node in functions) + main).encode("utf-8")


def guard_image_context(root, policy_ref, context):
    """Build the canonical image context used by production and Docker controls."""
    context.mkdir()
    inputs_hash = hashlib.sha256()
    # Keep image bindings inside the existing executable fingerprint format.
    # A changed image/entrypoint must update its pin before any Docker work.
    pins = {
        "Dockerfile.guard": "b21df0c17494cfbec786310c68e9012f2be203cd2448ef31c7a8789786be7974",
        "guard.py": "aa5b4f97745372efeef46c6502980fd31427617ef0b7f7418b907f213839a308",
    }
    for name, destination in [("Dockerfile.guard", "Dockerfile"), ("guard.py", "guard.py")]:
        contents = subprocess.check_output(["git", "show", f"{policy_ref}:.github/local-acceptance/{name}", "--"], cwd=root)
        actual = hashlib.sha256(contents).hexdigest()
        if actual != pins[name]:
            raise RuntimeError(f"Guard input {name} changed: SHA256 {actual}. Update its protected pin in "
                               ".github/local-acceptance/checks.py guard_image_context, then rerun the installed "
                               "contract-update command; no image build started.")
        (context / destination).write_bytes(contents)
        inputs_hash.update(name.encode() + b"\0" + contents + b"\0")
    dependencies = context / "compiler-objects"
    dependencies.mkdir()
    for filename in ("Cargo.toml", "Cargo.lock"):
        relative = "e2e/local-acceptance/compiler-objects." + filename
        contents = subprocess.check_output(["git", "show", f"{policy_ref}:{relative}"], cwd=root)
        if hashlib.sha256(contents).hexdigest() != native_guard_files()[relative]:
            raise RuntimeError(f"Guard compiler dependency input {relative} differs from its protected pin; "
                               "update the source pin and rerun the supported contract-update before building the image")
        (dependencies / filename).write_bytes(contents)
        inputs_hash.update(relative.encode() + b"\0" + contents + b"\0")
    seed = guard_tool_source(root, policy_ref)
    (context / "nextest_seed.py").write_bytes(seed)
    inputs_hash.update(b"nextest_seed.py\0" + seed + b"\0")
    return inputs_hash.hexdigest()


def verify_guard_image(image, command):
    """Recheck the image's verified tool offline before the decision matrix."""
    command("docker", "run", "--rm", "--init", "--network", "none", "--entrypoint", "python3",
            image, "/opt/nextest_seed.py")
    print("GUARD_COMPILER_DEPENDENCIES: verify the protected manifest/lock dependencies offline "
          "before either guard matrix; original compiler and PTY controls remain required.", flush=True)
    command("docker", "run", "--rm", "--init", "--network", "none", "--entrypoint", "cargo",
            image, "fetch", "--offline", "--locked", "--manifest-path", "/opt/compiler-objects/Cargo.toml")


def prepare_contract_container(root, output, command):
    """Finish the original image, tool and source checks before either guard starts."""
    try:
        policy_ref, candidate_ref = run.execution_refs
    except AttributeError:
        policy_ref = candidate_ref = None
    def revision(ref):
        return subprocess.check_output(["git", "rev-parse", "--verify", f"{ref}^{{commit}}"], cwd=root).decode().strip()
    protected = revision(policy_ref or "origin/main")
    candidate = revision(candidate_ref or "HEAD")
    context = output / "guard-image"
    context_sha256 = guard_image_context(root, protected, context)
    image_receipt = output / "guard-image-id.txt"
    image_tag = container_image_tag("sudocode-contract-guard", context_sha256)
    print("GUARD_TOOL_PREFLIGHT: prepare the checksum-pinned nextest image layer before test execution; "
          "the image cache retains verified tools for later runs.", flush=True)
    try:
        command("docker", "build", "--iidfile", str(image_receipt), "--tag",
                image_tag, str(context), extra={"DOCKER_BUILDKIT": "1"})
    except RuntimeError as error:
        raise RuntimeError("Guard image/tool preparation failed before the decision matrix and native guards. "
                           "Inspect the saved Docker build log, correct that error and retry the printed "
                           f"acceptance command: {error}") from error
    image = image_receipt.read_text().strip()
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", image):
        raise RuntimeError("Contract image build did not return an immutable image ID")
    print("GUARD_TOOL_PREFLIGHT: verified pinned nextest is provisioned during image build; "
          "image preparation retains corruption controls; check offline execution before the full matrix.", flush=True)
    verify_guard_image(image, command)
    inputs = output / "guard-inputs"
    snapshot(root, candidate, inputs)
    (output / "guard-provenance.json").write_text(json.dumps({
        "candidate": candidate, "policy": protected, "image_id": image, "image_tag": image_tag,
        "context_sha256": context_sha256}, indent=2))
    # This suite uses private temporary Git/Cargo fixtures, not benchmark state.
    return ("docker", "run", "--rm", "--init", "--cpus", "2", "--memory", "2g", "--pids-limit", "256",
            "--mount", f"type=bind,source={inputs},target=/inputs,readonly", image)


def contract_container(root, output, command):
    """Retain the original complete serial Docker entry for existing callers."""
    command(*prepare_contract_container(root, output, command))


def parallel_contract_guards(root, output, entry, command, results):
    """Keep original command evidence while supervising both independent guards."""
    import uuid

    docker_args = prepare_contract_container(root, output, command)
    native_args = [sys.executable, str(entry), "--repository", str(root),
                   "--output", str(output / "native-guard"),
                   "--executable", str(nextest_binary(output)),
                   "--source-receipt", str(output / "nextest-source.json"),
                   "--options", json.dumps(nextest_options(output))]
    runner_path = entry.parent / "parallel_guard.py"
    spec = importlib.util.spec_from_file_location("protected_parallel_guard", runner_path)
    runner = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(runner)
    owner = uuid.uuid4().hex
    cidfile = output / "guard-container-id.txt"
    configuration = output / "parallel-guards.json"
    image = json.loads((output / "guard-provenance.json").read_text())["image_id"]
    docker = shutil.which("docker") or "docker"
    assert docker_args[:2] == ("docker", "run")
    command(sys.executable, str(entry.parents[2] / "e2e/local-acceptance/parallel_guards_acceptance.py"),
            "--output", str(output / "parallel-guard-controls"), "--image", image)
    index = len(results) + 1  # The coordinator command owns the preceding log.
    data = {"version": 1, "root": str(root), "container": {
        "docker": docker, "cidfile": str(cidfile), "image": image, "owner": owner,
    }, "branches": [
        {"name": "docker", "command": [docker, "run", "--cidfile", str(cidfile),
            "--label", f"{runner.LABEL}={owner}", *docker_args[2:]],
         "log": str(output / f"{index:03d}.log")},
        {"name": "native", "command": native_args,
         "log": str(output / f"{index + 1:03d}.log")},
    ]}
    try:
        configuration.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
        command(sys.executable, str(runner_path), "--configuration", str(configuration))
        for branch in data["branches"]:
            log = Path(branch["log"])
            result = json.loads(log.with_suffix(".command.json").read_text())
            expected = [shutil.which(branch["command"][0]) or branch["command"][0], *branch["command"][1:]]
            if (result["command"] != expected or result["exit_code"] != 0
                    or hashlib.sha256(log.read_bytes()).hexdigest() != result["log_sha256"]):
                raise RuntimeError(f"Original parallel guard command evidence differs: {log}")
            results.append(result)
    finally:
        # Also runs after the outer executor interrupts/kills the coordinator.
        # A cleanup error still refuses completed acceptance.
        cleanup = runner.cleanup_container(data)
        (output / "parallel-guards-cleanup.json").write_text(json.dumps(cleanup, indent=2) + "\n")


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
    snapshots = {"candidate": candidate}
    if name == "rendering":
        # The paired matrix compares the candidate with the policy's fixed
        # baseline, using the trusted driver and harness.
        policy = json.loads(subprocess.check_output(
            ["git", "show", f"{protected}:e2e/terminal-resize/performance-policy.json"], cwd=root))
        snapshots.update(baseline=revision(policy["baseline_commit"]), trusted=protected)
    sources = output / "inputs"
    sources.mkdir()
    for label, ref in snapshots.items():
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
    image_receipt = output / "image-id.txt"
    # Only a successful rendering call in this process can authorize reuse.
    try:
        invocations = container.invocations
    except AttributeError:
        invocations = {}
    key = (str(output.parent.resolve()), candidate, protected, context_hash.hexdigest())
    previous = invocations.get(key) if name == "browser" else None
    image = (previous["image_tag"] if previous is not None
             else container_image_tag("sudocode-local-acceptance", context_hash.hexdigest()))
    if previous is None:
        command("docker", "build", "--iidfile", str(image_receipt), "--tag", image, str(context),
                extra={"DOCKER_BUILDKIT": "1"})
    else:
        image_receipt.write_text(previous["image_id"] + "\n", encoding="utf-8")
        print("Browser smoke reuses this invocation's measured rendering image and release: "
              f"{previous['image_id']}", flush=True)
    image_id = image_receipt.read_text().strip()
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", image_id):
        raise RuntimeError(f"Docker build did not produce an immutable image ID: {image_receipt}")
    identity = {
        **snapshots, "policy": protected,
        "image_id": image_id, "image_tag": image, "context_sha256": context_hash.hexdigest(),
        "rendering_release_reused": previous is not None,
    }
    (output / "container-provenance.json").write_text(json.dumps(identity, indent=2))
    (sources / "identity.json").write_text(json.dumps(identity, indent=2))
    if previous is not None:
        binary = previous["binary"]
        if checkout_link(binary) or hashlib.sha256(binary.read_bytes()).hexdigest() != previous["sha256"]:
            raise RuntimeError("Measured rendering release changed before browser reuse")
        reused = sources / "candidate-release"
        reused.mkdir()
        shutil.copy2(binary, reused / "scode")
        (reused / "identity.json").write_text(json.dumps({
            **identity, "version": 1, "sha256": previous["sha256"],
        }, indent=2))
        verified_candidate_release(sources)
    runner = sources / "runner/.github"
    (runner / "local-acceptance").mkdir(parents=True)
    (runner / "scripts").mkdir()
    shutil.copyfile(__file__, runner / "local-acceptance/checks.py")
    shutil.copyfile(execute.__code__.co_filename, runner / "scripts/local_acceptance.py")
    if name == "browser":
        # The executor owns this harness, including for candidates that predate
        # it. Stage only its Python imports/policy, not another source checkout.
        for relative in ["e2e/terminal-resize/gui-smoke.py", "e2e/terminal-resize/benchmark.py",
                         "e2e/terminal-resize/gui-policy.json"]:
            path = sources / "runner" / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(subprocess.check_output(["git", "show", f"{protected}:{relative}"], cwd=root))
    scratch = []
    if name == "rendering":
        scratch = ["--tmpfs", "/benchmark-tmp:rw,nosuid,nodev,exec,size=512m",
                   "--env", "TMPDIR=/benchmark-tmp"]
        print("RENDERING_FIXTURE_STORAGE: throwaway sessions use a private 512 MiB tmpfs; "
              "reports, binaries and build caches retain their existing storage. "
              "This isolates render measurements from host disk flushes and does not qualify "
              "persistent-storage performance. Other profiles retain their storage; "
              "exit assertions, deadlines and performance budgets are unchanged.", flush=True)
    command("docker", "run", "--rm", "--init", "--cpus", "2", "--memory", "8g", "--pids-limit", "2048", *scratch,
            "--mount", f"type=bind,source={sources},target=/inputs,readonly",
            "--mount", f"type=bind,source={output},target=/evidence",
            "--mount", f"type=volume,source={lease_volume}-cache-v1,target=/state",
            "--mount", f"type=bind,source={state},target=/legacy-state,readonly",
            "--mount", f"type=volume,source={lease_volume},target=/locks",
            "--env", "SCODE_ACCEPTANCE_CONTAINER=1", "--env", "CARGO_HOME=/state/cargo",
            "--env", f"SCODE_RENDERING_PROFILE={rendering_profile()}", image_id,
            "/inputs/runner/.github/local-acceptance/checks.py", "--container-check", name,
            "--base", base)
    if name == "rendering":
        profile_output = container_profile_output(output, name)
        binary = profile_output / "scode-candidate"
        report = json.loads((profile_output / "measurements/report.json").read_text())
        digest = hashlib.sha256(binary.read_bytes()).hexdigest()
        if report["status"] != "passed" or report["candidate_sha256"] != digest or checkout_link(binary):
            raise RuntimeError("Rendering candidate binary does not match its completed measurements")
        invocations[key] = {"binary": binary, "sha256": digest, "image_id": image_id, "image_tag": image}
        container.invocations = invocations


def verified_candidate_release(inputs):
    """Verify the read-only release promoted by this invocation's rendering."""
    directory = inputs / "candidate-release"
    if not directory.exists():
        return None
    binary, receipt = directory / "scode", directory / "identity.json"
    if any(checkout_link(path) for path in [directory, binary, receipt]) or not binary.is_file():
        raise RuntimeError("Reused rendering release must contain regular files")
    expected = json.loads((inputs / "identity.json").read_text())
    observed = json.loads(receipt.read_text())
    if observed.get("version") != 1 or any(observed.get(name) != expected[name]
            for name in ["candidate", "policy", "image_id", "context_sha256"]):
        raise RuntimeError("Reused rendering release belongs to different source, policy or image")
    if hashlib.sha256(binary.read_bytes()).hexdigest() != observed.get("sha256"):
        raise RuntimeError("Reused rendering release bytes changed")
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


def rendering_profile():
    """Per-PR acceptance judges `core`. Release acceptance selects `stress`,
    which adds the foreground/background task and repeated-turn scenarios."""
    profile = os.environ.get("SCODE_RENDERING_PROFILE", "core")
    if profile not in {"core", "stress"}:
        raise RuntimeError(f"Unknown rendering profile: {profile}")
    return profile


def rendering_cases(terminal, arguments, output, command, plan):
    """Finish and judge each complete scenario before admitting the next one."""
    policy_path = terminal / "performance-policy.json"
    policy = json.loads(policy_path.read_text(encoding="utf-8"))
    profile = rendering_profile()
    cases = policy["profiles"][profile]
    if (plan.get("unjudged_warmups") != 0
            or plan["policy_sha256"] != hashlib.sha256(policy_path.read_bytes()).hexdigest()
            or plan["cases"] != cases):
        raise RuntimeError("Rendering policy changed after its pre-build validation")
    if policy.get("case_sampling") and set(plan.get("sampling", {})) != set(cases):
        raise RuntimeError("Rendering sampling overrides require a complete resolved per-case plan")
    if policy.get("case_sampling") and set(plan.get("observations", {})) != set(cases):
        raise RuntimeError("Rendering sampling overrides require complete raw observation counts")
    inventory = None
    if "observations" in plan:
        spec = importlib.util.spec_from_file_location("rendering_observation_inventory", terminal / "benchmark.py")
        inventory = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(inventory)
    if not cases or len(cases) != len(set(cases)) or any(
            not re.fullmatch(r"[a-zA-Z0-9_-]+", case) for case in cases):
        raise RuntimeError("Rendering policy must declare distinct, safe scenario names")
    output.mkdir(parents=True, exist_ok=False)
    report = {"version": 1, "profile": profile, "preview": "0", "policy": policy,
              "policy_sha256": hashlib.sha256(policy_path.read_bytes()).hexdigest(),
              "status": "running", "samples": [], "metrics": [], "calibration": [],
              "completed_cases": [], "remaining_cases": list(cases), "case_policy_sha256": {},
              "expected_samples": plan["total_samples"], "case_expected_samples": plan["samples"],
              "unjudged_warmups": 0}
    if "sampling" in plan:
        report["sampling"] = plan["sampling"]
    if inventory is not None:
        report["observations"] = plan["observations"]
    metadata = None

    def save():
        (output / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    for case in cases:
        # Partition the declared scenarios without changing their workloads,
        # paired round policy, interaction samples or budgets.
        selected = dict(policy, profiles={**policy["profiles"], profile: [case]})
        selected_path = output / f"{case}-policy.json"
        selected_path.write_text(json.dumps(selected, indent=2) + "\n", encoding="utf-8")
        report["case_policy_sha256"][case] = hashlib.sha256(selected_path.read_bytes()).hexdigest()
        report["active_case"] = case
        save()
        print(f"Rendering scenario {len(report['completed_cases']) + 1}/{len(cases)}: {case}. "
              f"Paired rounds: {policy.get('case_rounds', {}).get(case, policy['rounds'])}; no unjudged warm-ups. "
              f"Complete paired result: {output / case / 'report.json'}. "
              "A failed scenario stops this matrix and releases the benchmark lease.", flush=True)
        succeeded = False
        try:
            command(*arguments, "--output", str(output / case), "--policy", str(selected_path), "--profile", profile)
            succeeded = True
        finally:
            child_path = output / case / "report.json"
            try:
                if child_path.exists():
                    child = json.loads(child_path.read_text(encoding="utf-8"))
                    if child["policy"] != selected or child["policy_sha256"] != report["case_policy_sha256"][case]:
                        raise RuntimeError(f"Rendering scenario {case} used a different policy")
                    if "sampling" in plan and child.get("sampling") != {case: plan["sampling"][case]}:
                        raise RuntimeError(f"Rendering scenario {case} used different resolved sampling")
                    if inventory is not None and child.get("observations") != {case: plan["observations"][case]}:
                        raise RuntimeError(f"Rendering scenario {case} declared different raw observation counts")
                    if succeeded and child["status"] == "passed" and len(child["samples"]) != plan["samples"][case]:
                        raise RuntimeError(f"Rendering scenario {case} reported {len(child['samples'])} samples; "
                                           f"required {plan['samples'][case]} judged observations and no extra warm-ups")
                    if succeeded and child["status"] == "passed":
                        observed = [{key: row[key] for key in ["stage", "round", "side"]} for row in child["samples"]]
                        if observed != plan["pairs"][case]:
                            raise RuntimeError(f"Rendering scenario {case} paired sample inventory differs from its declared rounds")
                    for key in ["base_sha256", "candidate_sha256", "driver_sha256", "harness_sha256"]:
                        if key in report and report[key] != child[key]:
                            raise RuntimeError(f"Rendering {key} changed between scenarios")
                        report[key] = child[key]
                    for sample in child["samples"]:
                        if inventory is not None and inventory.observation_counts(sample["data"]) != plan["observations"][case]:
                            raise RuntimeError(f"Rendering scenario {case} has missing or extra raw observations")
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
    if len(report["samples"]) != plan["total_samples"]:
        report.update(status="invalid", failures=["Rendering matrix did not execute its complete declared sample count"])
        save()
        raise RuntimeError("Rendering matrix did not execute its complete declared sample count")
    report.update(status="passed", active_case=None)
    save()


def benchmark_driver_package(source):
    """Older candidate snapshots retain their original CLI test targets."""
    if (source / "rust/crates/scode-benchmark-drivers/Cargo.toml").is_file():
        print("Benchmark drivers use scode-benchmark-drivers and the copied normal release; "
              "Cargo does not rebuild an unused CLI binary for these targets.", flush=True)
        return "scode-benchmark-drivers"
    print("This source predates the standalone benchmark driver package; "
          "using its original CLI test targets with the same copied release and observers.", flush=True)
    return "rusty-sudocode-cli"


def benchmark_locked(name, root, output, base, command, target):
    if name == "browser":
        browser_smoke(root.parent, output, command)
        return
    # Run the declared workload and paired sampling policy. Moving execution
    # hosts does not change budgets or per-run interaction assertions.
    inputs = root.parent
    work, targets = benchmark_storage(name)
    work.mkdir(parents=True, exist_ok=True)
    for label in ["candidate", "baseline", "trusted"]:
        # Sources are disposable copies from this invocation's exact Git archive.
        replace_checkout(inputs / label, work / label)
    candidate, baseline, trusted = (work / label for label in ["candidate", "baseline", "trusted"])
    terminal = trusted / "e2e/terminal-resize"
    plan_log = command(sys.executable, str(terminal / "benchmark.py"), "--describe-policy", "--profile", rendering_profile(),
                       "--policy", str(terminal / "performance-policy.json"))
    render_plan = json.loads(plan_log.read_text(encoding="utf-8"))
    print(f"Required rendering plan: {len(render_plan['cases'])} scenarios, {render_plan['total_samples']} samples. "
          "Each scenario's judged metrics and budgets remain required. Malformed plans fail before npm or Cargo.",
          flush=True)
    # Dependencies must match the checkout's lock; the image supplies the exact
    # Chromium and its system libraries, not a different JS dependency graph.
    command("npm", "ci", "--prefix", str(terminal))
    for label, source in [("baseline", baseline), ("candidate", candidate)]:
        command("cargo", "build", "--locked", "--release", "--manifest-path", str(source / "rust/Cargo.toml"),
                "--target-dir", str(targets / label), "-p", "rusty-sudocode-cli", "--bin", "scode")
        shutil.copy2(targets / label / "release/scode", output / f"scode-{label}")
    # The copied production binaries above stay fixed when dev features build the driver.
    package, prefix = benchmark_driver_targets(trusted)
    driver = f"{prefix}_render_performance"
    log = command("cargo", "test", "--locked", "--release", "--manifest-path", str(trusted / "rust/Cargo.toml"),
                  "--target-dir", str(targets / "driver"), "-p", package,
                  "--test", driver, "--no-run", "--message-format=json-render-diagnostics")
    drivers = compiled_drivers(log, [driver])
    args = [sys.executable, str(terminal / "benchmark.py"), "--driver", drivers[driver],
            "--base", str(output / "scode-baseline"), "--candidate", str(output / "scode-candidate")]
    expected_driver = hashlib.sha256(Path(drivers[driver]).read_bytes()).hexdigest()

    def measured_command(*args):
        return command(*args, extra={"SCODE_EXPECTED_DRIVER_SHA256": expected_driver})

    rendering_cases(terminal, args, output / "measurements", measured_command, render_plan)


def benchmark_driver_targets(source):
    """The driver package and its target-name prefix for this source snapshot."""
    package = benchmark_driver_package(source)
    return package, "benchmark" if package == "scode-benchmark-drivers" else "pty"


def compiled_drivers(log, names):
    """The single test executable Cargo reported for each requested target."""
    found = {name: [] for name in names}
    for line in log.read_text(errors="replace").splitlines():
        if line.startswith("{"):
            row = json.loads(line)
            target = row.get("target", {}).get("name")
            if row.get("reason") == "compiler-artifact" and target in found and row.get("executable"):
                found[target].append(row["executable"])
    if any(len(executables) != 1 for executables in found.values()):
        raise RuntimeError(f"Expected exactly one compiled driver per target: {found}")
    return {name: executables[0] for name, executables in found.items()}


def browser_viewport_cases():
    """Required native viewport journeys for browser execution and its build fixture."""
    return ("viewport_browser_interaction", "viewport_browser_resource_exit")


def browser_smoke(inputs, output, command):
    """Painted-terminal behaviour against the measured release or a dev build.

    Typing latency is judged by scoped local rendering; this pass checks
    what only a real browser terminal shows: clickable links, native selection
    surviving streaming, viewport wheel and selection, and an idle terminal.
    """
    reused = verified_candidate_release(inputs)
    work, targets = benchmark_storage("rendering" if reused is not None else "browser")
    work.mkdir(parents=True, exist_ok=True)
    # Rendering has finished with this mutable checkout. Synchronize the actual
    # candidate, then let normal Cargo dependency tracking validate every input.
    candidate = work / ("trusted" if reused is not None else "candidate")
    replace_checkout(inputs / "candidate", candidate)
    terminal = candidate / "e2e/terminal-resize"
    command("npm", "ci", "--prefix", str(terminal))
    manifest = str(candidate / "rust/Cargo.toml")
    build = str(targets / ("driver" if reused is not None else "candidate"))
    profile = ["--release"] if reused is not None else []
    binary = output / "scode-candidate"
    # Copy before the drivers build: their dev features rebuild the shared graph.
    if reused is None:
        command("cargo", "build", "--locked", "--manifest-path", manifest, "--target-dir", build,
                "-p", "rusty-sudocode-cli", "--bin", "scode")
        shutil.copy2(targets / "candidate/debug/scode", binary)
    else:
        shutil.copy2(reused, binary)
        print("Browser smoke launches the hash-verified normal release from this rendering invocation; "
              "native drivers share rendering's release Cargo cache and still build from the candidate.", flush=True)
    expected_digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    package, prefix = benchmark_driver_targets(candidate)
    links, viewport = f"{prefix}_hyperlinks", f"{prefix}_prompt_viewport"
    log = command("cargo", "test", "--locked", *profile, "--manifest-path", manifest, "--target-dir", build,
                  "-p", package, "--test", links, "--test", viewport,
                  "--no-run", "--message-format=json-render-diagnostics")
    drivers = compiled_drivers(log, [links, viewport])
    smoke = Path(__file__).resolve().parents[2] / "e2e/terminal-resize/gui-smoke.py"
    command(sys.executable, str(smoke), "--driver", drivers[links],
            "--candidate", str(binary), "--output", str(output / "smoke"),
            extra={"SCODE_EXPECTED_CANDIDATE_SHA256": expected_digest,
                   "SCODE_EXPECTED_DRIVER_SHA256": hashlib.sha256(Path(drivers[links]).read_bytes()).hexdigest()})
    for case in browser_viewport_cases():
        log = command(drivers[viewport], case, "--ignored", "--exact", "--nocapture",
                      extra={"SCODE_TEST_BIN": str(binary), "SCODE_VIEWPORT_MEASURE_DIR": str(output / "viewport-paint")})
        counts = re.findall(r"test result: ok\. (\d+) passed; (\d+) failed;", log.read_text(errors="replace"))
        if not counts or sum(int(passed) for passed, _ in counts) == 0 or any(int(failed) for _, failed in counts):
            raise RuntimeError(f"No actual passing viewport lifecycle test: {case}")
    if hashlib.sha256(binary.read_bytes()).hexdigest() != expected_digest:
        raise RuntimeError("Browser candidate changed after validation")

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
