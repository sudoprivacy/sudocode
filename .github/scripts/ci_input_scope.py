#!/usr/bin/env python3
"""Select expensive provider/daemon acceptance from actual changed Git inputs."""

import ast
import builtins
import fnmatch
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import posixpath
import re
import subprocess
import symtable
import tempfile

from local_acceptance import (
    POLICY, SIGNERS, opt_in_test_target_changes, production_build_paths, referenced_globals, selected,
)


def git(*args):
    return subprocess.check_output(["git", *args]).decode().strip()



# Unknown import/declaration/consumer layouts retain the generic daemon jobs.
REVIEWED_VERIFIER_LAYOUTS = {
    "08cc01e8f8fd0d9195d953204c9e152bd8146d6d444374deafa72816660dc766",
    "fca56dddf2f8f74df039a1008f5a26c1a1cca118b09fa650dae642b1705b6e45",
}


def verifier_layout_digest(source, mode, consumers):
    """Bind reviewed eager context, every declaration and complete consumer code."""
    tree = ast.parse(source)
    # Python normalizes CRLF while compiling; retain all other source text.
    lines = source.replace("\r\n", "\n").splitlines(keepends=True)
    layout = []
    for node in tree.body:
        if isinstance(node, ast.FunctionDef):
            if node.body[0].lineno <= node.lineno:
                raise ValueError("Unknown inline runner declaration")
            first = min([node.lineno, *[item.lineno for item in node.decorator_list]])
            item = {"function": node.name,
                    "declaration": "".join(lines[first - 1:node.body[0].lineno - 1])}
            if node.name in consumers:
                item["consumer"] = "".join(lines[first - 1:node.end_lineno])
        else:
            item = {"context": "".join(lines[node.lineno - 1:node.end_lineno])}
        layout.append(item)
    encoded = json.dumps({"mode": mode, "layout": layout}, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def unchanged_profile_execution(base, head):
    """Use comparison-base code; the merge queue supplies its protected base."""
    previous = json.loads(git("show", f"{base}:{POLICY}"))
    current = json.loads(git("show", f"{head}:{POLICY}"))
    if previous != current:
        raise ValueError("Required acceptance policy changed")
    names = [name for name in current["checks"] if name != "guard-contract"]
    if not names:
        raise ValueError("No non-guard execution identity is available")
    with tempfile.TemporaryDirectory(prefix="scode-scope-policy-") as temporary:
        path = Path(temporary) / "protected_runner.py"
        path.write_bytes(subprocess.check_output(["git", "show", f"{base}:.github/scripts/local_acceptance.py"]))
        spec = importlib.util.spec_from_file_location("protected_scope_execution", path)
        helper = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(helper)
        helper.ROOT = Path.cwd()
        before = helper.reachable_profile_commands_hashes(base, names)
        after = helper.reachable_profile_commands_hashes(head, names)
    changed = [name for name in names if before[name] != after[name]]
    if changed:
        raise ValueError(f"Product or metadata execution changed: {changed}")
    return names


def metadata_only_inputs(paths, base, head, details):
    """Omit generic daemon smoke only for bounded verifier-body edits."""
    runner = ".github/scripts/local_acceptance.py"
    allowed = {runner, ".github/local-acceptance/checks.py",
               "e2e/local-acceptance/profile_compat_acceptance.py",
               "e2e/local-acceptance/parallel_acceptance.py",
               "e2e/local-acceptance/resource_scheduling_acceptance.py"}
    if runner not in paths or not set(paths) <= allowed:
        details.update(decision="not_applicable")
        return False
    base_kind = {"merge_group": "protected_merge_queue_base", "pull_request": "target_merge_base",
                 "push": "push_previous_commit"}.get(os.environ.get("GITHUB_EVENT_NAME"), "caller_comparison_base")
    details.update(comparison_base=base, comparison_base_kind=base_kind)
    metadata = {"commands_hash", "profile_commands_hashes", "reachable_profile_commands_hashes",
                "resource_executor_identity", "resource_equivalent_profiles", "compatible_native_dispatcher",
                "compatible_profile_commands", "verify_execution_plan", "verify_envelope"}
    roots = {"run_command", "selected", "opt_in_test_target_changes", "production_build_paths"}
    reflective = {"globals", "locals", "vars", "dir", "eval", "exec", "getattr", "setattr", "delattr", "__import__", "__builtins__"}
    namespaces = {"__dict__", "__globals__", "modules", "f_globals", "f_locals", "_getframe", "get_objects"}

    def identity(ref):
        row = git("ls-tree", ref, "--", runner).split("\t", 1)[0].split()
        if len(row) != 3 or row[0] not in {"100644", "100755"} or row[1] != "blob":
            raise ValueError("Verifier proof requires a regular committed executor")
        source = git("show", f"{ref}:{runner}")
        tree = ast.parse(source)
        functions = {node.name: node for node in tree.body if isinstance(node, ast.FunctionDef)}
        if len(functions) != sum(isinstance(node, ast.FunctionDef) for node in tree.body):
            raise ValueError("Duplicate executor function declaration")
        if not (metadata | roots) <= functions.keys():
            raise ValueError("Missing known verifier or queue-consumer function")
        symbols = symtable.symtable(source, runner, "exec")
        bound, pending = set(), list(roots)
        while pending:
            name = pending.pop()
            if name in bound:
                continue
            if name in metadata:
                raise ValueError("A queue consumer can observe the changed verifier")
            bound.add(name)
            function = functions[name]
            for node in ast.walk(function):
                if ((isinstance(node, ast.Name) and node.id in reflective)
                        or (isinstance(node, ast.Attribute) and node.attr in reflective | namespaces)):
                    raise ValueError("Queue consumer has dynamic executor dependencies")
            used = referenced_globals(symbols.lookup(name).get_namespace())
            if "RUNNER_CODE" in used:
                raise ValueError("Queue consumer observes the runner code frame")
            pending.extend(used & functions.keys() - bound)
        capture = ast.dump(ast.parse("RUNNER_CODE = sys._getframe().f_code").body[0], include_attributes=False)
        for node in tree.body:
            # The existing managed refresh captures its module code once.
            # Queue consumers cannot read it; keep the exact capture bound.
            if ast.dump(node, include_attributes=False) == capture:
                continue
            if isinstance(node, ast.FunctionDef):
                declaration = [node.args, *node.decorator_list, *([node.returns] if node.returns else []),
                               *getattr(node, "type_params", [])]
                if any((isinstance(item, ast.Name) and item.id in metadata | reflective | {"RUNNER_CODE"})
                       or (isinstance(item, ast.Attribute) and item.attr in reflective | namespaces)
                       for field in declaration for item in ast.walk(field)):
                    raise ValueError("Import-time declaration can observe a changed verifier")
                if node.name in metadata:
                    node.body = [ast.Pass()]
            elif any((isinstance(item, ast.Name) and item.id in metadata | reflective | {"RUNNER_CODE"})
                     or (isinstance(item, ast.Attribute) and item.attr in reflective | namespaces)
                     or (isinstance(item, (ast.ClassDef, ast.AsyncFunctionDef)) and item.name in metadata | roots)
                     for item in ast.walk(node)):
                raise ValueError("Module context observes a changed verifier or namespace")
        return row[0], ast.dump(tree, include_attributes=False), sorted(bound), verifier_layout_digest(source, row[0], bound)

    try:
        previous, current = identity(base), identity(head)
        if previous != current:
            raise ValueError("Code outside verifier bodies or function declarations changed")
        details.update(reviewed_layouts=sorted(REVIEWED_VERIFIER_LAYOUTS), observed_layout=current[3])
        if current[3] not in REVIEWED_VERIFIER_LAYOUTS:
            raise ValueError("Runner imports, declarations or consumers are outside the reviewed layout; full daemon checks continue")
        names = unchanged_profile_execution(base, head)
        details.update(decision="unchanged_queue_executors", verifier_functions=sorted(metadata),
                       consumer_functions=current[2], profiles=names,
                       fingerprint_source="comparison_base", contract_guard_required=True,
                       generic_queue_daemon_smoke_omitted=True)
        return True
    except (ValueError, SyntaxError, TypeError, KeyError, AttributeError, ImportError, OSError,
            StopIteration, subprocess.CalledProcessError) as error:
        details.update(decision="unknown_or_changed", reason=str(error), full_daemon_scope_retained=True)
        return False


def scheduling_only_inputs(paths, base, head, details):
    """Keep unchanged queue executors out of author-scheduling-only acceptance."""
    runner = ".github/scripts/local_acceptance.py"
    allowed = {runner, ".github/local-acceptance/checks.py",
               "e2e/local-acceptance/host_lease_acceptance.py"}
    if runner not in paths or not set(paths) <= allowed:
        details.update(decision="not_applicable")
        return False
    functions = {"host_acceptance_lease", "perform_check"}

    def identity(ref):
        row = git("ls-tree", ref, "--", runner)
        mode, kind, _object = row.split("\t", 1)[0].split()
        if mode not in {"100644", "100755"} or kind != "blob":
            raise ValueError("Scheduling proof requires a regular committed executor")
        tree = ast.parse(git("show", f"{ref}:{runner}"))
        projected, seen = [], set()
        for node in tree.body:
            if isinstance(node, ast.FunctionDef) and node.name == "profile_reservations":
                args = node.args
                if (node.name in seen or node.decorator_list or node.returns
                        or getattr(node, "type_params", []) or args.posonlyargs or args.kwonlyargs
                        or args.vararg or args.kwarg or args.defaults or args.kw_defaults
                        or [argument.arg for argument in args.args] != ["ordered", "pending"]
                        or any(argument.annotation for argument in args.args)):
                    raise ValueError("Unknown reservation declaration or import-time execution")
                seen.add(node.name)
                continue
            if isinstance(node, ast.FunctionDef) and node.name in functions:
                if node.name in seen:
                    raise ValueError("Scheduling function has multiple definitions")
                seen.add(node.name)
                node.body = [ast.Pass()]
            projected.append(node)
        if not functions <= seen:
            raise ValueError("Missing author scheduling functions")
        tree.body = projected
        refresh = ast.dump(ast.parse(
            "exec(compile(current_runner, namespace['__file__'], 'exec'), namespace)").body[0].value,
            include_attributes=False)
        for top in tree.body:
            for node in ast.walk(top):
                if ((isinstance(node, ast.Name) and node.id == "profile_reservations")
                        or (isinstance(node, ast.Constant) and node.value == "profile_reservations")
                        or (isinstance(node, ast.Call) and isinstance(node.func, ast.Name)
                            and node.func.id in {"globals", "locals", "vars", "eval"})):
                    raise ValueError("Code outside scheduling can observe the reservation function")
                if isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and node.func.id == "exec":
                    # Preserve the existing managed start-pr refresh, whose
                    # complete body still compares equal in this projection.
                    if (not isinstance(top, ast.FunctionDef) or top.name != "start_pr"
                            or ast.dump(node, include_attributes=False) != refresh):
                        raise ValueError("Unknown dynamic executor namespace")
        return mode, ast.dump(tree, include_attributes=False)

    try:
        if identity(base) != identity(head):
            raise ValueError("Code outside author scheduling or its declaration changed")
        names = unchanged_profile_execution(base, head)
        details.update(decision="unchanged_queue_executors", profiles=names,
                       author_scheduling_functions=sorted(functions | {"profile_reservations"}),
                       contract_guard_required=True)
        return True
    except (ValueError, SyntaxError, TypeError, KeyError, AttributeError, ImportError, OSError,
            StopIteration, subprocess.CalledProcessError) as error:
        details.update(decision="unknown_or_changed", reason=str(error), full_daemon_scope_retained=True)
        return False


def mailbox_executor_changed(base, head, details=None):
    """Bind reachable nextest functions, retaining unknown module dependencies."""
    path = ".github/local-acceptance/checks.py"
    helpers = {"nextest_binary", "nextest_options"}

    def identity(ref):
        source = git("show", f"{ref}:{path}")
        tree = ast.parse(source)
        functions = {node.name: node for node in tree.body if isinstance(node, ast.FunctionDef)}
        if not helpers <= functions.keys():
            raise ValueError("Missing shared nextest helper")
        pathlib_path = any(isinstance(node, ast.ImportFrom) and node.module == "pathlib"
                           and any(alias.name == "Path" and not alias.asname for alias in node.names)
                           for node in tree.body)
        context_manager = any(isinstance(node, ast.ImportFrom) and node.module == "contextlib"
                              and any(alias.name == "contextmanager" and not alias.asname for alias in node.names)
                              for node in tree.body)
        imports = set()
        rebound = set()
        reflective = {"eval", "exec", "globals", "locals", "vars", "__import__",
                      "getattr", "setattr", "delattr", "import_module"}
        context = []
        for node in tree.body:
            if isinstance(node, ast.FunctionDef):
                if any(not isinstance(decorator, ast.Name) or decorator.id != "contextmanager"
                       or not context_manager
                       for decorator in node.decorator_list):
                    raise ValueError("Unknown import-time decorator execution")
                for default in [*node.args.defaults, *node.args.kw_defaults]:
                    if default is not None:
                        if (pathlib_path and isinstance(default, ast.Call)
                                and isinstance(default.func, ast.Name) and default.func.id == "Path"
                                and len(default.args) == 1 and not default.keywords
                                and isinstance(default.args[0], ast.Constant) and isinstance(default.args[0].value, str)):
                            continue
                        ast.literal_eval(default)
                if node.returns or any(argument.annotation for argument in
                                       [*node.args.posonlyargs, *node.args.args, *node.args.kwonlyargs]):
                    raise ValueError("Unknown import-time annotation execution")
            if isinstance(node, ast.Import):
                imports.update(alias.asname or alias.name.split(".")[0] for alias in node.names)
            elif isinstance(node, ast.ImportFrom):
                imports.update(alias.asname or alias.name for alias in node.names)
                reflective.update(alias.asname or alias.name for alias in node.names
                                  if alias.name in reflective)
            if not isinstance(node, ast.FunctionDef) and not (
                    isinstance(node, ast.Expr) and isinstance(node.value, ast.Constant)
                    and isinstance(node.value.value, str)):
                context.append(ast.dump(node, include_attributes=False))
                rebound.update(item.id for item in ast.walk(node)
                               if isinstance(item, ast.Name) and isinstance(item.ctx, ast.Store))
                rebound.update(item.name for item in ast.walk(node)
                               if isinstance(item, (ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)))
        if {"Path", "contextmanager"} & (functions.keys() | rebound):
            raise ValueError("Rebound import-time constructor or decorator")
        symbols = symtable.symtable(source, path, "exec")
        required = set(helpers)
        bound = {}
        while required:
            name = required.pop()
            if name in bound:
                continue
            function = functions[name]
            if name in imports | rebound:
                raise ValueError("Shared function has another module binding")
            if function.decorator_list or function.args.defaults or function.args.kw_defaults:
                raise ValueError("Unknown helper decorator/default execution")
            for node in ast.walk(function):
                if (isinstance(node, ast.Call)
                        and ((isinstance(node.func, ast.Name) and node.func.id in reflective)
                             or (isinstance(node.func, ast.Attribute) and node.func.attr in reflective))):
                    raise ValueError("Shared helper resolves dynamic dependencies")
                if isinstance(node, ast.Attribute) and node.attr in {"__dict__", "__globals__", "f_globals", "f_locals", "_getframe"}:
                    raise ValueError("Shared helper inspects a runtime namespace")
            bound[name] = ast.dump(function, include_attributes=False)
            table = symbols.lookup(name).get_namespace()
            globals_used = referenced_globals(table)
            for used in globals_used:
                if used in rebound:
                    raise ValueError("Shared helper depends on a rebound module global")
                if used in functions:
                    required.add(used)
                elif used not in imports | set(dir(builtins)):
                    raise ValueError("Shared helper depends on an unknown module global")
        return context, bound

    try:
        before, after = identity(base), identity(head)
        changed = before != after
        if details is not None:
            details.update(decision="changed" if changed else "unchanged", bound_functions=sorted(after[1]))
        return changed
    except (ValueError, SyntaxError, TypeError, KeyError, AttributeError, subprocess.CalledProcessError) as error:
        if details is not None:
            details.update(decision="unknown", reason=str(error), full_a2a_required=True)
        return True


def independent_cli_sources(*refs):
    """Require committed binary-only layouts and no declared CLI dependents."""
    try:
        import tomllib
    except ImportError:
        return False
    name = "rusty-sudocode-cli"

    def imports_cli(value):
        if isinstance(value, dict):
            for key, child in value.items():
                if key in {"dependencies", "dev-dependencies", "build-dependencies"} and isinstance(child, dict):
                    for dependency, settings in child.items():
                        if dependency.replace("_", "-") == name:
                            return True
                        if isinstance(settings, dict) and (
                                settings.get("package", "").replace("_", "-") == name
                                or (isinstance(settings.get("path"), str) and
                                    posixpath.basename(posixpath.normpath(settings["path"].replace("\\", "/"))) == name)):
                            return True
                if imports_cli(child):
                    return True
        elif isinstance(value, list):
            return any(imports_cli(child) for child in value)
        return False

    try:
        manifest = f"rust/crates/{name}/Cargo.toml"
        for ref in refs:
            workspace = tomllib.loads(git("show", f"{ref}:rust/Cargo.toml"))
            if workspace.get("workspace", {}).get("members") != ["crates/*"]:
                return False
            package = tomllib.loads(git("show", f"{ref}:{manifest}"))
            if package.get("package", {}).get("name") != name or "lib" in package:
                return False
            if git("ls-tree", "--name-only", ref, "--", f"rust/crates/{name}/src/lib.rs"):
                return False
            matches = git("grep", "--no-color", "--full-name", "-l", "-F", "-e", name,
                          "-e", name.replace("-", "_"), ref,
                          "--", ":(glob)rust/**/Cargo.toml").splitlines()
            for match in matches:
                prefix, path = match.split(":", 1)
                if prefix != ref:
                    return False
                if imports_cli(tomllib.loads(git("show", f"{ref}:{path}"))):
                    return False
        return True
    except (ValueError, TypeError, AttributeError, subprocess.CalledProcessError):
        return False


def main():
    nextest_scope = {}
    scheduling_scope = {}
    metadata_scope = {}
    ignored_product_inputs = []
    ignored_cohost_cli_sources = []
    ignored_disabled_test_sources = []
    # The workflow changes real compiler/daemon commands; this selector does
    # not. Its own edits retain preflight and the complete contract guard.
    # The toolchain pin and its installer change the compiler of every job.
    queue_inputs = {".github/workflows/rust-ci.yml", "rust-toolchain.toml",
                    ".github/actions/rust-toolchain/action.yml"}
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text(encoding="utf-8"))
    name = os.environ["GITHUB_EVENT_NAME"]
    if name == "workflow_dispatch":
        paths, live, daemon, a2a, cohost, clippy = [], True, True, True, True, True
    else:
        if name == "push":
            base, head = event["before"], event["after"]
        elif name == "pull_request":
            base, head = event["pull_request"]["base"]["sha"], event["pull_request"]["head"]["sha"]
        elif name == "merge_group":
            base, head = event["merge_group"]["base_sha"], event["merge_group"]["head_sha"]
        else:
            raise RuntimeError(f"Unsupported CI event: {name}")
        if not all(re.fullmatch(r"[a-f0-9]{40}", ref) for ref in [base, head]):
            raise RuntimeError("Invalid CI commit identity")
        if base == "0" * 40:
            paths, live, daemon, a2a, cohost, clippy = [], True, True, True, True, True
        else:
            for ref in [base, head]:
                if subprocess.run(["git", "cat-file", "-e", ref + "^{commit}"], capture_output=True).returncode:
                    subprocess.run(["git", "fetch", "--no-tags", "origin", ref], check=True)
            if name == "pull_request":
                base = git("merge-base", base, head)
            paths = git("diff", "--name-only", "--no-renames", base, head).splitlines()
            relevant = [path for path in paths if not path.lower().endswith((".md", ".html"))
                        or (path.startswith("rust/") and Path(path).name.lower() != "readme.md")]
            live_inputs = ["rust/**", "spec/**", "e2e/thinking/**", ".github/scripts/probe_live_usage*",
                           ".github/scripts/live_model_evidence.cjs", ".github/scripts/model_compat.py"]
            daemon_inputs = ["rust/**", "e2e/nexus-a2a/**", "e2e/shared-services/**", ".github/ci/**",
                             ".github/local-acceptance/parallel_guard.py", ".github/scripts/local_acceptance.py",
                             ".github/scripts/vfs_search_cache.py", ".github/actions/vfs-search-cache/**",
                             ".github/workflows/ci-vfs-search-cache.yml", *queue_inputs]
            # Acceptance metadata changes execution authorization, not provider
            # requests. Keep these identities with the acceptance authority.
            live = any(fnmatch.fnmatchcase(path, rule)
                       for path in relevant if path not in {POLICY, SIGNERS}
                       for rule in live_inputs)
            # Public specs can be compiled into Rust just like source files.
            # The native policy owns their scope and its metadata exclusions;
            # reuse that decision for the actual integrated queue tree.
            spec_paths = [path for path in relevant if path.startswith("spec/")]
            policy = json.loads(git("show", f"{head}:{POLICY}")) if spec_paths else None
            native_specs = [path for path in spec_paths
                            if "native-workspace" in selected(policy, [path])]
            # Test modules remain part of all-feature linting even
            # when they do not rebuild or exercise the daemon binary.
            # Workflow edits still exercise the real workspace/platforms.
            clippy = bool(native_specs) or any(
                path.startswith("rust/") or path in queue_inputs | {".github/scripts/ci_clippy.py"}
                for path in relevant)
            # CLI integration modules are not inputs to the daemon/cohost
            # binaries or shell journeys. Native PTYs and live model scope
            # remain unchanged; manifests, src and moved source still qualify.
            opt_in_manifests, opt_in_sources = opt_in_test_target_changes(relevant, base, head)
            normal_inputs = production_build_paths(relevant, base, head, opt_in=opt_in_manifests)
            ignored_product_inputs = [path for path in relevant if path not in normal_inputs]
            # A newly declared test=false target cannot be selected by the
            # named daemon journeys. Reuse the same committed proof as its
            # manifest; existing/ordinary test sources keep their full scope.
            ignored_disabled_test_sources = sorted(set(normal_inputs) & opt_in_sources)
            scheduling_only = scheduling_only_inputs(relevant, base, head, scheduling_scope)
            metadata_only = metadata_only_inputs(relevant, base, head, metadata_scope)
            daemon_paths = [path for path in normal_inputs
                            if not path.startswith("rust/crates/rusty-sudocode-cli/tests/")
                            and path not in opt_in_sources
                            and not ((scheduling_only or metadata_only) and path == ".github/scripts/local_acceptance.py")
                            and (path in native_specs
                                 or any(fnmatch.fnmatchcase(path, rule) for rule in daemon_inputs))]
            executor_path = ".github/local-acceptance/checks.py"
            if executor_path in normal_inputs and mailbox_executor_changed(base, head, nextest_scope):
                daemon_paths.append(executor_path)
            # These helpers only prepare the separate fixed search driver.
            # They cannot change the CLI, its pinned daemon or A2A harness.
            a2a = any(path not in {".github/scripts/vfs_search_cache.py",
                                  ".github/workflows/ci-vfs-search-cache.yml"}
                      and not path.startswith(".github/actions/vfs-search-cache/")
                      for path in daemon_paths)
            # The CLI package has only a binary; the co-host and VFS search
            # fixture do not import its source. Keep the actual CLI daemon
            # journeys above, and retain all shared/build/harness inputs here.
            cli_sources = [path for path in normal_inputs if path.startswith("rust/crates/rusty-sudocode-cli/src/")]
            if cli_sources and independent_cli_sources(base, head):
                ignored_cohost_cli_sources = cli_sources
            cohost = any(path not in ignored_cohost_cli_sources and path != executor_path for path in daemon_paths)
            daemon = a2a or cohost
    executor_inputs = {".github/local-acceptance/checks.py", ".github/local-acceptance/Dockerfile",
                       ".github/scripts/local_acceptance.py", ".github/scripts/ci_input_scope.py",
                       ".github/workflows/rust-ci.yml"}
    benchmark = any(path in executor_inputs | {"e2e/local-acceptance/benchmark_targets_acceptance.py",
                                              "e2e/terminal-resize/benchmark.py",
                                              "e2e/terminal-resize/performance-policy.json",
                                              "e2e/terminal-resize/gui-benchmark.py",
                                              "e2e/terminal-resize/gui-policy.json"}
                    or path.startswith("rust/crates/scode-benchmark-drivers/")
                    for path in paths)
    container = any(path in executor_inputs | {"e2e/local-acceptance/container_inputs_acceptance.py"}
                    for path in paths)
    release_artifact = any(path in executor_inputs | {".github/workflows/release.yml",
        ".github/scripts/release_artifact_acceptance.py", "e2e/local-acceptance/release_artifact_acceptance.py",
        "rust/crates/rusty-sudocode-cli/build.rs"} or path.startswith(".github/actions/release-artifact/")
        for path in paths)
    clippy_executor = any(path in {".github/workflows/rust-ci.yml", ".github/scripts/ci_input_scope.py",
                                  ".github/scripts/ci_clippy.py",
                                  "e2e/local-acceptance/clippy_fail_fast.py",
                                  ".github/workflows/ci-cancel-failed-run.yml",
                                  "e2e/local-acceptance/failed_ci_live.py"} for path in paths)
    gzip_codec = any(path.startswith("e2e/response-gzip/")
                     or path in executor_inputs | {"rust/crates/runtime/src/response_archive.rs",
                                                   "rust/crates/runtime/src/response_archive/gzip.rs"}
                     for path in paths)
    vfs_cache = any(path in {".github/scripts/vfs_search_cache.py", ".github/workflows/rust-ci.yml",
                             ".github/workflows/ci-vfs-search-cache.yml", ".github/scripts/ci_input_scope.py",
                             "e2e/local-acceptance/vfs_search_cache_acceptance.py"}
                    or path.startswith(".github/actions/vfs-search-cache/") for path in paths)
    result = {"live_required": str(live).lower(), "daemon_required": str(daemon).lower(),
              "a2a_required": str(a2a).lower(),
              "cohost_required": str(cohost).lower(),
              "benchmark_required": str(benchmark).lower(), "container_required": str(container).lower(),
              "clippy_required": str(clippy).lower(), "clippy_executor_required": str(clippy_executor).lower(),
              "gzip_codec_required": str(gzip_codec).lower(),
              "release_artifact_required": str(release_artifact).lower()}
    result["vfs_cache_required"] = str(vfs_cache).lower()
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
        for key, value in result.items():
            stream.write(f"{key}={value}\n")
    print(json.dumps({**result, "changed_paths": paths, "normal_product_ignored": ignored_product_inputs,
                      "daemon_disabled_test_sources_ignored": ignored_disabled_test_sources,
                      "cohost_cli_source_ignored": ignored_cohost_cli_sources, "nextest_dependency_scope": nextest_scope,
                      "acceptance_scheduling_scope": scheduling_scope,
                      "acceptance_metadata_scope": metadata_scope,
                      "selector_only_product_checks_skipped": ".github/scripts/ci_input_scope.py" in paths
                          and not daemon and not clippy}))


if __name__ == "__main__":
    main()
