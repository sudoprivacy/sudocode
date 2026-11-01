"""Exercise development-only build selection through real Git, Cargo and CLI."""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


SOURCE = Path(__file__).resolve().parents[2]
PRODUCT = {"native-release", "native-services", "native-cohost"}


def workflow_scope(output, environment):
    """Execute the actual workflow guards and bind their conditional steps."""
    output.mkdir()
    bash = shutil.which("bash", path=environment.get("PATH"))
    if os.name == "nt":
        # CreateProcess can resolve a bare bash against the parent's PATH,
        # selecting WSL even when the explicit child environment lists Git first.
        git = shutil.which("git", path=environment.get("PATH"))
        candidates = [Path(git).resolve().parent / "bash.exe",
                      Path(git).resolve().parent.parent / "bin/bash.exe"] if git else []
        bash = next((str(path) for path in candidates if path.is_file()), None)
    if not bash:
        raise RuntimeError("Workflow guard acceptance requires Bash; on Windows install Git for Windows with Bash")
    workflow = (SOURCE / ".github/workflows/rust-ci.yml").read_text(encoding="utf-8")
    lines = workflow.splitlines()

    def step(name):
        matches = [index for index, line in enumerate(lines) if line == "      - name: " + name]
        assert len(matches) == 1, name
        start = matches[0]
        end = start + 1
        while end < len(lines) and (not lines[end].strip() or
                                   (lines[end].startswith("      ") and not lines[end].startswith("      - "))):
            end += 1
        return lines[start:end]

    condition = "        if: needs.one-nexus-vfs-pin.outputs.cohost_required == 'true'"
    for name in ["Co-host duet against a daemon built from this checkout", "Checkout the VFS contract used by the co-host",
                 "Search authorization and restart under the confined co-host owner", "The driver-ai co-host still resolves"]:
        assert [line for line in step(name) if line.startswith("        if:")] == [condition], name
    assert "      cohost_required: ${{ steps.scope.outputs.cohost_required }}" in lines
    for name in ["A2A round-trip against the pinned daemon", "Cross-node A2A against two nodes",
                 "Auth-on A2A — the node decides who a message is from"]:
        assert not any(line.startswith("        if:") for line in step(name)), name

    def script(name):
        block = step(name)
        start = block.index("        run: |") + 1
        body = []
        for line in block[start:]:
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            assert line.startswith("          "), (name, line)
            body.append(line[10:])
        return "set -eu\n" + "\n".join(body) + "\n"

    scripts = {"explain": script("Explain selected daemon acceptance"),
               "required": script("Require native daemon acceptance and zero workspace warnings")}
    for name, source in scripts.items():
        (output / f"{name}.sh").write_text(source, encoding="utf-8")
    cases = [
        ("explain", "true", "true", "success", True),
        ("explain", "false", "true", "success", True),
        ("explain", "", "true", "success", False),
        ("required", "true", "true", "success", True),
        ("required", "false", "true", "success", True),
        ("required", "false", "false", "skipped", True),
        ("required", "true", "false", "skipped", False),
        ("required", "", "true", "success", False),
        ("required", "false", "true", "skipped", False),
        ("required", "true", "true", "failure", False),
    ]
    records = []
    for index, (name, cohost, daemon, actual, succeeds) in enumerate(cases):
        values = {"COHOST_REQUIRED": cohost, "DAEMON_REQUIRED": daemon, "A2A_RESULT": actual,
                  "PIN_RESULT": "success", "CLIPPY_REQUIRED": "false", "CLIPPY_RESULT": "skipped"}
        result = subprocess.run([bash, "-c", scripts[name]], env={**environment, **values}, capture_output=True)
        log = output / f"{index:02}.log"
        # Native setup failures can use a different encoding. Retain their raw
        # output so a decoder exception never hides the actual command result.
        log.write_bytes(result.stdout + result.stderr)
        assert (result.returncode == 0) == succeeds, (values, result.stdout, result.stderr)
        records.append({"script": name, "values": values, "exit_code": result.returncode,
                        "log_sha256": hashlib.sha256(log.read_bytes()).hexdigest()})
    (output / "result.json").write_text(json.dumps(records, indent=2) + "\n")
    return {"actual_workflow_guard_cases": len(records), "cohost_steps_bound_to_selector": True,
            "CLI_daemon_journeys_unconditional_inside_required_job": True}


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    repository = output / "repository"
    repository.mkdir()
    environment = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    environment.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1",
                       CARGO_BUILD_JOBS="1", CARGO_PROFILE_DEV_DEBUG="0",
                       CARGO_PROFILE_TEST_DEBUG="0", CARGO_INCREMENTAL="0")
    if os.name == "nt":
        # Identical Windows links otherwise change timestamps/PDB GUIDs.
        # Make this private canary reproducible; product flags stay untouched.
        if "CARGO_ENCODED_RUSTFLAGS" in environment:
            environment["CARGO_ENCODED_RUSTFLAGS"] += "\x1f-C\x1flink-arg=/Brepro"
        else:
            environment["RUSTFLAGS"] = environment.get("RUSTFLAGS", "") + " -C link-arg=/Brepro"
    commands, cases = [], []

    def command(*args, success=True):
        result = subprocess.run(list(map(str, args)), cwd=repository, env=environment,
                                capture_output=True, text=True, encoding="utf-8", errors="replace")
        if (result.returncode == 0) != success:
            raise AssertionError(str(args) + "\n" + result.stdout + result.stderr)
        return result.stdout.strip() if success else result.stdout + result.stderr

    def write(path, text):
        destination = repository / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(text, encoding="utf-8")

    def commit(subject):
        command("git", "add", ".")
        command("git", "commit", "-m", subject)
        return command("git", "rev-parse", "HEAD")

    command("git", "init", "--initial-branch=main")
    command("git", "config", "user.name", "Dependency scope acceptance")
    command("git", "config", "user.email", "scope@example.invalid")
    command("git", "config", "core.autocrlf", "false")
    actual_metadata = json.loads(command("cargo", "metadata", "--no-deps", "--locked", "--format-version=1",
                                         "--manifest-path", SOURCE / "rust/Cargo.toml"))
    actual_cli = next(package for package in actual_metadata["packages"] if package["name"] == "rusty-sudocode-cli")
    assert not any(set(target["kind"]) & {"lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"}
                   for target in actual_cli["targets"]), "The CLI gained a shared library; revisit co-host scope"
    cli_directory = Path(actual_cli["manifest_path"]).parent.resolve()
    assert not any(dependency["name"] == "rusty-sudocode-cli"
                   or (dependency.get("path") and Path(dependency["path"]).resolve() == cli_directory)
                   for package in actual_metadata["packages"] if package["id"] != actual_cli["id"]
                   for dependency in package["dependencies"]), "A workspace package now depends on the CLI"
    configuration = subprocess.run(["git", "config", "--get", "localAcceptance.vsdevcmd"],
                                   cwd=SOURCE, env=environment, capture_output=True, text=True).stdout.strip()
    if configuration:
        command("git", "config", "localAcceptance.vsdevcmd", configuration)
    for name in [".github/scripts/local_acceptance.py", ".github/scripts/ci_input_scope.py", "spec/local_acceptance.json"]:
        write(name, (SOURCE / name).read_text(encoding="utf-8"))
    write(".gitignore", "rust/target/\n")
    workspace = '[workspace]\nmembers = ["crates/*"]\nresolver = "2"\n'
    application = ('[package]\nname="rusty-sudocode-cli"\nversion="0.1.0"\nedition="2021"\n'
                   '[[bin]]\nname="scope-product"\npath="src/main.rs"\n'
                   '[dependencies]\nruntime={path="../runtime"}\n')
    write("rust/Cargo.toml", workspace)
    write("rust/crates/rusty-sudocode-cli/Cargo.toml", application)
    write("rust/crates/rusty-sudocode-cli/src/main.rs", 'fn main() { println!("product-{}", runtime::value()); }\n')
    write("rust/crates/rusty-sudocode-cli/tests/development.rs", '#[test] fn native_case() { assert_eq!(runtime::value(), 42); }\n')
    write("rust/crates/runtime/Cargo.toml", '[package]\nname="runtime"\nversion="0.1.0"\nedition="2021"\n[features]\ntest-helper=[]\n')
    write("rust/crates/runtime/src/lib.rs", '#[cfg(not(feature="test-helper"))] pub fn value() -> u8 { 42 }\n'
          '#[cfg(feature="test-helper")] pub fn value() -> u8 { 99 }\n')
    write("rust/crates/helper/Cargo.toml", '[package]\nname="scope-helper"\nversion="0.1.0"\nedition="2021"\n'
          '[dependencies]\nruntime={path="../runtime",features=["test-helper"]}\n')
    write("rust/crates/helper/src/lib.rs", 'pub fn value() -> u8 { runtime::value() }\n')
    write("rust/crates/cohost/Cargo.toml", '[package]\nname="scope-cohost"\nversion="0.1.0"\nedition="2021"\n'
          '[dependencies]\nruntime={path="../runtime"}\n')
    write("rust/crates/cohost/src/main.rs", 'fn main() { println!("cohost-{}", runtime::value()); }\n')
    command("cargo", "generate-lockfile", "--offline", "--manifest-path", "rust/Cargo.toml")
    base = commit("Baseline native product and unused development helper")
    helper = SOURCE / ".github/scripts/local_acceptance.py"
    spec = importlib.util.spec_from_file_location("dependency_scope_executor", helper)
    executor = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(executor)
    executor.ROOT = repository
    target = repository / "rust/target"
    build_environment = {**environment, "CARGO_TARGET_DIR": str(target)}

    def cargo(*args):
        result = executor.run_command(["cargo", *args], repository / "rust",
                                      output / f"cargo-{len(commands):02}.log", build_environment)
        commands.append(result)
        return result

    def product():
        cargo("build", "--locked", "--offline", "-p", "rusty-sudocode-cli")
        binary = target / "debug" / ("scope-product.exe" if os.name == "nt" else "scope-product")
        assert command(binary) == "product-42"
        return hashlib.sha256(binary.read_bytes()).hexdigest()

    original_binary = product()

    def cohost(value=42):
        cargo("build", "--locked", "--offline", "-p", "scope-cohost")
        binary = target / "debug" / ("scope-cohost.exe" if os.name == "nt" else "scope-cohost")
        assert command(binary) == f"cohost-{value}"
        return hashlib.sha256(binary.read_bytes()).hexdigest()

    original_cohost = cohost()
    workflow = workflow_scope(output / "workflow-scope", environment)
    development = application + '[dev-dependencies]\nscope-helper={path="../helper"}\n'
    write("rust/crates/rusty-sudocode-cli/Cargo.toml", development)
    write("rust/crates/rusty-sudocode-cli/tests/development.rs", '#[test] fn native_case() { assert_eq!(scope_helper::value(), 99); }\n')
    command("cargo", "generate-lockfile", "--offline", "--manifest-path", "rust/Cargo.toml")
    changed = commit("Add development dependency and actual native helper case")

    def expect(label, diff_base, expected):
        text = command(sys.executable, helper, "plan", "--base", diff_base, "--policy-ref", "main",
                       "--host", "windows", "--architecture", "AMD64")
        plan = json.loads(next(line.split("=", 1)[1] for line in text.splitlines() if line.startswith("ACCEPTANCE_PLAN=")))
        names = set(plan["local_checks"])
        assert names & PRODUCT == set(expected), (label, text)
        assert "native-workspace" in names, (label, text)
        assert {"rendering", "browser"} <= names, (label, text)
        cases.append({"label": label, "base": diff_base, "head": command("git", "rev-parse", "HEAD"),
                      "product_checks": sorted(names & PRODUCT), "native_tests_and_performance_retained": True})
        (output / "cases.json").write_text(json.dumps(cases, indent=2) + "\n")
        print("PASS: " + label, flush=True)

    def ci_scope(diff_base, daemon, benchmark=False, cohost_required=None):
        cohost_required = daemon if cohost_required is None else cohost_required
        head = command("git", "rev-parse", "HEAD")
        keys = ["GITHUB_EVENT_NAME", "GITHUB_EVENT_PATH", "GITHUB_OUTPUT"]
        previous = {key: environment.get(key) for key in keys}
        try:
            for event, data in [
                ("pull_request", {"pull_request": {"base": {"sha": diff_base}, "head": {"sha": head}}}),
                ("push", {"before": diff_base, "after": head}),
                ("merge_group", {"merge_group": {"base_sha": diff_base, "head_sha": head}}),
            ]:
                event_path = output / "ci-event.json"
                event_path.write_text(json.dumps(data))
                result_path = output / "ci-output.txt"
                result_path.write_text("")
                environment.update(GITHUB_EVENT_NAME=event, GITHUB_EVENT_PATH=str(event_path),
                                   GITHUB_OUTPUT=str(result_path))
                result = json.loads(command(sys.executable, repository / ".github/scripts/ci_input_scope.py"))
                assert result["daemon_required"] == str(daemon).lower(), result
                assert result["cohost_required"] == str(cohost_required).lower(), result
                emitted = dict(line.split("=", 1) for line in result_path.read_text().splitlines())
                assert emitted["cohost_required"] == result["cohost_required"], emitted
                assert result["clippy_required"] == "true", result
                assert result["live_required"] == "true", result
                assert result["benchmark_required"] == str(benchmark).lower(), result
                print(f"PASS: actual {event} selector daemon={daemon}, cohost={cohost_required}; native lint/provider coverage retained", flush=True)
        finally:
            for key, value in previous.items():
                if value is None:
                    environment.pop(key, None)
                else:
                    environment[key] = value

    expect("dev-only manifest/lock skips unchanged normal products", base, [])
    ci_scope(base, False)
    assert product() == original_binary
    cargo("test", "--locked", "--offline", "-p", "rusty-sudocode-cli", "--test", "development")
    assert product() == original_binary
    positive_lock = (repository / "rust/Cargo.lock").read_text()

    # The actual Cargo build must not import a broken, independent CLI source.
    write("rust/crates/rusty-sudocode-cli/src/main.rs", 'compile_error!("unused-CLI-control");\n')
    commit("CLI source keeps daemon journeys without rebuilding the independent co-host")
    expect("CLI source retains normal CLI checks and skips the unchanged co-host", base,
           {"native-release", "native-services"})
    ci_scope(base, True, cohost_required=False)
    assert cohost() == original_cohost
    refused_log = output / "independent-cli-refusal.log"
    try:
        executor.run_command(["cargo", "build", "--locked", "--offline", "-p", "rusty-sudocode-cli"],
                             repository / "rust", refused_log, build_environment)
    except RuntimeError:
        refused_cli = json.loads(refused_log.with_suffix(".command.json").read_text())
        commands.append(refused_cli)
        assert refused_cli["exit_code"] != 0 and "unused-CLI-control" in refused_log.read_text()
    else:
        raise AssertionError("The deliberately broken CLI unexpectedly compiled")
    write("rust/crates/rusty-sudocode-cli/src/lib.rs", 'pub fn shared() {}\n')
    library_head = commit("Automatic CLI library target retains co-host scope")
    expect("a new CLI library target retains conservative co-host scope", base,
           {"native-release", "native-services"})
    ci_scope(base, True)
    command("git", "rm", "rust/crates/rusty-sudocode-cli/src/lib.rs")
    binary_head = commit("Removing a prior CLI library also retains co-host scope")
    expect("a removed CLI library is not mistaken for an unchanged binary boundary", library_head,
           {"native-release", "native-services"})
    ci_scope(library_head, True)
    cohost_manifest = (repository / "rust/crates/cohost/Cargo.toml").read_text()
    write("rust/crates/cohost/Cargo.toml", cohost_manifest +
          '[build-dependencies]\ncli_runner={package="rusty-sudocode-cli",path="../rusty-sudocode-cli"}\n')
    importer = commit("An unknown declared CLI consumer invalidates independence")
    write("rust/crates/rusty-sudocode-cli/src/main.rs", 'compile_error!("unused-CLI-control");\n// consumer boundary\n')
    commit("CLI source with a declared consumer keeps co-host acceptance")
    expect("a declared CLI consumer retains scope for later CLI source", importer,
           {"native-release", "native-services"})
    ci_scope(importer, True)
    command("git", "switch", "--detach", binary_head)
    write("rust/Cargo.toml", workspace.replace('["crates/*"]',
          '["crates/rusty-sudocode-cli","crates/runtime","crates/helper","crates/cohost"]'))
    layout = commit("A different workspace layout keeps conservative scope")
    write("rust/crates/rusty-sudocode-cli/src/main.rs", 'compile_error!("unused-CLI-control");\n// workspace boundary\n')
    commit("CLI source with an unfamiliar workspace layout keeps co-host acceptance")
    expect("an unfamiliar workspace layout retains scope for later CLI source", layout,
           {"native-release", "native-services"})
    ci_scope(layout, True)
    command("git", "switch", "--detach", binary_head)
    write("rust/crates/runtime/src/lib.rs", 'pub fn value() -> u8 { 43 }\n')
    commit("A mixed CLI and shared runtime change retains co-host acceptance")
    expect("shared runtime source still selects the co-host with CLI source", base, PRODUCT)
    ci_scope(base, True)
    assert cohost(43) != original_cohost
    command("git", "switch", "--detach", changed)

    variants = [
        ("normal dependency features", "rust/crates/rusty-sudocode-cli/Cargo.toml",
         development.replace('runtime={path="../runtime"}', 'runtime={path="../runtime",features=["test-helper"]}'), PRODUCT),
        ("optional normal dependency", "rust/crates/rusty-sudocode-cli/Cargo.toml",
         development.replace('[dependencies]', '[dependencies]\nother={package="scope-helper",path="../helper",optional=true}'), PRODUCT),
        ("build dependency", "rust/crates/rusty-sudocode-cli/Cargo.toml",
         development + '[build-dependencies]\nscope-helper={path="../helper"}\n', PRODUCT),
        ("target normal dependency", "rust/crates/rusty-sudocode-cli/Cargo.toml",
         development + '[target.\'cfg(windows)\'.dependencies]\nother={package="scope-helper",path="../helper"}\n', PRODUCT),
        ("feature definition", "rust/crates/rusty-sudocode-cli/Cargo.toml", development + '[features]\nextra=[]\n', PRODUCT),
        ("workspace resolver change", "rust/Cargo.toml", workspace.replace('"2"', '"1"'), PRODUCT),
        ("unknown workspace setting", "rust/Cargo.toml", workspace + '[profile.dev]\nopt-level=1\n', PRODUCT),
        ("malformed manifest", "rust/crates/rusty-sudocode-cli/Cargo.toml", '[package\n', PRODUCT),
        ("normal lock package changed", "rust/Cargo.lock", positive_lock.replace('name = "runtime"\nversion = "0.1.0"', 'name = "runtime"\nversion = "0.2.0"'), PRODUCT),
        ("ambiguous additional lock version", "rust/Cargo.lock", positive_lock + '\n[[package]]\nname="runtime"\nversion="0.2.0"\n', PRODUCT),
        ("lock format changed", "rust/Cargo.lock", positive_lock.replace('version = 4', 'version = 3'), PRODUCT),
        ("source stays a normal product input", "rust/crates/runtime/src/lib.rs", 'pub fn value() -> u8 { 43 }\n', PRODUCT),
        ("CLI build script keeps conservative co-host scope", "rust/crates/rusty-sudocode-cli/build.rs",
         'fn main() { println!("cargo:rerun-if-changed=build.rs"); }\n', {"native-release", "native-services"}),
        ("release assertion stays required", "rust/crates/rusty-sudocode-cli/tests/pty_model_stream_close.rs", '// actual changed release fixture\n', {"native-release"}),
        ("service assertion stays required", "e2e/shared-services/scope.py", '# actual changed service fixture\n', {"native-services"}),
        ("target-specific development alias", "rust/crates/rusty-sudocode-cli/Cargo.toml",
         application + '[target.\'cfg(windows)\'.dev-dependencies]\nalias={package="scope-helper",path="../helper"}\n', []),
        ("standalone benchmark driver metadata", "rust/crates/scode-benchmark-drivers/Cargo.toml",
         # This test package is outside the policy's explicit co-host inputs.
         '[package]\nname="scode-benchmark-drivers"\nversion="0.1.0"\nedition="2021"\n', {"native-release", "native-services"}),
    ]
    for index, (label, path, text, expected) in enumerate(variants):
        command("git", "switch", "--detach", changed)
        write(path, text)
        if path.endswith("Cargo.lock"):
            assert text != positive_lock, label
        commit("Scope boundary " + str(index))
        expect(label, base, expected)
        if label in {"source stays a normal product input", "CLI build script keeps conservative co-host scope",
                     "normal dependency features", "build dependency", "normal lock package changed"}:
            ci_scope(base, True)
        elif label == "standalone benchmark driver metadata":
            ci_scope(base, True, benchmark=True)
    command("git", "switch", "--detach", changed)
    command("git", "mv", "rust/crates/runtime/src/lib.rs", "rust/crates/runtime/src/renamed.rs")
    commit("Keep both sides of a renamed source")
    expect("renamed product source stays required", base, PRODUCT)
    command("git", "switch", "--detach", changed)
    assert not command("git", "status", "--porcelain")
    # An earlier broader receipt/checkpoint must retain actual completed work
    # when this protected selector removes only irrelevant normal builds.
    command("git", "switch", "--detach", base)
    key = output / "signing-key"
    command("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", key)
    policy = {"version": 1, "repository": "acceptance/fixture", "checks": {
        "preflight": {"include": ["**"]},
        "native-workspace": {"include": ["rust/**"]},
        **{name: {"include": ["rust/Cargo.lock", "rust/crates/*/Cargo.toml",
                              "rust/crates/rusty-sudocode-cli/tests/**"]}
           for name in ["native-release", "native-services", "native-cohost"]},
    }}
    write("spec/local_acceptance.json", json.dumps(policy))
    write("spec/local_acceptance_signers.json", json.dumps({"signers": {"fixture": {
        "platform": executor.host()["system"], "public_key": key.with_suffix(".pub").read_text().strip()}}}))
    write(".github/workflows/local-acceptance.yml", "# Private acceptance metadata\n")
    write(".github/workflows/local-acceptance-publish.yml", "# Private publisher metadata\n")
    write(".github/local-acceptance/Dockerfile", "FROM scratch\n")
    write(".github/local-acceptance/checks.py", 'import sys\n'
          'def run(name, root, output, base, execute):\n'
          ' output.mkdir(parents=True)\n'
          ' return [execute([sys.executable, "scope_check.py", name], root, output / "command.log")]\n')
    write("scope_check.py", 'import os,sys\nfrom pathlib import Path\n'
          'with Path(os.environ["SCOPE_CHECK_LOG"]).open("a") as log: log.write(sys.argv[1]+"\\n")\n'
          'print("Actual native profile process: "+sys.argv[1])\n'
          'raise SystemExit(9 if os.environ.get("FAIL_SCOPE_PROFILE")==sys.argv[1] else 0)\n')
    protected = commit("Private signed-scope policy and native process checker")
    command("git", "config", "localAcceptance.signer", "fixture")
    command("git", "config", "localAcceptance.key", key)
    # Use a real earlier broader protected policy, so the regression also
    # runs from Docker's history-free tracked-source snapshot.
    old_helper = helper
    write("rust/crates/rusty-sudocode-cli/Cargo.toml", development)
    write("rust/crates/rusty-sudocode-cli/tests/earlier_scope.rs", "// Earlier policy selects this native input\n")
    command("cargo", "generate-lockfile", "--offline", "--manifest-path", "rust/Cargo.toml")
    full_head = commit("Development-only source for an earlier broader full receipt")
    environment["SCOPE_CHECK_LOG"] = str(output / "full-profile-count.txt")
    old_full = output / "old-full"
    command(sys.executable, old_helper, "check", "--base", protected, "--policy-ref", protected, "--output", old_full)
    assert len(Path(environment["SCOPE_CHECK_LOG"]).read_text().splitlines()) == 5
    note = output / "actual-signed-note.json"
    note.write_text(json.dumps({"version": 1, "attestations": {
        "fixture": json.loads((old_full / "envelope.json").read_text())}}))
    command("git", "notes", "--ref=refs/notes/local-acceptance", "add", "-F", note, full_head)
    command("git", "switch", "--detach", protected)
    for name in PRODUCT:
        policy["checks"][name]["include"].remove("rust/crates/rusty-sudocode-cli/tests/**")
    write("spec/local_acceptance.json", json.dumps(policy))
    narrowed = commit("Protected policy narrows unchanged normal product selection")
    command("git", "switch", "--detach", full_head)
    text = command(sys.executable, helper, "verify", "--head", full_head, "--base", protected, "--policy-ref", narrowed)
    assert "earlier scope" in text, text
    assert len(Path(environment["SCOPE_CHECK_LOG"]).read_text().splitlines()) == 5
    print("PASS: actual earlier full Git/SSH receipt keeps valid execution without rerunning profiles", flush=True)
    legacy = json.loads((old_full / "receipt.json").read_text())
    legacy.pop("plan")
    envelope = executor.sign_receipt(legacy, output / "legacy-without-plan", "fixture", key)
    legacy_note = output / "legacy-signed-note.json"
    legacy_note.write_text(json.dumps({"version": 1, "attestations": {"fixture": envelope}}))
    command("git", "notes", "--ref=refs/notes/local-acceptance", "add", "--force", "-F", legacy_note, full_head)
    command(sys.executable, helper, "verify", "--head", full_head, "--base", protected, "--policy-ref", narrowed)
    assert len(Path(environment["SCOPE_CHECK_LOG"]).read_text().splitlines()) == 5
    print("PASS: actual legacy signed full receipt without a plan retains known completed checks", flush=True)
    # Produce a real later failure and retain its signed earlier progress.
    write("rust/crates/rusty-sudocode-cli/tests/scope_note.rs", "// A distinct unchanged-product commit\n")
    partial_head = commit("Distinct development-only source for actual failed broader attempt")
    environment["SCOPE_CHECK_LOG"] = str(output / "partial-profile-count.txt")
    environment["FAIL_SCOPE_PROFILE"] = "native-cohost"
    old_partial = output / "old-partial"
    command(sys.executable, old_helper, "check", "--base", protected, "--policy-ref", protected,
            "--output", old_partial, success=False)
    assert len(Path(environment["SCOPE_CHECK_LOG"]).read_text().splitlines()) == 5
    resumed = output / "new-resumed"
    command(sys.executable, helper, "check", "--base", protected, "--policy-ref", narrowed,
            "--resume-from", old_partial, "--output", resumed)
    assert len(Path(environment["SCOPE_CHECK_LOG"]).read_text().splitlines()) == 5
    saved = json.loads((resumed / "receipt.json").read_text())
    assert set(saved["checks"]) == {"preflight", "native-workspace"}
    checkpoint = json.loads((old_partial / "attempt.json").read_text())
    assert saved["timing"]["first_recorded_started_at"] == checkpoint["first_recorded_started_at"]
    assert json.loads((old_partial / "native-cohost/command.command.json").read_text())["exit_code"] == 9
    environment.pop("FAIL_SCOPE_PROFILE")
    print("PASS: actual failed broader checkpoint resumes only current checks without repeating completed work or erasing failure/time", flush=True)
    command("git", "switch", "--detach", narrowed)
    policy["checks"]["new-required"] = {"include": ["**"]}
    write("spec/local_acceptance.json", json.dumps(policy))
    expanded = commit("Protected policy adds an actual required check")
    command("git", "switch", "--detach", partial_head)
    failure = command(sys.executable, helper, "verify", "--head", full_head, "--base", protected,
                      "--policy-ref", expanded, success=False)
    assert "new-required" in failure or "Acceptance commands changed" in failure, failure
    print("PASS: the same actual signed full receipt cannot omit a newly required profile", flush=True)
    command("git", "switch", "--detach", protected)
    write("rust/crates/rusty-sudocode-cli/Cargo.toml", development.replace(
        'runtime={path="../runtime"}', 'runtime={path="../runtime",features=["test-helper"]}'))
    command("cargo", "generate-lockfile", "--offline", "--manifest-path", "rust/Cargo.toml")
    commit("Normal feature change retains the actual failing profile")
    environment["SCOPE_CHECK_LOG"] = str(output / "normal-failure-count.txt")
    environment["FAIL_SCOPE_PROFILE"] = "native-cohost"
    normal_partial = output / "normal-old-partial"
    command(sys.executable, old_helper, "check", "--base", protected, "--policy-ref", protected,
            "--output", normal_partial, success=False)
    assert len(Path(environment["SCOPE_CHECK_LOG"]).read_text().splitlines()) == 5
    failure = command(sys.executable, helper, "check", "--base", protected, "--policy-ref", narrowed,
                      "--resume-from", normal_partial, "--output", output / "normal-new-still-fails", success=False)
    assert len(Path(environment["SCOPE_CHECK_LOG"]).read_text().splitlines()) == 6
    assert "Local acceptance failed (9)" in failure, failure
    environment.pop("FAIL_SCOPE_PROFILE")
    print("PASS: an actual normal-input change still reruns and fails its required profile", flush=True)
    observed = [priority for row in commands for priority in row.get("compiler_priorities", {}).values()]
    expected_priority = 16384 if os.name == "nt" else 10
    assert observed and all(priority == expected_priority for priority in observed), observed
    result = {"status": "PASS", "cases": cases, "commands": commands,
              "unchanged_normal_binary_sha256": original_binary,
              "actual_development_feature_case_passed": True,
              "normal_binary_after_development_driver_unchanged": True,
              "earlier_broader_full_and_partial_signed_evidence_reused": True,
              "legacy_signed_receipt_without_plan_reused": True,
              "original_failure_and_first_timing_preserved": True,
              "newly_required_profile_refused": True,
              "required_normal_failure_still_fails": True,
              "actual_pr_push_queue_selectors_keep_lint_and_provider_scope": True,
              "actual_cli_package_is_binary_only_and_has_no_workspace_dependents": True,
              "broken_independent_cli_does_not_build_into_cohost": True,
              "shared_runtime_source_rebuilds_actual_cohost": True,
              "workflow_scope": workflow,
              "compiler_priority": expected_priority,
              "scope": "Actual Git/public planner and Cargo/native canaries. No product runtime, provider or performance coverage claimed."}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(f"PASS: {len(cases)} actual Git boundaries, native development case and unchanged normal binary; {output}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    run(parser.parse_args().output.resolve())
