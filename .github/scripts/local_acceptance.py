#!/usr/bin/env python3
"""Run commit-bound local acceptance and verify its SSH-signed Git note.

The signature authenticates an enrolled developer's attestation. It does not
make an untrusted developer machine a remotely attested execution environment.
No candidate code is executed by the GitHub verification job.
"""

import argparse
import ast
import base64
import contextlib
import copy
import errno
import fnmatch
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import re
import secrets
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
try:
    import tomllib
except ImportError:
    tomllib = None
import urllib.error
import urllib.parse


NOTE_REF = "refs/notes/local-acceptance"
PLAN_NOTE_REF = "refs/notes/local-acceptance-plan"
POLICY = "spec/local_acceptance.json"
SIGNERS = "spec/local_acceptance_signers.json"
NAMESPACE = "sudocode-local-acceptance"
AUTHORIZATION_CONTEXT = "Acceptance contract authorization"
CONTRACT_UPDATE_COMMAND = ('python "$(git rev-parse --path-format=absolute --git-common-dir)/'
                           'local-acceptance-hooks/local_acceptance.py" contract-update --remote origin')
HOST_LEASE_ENV = "SUDOCODE_ACCEPTANCE_HOST_LEASE"
# Carried origins verify recursively to at most this depth; a deeper origin is
# not carried, so its profiles run again.
MAX_CARRY_DEPTH = 32
# Earlier envelopes one receipt references may total at most this many
# canonical bytes, keeping the note well under read_note's 2 MB cap. A carry
# that would exceed it runs its profile again instead.
CARRY_STORE_BUDGET = 1_000_000
ROOT = Path(__file__).resolve().parents[2]
MSVC_ENV = None
RUNNER_CODE = sys._getframe().f_code


def git(*args, root=None, data=None, check=True, env=None):
    result = subprocess.run(["git", *args], cwd=root or ROOT, input=data, env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if check and result.returncode:
        raise RuntimeError(result.stderr.decode(errors="replace").strip())
    return result


def value(*args, root=None):
    return git(*args, root=root).stdout.decode().strip()


# Git's repository selectors, mirroring `git rev-parse --local-env-vars`
# without requiring Git in minimal execution containers. The native guard
# injects the installed Git's full list, so an added selector requires an
# explicit update. Git exports them to a hook for the checkout running it.
GIT_LOCAL_ENV = frozenset({
    "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_CONFIG", "GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY", "GIT_DIR", "GIT_WORK_TREE", "GIT_IMPLICIT_WORK_TREE", "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE", "GIT_NO_REPLACE_OBJECTS", "GIT_REPLACE_REF_BASE", "GIT_PREFIX", "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR"})


def without_git_selectors(environment):
    return {key: setting for key, setting in environment.items() if key not in GIT_LOCAL_ENV
            and not key.startswith(("GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"))}


def canonical(obj):
    return json.dumps(obj, sort_keys=True, separators=(",", ":")).encode()


def sha(data):
    return hashlib.sha256(data).hexdigest()


def tracked(ref, path):
    # Explicitly end revision arguments: Git for Windows otherwise probes this
    # object name as a filesystem path and can reject deep evidence directories.
    return git("show", f"{ref}:{path}", "--").stdout


def load_policy(ref):
    return json.loads(tracked(ref, POLICY))


def committed_blob_bytes(blobs):
    """Read immutable Git objects together, preserving exact binary contents."""
    if not blobs:
        return {}
    contents = git("cat-file", "--batch", data=("\n".join(blobs.values()) + "\n").encode("ascii")).stdout
    documents, position = {}, 0
    for path, identity in blobs.items():
        end = contents.index(b"\n", position)
        header = contents[position:end].decode("ascii").split()
        if len(header) != 3 or header[:2] != [identity, "blob"]:
            raise ValueError("Unexpected committed metadata object")
        size = int(header[2])
        position = end + 1
        if size < 0 or contents[position + size:position + size + 1] != b"\n":
            raise ValueError("Incomplete committed metadata object")
        documents[path] = contents[position:position + size]
        position += size + 1
    if position != len(contents):
        raise ValueError("Unexpected trailing committed metadata")
    return documents


def committed_files(ref, paths, *extra):
    # Resolve the complete tree once, then read by immutable object ID. Mutable
    # refs are never memoized and archive attributes cannot omit protected data.
    rows = git("ls-tree", "-r", "-z", ref, "--", *paths, *extra).stdout
    blobs = {}
    for row in rows.split(b"\0"):
        if not row:
            continue
        info, path = row.split(b"\t", 1)
        _mode, kind, identity = info.decode("ascii").split()
        if kind != "blob":
            raise RuntimeError("Acceptance contract contains a non-blob Git entry")
        blobs[path.decode("utf-8")] = identity
    missing = sorted(set(paths) - blobs.keys())
    if missing:
        raise RuntimeError(f"Acceptance contract is missing committed files: {missing}")
    return committed_blob_bytes(blobs)


def contract_hash(ref):
    paths = [POLICY, SIGNERS, ".github/scripts/local_acceptance.py", ".github/workflows/local-acceptance.yml",
             ".github/workflows/local-acceptance-publish.yml"]
    # The periodic workflow did not exist in older authentic plans. Bind it
    # when present without changing those original contract hashes.
    files = committed_files(ref, paths, ".github/local-acceptance", ".github/workflows/local-acceptance-workspace.yml")
    return sha(canonical({path: sha(data) for path, data in sorted(files.items())}))


def run_command_source(source, *, function=None):
    if function is None:
        function = next(node for node in ast.parse(source).body
                        if isinstance(node, ast.FunctionDef) and node.name == "run_command")
    return "\n".join(source.splitlines()[function.lineno - 1:function.end_lineno])


def command_git_selector_inputs(module, function):
    """Bind the extracted environment filter without rewriting existing signed hashes."""
    names = {"without_git_selectors", "GIT_LOCAL_ENV", "frozenset"}
    if not any(isinstance(node, ast.Name) and node.id == "without_git_selectors"
               for node in ast.walk(function)):
        return None
    bindings = []
    for statement in module.body:
        if isinstance(statement, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            if statement.name in names:
                bindings.append(statement)
        elif any((isinstance(node, ast.Name) and node.id in names)
                 or (isinstance(node, ast.alias) and (node.asname or node.name.split(".")[0]) in names)
                 for node in ast.walk(statement)):
            bindings.append(statement)
    return sha(canonical(profile_ast_value(bindings)))


def compatible_git_selector_inputs(before, current):
    """Allow the reviewed extraction only; keep every later input change bound."""
    if before is None:
        return current[3] is None
    if before[3] == current[3]:
        return True
    # Before #1516 the literal names and filter lived in run_command.
    # This one-way bridge proves the filter shape only. Commands with
    # different fingerprints still need exact per-profile approval.
    return (before[3] is None
            and before[0] == "6f987d75cd9e3472946c39861729d8b503905b9db3845027dd884395f0ebd51d"
            and current[0] == "ab7ac34c8cbdf5f87025d92f345993c07334c4ae2c33deabdac187a7b1556323"
            and current[3] == "3b5bed1200aa1b8b1b5d5064a647f1851bd5d1d4bb3c7143b32dc69739b8e7a5")


def execution_inputs(ref, *, executor_identity=None):
    checks = ".github/local-acceptance/checks.py"
    image = ".github/local-acceptance/Dockerfile"
    executor = ".github/scripts/local_acceptance.py"
    paths = [checks, image] if executor_identity is not None else [checks, image, executor]
    files = committed_files(ref, paths)
    selector_inputs = None
    if executor_identity is None:
        source = files[executor].decode("utf-8")
        module = ast.parse(source)
        function = next(node for node in module.body
                        if isinstance(node, ast.FunctionDef) and node.name == "run_command")
        executor_identity = run_command_source(source, function=function)
        selector_inputs = command_git_selector_inputs(module, function)
    return sha(executor_identity.encode()), files[checks], sha(files[image]), selector_inputs


def commands_hash(ref, *, inputs=None):
    # Scheduling, publication and selection edits do not change a completed
    # command's evidence. Bind actual executors/images, then require the current
    # selected check set to be covered. New commands still require a real run.
    executor, checks, image, _ = execution_inputs(ref) if inputs is None else inputs
    return sha(canonical({"executor": executor, ".github/local-acceptance/checks.py": sha(checks),
                          ".github/local-acceptance/Dockerfile": image}))


def referenced_globals(table):
    """Use Python's scope rules, including nested/comprehension scopes."""
    result, pending = set(), [table]
    while pending:
        current = pending.pop()
        result.update(symbol.get_name() for symbol in current.get_symbols() if symbol.is_global())
        pending.extend(current.get_children())
    return result


def profile_ast_value(value):
    """Serialize semantic AST fields independently of Python's dump formatting."""
    if isinstance(value, ast.AST):
        return {"node": type(value).__name__, "fields": {
            name: profile_ast_value(field) for name, field in ast.iter_fields(value)
            if field is not None and not (isinstance(field, list) and not field)
        }}
    if isinstance(value, list):
        return [profile_ast_value(item) for item in value]
    if isinstance(value, bytes):
        return {"bytes": value.hex()}
    if isinstance(value, complex):
        return {"complex": [value.real, value.imag]}
    if value is Ellipsis:
        return {"ellipsis": True}
    if value is None or isinstance(value, (str, int, float, bool)):
        return value
    raise RuntimeError(f"Unsupported acceptance AST value: {type(value).__name__}")


def copy_profile_module(module, dispatcher):
    """Share read-only statements while projecting a fresh dispatcher body."""
    projected = copy.copy(module)
    dispatch = copy.copy(dispatcher)
    projected.body = [dispatch if node is dispatcher else node for node in module.body]
    return projected, dispatch


def profile_module_value(module, statements):
    """Keep the existing JSON shape without serializing shared statements again."""
    def statement_value(statement):
        if statement not in statements:
            statements[statement] = profile_ast_value(statement)
        return statements[statement]

    return {"node": type(module).__name__, "fields": {
        name: ([statement_value(statement) for statement in field]
               if name == "body" else profile_ast_value(field))
        for name, field in ast.iter_fields(module)
        if field is not None and not (isinstance(field, list) and not field)
    }}


def profile_dispatcher(module):
    dispatcher = next(node for node in module.body if isinstance(node, ast.FunctionDef) and node.name == "run")
    parameter = dispatcher.args.args[0].arg
    # Reassigning the profile parameter can make dispatch branches overlap.
    reassigned = any(isinstance(node, ast.Name) and node.id == parameter and isinstance(node.ctx, ast.Store)
                     for node in ast.walk(dispatcher))
    return dispatcher, parameter, reassigned


def profile_commands_hashes(ref, names, *, inputs=None):
    """Bind each literal dispatch branch and all common code without importing it."""
    executor, checks, image, _ = execution_inputs(ref) if inputs is None else inputs
    module = ast.parse(checks)
    names = tuple(names)
    if not names:
        return {}
    original_dispatcher, parameter, reassigned = profile_dispatcher(module)
    statements = {}
    result = {}
    for name in names:
        projected, dispatcher = copy_profile_module(module, original_dispatcher)

        def project(node):
            test = node.test if isinstance(node, ast.If) else None
            if (not isinstance(test, ast.Compare) or not isinstance(test.left, ast.Name)
                    or test.left.id != parameter or len(test.ops) != 1 or not isinstance(test.ops[0], ast.Eq)
                    or len(test.comparators) != 1 or not isinstance(test.comparators[0], ast.Constant)
                    or not isinstance(test.comparators[0].value, str)):
                return [node]
            tail = project(node.orelse[0]) if len(node.orelse) == 1 else node.orelse
            if test.comparators[0].value != name:
                return tail
            selected = copy.copy(node)
            selected.orelse = tail
            return [selected]

        if not reassigned:
            dispatcher.body = [child for node in dispatcher.body for child in project(node)]
        result[name] = sha(canonical({"version": 1, "executor": executor, "image": image,
                                     "profile": profile_module_value(projected, statements)}))
    return result


def reachable_profile_commands_hashes(ref, names, *, executor_identity=None, inputs=None):
    """Keep import effects and bind helpers reachable from each selected dispatch."""
    executor, checks, image, _ = (execution_inputs(ref, executor_identity=executor_identity)
                               if inputs is None else inputs)
    module = ast.parse(checks)
    names = tuple(names)
    if not names:
        return {}
    original_dispatcher, parameter, reassigned = profile_dispatcher(module)
    statements, references = {}, {}
    result = {}
    # Reflection can find a function without a static Name reference. Retain
    # the complete module rather than guessing which code it might execute.
    reflection_names = {"globals", "locals", "vars", "dir", "eval", "exec", "getattr", "__import__", "__builtins__"}
    reflected = any((isinstance(node, ast.Name) and node.id in reflection_names)
                    or (isinstance(node, ast.Attribute) and node.attr in reflection_names
                        | {"__dict__", "__globals__", "modules", "f_globals", "f_locals", "_getframe", "get_objects"})
                    for node in ast.walk(module))
    for name in names:
        projected, dispatcher = copy_profile_module(module, original_dispatcher)

        def project(node):
            test = node.test if isinstance(node, ast.If) else None
            if (not isinstance(test, ast.Compare) or not isinstance(test.left, ast.Name)
                    or test.left.id != parameter or len(test.ops) != 1 or len(test.comparators) != 1):
                return [node]
            expected = test.comparators[0]
            if isinstance(expected, ast.Constant) and isinstance(expected.value, str):
                values = [expected.value]
            elif isinstance(expected, (ast.Set, ast.List, ast.Tuple)) and all(
                    isinstance(item, ast.Constant) and isinstance(item.value, str) for item in expected.elts):
                values = [item.value for item in expected.elts]
            else:
                return [node]
            operation = test.ops[0]
            if isinstance(operation, (ast.Eq, ast.NotEq)) and isinstance(expected, ast.Constant):
                matched = name == expected.value
            elif isinstance(operation, (ast.In, ast.NotIn)) and not isinstance(expected, ast.Constant):
                matched = name in values
            else:
                return [node]
            if isinstance(operation, (ast.NotEq, ast.NotIn)):
                matched = not matched
            return [child for item in (node.body if matched else node.orelse) for child in project(item)]

        if not reflected and not reassigned:
            dispatcher.body = [child for node in dispatcher.body for child in project(node)]
            functions = {node.name: node for node in projected.body if isinstance(node, ast.FunctionDef)}
            # Decorators/default expressions can execute at import time. Keep
            # those definitions even if the selected branch does not call them.
            roots = [node for node in projected.body if not isinstance(node, ast.FunctionDef)
                     or node.name == "run" or node.decorator_list or node.args.defaults or node.args.kw_defaults
                     or node.returns is not None or any(argument.annotation is not None for argument in (
                         node.args.posonlyargs + node.args.args + node.args.kwonlyargs
                         + [argument for argument in [node.args.vararg, node.args.kwarg] if argument]))
                     or getattr(node, "type_params", [])]
            pending = roots[:]
            retained = {node.name for node in roots if isinstance(node, ast.FunctionDef)}
            while pending:
                definition = pending.pop()
                if definition not in references:
                    references[definition] = {node.id for node in ast.walk(definition)
                                              if isinstance(node, ast.Name) and isinstance(node.ctx, ast.Load)}
                for reference in references[definition] & functions.keys() - retained:
                    retained.add(reference)
                    pending.append(functions[reference])
            projected.body = [node for node in projected.body
                              if not isinstance(node, ast.FunctionDef) or node.name in retained]
        result[name] = sha(canonical({"version": 2, "executor": executor, "image": image,
                                     "profile": profile_module_value(projected, statements)}))
    return result


def attested_commands_ref(receipt, read_inputs):
    """Find immutable source that matches the signed complete execution hash."""
    candidates = ([receipt["commands_ref"]] if "commands_ref" in receipt
                  else [receipt["head"], receipt["base"]])
    for candidate in candidates:
        if not isinstance(candidate, str) or not re.fullmatch(r"[a-f0-9]{40}", candidate):
            continue
        try:
            if receipt.get("commands_sha256") == commands_hash(candidate, inputs=read_inputs(candidate)):
                return candidate
        except (RuntimeError, subprocess.CalledProcessError, StopIteration):
            continue
    return None


def committed_test_groups(head):
    """Conservatively inspect committed group files/declarations without running Cargo."""
    if tomllib is None:
        return True
    try:
        paths = value("ls-tree", "-r", "--name-only", head, "rust").splitlines()
        if any("/tests/acceptance_groups/" in path.lower() for path in paths):
            return True
        manifests = [path for path in paths if path.endswith("/Cargo.toml")]
        if not manifests:
            return False
        bundle = git("archive", "--format=tar", head, "--", *manifests).stdout
        seen = set()
        with tarfile.open(fileobj=io.BytesIO(bundle), mode="r:") as archive:
            for entry in archive:
                if entry.isdir():
                    continue
                if entry.name not in manifests or not entry.isfile():
                    return True
                seen.add(entry.name)
                manifest = tomllib.loads(archive.extractfile(entry).read().decode("utf-8"))
                targets = manifest.get("test", [])
                if not isinstance(targets, list):
                    return True
                for target in targets:
                    path = target.get("path", "")
                    if not isinstance(path, str) or "acceptance_groups" in path.lower():
                        return True
                members = manifest.get("workspace", {}).get("members", [])
                if not isinstance(members, list):
                    return True
                for member in members:
                    if not isinstance(member, str) or Path(member).is_absolute() or ".." in Path(member).parts:
                        return True
        return seen != set(manifests)
    except (RuntimeError, KeyError, TypeError, ValueError, AttributeError, OSError, tarfile.TarError):
        return True


def resource_executor_identity(ref):
    """Normalize only known compiler budgets and command-completion waits."""
    import symtable

    path = ".github/scripts/local_acceptance.py"
    row = value("ls-tree", ref, "--", path).split("\t", 1)[0].split()
    if len(row) != 3 or row[0] not in {"100644", "100755"} or row[1] != "blob":
        raise ValueError("Compiler resource identity requires a regular committed executor")
    source = tracked(ref, path).decode("utf-8")
    tree = ast.parse(source)
    functions = [node for node in tree.body if isinstance(node, ast.FunctionDef)]
    reserved = {"os", "psutil", "time", "subprocess", "max", "min", "str", "int", "RuntimeError"}
    if any(node.name in reserved for node in functions):
        raise ValueError("Compiler budget helper is shadowed by a module function")
    if len({node.name for node in functions}) != len(functions):
        raise ValueError("Duplicate executor function binding")
    # New declarations must have no unknown import-time effects. The existing
    # stdlib contextmanager wraps a function without running its body.
    decorator = profile_ast_value(ast.parse("contextlib.contextmanager", mode="eval").body)
    for function in functions:
        if (any(profile_ast_value(node) != decorator for node in function.decorator_list)
                or function.returns or getattr(function, "type_params", [])
                or any(arg.annotation for arg in [*function.args.posonlyargs, *function.args.args,
                    *function.args.kwonlyargs, *[arg for arg in [function.args.vararg, function.args.kwarg] if arg]])):
            raise ValueError("Unknown executor import-time declaration")
        for default in [*function.args.defaults, *function.args.kw_defaults]:
            if default is not None and not isinstance(default, ast.Name):
                ast.literal_eval(default)
    contexts = [node for node in tree.body if not isinstance(node, ast.FunctionDef)]
    for statement in contexts:
        if any(isinstance(node, ast.Name) and node.id in reserved and isinstance(node.ctx, ast.Store)
               for node in ast.walk(statement)):
            raise ValueError("Compiler budget namespace has a module assignment")
        if isinstance(statement, (ast.Import, ast.ImportFrom)):
            for alias in statement.names:
                binding = alias.asname or alias.name.split(".")[0]
                if alias.name == "*" or (binding in reserved and not (
                        isinstance(statement, ast.Import) and alias.name in {"os", "time", "subprocess"}
                        and alias.asname is None)):
                    raise ValueError("Compiler budget namespace has an unknown import")
    imports = [node for node in contexts if isinstance(node, ast.Import)
               and any(alias.name == "os" and alias.asname is None for alias in node.names)]
    if len(imports) != 1 or any(isinstance(node, ast.Name) and node.id == "os" and isinstance(node.ctx, ast.Store)
                                for statement in contexts for node in ast.walk(statement)):
        raise ValueError("Unknown operating-system import binding")
    function = copy.deepcopy(next(node for node in functions if node.name == "run_command"))
    for node in ast.walk(function):
        if ((isinstance(node, ast.Name) and node.id in reserved and isinstance(node.ctx, ast.Store))
                or (isinstance(node, (ast.FunctionDef, ast.ClassDef)) and node.name in reserved)
                or (isinstance(node, (ast.Global, ast.Nonlocal)) and set(node.names) & reserved)):
            raise ValueError("Compiler budget namespace has a local assignment")
        if isinstance(node, (ast.Import, ast.ImportFrom)):
            for alias in node.names:
                binding = alias.asname or alias.name.split(".")[0]
                if alias.name == "*" or (binding in reserved and not (
                        isinstance(node, ast.Import) and alias.name == "psutil" and alias.asname is None)):
                    raise ValueError("Compiler budget namespace has an unknown local import")
    updates = [node for node in function.body
               if isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)
               and profile_ast_value(node.value.func) == profile_ast_value(ast.parse("environment.update", mode="eval").body)
               and len(node.value.args) == 1 and isinstance(node.value.args[0], ast.Dict)
               and any(isinstance(key, ast.Constant) and key.value == "CARGO_BUILD_JOBS" for key in node.value.args[0].keys)]
    if len(updates) != 1:
        raise ValueError("Unknown compiler environment declaration")
    statement = updates[0]
    settings = statement.value.args[0]
    index = next(index for index, key in enumerate(settings.keys)
                 if isinstance(key, ast.Constant) and key.value == "CARGO_BUILD_JOBS")
    selected = settings.values[index]
    if selected.lineno != selected.end_lineno:
        raise ValueError("Unknown multiline compiler environment field")
    lines = source.splitlines(keepends=True)[function.lineno - 1:function.end_lineno]
    line = selected.lineno - function.lineno
    lines[line] = lines[line][:selected.col_offset] + '"1"' + lines[line][selected.end_col_offset:]
    budget_form = "serial"
    if profile_ast_value(selected) == profile_ast_value(ast.parse("str(jobs)", mode="eval").body):
        budget_form = "adaptive"
        budget = ast.parse('''
jobs = max(1, min((os.cpu_count() or 1) // 2, psutil.virtual_memory().total // (3 * 1024 ** 3)))
cap = os.environ.get("SCODE_ACCEPTANCE_BUILD_JOBS")
if cap is not None:
    if not cap.isdigit() or int(cap) < 1:
        raise RuntimeError("SCODE_ACCEPTANCE_BUILD_JOBS must be a positive integer")
    jobs = min(jobs, int(cap))
''').body
        at = function.body.index(statement)
        if at < 3 or profile_ast_value(function.body[at - 3:at]) != profile_ast_value(budget):
            raise ValueError("Unknown compiler budget calculation or effects")
        del lines[function.body[at - 3].lineno - function.lineno:function.body[at - 1].end_lineno - function.lineno + 1]
        del function.body[at - 3:at]
        settings.values[index] = ast.Constant(value="1")
    elif not isinstance(selected, ast.Constant) or selected.value != "1":
        raise ValueError("Unknown serial compiler budget")
    if any(isinstance(node, ast.Name) and node.id in {"jobs", "cap"} for node in ast.walk(function)):
        raise ValueError("Compiler budget is observed outside its environment field")
    text = "".join(lines)
    budget_comment = """    # Compiler children run at BelowNormal priority (below), so foreground work
    # keeps the CPU it needs; a serial build only lengthened every queued
    # acceptance. Use half the logical CPUs and at most one job per 3 GiB of
    # memory; SCODE_ACCEPTANCE_BUILD_JOBS lowers it and never raises it. An
    # explicit env (hosted runners) still wins below.
"""
    if text.count(budget_comment) > 1:
        raise ValueError("Duplicate compiler budget explanation")
    text = text.replace(budget_comment, "", 1)
    poll = ast.parse("process.poll() is None", mode="eval").body
    loops = [node for node in ast.walk(function)
             if isinstance(node, ast.While) and profile_ast_value(node.test) == profile_ast_value(poll)]
    if len(loops) != 1 or len(loops[0].body) != 2:
        raise ValueError("Unknown command-completion supervision loop")
    sleep = ast.parse("time.sleep(0.15)").body[0]
    notify = ast.parse("""try:
    process.wait(timeout=0.15)
except subprocess.TimeoutExpired:
    pass
""").body[0]
    wait = loops[0].body[-1]
    wait_form = "sleep"
    if profile_ast_value(wait) == profile_ast_value(notify):
        wait_form = "completion"
        original = "\n".join(source.splitlines()[wait.lineno - 1:wait.end_lineno])
        if text.count(original) != 1:
            raise ValueError("Unknown command-completion wait source")
        text = text.replace(original, "                time.sleep(0.15)", 1)
        loops[0].body[-1] = sleep
    elif profile_ast_value(wait) != profile_ast_value(sleep):
        raise ValueError("Unknown command-completion wait or effects")
    # Equal unknown callbacks can observe a changed executor without changing
    # their own source. Bind reviewed eager context AND command consumers after
    # normalizing the two supported resource differences; never import it.
    symbols = symtable.symtable(source, path, "exec")
    definitions = {node.name: node for node in functions}
    consumers, pending = {}, ["run_command", "selected", "opt_in_test_target_changes", "production_build_paths"]
    while pending:
        name = pending.pop()
        if name in consumers:
            continue
        consumers[name] = profile_ast_value(function if name == "run_command" else definitions[name])
        used = referenced_globals(symbols.lookup(name).get_namespace())
        pending.extend(used & definitions.keys() - consumers.keys())
    layout = sha(canonical({"mode": row[0], "context": profile_ast_value(contexts), "consumers": consumers}))
    reviewed = "83c7116f8d7e143dda12e23ef9458b1b98a7dbc3b30c7230f73ce8351af533c9"
    if layout != reviewed:
        print("LOCAL_ACCEPTANCE_RESOURCE_LAYOUT_REFUSED=" + canonical({
            "ref": ref, "reviewed_layout": reviewed, "observed_layout": layout,
            "full_current_checks_required": True,
            "next": "Use the supported original-author acceptance command. Unknown layouts grant no reuse or waiver.",
        }).decode(), flush=True)
        raise ValueError("Unreviewed eager context or command consumer layout; full current checks remain required")
    # Callers such as main/perform_check can observe the executor too. Keep
    # complete reviewed program layouts, not only the command's callees. Erase
    # only this literal allowlist from its own checksum to avoid a hash cycle.
    program = copy.deepcopy(tree)
    for index, node in enumerate(program.body):
        if isinstance(node, ast.FunctionDef) and node.name == "run_command":
            program.body[index] = function
        if isinstance(node, ast.FunctionDef) and node.name == "resource_executor_identity":
            declarations = [item for item in node.body if isinstance(item, ast.Assign)
                            and any(isinstance(target, ast.Name) and target.id == "reviewed_sources"
                                    for target in item.targets)]
            if len(declarations) > 1:
                raise ValueError("Duplicate reviewed source declaration")
            for declaration in declarations:
                values = ast.literal_eval(declaration.value)
                if not isinstance(values, set) or not values or any(
                        not isinstance(value, str) or not re.fullmatch(r"[a-f0-9]{64}", value) for value in values):
                    raise ValueError("Unknown reviewed source declaration")
                declaration.value = ast.Constant(value="reviewed-resource-source-layouts")
    source_layout = sha(canonical(profile_ast_value(program)))
    reviewed_sources = {
        "b270e25b648c51f5305b8e720aab28aa5f3da6199727e4d7c44c579a80da7b27",
        "9655f97ff9d100f00c534afca56a5fbbea2cca548323f0fd4c8ae8740f8a69c9",
        "f2a0e6d04188540e13d970c3de9fe91d1c835bb01b2bf61226023d6e89d89043",
        "451f0720da61bb64c724d44019faeb0b79d0e4047986ebccfc0b1da14efd206a",
        "25b7f4ae7b0b9495d03828050b9b00821e7e7afd265280d666bae7864f4af082",
        "69e1e350a7a41034e1b73df10a4c80d00398fab33e2d79b908b0944509c49b2e",
        "b880640251c06c7bcc9ae05bf12f78ecfd7371af6c97abd47741c8bc00c693be",
        "df83d44536a720efc03d1c026136c079d62405129b945f87f899a1e6bd289585",
        "4f2966f3298eef8bb090cee7b4490f4bd2511f6644d0c7aea55b786d92580875",
        "51b69bebd2b48ed2b2fc90552c624a525d61f9db280ae1599e16b53bcf62bd83",
        "5a1503cd97bac0fb7ec58689f14b64530d2c564606a91484cecca76fd380ea14",
        "8d2689e3285deeb2e17695bb58f28a93b584365a3aae8f59e34bc99ed41fbe41",
        "f9c4f0cd610f4f9f6c954360b0b6364fce4dc4f962dac42236b7cc833f64c71b",
        "74ba17d2949bd3d630a49da8b4948443b6ba469464554ef012300611d4dc6712",
        "fb7a7547e1b39fdf316e3f98af6c26fc6e2690e294a5b9b6ccba1f465b0a503a",
        "01ea6891f69b892dae1389bbdd9fdce887038e7cb1cce302e4215614f6face0e",
        "2ddedba1c6f5bdaee370752430cb40c802af8f10c2c552f76808380051dc2d91",
        "968aed4934bc9a4c0db73fe4387a38886204d4fe9a74ad5ad511eaedb7c4b13a",
        "6283722865444555127b62148aea36ae74b7e002d6cfe24540afd39fe61f2380",
        "e9be3603ef2402aa7ed98b62b2ad7729418d4f8ebacacafc01401e1aa542d825",
    }
    if source_layout not in reviewed_sources:
        print("LOCAL_ACCEPTANCE_RESOURCE_SOURCE_REFUSED=" + canonical({
            "ref": ref, "observed_source_layout": source_layout, "full_current_checks_required": True,
            "next": "Use the supported original-author acceptance command; unreviewed callers grant no reuse.",
        }).decode(), flush=True)
        raise ValueError("Unreviewed complete executor program; full current checks remain required")
    return (budget_form, wait_form), canonical({"mode": row[0], "context": profile_ast_value(contexts),
                                                "executor": profile_ast_value(function), "executor_text": text}).decode()


def resource_equivalent_profiles(before, after, names):
    """Prove resource-only differences, without accepting a supplied hash pair."""
    allowed = {"preflight", "native-workspace", "native-examples", "native-release",
               "native-context", "native-services", "native-cohost"}
    names = [name for name in names if name in allowed]
    if not names:
        return set()
    try:
        before = value("rev-parse", f"{before}^{{commit}}")
        after = value("rev-parse", f"{after}^{{commit}}")
        old_budget, old_executor = resource_executor_identity(before)
        new_budget, new_executor = resource_executor_identity(after)
        if old_budget == new_budget:
            return set()
        previous = reachable_profile_commands_hashes(before, names, executor_identity=old_executor)
        current = reachable_profile_commands_hashes(after, names, executor_identity=new_executor)
        return {name for name in names if previous[name] == current[name]}
    except (ValueError, TypeError, KeyError, StopIteration, SyntaxError, RuntimeError, subprocess.CalledProcessError):
        return set()


def compatible_native_dispatcher(before, after):
    """Retain exact dispatcher identity, allowing only its two known budget forms."""
    path = ".github/local-acceptance/cross_platform.py"

    def source(ref):
        row = value("ls-tree", ref, "--", path).split("\t", 1)[0].split()
        if len(row) != 3 or row[0] not in {"100644", "100755"} or row[1] != "blob":
            raise ValueError("Native dispatcher must be a regular committed file")
        return row[0], tracked(ref, path)

    try:
        old, current = source(before), source(after)
        if old == current:
            return True
        if old[0] != current[0]:
            return False
        forms = [ast.parse(text).body[0] for text in ['''
def execute(command, cwd, log, env=None):
    jobs = (os.cpu_count() or 1) if os.environ.get("GITHUB_ACTIONS") == "true" else 1
    return helper.run_command(command, cwd, log, {**(env or {}), "CARGO_BUILD_JOBS": str(jobs)})
''', '''
def execute(command, cwd, log, env=None):
    hosted = {"CARGO_BUILD_JOBS": str(os.cpu_count() or 1)} if os.environ.get("GITHUB_ACTIONS") == "true" else {}
    return helper.run_command(command, cwd, log, {**(env or {}), **hosted})
''']]

        def identity(contents):
            tree = ast.parse(contents.decode("utf-8"))
            functions = [node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name == "execute"]
            if len(functions) != 1 or profile_ast_value(functions[0]) not in [profile_ast_value(form) for form in forms]:
                raise ValueError("Unknown native dispatch or environment effects")
            function = functions[0]
            form = [profile_ast_value(item) for item in forms].index(profile_ast_value(function))
            # A changed dispatcher must actually switch known budget forms.
            # All bytes outside that function, including comments, stay bound.
            lines = contents.decode("utf-8").splitlines(keepends=True)
            outside = "".join(lines[:function.lineno - 1] + lines[function.end_lineno:])
            return form, outside

        previous, updated = identity(old[1]), identity(current[1])
        return previous[0] != updated[0] and previous[1] == updated[1]
    except (ValueError, TypeError, SyntaxError, subprocess.CalledProcessError):
        return False


def reviewed_native_dependency_cache_transition(before, after, protected):
    """Accept the two reviewed native cache changes, in isolation or together."""
    toolchain_step = (b"      - uses: ./.trusted-acceptance/.github/actions/rust-toolchain\n"
                      b"        if: matrix.family == 'native'\n")
    cache_step = (b"      - uses: Swatinem/rust-cache@v2\n"
                  b"        if: matrix.family == 'native'\n"
                  b"        with:\n"
                  b"          workspaces: ${{ steps.cache_layout.outputs.workspaces }}\n"
                  b"          shared-key: ci-dependencies\n"
                  b"          env-vars: ImageOS\n"
                  b"          save-if: false\n")
    alternatives = []
    if before.count(toolchain_step) == before.count(cache_step) == 1:
        alternatives.append(before.replace(toolchain_step, toolchain_step + b"        id: rust_toolchain\n").replace(
            cache_step, cache_step.replace(
                b"          save-if:", b"          cmd-format: ${{ steps.rust_toolchain.outputs.dependency-cache-command }}\n"
                b"          save-if:")))
    # The upstream cache hashes raw configuration files in path order. The
    # new checkout sorts after the root pin, like the publisher's rust tree.
    paths = (
        b"        working-directory: candidate\n",
        b"          path: candidate\n",
        b"            candidate/rust-toolchain.toml\n",
        b" --source candidate/rust --github-output\n",
        b"          path: candidate/sdk/typescript/artifacts/*\n",
    )
    for workflow in (before, *alternatives):
        if all(workflow.count(path) == 1 for path in paths):
            for path in paths:
                workflow = workflow.replace(path, path.replace(b"candidate", b"worktree"))
            alternatives.append(workflow)
    if after not in alternatives:
        return False
    # The new outputs must come from the reviewed regular-file action and
    # adapter. Their blobs are outside the native workflow's own byte hash.
    for path, identity in (
            (".github/actions/rust-toolchain/action.yml", "ce7b8ad4ca80cc495808d0862d91bdbade600f70"),
            (".github/actions/rust-toolchain/cache-command.mjs", "973592bd8bdc233854295869e87dc24ae00fd6d7"),
            (".github/local-acceptance/cache_layout.py", "82bdafe46048139a6aae326580feca70ab543396")):
        if value("ls-tree", protected, "--", path) != f"100644 blob {identity}\t{path}":
            return False
    return True


def approved_equivalent_profiles(before, current, policy_ref, names, head):
    """Share protected exact recipe approvals between scheduling and completion."""
    policy = load_policy(policy_ref)
    equivalents = policy.get("equivalent_profile_commands", {})
    ungrouped = policy.get("equivalent_profile_commands_without_test_groups", {})
    accepted = {}
    for name in names:
        approved = equivalents.get(name, {}) if isinstance(equivalents, dict) else {}
        predecessors = approved.get(current[name], []) if isinstance(approved, dict) else []
        if isinstance(predecessors, list) and before[name] in predecessors:
            accepted[name] = "declared"
            continue
        conditional = ungrouped.get(name, {}) if isinstance(ungrouped, dict) else {}
        predecessors = conditional.get(current[name], []) if isinstance(conditional, dict) else []
        if (isinstance(predecessors, list) and before[name] in predecessors
                and not committed_test_groups(head)):
            accepted[name] = "without_test_groups"
    return accepted


def compatible_profile_commands(receipt, policy_ref, required):
    recorded = receipt.get("profile_commands_sha256")
    inputs = {}

    def read_inputs(ref):
        if ref not in inputs:
            inputs[ref] = execution_inputs(ref)
        return inputs[ref]

    try:
        if not re.fullmatch(r"[a-f0-9]{40}", policy_ref):
            # The signed predecessor is an object ID. Read a named policy once
            # so an alias of that same object shares its recipe calculation.
            policy_ref = value("rev-parse", "--verify", "--end-of-options", policy_ref)
        previous = attested_commands_ref(receipt, read_inputs)
        if "commands_ref" in receipt and previous is None:
            return False
        if previous != policy_ref:
            previous_inputs = read_inputs(previous) if previous is not None else None
            if not compatible_git_selector_inputs(previous_inputs, read_inputs(policy_ref)):
                return False
        if "profile_commands_sha256" not in receipt:
            if previous is None:
                return False
            recorded = profile_commands_hashes(previous, required, inputs=read_inputs(previous))
        if not isinstance(recorded, dict) or not all(isinstance(recorded.get(name), str) for name in required):
            return False
        # Modern receipts validate both maps against their original source;
        # only the reachable map is compared with the current policy.
        current = None
        if "reachable_profile_commands_sha256" not in receipt:
            current = profile_commands_hashes(policy_ref, required, inputs=read_inputs(policy_ref))
            if all(recorded.get(name) == current[name] for name in required):
                return True
        # A changed recipe is recovered from attested source, never from a
        # supplied hash alone. This also validates both maps on new receipts.
        if previous is None:
            return False
        original = (current if previous == policy_ref and current is not None
                    else profile_commands_hashes(previous, required, inputs=read_inputs(previous)))
        if not all(recorded.get(name) == original[name] for name in required):
            return False
        original_reachable = reachable_profile_commands_hashes(previous, required, inputs=read_inputs(previous))
        if "reachable_profile_commands_sha256" in receipt:
            reachable = receipt["reachable_profile_commands_sha256"]
            if not isinstance(reachable, dict) or any(
                    reachable.get(name) != original_reachable[name] for name in required):
                return False
        expected = (original_reachable if previous == policy_ref
                    else reachable_profile_commands_hashes(policy_ref, required, inputs=read_inputs(policy_ref)))
        differences = [name for name in required if original_reachable[name] != expected[name]]
        if not differences:
            return True
        resources = resource_equivalent_profiles(previous, policy_ref, differences)
        if resources:
            print("LOCAL_ACCEPTANCE_RESOURCE_EQUIVALENCE=" + canonical({
                "original_policy": previous, "current_policy": value("rev-parse", policy_ref),
                "profiles": sorted(resources), "proof": "only_known_compiler_budget_or_completion_wait_forms",
                "original_signed_maps_revalidated": True, "checks_executed_by_this_proof": False,
            }).decode(), flush=True)
        approved = approved_equivalent_profiles(original_reachable, expected, policy_ref, differences, receipt["head"])
        for name in required:
            if original_reachable[name] == expected[name] or name in resources:
                continue
            if name not in approved:
                return False
            if approved[name] == "without_test_groups":
                print(f"Preserving exact-head {name} evidence for a committed source without test groups; "
                      "the exact protected recipe pair applies only to unchanged original target selection. "
                      "Grouped or unknown sources still need fresh validation.", flush=True)
                continue
            print(f"Preserving exact-head {name} evidence: trusted policy approves this previous "
                  "execution recipe as equivalent. Required checks and signed source remain verified.", flush=True)
        return True
    except (RuntimeError, subprocess.CalledProcessError, SyntaxError, StopIteration, IndexError, KeyError, ValueError):
        return False


def changed_paths(base, head):
    # --no-renames includes both sides of a rename so moving a file out of a
    # guarded directory cannot make its checks disappear.
    return git("diff", "--name-only", "--no-renames", "-z", base, head, "--").stdout.decode().split("\0")[:-1]


def manifest_dependencies(manifest, workspace, kind):
    """Include optional and target-specific edges without executing Cargo/code."""
    names = set()
    tables = [manifest, *manifest.get("target", {}).values()]
    for table in tables:
        for alias, dependency in table.get(kind, {}).items():
            if isinstance(dependency, dict) and dependency.get("workspace"):
                dependency = workspace.get("workspace", {}).get("dependencies", {})[alias]
            names.add(dependency.get("package", alias) if isinstance(dependency, dict) else alias)
    return names


def without_development_dependencies(manifest):
    result = copy.deepcopy(manifest)
    result.pop("dev-dependencies", None)
    for target in result.get("target", {}).values():
        target.pop("dev-dependencies", None)
    # Removing a target's only dev table must not become a product change.
    if "target" in result:
        result["target"] = {name: value for name, value in result["target"].items() if value}
        if not result["target"]:
            result.pop("target")
    return result


def opt_in_test_target_changes(paths, base, head):
    """Prove new disabled target declarations and their newly committed sources together."""
    manifests = [path for path in paths if path.startswith("rust/crates/")
                 and path.count("/") == 3 and path.endswith("/Cargo.toml")]
    if tomllib is None or not manifests:
        return set(), set()
    ignored, sources = set(), set()
    try:
        def document(ref, path):
            return tomllib.loads(tracked(ref, path).decode("utf-8"))
        if document(base, "rust/Cargo.toml") != document(head, "rust/Cargo.toml"):
            return ignored, sources
        for path in manifests:
            try:
                before, after = document(base, path), document(head, path)
            except RuntimeError:
                continue
            old, new = before.get("test", []), after.get("test", [])
            if (not isinstance(old, list) or not isinstance(new, list) or len(new) <= len(old)
                    or new[:len(old)] != old
                    or {k: v for k, v in before.items() if k != "test"}
                       != {k: v for k, v in after.items() if k != "test"}):
                continue
            names = {target["name"] for target in old}
            directory = path.removesuffix("Cargo.toml")
            def files(ref):
                rows = git("ls-tree", "-r", "-z", ref, "--", directory + "tests").stdout.decode("utf-8").split("\0")
                return {name: info.split()[0] for row in rows if row for info, name in [row.split("\t", 1)]}
            old_files, new_files = files(base), files(head)
            valid, added_sources = True, set()
            for target in new[len(old):]:
                name = target.get("name", "")
                relative = target.get("path", "")
                if (set(target) != {"name", "path", "test"} or target["test"] is not False
                        or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name) or name in names
                        or relative != f"tests/acceptance_groups/{name}.rs"):
                    valid = False
                    break
                names.add(name)
                if any(directory + candidate in old_files
                       for candidate in [relative, f"tests/{name}.rs", f"tests/{name}/main.rs"]):
                    valid = False
                    break
                mode = new_files.get(directory + relative)
                if mode not in {"100644", "100755"}:
                    valid = False
                    break
                added_sources.add(directory + relative)
            if valid:
                ignored.add(path)
                sources.update(added_sources)
    except (RuntimeError, KeyError, TypeError, ValueError, AttributeError, IndexError):
        return set(), set()
    return ignored, sources


def opt_in_test_target_inputs(paths, base, head):
    """Retain the manifest-only scope used by the native acceptance planner."""
    manifests, _sources = opt_in_test_target_changes(paths, base, head)
    return manifests


def development_lock_change(before, after, manifests):
    """Require every existing production resolution/edge to remain unchanged."""
    if {k: v for k, v in before.items() if k != "package"} != {k: v for k, v in after.items() if k != "package"}:
        return False
    def packages(lock):
        items = lock.get("package", [])
        indexed = {(p["name"], p["version"], p.get("source")): p for p in items}
        if len(items) != len(indexed):
            raise ValueError("Duplicate lock package identities")
        return indexed
    old, new = packages(before), packages(after)
    if not old.keys() <= new.keys():
        return False
    # A new version/source of an existing name can change how an unqualified
    # lock edge resolves. Keep all normal gates for that case.
    old_names = {key[0] for key in old}
    if any(key[0] in old_names for key in new.keys() - old.keys()):
        return False
    for identity, previous in old.items():
        current = new[identity]
        if previous == current:
            continue
        allowed = manifests.get(identity[0]) if identity[2] is None else None
        if allowed is None:
            return False
        if {k: v for k, v in previous.items() if k != "dependencies"} != {k: v for k, v in current.items() if k != "dependencies"}:
            return False
        changed = set(previous.get("dependencies", [])) ^ set(current.get("dependencies", []))
        if any(edge.split()[0] not in allowed for edge in changed):
            return False
    return True


def production_dependency_state(ref, workspace):
    """Read normal/build lock edges from committed manifests, never candidate code."""
    settings = workspace["workspace"]
    if settings.get("resolver") != "2" or settings.get("members") != ["crates/*"]:
        raise ValueError("Unknown workspace dependency layout")
    inherited = settings.get("dependencies", {})
    if not isinstance(inherited, dict):
        raise ValueError("Invalid workspace dependency table")
    manifests, roots, development, normal_inherited = {}, {}, set(), set()
    blobs = {}
    rows = git("ls-tree", "-r", "-z", ref, "--", "rust/crates", "rust/Cargo.lock").stdout.decode("utf-8").split("\0")
    for row in rows:
        if not row:
            continue
        info, path = row.split("\t", 1)
        mode, kind, _identity = info.split()
        if re.fullmatch(r"rust/crates/[^/]+", path) and mode in {"120000", "160000"}:
            raise ValueError("Indirect workspace member")
        if path != "rust/Cargo.lock" and not re.fullmatch(r"rust/crates/[^/]+/Cargo.toml", path):
            continue
        if mode not in {"100644", "100755"} or kind != "blob":
            raise ValueError("Indirect workspace manifest")
        blobs[path] = _identity
    if "rust/Cargo.lock" not in blobs:
        raise ValueError("Missing committed Cargo lock")
    documents = {path: tomllib.loads(data.decode("utf-8"))
                 for path, data in committed_blob_bytes(blobs).items()}
    lock = documents.pop("rust/Cargo.lock")
    for path, manifest in documents.items():
        manifests[path] = without_development_dependencies(manifest)
        package = manifest["package"]
        version = package.get("version")
        if version == {"workspace": True}:
            version = settings["package"]["version"]
        if not isinstance(package["name"], str) or not isinstance(version, str):
            raise ValueError("Unknown workspace package identity")
        identity = package["name"], version, None
        if identity in roots:
            raise ValueError("Duplicate workspace package identity")
        normal = set().union(*(manifest_dependencies(manifest, workspace, name)
                               for name in ["dependencies", "build-dependencies"]))
        roots[identity] = normal, manifest_dependencies(manifest, workspace, "dev-dependencies")
        for table in [manifest, *manifest.get("target", {}).values()]:
            for name in ["dependencies", "build-dependencies", "dev-dependencies"]:
                for alias, dependency in table.get(name, {}).items():
                    if isinstance(dependency, dict) and dependency.get("workspace") is True:
                        if alias not in inherited:
                            raise ValueError("Missing inherited dependency")
                        (development if name == "dev-dependencies" else normal_inherited).add(alias)
    if not roots:
        raise ValueError("No committed workspace packages")
    packages, aliases = {}, {}
    for package in lock["package"]:
        identity = package["name"], package["version"], package.get("source")
        if identity in packages:
            raise ValueError("Duplicate lock package identity")
        packages[identity] = package
        name, version, source = identity
        spellings = [name, f"{name} {version}"]
        if source is not None:
            spellings.append(f"{name} {version} ({source})")
        for spelling in spellings:
            aliases.setdefault(spelling, set()).add(identity)

    def resolve(edge):
        matches = aliases.get(edge, set())
        if len(matches) != 1:
            raise ValueError("Missing or ambiguous production lock edge")
        return next(iter(matches))

    # Validate even unused development edges before discarding their scope.
    resolved = {identity: {resolve(edge) for edge in package.get("dependencies", [])}
                for identity, package in packages.items()}
    reachable, pending = {}, list(roots)
    while pending:
        identity = pending.pop()
        if json.dumps(identity) in reachable:
            continue
        package = packages[identity]
        edges = package.get("dependencies", [])
        if identity in roots:
            normal, dev = roots[identity]
            names = {edge.split()[0] for edge in edges}
            if names != normal | dev:
                raise ValueError("Unknown or missing workspace dependency edge")
            # Lock entries combine dev and normal edges. Retain every matching
            # name, including alternate versions, rather than guess a binding.
            edges = [edge for edge in edges if edge.split()[0] in normal]
        dependencies = ({resolve(edge) for edge in edges} if identity in roots else resolved[identity])
        record = {key: value for key, value in package.items() if key != "dependencies"}
        record["dependencies"] = sorted(json.dumps(edge) for edge in dependencies)
        reachable[json.dumps(identity)] = record
        pending.extend(dependencies)
    declaration = copy.deepcopy(workspace)
    declaration["workspace"].pop("dependencies", None)
    return ({"workspace": declaration, "manifests": manifests,
             "inherited": {name: inherited[name] for name in normal_inherited},
             "lock": {key: value for key, value in lock.items() if key != "package"},
             "packages": reachable}, development)


def unchanged_production_dependencies(base, head, before, after):
    """Accept shared dev updates only when all normal declarations/resolution agree."""
    previous, old_development = production_dependency_state(base, before)
    current, new_development = production_dependency_state(head, after)
    old = before["workspace"].get("dependencies", {})
    new = after["workspace"].get("dependencies", {})
    changed = {name for name in old.keys() | new.keys() if old.get(name) != new.get(name)}
    return previous == current and changed <= old_development | new_development


def production_build_paths(paths, base, head, *, opt_in=None):
    """Discard proven dev-only Cargo inputs for normal product builds only.

    All Rust sources, test harnesses, normal/build dependencies, features,
    workspace settings and unknown inputs retain their ordinary path scope.
    The metadata verifier reads Git/TOML; candidate code is never executed.
    """
    paths = list(paths)
    if opt_in is None:
        opt_in = opt_in_test_target_inputs(paths, base, head)
    paths = [path for path in paths if path not in opt_in]
    metadata = [p for p in paths if p == "rust/Cargo.lock" or p == "rust/Cargo.toml"
                or (p.startswith("rust/crates/") and p.count("/") == 3 and p.endswith("/Cargo.toml"))]
    if not metadata or tomllib is None:
        return paths
    try:
        def document(ref, path):
            return tomllib.loads(tracked(ref, path).decode("utf-8"))
        old_workspace = document(base, "rust/Cargo.toml")
        new_workspace = document(head, "rust/Cargo.toml")
        try:
            if unchanged_production_dependencies(base, head, old_workspace, new_workspace):
                return [path for path in paths if path not in metadata]
        except (RuntimeError, KeyError, TypeError, ValueError, AttributeError, IndexError):
            # Older/custom layouts retain the existing conservative verifier.
            pass
        if old_workspace != new_workspace or old_workspace.get("workspace", {}).get("resolver") != "2":
            return paths
        ignored = {"rust/Cargo.toml"} & set(metadata)
        allowed = {}
        for path in metadata:
            if path in {"rust/Cargo.toml", "rust/Cargo.lock"}:
                continue
            # Added/deleted crates keep their own build scope. Their unique
            # dev-only lock nodes may still be irrelevant to other products.
            try:
                before, after = document(base, path), document(head, path)
            except RuntimeError:
                continue
            if without_development_dependencies(before) != without_development_dependencies(after):
                continue
            name = before["package"]["name"]
            development = (manifest_dependencies(before, old_workspace, "dev-dependencies")
                           | manifest_dependencies(after, new_workspace, "dev-dependencies"))
            normal = set().union(*(manifest_dependencies(before, old_workspace, kind)
                                   for kind in ["dependencies", "build-dependencies"]))
            allowed[name] = development - normal
            ignored.add(path)
        if "rust/Cargo.lock" in metadata and development_lock_change(
                document(base, "rust/Cargo.lock"), document(head, "rust/Cargo.lock"), allowed):
            ignored.add("rust/Cargo.lock")
    except (RuntimeError, KeyError, TypeError, ValueError, AttributeError, IndexError):
        return paths
    return [path for path in paths if path not in ignored]


def isolated_cli_test_inputs(paths, head):
    """Prove that only existing individual CLI test modules changed in Git."""
    inputs = [path for path in paths if path != "README.md"
              and not (path.startswith("docs/") and path.endswith(".md"))]
    pattern = r"rust/crates/rusty-sudocode-cli/tests/([A-Za-z0-9_]+)\.rs"
    if not inputs or any(not re.fullmatch(pattern, path) for path in inputs):
        return False
    try:
        # Deleted, redirected and unknown files retain ordinary example builds.
        for path in inputs:
            entry = git("ls-tree", "-z", head, "--", path).stdout.split(b"\0")
            if len(entry) != 2 or not entry[0].startswith(b"100644 blob ") or entry[1]:
                return False
        return True
    except (RuntimeError, OSError, ValueError):
        return False


def selected(policy, paths, *, base=None, head=None):
    paths = list(paths)
    separate_examples = False
    if "native-examples" in policy["checks"] and any(path.startswith("rust/") for path in paths):
        separate_examples = head is None or committed_test_groups(head)
        isolated_tests = head is not None and isolated_cli_test_inputs(paths, head)
        separate_examples = separate_examples and not isolated_tests
        if head is not None:
            print("EXAMPLE_BUILD_SCOPE=" + json.dumps({"head": head, "separate_profile": separate_examples,
                  "reason": "isolated_cli_tests" if isolated_tests else
                            "grouped_or_unknown_source" if separate_examples else "original_target_selection"}), flush=True)
    opt_in = opt_in_test_target_inputs(paths, base, head) if base and head else set()
    product_paths = production_build_paths(paths, base, head, opt_in=opt_in) if base and head else paths
    functional_native = "native-context" if "native-context" in policy["checks"] else "native-release"
    product_profiles = {functional_native, "native-services", "native-cohost"}
    if product_paths != paths:
        print("Development-only Cargo metadata skips unchanged normal product builds; "
              + ("native workspace validation remains selected." if opt_in
                 else "native test and performance input scopes remain unchanged."), flush=True)
        print("BUILD_INPUT_SCOPE=" + json.dumps({"base": base, "head": head,
              "normal_product_ignored": [p for p in paths if p not in product_paths],
              "applies_to": [functional_native, "native-services", "native-cohost"],
              "native_tests_and_performance_scope_unchanged": not bool(opt_in)}), flush=True)
    if opt_in:
        print("New disabled acceptance targets preserve existing product and named performance builds; "
              "native workspace validation still runs. Existing target edits or unknown metadata keep full scope.", flush=True)
        print("OPT_IN_TEST_TARGET_SCOPE=" + json.dumps({"base": base, "head": head,
              "unchanged_benchmark_metadata": sorted(opt_in)}), flush=True)
    names = [name for name, rule in policy["checks"].items()
            if (name != "native-examples" or separate_examples)
            and (name == "preflight" or any(any(fnmatch.fnmatchcase(p, pattern) for pattern in rule["include"])
                   and not any(fnmatch.fnmatchcase(p, pattern) for pattern in rule.get("exclude", []))
                   for p in (product_paths if name in product_profiles
                             else [p for p in paths if p not in opt_in] if name in {"rendering", "browser"}
                             else paths)))]
    harness = [path for path in paths if path.startswith("e2e/nexus-a2a/")
               and not path.lower().endswith((".md", ".html"))]
    if base and head and harness and "native-workspace" in policy["checks"] and "native-workspace" not in names:
        print("A2A_HARNESS_SCOPE=" + json.dumps({"changed_harness_inputs": harness,
              "native_workspace": "omitted_unchanged_product",
              "required_validation": "real pinned-daemon scripts in the Linux merge queue",
              "mixed_Rust_inputs_retain_native_workspace": True}), flush=True)
    return names


NATIVE_TARGETS = {
    "linux": {"system": "linux", "runner": "ubuntu-latest", "label": "Linux"},
    "linux-x64": {"system": "linux", "architecture": "x64", "runner": "ubuntu-latest", "label": "Linux x64"},
    "linux-arm64": {"system": "linux", "architecture": "arm64", "runner": "ubuntu-24.04-arm", "label": "Linux ARM64"},
    "macos": {"system": "macos", "runner": "macos-latest", "label": "macOS"},
    "windows": {"system": "windows", "runner": "windows-latest", "label": "Windows"},
}
DEFAULT_NATIVE_TARGETS = ["linux", "macos", "windows"]


def acceptance_plan(policy, names, author):
    """One plan drives local receipts, missing-host CI and agent-visible reports."""
    architecture = {"x86_64": "x64", "amd64": "x64", "aarch64": "arm64"}.get(
        author["architecture"].lower(), author["architecture"].lower())
    if author["system"] not in {"linux", "macos", "windows"}:
        raise RuntimeError("Unknown author platform; no CI coverage may be inferred")
    jobs = {}
    local_targets = []
    for name in names:
        complementary = policy["checks"][name].get("complementary")
        if "complementary" in policy["checks"][name] and (name != "native-workspace" or complementary != "periodic"):
            raise RuntimeError(f"Invalid complementary acceptance responsibility for {name}")
        if not name.startswith("native-"):
            continue
        targets = policy["checks"][name].get("targets", DEFAULT_NATIVE_TARGETS)
        if not isinstance(targets, list) or not targets or any(not isinstance(target, str) for target in targets):
            raise RuntimeError(f"Invalid acceptance targets for {name}")
        if len(targets) != len(set(targets)):
            raise RuntimeError(f"Duplicate acceptance targets for {name}")
        covered = []
        for identifier in targets:
            if identifier not in NATIVE_TARGETS:
                raise RuntimeError(f"Unknown acceptance target: {identifier}")
            target = NATIVE_TARGETS[identifier]
            if target["system"] == author["system"] and target.get("architecture", architecture) == architecture:
                covered.append(identifier)
                continue
            if complementary == "periodic":
                continue
            family = "sdk" if name == "native-sdk" else "native"
            key = family, target["runner"]
            if key not in jobs:
                jobs[key] = {"os": target["runner"], "family": family, "checks": [], "targets": [],
                             "name": (f"Complement local host ({target['runner']})" if family == "native" else
                                      f"Complement local host ({family}, {target['runner']})"),
                             "node": "20" if family == "sdk" else "22", "timeout": 15 if family == "sdk" else 90}
            if name not in jobs[key]["checks"]:
                jobs[key]["checks"].append(name)
            if identifier not in jobs[key]["targets"]:
                jobs[key]["targets"].append(identifier)
        if not covered:
            raise RuntimeError(f"Author host does not cover a required target of {name}: {author}")
        local_targets.append({"check": name, "targets": covered})
    return {"version": 1, "scope": "source-acceptance", "author": author,
            "local_checks": list(names), "local_targets": local_targets,
            "ci": list(jobs.values()), "deployment_verified": False}


def compatible_acceptance_plan(policy, names, author, plan):
    """Accept only the current plan or the exact former full workspace split."""
    current = acceptance_plan(policy, names, author)
    if plan == current:
        return True
    workspace = policy["checks"].get("native-workspace", {})
    if "native-workspace" not in names or workspace.get("complementary") != "periodic":
        return False
    legacy = copy.deepcopy(policy)
    del legacy["checks"]["native-workspace"]["complementary"]
    return plan == acceptance_plan(legacy, names, author)


def report_plan(plan, current_environment=False):
    label = {"macos": "macOS", "windows": "Windows", "linux": "Linux"}[plan["author"]["system"]]
    lines = [f"Author-local {label}: {', '.join(plan['local_checks'])}"]
    workspace = "native-workspace" in plan["local_checks"]
    periodic = workspace and not any("native-workspace" in job["checks"] for job in plan["ci"])
    if periodic:
        lines.append("The original native-workspace profile runs on the original author host. Other OS workspace "
                     "execution moves to weekly/manual three-platform regression; OS-specific failures may "
                     "surface after merge. Selected complementary native journeys and three-platform queue "
                     "Clippy remain required. Periodic execution costs belong in lifecycle accounting.")
    concurrency = None
    if current_environment and "native-workspace" in plan["local_checks"]:
        cap = os.environ.get("SCODE_ACCEPTANCE_TEST_THREADS")
        rule = "half the logical CPUs, at most one per 3 GiB of memory"
        concurrency = {"source": "current invocation environment", "compiler_jobs_per_cargo": rule,
                       "compiler_jobs_setting": "SCODE_ACCEPTANCE_BUILD_JOBS",
                       "requested_test_worker_cap": cap, "cap_setting": "SCODE_ACCEPTANCE_TEST_THREADS"}
        lines.append(f"Author-native Cargo compilation uses {rule} per command (SCODE_ACCEPTANCE_BUILD_JOBS "
                     "lowers it) at BelowNormal priority. Native test processes use "
                     + ("the protected profile default." if cap is None else
                        f"the requested SCODE_ACCEPTANCE_TEST_THREADS={cap} cap."))
        try:
            serial = cap is not None and int(cap) == 1
        except ValueError:
            serial = False
        if serial:
            clear = ("Remove-Item Env:SCODE_ACCEPTANCE_TEST_THREADS" if os.name == "nt"
                     else "unset SCODE_ACCEPTANCE_TEST_THREADS")
            lines.append("SCODE_ACCEPTANCE_TEST_THREADS=1 runs native test cases serially. "
                         f"To use the protected test-pool default, run `{clear}` before retrying this command. "
                         "Cargo compilation keeps its independent job setting; the explicit test cap is honored.")
    performance = [name for name in ["rendering"] if name in plan["local_checks"]]
    lines.append("Statistical performance profiles: " + (", ".join(performance) if performance else
                 "none selected; release acceptance judges indirect performance effects with the stress matrix."))
    release = "native-release" in plan["local_checks"] or any("native-release" in job["checks"] for job in plan["ci"])
    lines.append("Native release build/startup: " + ("selected on the author and complementary native hosts." if release else
                 "not selected for ordinary product edits; every newly built release artifact must pass optimized "
                 "stream/resume, the 100 ms startup budget and compiler context before packaging/publication.")
                 + " Optimized acceptance harness edits still require local validation. Source compiler-context "
                   "acceptance uses the current dev/mailbox binary; native functional checks and selected Linux "
                   "release rendering remain required. Optimization-specific failures may surface at release.")
    lines.append("The browser terminal functional smoke runs locally." if "browser" in plan["local_checks"]
                 else "No browser terminal input changed.")
    for job in plan["ci"]:
        lines.append(f"CI {', '.join(NATIVE_TARGETS[target]['label'] for target in job['targets'])}: "
                     f"{', '.join(job['checks'])} ({job['os']})")
    lines.append("Hosted CI preflight also runs changed Cargo/Docker/Clippy executor fixtures, "
                 "selected from committed Git inputs.")
    lines.append("Scope: source tests only; installation and deployed-system acceptance are not verified by this receipt.")
    print("\n".join(lines), flush=True)
    print("ACCEPTANCE_PLAN=" + json.dumps(plan, separators=(",", ":")), flush=True)
    if workspace:
        print("WORKSPACE_ACCEPTANCE_SCOPE=" + canonical({
            "author": "full", "complementary_workspace": "periodic" if periodic else "per_pr",
            "cross_os_workspace_coverage_reduced": periodic,
            "periodic_workflow": ".github/workflows/local-acceptance-workspace.yml" if periodic else None,
            "periodic_costs_must_be_accounted": periodic,
        }).decode(), flush=True)
    if concurrency is not None:
        print("LOCAL_ACCEPTANCE_CONCURRENCY=" + json.dumps(concurrency, separators=(",", ":")), flush=True)
    print("RELEASE_ACCEPTANCE_SCOPE=" + json.dumps({"selected": release,
          "new_release_artifacts": "mandatory_before_packaging_and_publication",
          "ordinary_source_optimized_native_coverage_reduced": True}, separators=(",", ":")), flush=True)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as stream:
            stream.write("## Acceptance responsibilities\n\n" + "\n".join("- " + line for line in lines) + "\n")


def clean(head):
    if value("rev-parse", "HEAD") != head:
        raise RuntimeError("HEAD changed during acceptance")
    # Untracked build outputs are ignored by Git. An untracked source file is
    # not a verified input, even if Cargo happened to compile it successfully.
    if value("status", "--porcelain", "--untracked-files=all"):
        raise RuntimeError("Acceptance requires a clean worktree, including untracked source files")


def host():
    system = {"Windows": "windows", "Darwin": "macos", "Linux": "linux"}.get(platform.system())
    if system is None:
        raise RuntimeError("Supported author hosts are Windows, macOS and Linux")
    return {"system": system, "architecture": platform.machine().lower()}


def changed_contract_files(head, base):
    protected = [POLICY, SIGNERS, ".github/scripts/local_acceptance.py", ".github/workflows/local-acceptance.yml",
                 ".github/workflows/local-acceptance-publish.yml", ".github/workflows/local-acceptance-workspace.yml"]
    return sorted(path for path in changed_paths(value("merge-base", base, head), head)
                  if path in protected or path.startswith(".github/local-acceptance/"))


def check_contract_changes(head, base, policy_ref):
    # An existing PR need not merge main merely to acquire newly added hook
    # files. It uses the protected runner. All candidate contract edits,
    # including additional checks, are blocked from authorizing themselves.
    changed = changed_contract_files(head, base)
    if changed:
        if contract_hash(head) != contract_hash(policy_ref):
            raise RuntimeError(
                "Candidate changes the protected acceptance contract. "
                f"Changed protected files: {json.dumps(changed)}. "
                f"Trusted policy: {policy_ref} ({value('rev-parse', policy_ref)}). "
                "Normal acceptance/push blocks every contract change, including added checks; "
                "the candidate cannot authorize its own check definitions. "
                f"Next: {CONTRACT_UPDATE_COMMAND}. "
                "That command requires a GitHub repository maintain/admin actor and an already enrolled signing device, "
                "runs the candidate checks, and publishes exact-head contract authorization; CI still verifies all tests. "
                "this is not a request for the end user to review code or approve this push. "
                "See CONTRIBUTING.md#acceptance-contract-changes."
            )


def github_api(path, payload=None):
    executable = shutil.which("gh") or "gh"
    command = [executable, "api", path, "--method", "POST" if payload is not None else "GET"]
    if os.name == "nt" and Path(executable).suffix.lower() in {".cmd", ".bat"}:
        raise RuntimeError("Use the native GitHub CLI gh.exe for contract-update so API arguments avoid shell expansion")
    if payload is not None:
        command += ["--input", "-"]
    try:
        result = subprocess.run(command, cwd=ROOT, input=canonical(payload) if payload is not None else None,
                                capture_output=True, timeout=60)
    except subprocess.TimeoutExpired as error:
        raise RuntimeError("GitHub contract authorization timed out after 60s. Retry contract-update; "
                           "an existing valid exact-head receipt is reused.") from error
    if result.returncode:
        raise RuntimeError("GitHub contract authorization request failed: " + result.stderr.decode(errors="replace"))
    return json.loads(result.stdout) if result.stdout.strip() else None


def require_contract_maintainer(repository, actor, api=github_api):
    permission = api(f"repos/{repository}/collaborators/{urllib.parse.quote(actor, safe='')}/permission")
    if permission.get("role_name") not in {"maintain", "admin"} and permission.get("permission") != "admin":
        raise RuntimeError(f"Contract update requires repository maintain/admin authority; {actor} has "
                           f"{permission.get('role_name', permission.get('permission'))}. "
                           "Ask an authorized repository maintainer or agent to run the printed contract-update command; "
                           "the end user need not personally review code.")


def authorization_description(head, trusted):
    # Both full hashes fit GitHub's 140-character status description limit.
    return f"v1 {contract_hash(trusted)} {contract_hash(head)}"


def authorized_policy(head, base, trusted, api=github_api):
    if not changed_contract_files(head, base) or contract_hash(head) == contract_hash(trusted):
        return trusted
    repository = load_policy(trusted)["repository"]
    if load_policy(head)["repository"] != repository:
        raise RuntimeError("Contract authorization cannot change this repository's identity")
    statuses = []
    for page in range(1, 101):
        try:
            batch = api(f"repos/{repository}/commits/{head}/statuses?per_page=100&page={page}")
        except (RuntimeError, OSError, urllib.error.HTTPError) as error:
            try:
                check_contract_changes(head, base, trusted)
            except RuntimeError as diagnostic:
                raise RuntimeError(f"{diagnostic} Authorization lookup failed: {error}") from error
            raise
        statuses += [item for item in batch if item["context"] == AUTHORIZATION_CONTEXT]
        if statuses or len(batch) < 100:
            break
    expected_url = f"https://github.com/{repository}/commit/{head}"
    if not statuses:
        check_contract_changes(head, base, trusted)
    status = statuses[0]
    if (status["state"] != "success" or status.get("description") != authorization_description(head, trusted)
            or status.get("target_url") != expected_url):
        raise RuntimeError(f"Contract authorization is stale or revoked. Next: {CONTRACT_UPDATE_COMMAND}. "
                           "This rechecks the exact head and current trusted rules; "
                           "it does not waive failed tests.")
    require_contract_maintainer(repository, status["creator"]["login"], api)
    print(f"Contract authorization by {status['creator']['login']} for {head[:12]} verified; tests remain required.", flush=True)
    return head


def run_command(command, cwd, log, env=None):
    """Keep tests Normal while lowering only this command's compiler children."""
    import psutil

    environment = os.environ.copy()
    if os.name == "nt" and not environment.get("LIB") and (
            Path(command[0]).stem.lower() in {"cargo", "cargo-nextest"} or any(
                path in str(arg) for arg in command for path in ("e2e/build-metadata/run.py", "e2e/startup/run.py"))):
        global MSVC_ENV
        if MSVC_ENV is None:
            configured = git("config", "--get", "localAcceptance.vsdevcmd", check=False).stdout.decode().strip()
            locator = Path(os.environ.get("ProgramFiles(x86)", "C:/Program Files (x86)")) / "Microsoft Visual Studio/Installer/vswhere.exe"
            if configured:
                batch = Path(configured)
            elif locator.exists():
                installation = subprocess.check_output([str(locator), "-latest", "-products", "*", "-requires",
                                                        "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                                                        "-property", "installationPath"], text=True).strip()
                batch = Path(installation) / "Common7/Tools/VsDevCmd.bat"
            else:
                batch = Path("missing-vsdevcmd")
            if not batch.is_file():
                raise RuntimeError("Use Developer PowerShell or set git config localAcceptance.vsdevcmd to your VsDevCmd.bat")
            output = subprocess.check_output(f'cmd.exe /d /s /c ""{batch}" -no_logo -arch=x64 -host_arch=x64 && set"',
                                             encoding="mbcs", errors="replace")
            MSVC_ENV = {key.upper(): value for line in output.splitlines() if "=" in line and not line.startswith("=")
                        for key, value in [line.split("=", 1)]}
        environment.update(MSVC_ENV)
    linker = Path(environment.get("VCTOOLSINSTALLDIR", "")) / "bin/Hostx64/x64"
    if (os.name == "nt" and environment.get("VCTOOLSINSTALLDIR") and (linker / "link.exe").is_file()
            and Path(command[0]).stem.lower() in {"cargo", "cargo-nextest", "rustc"}):
        # With VCINSTALLDIR set, rustc takes link.exe from PATH, and Git for
        # Windows runs hooks with its own /usr/bin (and its link) first.
        environment["PATH"] = str(linker) + os.pathsep + environment.get("PATH", "")
    # SDK discovery also captures hook-exported repository selectors. Fixtures
    # run Git in their own repositories, so clear inherited selectors after
    # setup and before applying explicit per-command settings.
    environment = without_git_selectors(environment)
    # Explicit fixture values below still select NoColor, including when the
    # SDK environment is cached; the launcher's log preference stays outside.
    environment.pop("NO_COLOR", None)
    # Compiler children run at BelowNormal priority (below), so foreground work
    # keeps the CPU it needs; a serial build only lengthened every queued
    # acceptance. Use half the logical CPUs and at most one job per 3 GiB of
    # memory; SCODE_ACCEPTANCE_BUILD_JOBS lowers it and never raises it. An
    # explicit env (hosted runners) still wins below.
    jobs = max(1, min((os.cpu_count() or 1) // 2, psutil.virtual_memory().total // (3 * 1024 ** 3)))
    cap = os.environ.get("SCODE_ACCEPTANCE_BUILD_JOBS")
    if cap is not None:
        if not cap.isdigit() or int(cap) < 1:
            raise RuntimeError("SCODE_ACCEPTANCE_BUILD_JOBS must be a positive integer")
        jobs = min(jobs, int(cap))
    environment.update({"CARGO_BUILD_JOBS": str(jobs), "CARGO_INCREMENTAL": "0",
                        "CARGO_PROFILE_DEV_DEBUG": "0", "CARGO_PROFILE_TEST_DEBUG": "0", "PYTHONIOENCODING": "utf-8"})
    environment.update(env or {})
    # Neither ambient credentials, SDK discovery nor per-command overrides may
    # pass GitHub API authority to candidate builds, tests or product processes.
    # Windows resolves environment names without regard to case, including
    # explicit dictionary overrides that have not passed through os.environ.
    environment = {key: value for key, value in environment.items()
                   if key.upper() not in {"GH_TOKEN", "GITHUB_TOKEN"}}
    command = [shutil.which(str(command[0])) or str(command[0]), *map(str, command[1:])]
    started = time.monotonic()
    observed = {}
    log.parent.mkdir(parents=True, exist_ok=True)
    print("+ " + shlex.join(map(str, command)), flush=True)
    with log.open("wb") as stream:
        process = subprocess.Popen(list(map(str, command)), cwd=cwd, env=environment,
                                   stdout=stream, stderr=subprocess.STDOUT)
        try:
            while process.poll() is None:
                try:
                    children = psutil.Process(process.pid).children(recursive=True)
                    for child in children:
                        try:
                            # name() may read argv on macOS and trigger psutil
                            # 7.2.2's KERN_PROCARGS2 error path for short-lived children.
                            name = Path(child.exe()).name.lower()
                            if (name in {"rustc", "rustc.exe", "clippy-driver", "clippy-driver.exe", "rust-lld", "rust-lld.exe", "link.exe", "ld", "ld.lld", "ld64.lld", "cc", "cc1", "cc1plus", "clang-cl.exe", "cl.exe", "c1.exe", "c1xx.exe", "ml64.exe"}
                                    or re.fullmatch(r"(?:.*-)?(?:clang(?:\+\+)?|gcc|g\+\+)(?:-\d+(?:\.\d+)*)?(?:\.exe)?", name)):
                                child.nice(psutil.BELOW_NORMAL_PRIORITY_CLASS if os.name == "nt" else 10)
                                observed[child.pid, name] = child.nice()
                        except (psutil.NoSuchProcess, psutil.AccessDenied):
                            continue
                except (psutil.NoSuchProcess, psutil.AccessDenied):
                    pass
                try:
                    process.wait(timeout=0.15)
                except subprocess.TimeoutExpired:
                    pass
        except BaseException:
            # Interrupting owned validation must not orphan compiler children.
            for child in psutil.Process(process.pid).children(recursive=True)[::-1]:
                try:
                    child.terminate()
                except psutil.NoSuchProcess:
                    pass
            process.terminate()
            process.wait()
            raise
    # Sign how many distinct compiler children ran at each priority, by name.
    # The summary stays bounded however many children a build spawns.
    priorities = {}
    for (_pid, name), priority in observed.items():
        counts = priorities.setdefault(name, {})
        counts[str(priority)] = counts.get(str(priority), 0) + 1
    result = {"command": list(map(str, command)), "exit_code": process.returncode,
              "elapsed_seconds": round(time.monotonic() - started, 3),
              "log_sha256": sha(log.read_bytes()), "compiler_priorities": priorities}
    log.with_suffix(".command.json").write_bytes(canonical(result))
    if process.returncode:
        print(log.read_text(errors="replace")[-10000:], file=sys.stderr)
        raise RuntimeError(f"Local acceptance failed ({process.returncode}); evidence: {log}")
    return result


def read_note(head, ref=NOTE_REF):
    result = git("notes", f"--ref={ref}", "show", head, check=False)
    if result.returncode:
        return {"version": 1, "attestations": {}}
    if len(result.stdout) > 2_000_000:
        raise RuntimeError("Acceptance note exceeds 2 MB")
    obj = json.loads(result.stdout)
    # "envelopes" stores each earlier signed envelope that a version 2 receipt
    # carries from, once, keyed by envelope_identity. Notes without carried
    # references omit it; older readers ignore it and refuse version 2 receipts.
    if (obj.get("version") != 1 or not isinstance(obj.get("attestations"), dict)
            or not isinstance(obj.get("envelopes", {}), dict)):
        raise RuntimeError("Invalid acceptance note")
    return obj


def envelope_identity(envelope):
    """Content address of one signed envelope: SHA-256 of its canonical JSON."""
    return sha(canonical(envelope))


def signed_envelope(envelope):
    if (not isinstance(envelope, dict) or set(envelope) != {"signer", "payload", "signature"}
            or not all(isinstance(field, str) for field in envelope.values())):
        raise RuntimeError("Carried evidence is not a signed envelope")
    return envelope


def carried_envelope(group, store, version):
    """Resolve the earlier envelope one signed carry group names.

    Version 2 receipts name it by envelope_identity inside the signed payload,
    and the note or local output stores its exact bytes once. Version 1
    receipts, written before that store existed, embed it whole.
    """
    if version == 1:
        if "envelope_sha256" in group:
            raise RuntimeError("A version 1 receipt embeds its carried envelopes")
        return signed_envelope(group.get("envelope"))
    reference = group.get("envelope_sha256")
    if "envelope" in group or not isinstance(reference, str) or not re.fullmatch(r"[a-f0-9]{64}", reference):
        raise RuntimeError("Carried evidence must reference its origin envelope by SHA-256")
    if reference not in store:
        raise RuntimeError(f"Carried evidence {reference[:12]} is missing from its envelope store")
    envelope = signed_envelope(store[reference])
    if envelope_identity(envelope) != reference:
        raise RuntimeError(f"Stored carried evidence {reference[:12]} differs from its signed SHA-256")
    return envelope


def carried_store(envelopes, store):
    """Stored envelopes reachable through these envelopes' carry references."""
    reachable, pending = {}, list(envelopes)
    while pending:
        try:
            groups = json.loads(base64.b64decode(pending.pop()["payload"], validate=True)).get("carried", [])
        except (ValueError, KeyError, TypeError, AttributeError):
            continue
        for group in groups if isinstance(groups, list) else []:
            if not isinstance(group, dict):
                continue
            reference = group.get("envelope_sha256")
            if isinstance(reference, str) and reference in store and reference not in reachable:
                reachable[reference] = store[reference]
                pending.append(store[reference])
            elif isinstance(group.get("envelope"), dict):
                pending.append(group["envelope"])
    return reachable


def referenced_store(groups, store):
    """The stored envelopes these carry groups reference, directly or through their origins."""
    direct = {group["envelope_sha256"]: store[group["envelope_sha256"]] for group in groups}
    return {**direct, **carried_store(direct.values(), store)}


def local_envelope_store(proof):
    """Earlier envelopes a local signed receipt references; runners before the store wrote none."""
    path = proof / "envelopes.json"
    if not path.exists():
        return {}
    store = json.loads(path.read_bytes())
    if not isinstance(store, dict):
        raise RuntimeError("Invalid local carried evidence store")
    return store


def verify_signed_payload(envelope, policy_ref, signers_ref=None, kind=None):
    """Authenticate one explicit receipt purpose without promoting a launch plan."""
    payload = base64.b64decode(envelope["payload"], validate=True)
    receipt = json.loads(payload)
    namespaces = {None: NAMESPACE, "completed-profiles": NAMESPACE + "-checkpoint",
                  "execution-plan": NAMESPACE + "-plan"}
    if kind not in namespaces or receipt.get("kind") != kind:
        raise RuntimeError("Wrong receipt kind; completed profiles do not satisfy full acceptance")
    signer_id = envelope["signer"]
    registry = json.loads(tracked(signers_ref or policy_ref, SIGNERS))["signers"]
    signer = registry.get(signer_id)
    if not signer:
        raise RuntimeError(f"Unenrolled acceptance signer: {signer_id}")
    if receipt.get("host", {}).get("system") != signer["platform"]:
        raise RuntimeError("Signer used the wrong native platform")
    with tempfile.TemporaryDirectory(prefix="scode-verify-") as folder:
        directory = Path(folder)
        (directory / "allowed").write_text(f"{signer_id} {signer['public_key']}\n", encoding="utf-8")
        (directory / "proof.sig").write_bytes(base64.b64decode(envelope["signature"], validate=True))
        result = subprocess.run(["ssh-keygen", "-Y", "verify", "-f", str(directory / "allowed"),
                                 "-I", signer_id, "-n", namespaces[kind],
                                 "-s", str(directory / "proof.sig")],
                                input=payload, capture_output=True)
        if result.returncode:
            raise RuntimeError("Invalid SSH acceptance signature")
    # Version 2 receipts reference carried envelopes by SHA-256; version 1
    # receipts embed them. Every other version is unknown and refused.
    if (receipt.get("version") not in (1, 2) or isinstance(receipt.get("version"), bool)
            or receipt.get("repository") != load_policy(policy_ref)["repository"]):
        raise RuntimeError("Wrong acceptance schema or repository")
    return receipt, signer_id, signer


def verify_execution_plan(envelope, head, base, policy_ref, signers_ref=None):
    """Authorize early native scheduling; this proves no completed tests."""
    receipt, _identity, _signer = verify_signed_payload(
        envelope, policy_ref, signers_ref, kind="execution-plan")
    if receipt.get("head") != head or receipt.get("tree") != value("rev-parse", f"{head}^{{tree}}"):
        raise RuntimeError("Execution plan is for another commit or tree")
    if receipt.get("base") != value("merge-base", base, head):
        raise RuntimeError("Execution plan used a different PR base")
    protected = value("rev-parse", f"{policy_ref}^{{commit}}")
    planned_policy = receipt.get("policy")
    if (not isinstance(planned_policy, str) or not re.fullmatch(r"[a-f0-9]{40}", planned_policy)
            or git("merge-base", "--is-ancestor", planned_policy, protected, check=False).returncode):
        raise RuntimeError("Execution plan policy is not an ancestor of the protected policy")
    if (receipt.get("contract_sha256") != contract_hash(planned_policy)
            or changed_contract_files(head, base)):
        raise RuntimeError("Execution plan requires its original protected rules and unchanged source contract; use contract-update for rule changes")
    workflow = ".github/workflows/local-acceptance.yml"
    planned_workflow = tracked(planned_policy, workflow)
    current_workflow = tracked(protected, workflow)
    cache_only_workflow_change = planned_workflow != current_workflow
    if (tracked(head, workflow) != planned_workflow or (cache_only_workflow_change
            and not reviewed_native_dependency_cache_transition(planned_workflow, current_workflow, protected))):
        raise RuntimeError("Early native scheduling requires the current protected native workflow definition; integrate that workflow before starting")
    if receipt.get("checks") != {}:
        raise RuntimeError("An execution plan must contain no completed checks")
    policy = load_policy(policy_ref)
    names = selected(policy, changed_paths(receipt["base"], head), base=receipt["base"], head=head)
    plan = acceptance_plan(policy, names, receipt["host"])
    if not names or not compatible_acceptance_plan(policy, names, receipt["host"], receipt.get("plan")):
        raise RuntimeError("Execution plan differs from the protected source/author responsibility plan")
    if receipt["plan"] != plan:
        print("LOCAL_ACCEPTANCE_FULL_WORKSPACE_PLAN_REUSED: original signed full complementary plan retained; "
              "no checks executed or signatures changed by this responsibility proof.", flush=True)
    if contract_hash(planned_policy) != contract_hash(protected):
        before_inputs = execution_inputs(planned_policy)
        current_inputs = execution_inputs(protected)
        before = reachable_profile_commands_hashes(planned_policy, names, inputs=before_inputs)
        current = reachable_profile_commands_hashes(protected, names, inputs=current_inputs)
        selector_inputs_changed = not compatible_git_selector_inputs(before_inputs, current_inputs)
        changed = [name for name in names if before[name] != current[name]]
        resources = resource_equivalent_profiles(planned_policy, protected, changed)
        approved = approved_equivalent_profiles(before, current, protected, changed, head)
        dispatcher_matches = compatible_native_dispatcher(planned_policy, protected)
        if selector_inputs_changed or set(changed) - resources - approved.keys() or not dispatcher_matches:
            print("LOCAL_ACCEPTANCE_PLAN_RECOVERY=" + canonical({
                "version": 1, "run_on": "original author native host", "head": head,
                "original_policy": planned_policy, "current_policy": protected,
                "changed_profiles": sorted(names if selector_inputs_changed else set(changed) - resources - approved.keys()),
                "native_dispatcher_changed": not dispatcher_matches,
                "executor_git_selector_inputs_changed": selector_inputs_changed,
                "same_head_start_pr_renewal_supported": False,
                "next": "Preserve completed signed progress. Inspect actual changed profiles/dispatcher; "
                        "use the supported fresh-source plan route when required. Do not overwrite or re-sign the old plan.",
            }).decode(), file=sys.stderr, flush=True)
            raise RuntimeError("Execution plan commands or native dispatcher changed; this old plan cannot schedule current execution. "
                               "Publish a fresh source/plan from the original author host; all current checks remain required.")
        if approved:
            print("LOCAL_ACCEPTANCE_PLAN_DECLARED_EQUIVALENCE=" + canonical({
                "head": head, "original_policy": planned_policy, "current_policy": protected,
                "profiles": sorted(approved),
                "conditional_profiles": sorted(name for name, kind in approved.items() if kind == "without_test_groups"),
                "original_plan_and_signatures_unchanged": True, "checks_executed_by_this_proof": False,
                "full_acceptance_still_required": True,
            }).decode(), flush=True)
        if resources or tracked(planned_policy, ".github/local-acceptance/cross_platform.py") != tracked(protected, ".github/local-acceptance/cross_platform.py"):
            print("LOCAL_ACCEPTANCE_PLAN_RESOURCE_EQUIVALENCE=" + canonical({
                "head": head, "original_policy": planned_policy, "current_policy": protected,
                "profiles": sorted(resources), "dispatcher": "unchanged_or_known_compiler_budget_forms",
                "original_plan_and_signatures_unchanged": True, "full_acceptance_still_required": True,
            }).decode(), flush=True)
        print("LOCAL_ACCEPTANCE_PLAN_REUSED: protected metadata advanced; original contract/signature, source, "
              "author/native responsibility, selected command effects and native workflow verified; "
              "dispatcher unchanged or restricted to known compiler budgets.", flush=True)
    if cache_only_workflow_change:
        print("LOCAL_ACCEPTANCE_PLAN_DEPENDENCY_CACHE_ONLY: a reviewed cache-key change; "
              "the original signature, checks and complementary jobs remain required.", flush=True)
    return receipt


def read_execution_plan(head, base, policy_ref, signers_ref=None):
    """Read only a separately signed scheduling note; never a completion proof."""
    errors, valid = [], []
    for envelope in read_note(head, PLAN_NOTE_REF)["attestations"].values():
        try:
            valid.append(verify_execution_plan(envelope, head, base, policy_ref, signers_ref))
        except (RuntimeError, ValueError, KeyError) as error:
            errors.append(str(error))
    if valid:
        if any(receipt["plan"] != valid[0]["plan"] for receipt in valid):
            raise RuntimeError("Frozen source has conflicting signed author plans; keep the original author responsibility or publish a new head")
        return valid[0]
    raise RuntimeError("No current valid execution plan: " + ("; ".join(errors) or "signed plan missing"))


def verify_envelope(envelope, head, base, policy_ref, signers_ref=None, *, store=None, depth=0, trail=(),
                    completed_only=False):
    """Verify one signed receipt and, recursively, every envelope it carries from.

    `store` resolves version 2 carry references. `depth` and `trail` bound the
    recursion: origins nest at most MAX_CARRY_DEPTH deep and never repeat.
    """
    receipt, signer_id, signer = verify_signed_payload(
        envelope, policy_ref, signers_ref, kind="completed-profiles" if completed_only else None)
    if receipt.get("head") != head or receipt.get("tree") != value("rev-parse", f"{head}^{{tree}}"):
        raise RuntimeError("Acceptance is for another commit or tree")
    if receipt.get("base") != value("merge-base", base, head):
        raise RuntimeError("Acceptance used a different PR base")
    policy = load_policy(policy_ref)
    required = selected(policy, changed_paths(receipt["base"], head), base=receipt["base"], head=head)
    if not required:
        raise RuntimeError("Acceptance selected no actual checks; an empty contract cannot authorize a passing receipt")
    checks = receipt.get("checks", {})
    if completed_only and (not checks or not set(checks).issubset(policy["checks"])):
        raise RuntimeError("Checkpoint must contain nonempty known completed profiles from its protected plan")
    profiles = list(checks) if completed_only else required
    if {"commands_sha256", "profile_commands_sha256", "reachable_profile_commands_sha256"} & receipt.keys():
        compatible = compatible_profile_commands(receipt, policy_ref, profiles)
    else:
        # Bootstrap receipts bind the original protected base, including the
        # command environment helper. A match on the old command hash alone
        # cannot preserve evidence after one of those inputs changes.
        try:
            original_inputs = execution_inputs(receipt["base"])
            current_inputs = execution_inputs(policy_ref)
            compatible = (receipt.get("contract_sha256") == contract_hash(receipt["base"])
                          and commands_hash(receipt["base"], inputs=original_inputs)
                          == commands_hash(policy_ref, inputs=current_inputs)
                          and compatible_git_selector_inputs(original_inputs, current_inputs))
        except RuntimeError:
            compatible = False
    if not compatible:
        required_policy = value("rev-parse", policy_ref)
        # CI diagnostics can outlive another protected-main update. Preserve
        # explicit experimental/candidate snapshots, but refresh mainline rules.
        mainline = git("merge-base", "--is-ancestor", required_policy, "origin/main", check=False).returncode == 0
        recovery_policy = "origin/main" if mainline else required_policy
        recovery_base = "origin/main" if mainline else base
        print("LOCAL_ACCEPTANCE_RECOVERY=" + json.dumps({
            "version": 1, "run_on": "original author native host",
            "ci_proof_host_is_not_the_author_test_host": True,
            "runner": "<git-common-dir>/local-acceptance-hooks/local_acceptance.py",
            "observed_required_policy": required_policy,
            "policy_source": "current protected main" if mainline else "requested immutable snapshot",
            "prepare": [["git", "fetch", "--no-tags", "origin",
                         "+refs/heads/main:refs/remotes/origin/main"]] if mainline else [],
            "args": ["check", "--head", head, "--base", recovery_base,
                     "--policy-ref", recovery_policy, "--publish", "--remote", "origin"],
            "next": "On the original author host, run prepare first, then runner with args; "
                    "publish refreshed signed evidence and rerun the acceptance workflow",
        }), file=sys.stderr, flush=True)
        raise RuntimeError(
            f"Acceptance commands changed; refresh/publish on the AUTHOR native host "
            f"using {'freshly fetched protected main' if mainline else 'the requested snapshot'} "
            f"(observed policy {required_policy}); follow LOCAL_ACCEPTANCE_RECOVERY, then rerun CI")
    check_contract_changes(head, base, policy_ref)
    for name, group in carried_profiles(receipt).items():
        if name not in checks:
            raise RuntimeError(f"Carried {name} is missing from the signed checks")
    carriers = (*trail, envelope_identity(envelope))
    for group in receipt.get("carried", []):
        origin = verify_carried(group, head, receipt["base"], base, policy_ref, receipt["host"], signers_ref,
                                store=store, version=receipt["version"], depth=depth + 1, trail=carriers)
        for name in group["profiles"]:
            if checks[name] != origin["checks"][name]:
                raise RuntimeError(f"Carried {name} differs from its origin's signed execution")
    if not completed_only and not set(required).issubset(checks):
        raise RuntimeError(f"Acceptance check set differs from policy: need {required}")
    for name, commands in checks.items():
        if not isinstance(commands, list) or not commands:
            raise RuntimeError(f"Missing actual commands for {name}")
        for command in commands:
            if command.get("exit_code") != 0 or not command.get("command") or len(command.get("log_sha256", "")) != 64:
                raise RuntimeError(f"Incomplete/failed command in {name}")
    plan = acceptance_plan(policy, required, receipt["host"])
    legacy_names = [name for name in policy["checks"] if name in checks]
    original_plan = receipt.get("plan", acceptance_plan(policy, legacy_names, receipt["host"]))
    declared = original_plan.get("local_checks") if isinstance(original_plan, dict) else None
    ordered = [name for name in policy["checks"] if isinstance(declared, list) and name in declared]
    if (declared != ordered or not set(checks).issubset(declared)
            or not compatible_acceptance_plan(policy, declared, receipt["host"], original_plan)):
        raise RuntimeError("Signed acceptance plan does not match the protected policy/author host")
    if original_plan != plan:
        print("Reusing unchanged completed execution from an earlier scope; current protected checks "
              "remain required and a separately signed full launch retains its original CI hosts.", flush=True)
    if not completed_only and "execution_plan" in receipt:
        launch = verify_execution_plan(receipt["execution_plan"], head, base, policy_ref, signers_ref)
        if launch["host"] != receipt["host"] or launch["plan"] != original_plan:
            raise RuntimeError("Completed author evidence differs from its signed parallel execution plan")
        plan = launch["plan"]
    if completed_only:
        print(f"Verified completed profiles for {head[:12]}: {', '.join(checks)}; full acceptance still required.", flush=True)
    else:
        report_plan(plan)
        print(f"Verified {head[:12]} on {signer['platform']} ({signer_id}): {', '.join(required)}", flush=True)
    return receipt


def verify(head, base, policy_ref, note_ref=NOTE_REF, signers_ref=None):
    errors = []
    note = read_note(head, note_ref)
    for envelope in note["attestations"].values():
        try:
            return verify_envelope(envelope, head, base, policy_ref, signers_ref, store=note.get("envelopes", {}))
        except (RuntimeError, ValueError, KeyError) as error:
            errors.append(str(error))
    raise RuntimeError("No current valid local acceptance: " + ("; ".join(errors) or "signed note missing"))


def remote_note_tip(remote, note_ref=NOTE_REF):
    existing = value("ls-remote", remote, note_ref).split()
    return existing[0] if existing else ""


def publish(head, envelope, remote, note_ref=NOTE_REF, store=None):
    # Use a private temporary notes ref. Fetch+lease retries preserve evidence
    # another author publishes concurrently; never overwrite their notes tree.
    temporary = f"refs/notes/scode-publish-{os.getpid()}"

    def tls_connect_eof(message):
        message = message.lower()
        return "tls connect error:" in message and "unexpected eof while reading" in message

    try:
        for attempt in range(5):
            try:
                previous = remote_note_tip(remote, note_ref)
                if previous:
                    git("fetch", "--no-tags", remote, f"+{note_ref}:{temporary}")
                    # Lease the tree the fetch actually observed.
                    previous = value("rev-parse", temporary)
                else:
                    git("update-ref", "-d", temporary)
                notes = read_note(head, temporary)
                notes["attestations"][envelope["signer"]] = envelope
                # Keep each envelope still referenced by an attestation once.
                kept = carried_store(notes["attestations"].values(), {**notes.get("envelopes", {}), **(store or {})})
                if kept:
                    notes["envelopes"] = kept
                else:
                    notes.pop("envelopes", None)
                git("notes", f"--ref={temporary}", "add", "-f", "-F", "-", head, data=canonical(notes))
                result = git("push", f"--force-with-lease={note_ref}:{previous}", remote,
                             f"{temporary}:{note_ref}", check=False)
                if result.returncode == 0:
                    return
                message = result.stderr.decode(errors="replace")
                try:
                    current = remote_note_tip(remote, note_ref)
                except RuntimeError as read_error:
                    if not tls_connect_eof(message):
                        raise RuntimeError(message) from read_error
                    raise
                if current == previous:
                    raise RuntimeError(message)
                if tls_connect_eof(message) and current == value("rev-parse", temporary):
                    print("Acceptance note reached the remote before the Git TLS connection closed.", flush=True)
                    return
                # A different remote tip can be a concurrent receive transaction.
                # Retry from that tree without overwriting its original notes.
            except RuntimeError as error:
                if not tls_connect_eof(str(error)) or attempt == 4:
                    raise
                print("Git TLS connection closed during acceptance note publication; retrying the same signed evidence.",
                      flush=True)
                time.sleep(0.5 * (attempt + 1))
        raise RuntimeError("Concurrent acceptance publishers: retry the push")
    finally:
        git("update-ref", "-d", temporary)


def carried_change_paths(origin, origin_base, head, base):
    """Paths whose change since `origin` belongs to this PR rather than to protected main.

    Protected main's commits carry their own acceptance, and a clean branch
    behind main merges without rerunning its profiles; merging main into the
    branch must not cost more than that. A path counts when it changed since
    the evidence ran and either differs from current main or was part of the PR
    then, so conflict resolutions and PR changes replaced by main's still count.
    """
    since = set(changed_paths(origin, head))
    return sorted(since & (set(changed_paths(base, head)) | set(changed_paths(origin_base, origin))))


def verify_carried(group, head, base, base_ref, policy_ref, host_info, signers_ref=None, *,
                   store=None, version=2, depth=1, trail=()):
    """Check candidate recipe compatibility under the publication authority's trust.

    `version` is the carrying receipt's; `depth` counts this origin's carrying
    receipts and `trail` holds their envelope identities.
    """
    origin, profiles = (group.get("head"), group.get("profiles")) if isinstance(group, dict) else (None, None)
    if (not isinstance(origin, str) or origin == head or not isinstance(profiles, list) or not profiles
            or "preflight" in profiles):
        raise RuntimeError("Carried evidence must name an earlier origin head and its non-preflight profiles")
    if depth > MAX_CARRY_DEPTH:
        raise RuntimeError(f"Carried evidence nests more than {MAX_CARRY_DEPTH} origins deep; run its profiles again")
    envelope = carried_envelope(group, store or {}, version)
    if envelope_identity(envelope) in trail:
        raise RuntimeError("Carried evidence refers back to an envelope that carries it")
    if git("merge-base", "--is-ancestor", origin, head, check=False).returncode:
        raise RuntimeError("Carried evidence origin is not an ancestor of this head")
    payload = json.loads(base64.b64decode(envelope["payload"], validate=True))
    if not isinstance(payload.get("commands_ref"), str):
        raise RuntimeError("Carried evidence predates attested commands")
    origin_policy = value("rev-parse", payload["commands_ref"])
    authority_ref = signers_ref or policy_ref
    if git("merge-base", "--is-ancestor", origin_policy, authority_ref, check=False).returncode:
        raise RuntimeError(
            "Carried evidence policy is not an ancestor of the current protected policy: "
            f"origin={origin_policy}, protected={authority_ref}. "
            "If this is an earlier unmerged contract update, land its reviewed PR through CI and the merge queue, "
            "fetch protected main, then rerun the acceptance or push command. "
            "Otherwise produce fresh evidence under the current protected policy. "
            "Preserve the original evidence; do not relax the ancestry check."
        )
    receipt = verify_envelope(envelope, origin, base_ref, origin_policy, signers_ref=authority_ref,
                              completed_only=payload.get("kind") == "completed-profiles",
                              store=store, depth=depth, trail=trail)
    if receipt["host"] != host_info:
        raise RuntimeError("Carried evidence belongs to a different native host")
    affected = set(selected(load_policy(policy_ref), carried_change_paths(origin, receipt["base"], head, base)))
    for name in profiles:
        if (name not in receipt["checks"] or name in affected
                or not compatible_profile_commands(receipt, policy_ref, [name])):
            raise RuntimeError(f"Changes since {origin[:12]} require {name} to run again")
    return receipt


def carried_profiles(receipt):
    """Profile name -> the group that carried it into this receipt."""
    groups = receipt.get("carried", [])
    if not isinstance(groups, list):
        raise RuntimeError("Carried evidence must be a list of origin groups")
    owners = {}
    for group in groups:
        for name in group.get("profiles", []) if isinstance(group, dict) else []:
            if name in owners:
                raise RuntimeError(f"{name} is carried from more than one origin")
            owners[name] = group
    return owners


def carry_checks(head, base, base_ref, policy_ref, output, needed, signers_ref=None, store=None):
    """Carry signed profiles from this branch's earlier heads when later changes do not select them.

    Same-host evidence only. Each carried group references its origin's signed
    envelope by SHA-256, and the returned store keeps those envelopes' exact
    bytes once, so the verifier re-checks the origin, its ancestry and the
    change set instead of trusting this receipt's copy. A candidate command
    policy never substitutes for the protected authority used at publication.
    """
    remaining = [name for name in needed if name != "preflight"]
    carried, groups, refused, store = {}, {}, set(), dict(store or {})
    for candidate in value("rev-list", "--topo-order", head, "--not", base_ref).splitlines()[1:200]:
        if not remaining:
            break
        for directory in local_output_directories(candidate):
            try:
                proof = local_proof_directory(directory)
                envelope = json.loads((proof / "envelope.json").read_text())
                receipt = json.loads(base64.b64decode(envelope["payload"], validate=True))
                if receipt.get("head") != candidate or receipt.get("host") != host():
                    continue
                owners = carried_profiles(receipt)
                stored = local_envelope_store(proof)
            except (OSError, ValueError, KeyError, RuntimeError):
                continue
            # The candidate's own executions, then the groups it carried itself.
            sources = [(candidate, envelope, [name for name in receipt.get("checks", {}) if name not in owners])]
            for group in receipt.get("carried", []):
                try:
                    sources.append((group["head"], carried_envelope(group, stored, receipt.get("version")),
                                    group["profiles"]))
                except (RuntimeError, KeyError, TypeError) as error:
                    print(f"Not carrying a group {candidate[:12]} carried: {error}", flush=True)
            for origin, origin_envelope, available in sources:
                reference = envelope_identity(origin_envelope)
                lookup = {**stored, reference: origin_envelope}
                for name in [name for name in remaining if name in available]:
                    key = (origin, name)
                    if key in refused:
                        continue
                    try:
                        addition = {reference: origin_envelope, **carried_store([origin_envelope], lookup)}
                        if len(canonical({**store, **addition})) > CARRY_STORE_BUDGET:
                            raise RuntimeError(f"its earlier envelopes would exceed the {CARRY_STORE_BUDGET}-byte "
                                               "carried evidence budget of one note")
                        origin_receipt = verify_carried({"head": origin, "profiles": [name], "envelope_sha256": reference},
                                                        head, base, base_ref, policy_ref, host(), signers_ref,
                                                        store=lookup)
                        logs = verified_profile_logs(directory, origin_receipt, name, "Carried")
                    except (RuntimeError, ValueError, KeyError, subprocess.CalledProcessError) as error:
                        refused.add(key)
                        print(f"Not carrying {name} from {origin[:12]}: {error}", flush=True)
                        continue
                    target = output / name
                    target.mkdir()
                    for filename, contents in logs.items():
                        (target / filename).write_bytes(contents)
                    carried[name] = origin_receipt["checks"][name]
                    store.update(addition)
                    groups.setdefault((origin, reference), {"head": origin, "envelope_sha256": reference,
                                                            "profiles": []})["profiles"].append(name)
                    remaining.remove(name)
            if not remaining:
                break
    return carried, [{**group, "profiles": sorted(group["profiles"])} for group in groups.values()], store


def local_proof_directory(directory):
    """Read one atomic checkpoint pointer to an immutable signed generation."""
    if (directory / "envelope.json").is_file():
        return directory
    pointer = directory / "checkpoint.json"
    if pointer.exists():
        value = json.loads(pointer.read_bytes())
        generation = value.get("generation")
        if value.get("version") != 1 or not isinstance(generation, str) or not re.fullmatch(r"[0-9]+", generation):
            raise RuntimeError("Invalid checkpoint generation; keep the original signed evidence")
        proof = directory / "checkpoints" / generation
        if (pointer.is_symlink() or proof.is_symlink() or (directory / "checkpoints").is_symlink()
                or not proof.resolve().is_relative_to(directory.resolve())):
            raise RuntimeError("Checkpoint generation escaped its acceptance output")
        return proof
    # Earlier runners wrote one checkpoint directly in this directory.
    return directory / "checkpoint"


def local_output_directories(head):
    """Discover default outputs and external locators; a locator grants no trust."""
    parent = Path.home() / ".nexus/local-acceptance/results" / head
    if not parent.is_dir():
        return []
    entries = []
    try:
        paths = list(parent.iterdir())
    except OSError as error:
        print(f"Automatic local progress discovery is unavailable: {error}; required checks remain.", flush=True)
        return []
    for path in paths:
        try:
            entries.append((path.stat().st_mtime_ns, path.name, path))
        except OSError:
            continue
    directories = []
    for _, _, path in sorted(entries, reverse=True):
        if path.is_dir():
            directory = path
        elif path.name.startswith("external-") and path.suffix == ".json":
            try:
                value = json.loads(path.read_bytes())
                if (not isinstance(value, dict) or value.get("version") != 1 or value.get("head") != head
                        or not isinstance(value.get("directory"), str)):
                    continue
                directory = Path(value["directory"])
                if not directory.is_absolute() or not directory.is_dir():
                    continue
            except (OSError, ValueError, TypeError):
                continue
        else:
            continue
        directory = directory.resolve()
        if directory not in directories:
            directories.append(directory)
    return directories


def register_local_output(head, directory):
    """Index an existing output without modifying or re-signing its evidence."""
    if not re.fullmatch(r"[a-f0-9]{40}", head):
        raise RuntimeError("Invalid local progress head")
    directory = Path(directory).resolve()
    parent = (Path.home() / ".nexus/local-acceptance/results" / head).resolve()
    if directory.parent == parent:
        return directory
    parent.mkdir(parents=True, exist_ok=True)
    reference = parent / ("external-" + sha(str(directory).encode("utf-8")) + ".json")
    existed = reference.exists()
    temporary = reference.with_suffix(f".{os.getpid()}.next")
    temporary.write_bytes(canonical({"version": 1, "head": head, "directory": str(directory)}))
    os.replace(temporary, reference)
    if not existed:
        print("LOCAL_ACCEPTANCE_OUTPUT_INDEX=" + canonical({"head": head, "directory": str(directory),
              "reference": str(reference), "scope": "discovery only; signatures, source, host, recipes and logs are reverified"}).decode(), flush=True)
    return reference


def verified_profile_logs(directory, receipt, name, context):
    """Verify original command log bytes before copying or indexing them."""
    logs = {path.name: path.read_bytes() for path in (directory / name).glob("*.log") if path.is_file()}
    digests = {filename: sha(contents) for filename, contents in logs.items()}
    expected = {command["log_sha256"] for command in receipt["checks"][name]}
    if not expected.issubset(set(digests.values())):
        raise RuntimeError(f"{context} log missing or changed for {name}")
    return {filename: contents for filename, contents in logs.items() if digests[filename] in expected}


def index_existing_output(directory, policy_ref):
    """Validate original signed progress before adding only its discovery locator."""
    directory = Path(directory).resolve()
    proof = local_proof_directory(directory)
    envelope = json.loads((proof / "envelope.json").read_bytes())
    payload = base64.b64decode(envelope["payload"], validate=True)
    if payload != (proof / "receipt.json").read_bytes():
        raise RuntimeError("Indexed receipt differs from its signed payload")
    previous = json.loads(payload)
    origin_policy = value("rev-parse", previous["commands_ref"])
    if git("merge-base", "--is-ancestor", origin_policy, policy_ref, check=False).returncode:
        raise RuntimeError("Indexed policy is not an ancestor of the current protected policy")
    receipt = verify_envelope(envelope, previous["head"], previous["base"], origin_policy,
                              signers_ref=policy_ref, completed_only=previous.get("kind") == "completed-profiles",
                              store=local_envelope_store(proof))
    if receipt["host"] != host():
        raise RuntimeError("Indexed receipt belongs to a different native host")
    for name in receipt["checks"]:
        verified_profile_logs(directory, receipt, name, "Indexed")
    register_local_output(receipt["head"], directory)
    print(f"Indexed original signed progress for {receipt['head'][:12]}. No checks ran; "
          "reuse still verifies the source, host, recipe, signature and original logs.", flush=True)


def prepare_resume_checks(directory, head, base, policy_ref, needed, signers_ref):
    """Read and verify exact-source resume evidence before copying any logs."""
    directory = Path(directory).resolve()
    proof = local_proof_directory(directory)
    completed_only = proof != directory
    envelope = json.loads((proof / "envelope.json").read_text())
    payload = base64.b64decode(envelope["payload"], validate=True)
    if payload != (proof / "receipt.json").read_bytes():
        raise RuntimeError("Resume receipt differs from its signed payload")
    previous = json.loads(payload)
    previous_policy = value("rev-parse", previous["commands_ref"])
    if git("merge-base", "--is-ancestor", previous_policy, policy_ref, check=False).returncode:
        raise RuntimeError("Resume policy is not an ancestor of the current protected policy")
    stored = local_envelope_store(proof)
    previous = verify_envelope(envelope, head, base, previous_policy, signers_ref=signers_ref or policy_ref,
                               completed_only=completed_only, store=stored)
    if previous["host"] != host():
        raise RuntimeError("Resume receipt belongs to a different native host")
    reusable, logs = {}, {}
    owners = carried_profiles(previous)
    for name in needed:
        if name not in previous["checks"] or not compatible_profile_commands(previous, policy_ref, [name]):
            continue
        logs[name] = verified_profile_logs(directory, previous, name, "Resume")
        reusable[name] = previous["checks"][name]
    # A reused profile that the earlier attempt carried keeps its origin,
    # referenced by SHA-256 even when that attempt embedded the envelope.
    groups, store = {}, {}
    for name in reusable:
        if name in owners:
            group = owners[name]
            origin = carried_envelope(group, stored, previous["version"])
            reference = envelope_identity(origin)
            store.update({reference: origin, **carried_store([origin], stored)})
            groups.setdefault((group["head"], reference), {"head": group["head"], "envelope_sha256": reference,
                                                           "profiles": []})["profiles"].append(name)
    return reusable, {"directory": str(directory), "receipt_sha256": sha(payload),
                      "commands_ref": previous_policy, "finished_at": previous["finished_at"],
                      "timing": previous.get("timing"),
                      "profiles": sorted(reusable)}, list(groups.values()), store, logs


def restore_resume_checks(prepared, output):
    reusable, resumed, groups, store, logs = prepared
    for name, files in logs.items():
        target = output / name
        target.mkdir()
        for filename, contents in files.items():
            (target / filename).write_bytes(contents)
    return reusable, resumed, groups, store


def resume_checks(directory, head, base, policy_ref, output, needed, signers_ref=None):
    """Reuse signed exact-source checks only while their commands and logs match."""
    return restore_resume_checks(prepare_resume_checks(directory, head, base, policy_ref, needed, signers_ref), output)


def sign_receipt(receipt, directory, identity, key, namespace=NAMESPACE):
    """Sign immutable local evidence; checkpoint signatures cannot authorize a push."""
    directory.mkdir(parents=True, exist_ok=True)
    payload = directory / "receipt.json"
    payload.write_bytes(canonical(receipt))
    signature = payload.with_suffix(".json.sig")
    subprocess.run(["ssh-keygen", "-Y", "sign", "-f", str(key), "-n", namespace, str(payload)], check=True)
    return {"signer": identity, "payload": base64.b64encode(payload.read_bytes()).decode(),
            "signature": base64.b64encode(signature.read_bytes()).decode()}


def save_completed_profiles(receipt, output, identity, key, head, base_ref, policy_ref, signers_ref=None, store=None):
    """Publish complete signed files before atomically advertising their generation."""
    clean(head)
    checkpoint = {**receipt, "kind": "completed-profiles", "finished_at": int(time.time())}
    generation = str(time.time_ns())
    directory = output / "checkpoints" / generation
    envelope = sign_receipt(checkpoint, directory, identity, key, NAMESPACE + "-checkpoint")
    verify_envelope(envelope, head, base_ref, policy_ref, signers_ref=signers_ref, completed_only=True, store=store)
    if store:
        (directory / "envelopes.json").write_bytes(canonical(store))
    (directory / "envelope.json").write_bytes(canonical(envelope))
    pointer = output / "checkpoint.next.json"
    pointer.write_bytes(canonical({"version": 1, "generation": generation}))
    os.replace(pointer, output / "checkpoint.json")
    try:
        register_local_output(head, output)
    except OSError as error:
        print(f"LOCAL_ACCEPTANCE_OUTPUT_INDEX_UNAVAILABLE: signed progress remains at {output}; "
              f"resume with --resume-from that directory. Index error: {error}", flush=True)


def resume_start(resumed):
    timing = resumed.get("timing")
    return timing.get("first_recorded_started_at") if isinstance(timing, dict) else None


def resume_local_outputs(head, base, base_ref, policy_ref, output, needed, signers_ref):
    """Keep the newest matching output unless an older output proves strictly more checks."""
    required = set(needed)
    selected = None
    profiles = set()
    first_start = None
    for directory in local_output_directories(head):
        try:
            proof = local_proof_directory(directory)
            if not (proof / "envelope.json").is_file():
                continue
            receipt = json.loads((proof / "receipt.json").read_bytes())
            if not isinstance(receipt, dict):
                raise RuntimeError("Invalid resume receipt metadata")
            if receipt.get("head") != head or receipt.get("base") != base or receipt.get("host") != host():
                continue
            # This inventory only rules out candidates that cannot improve the current selection.
            recorded = receipt.get("checks", {})
            if not isinstance(recorded, dict):
                raise RuntimeError("Invalid resume check inventory")
            if selected is not None and not profiles < required.intersection(recorded):
                continue
            prepared = prepare_resume_checks(directory, head, base_ref, policy_ref, needed, signers_ref)
        except (OSError, ValueError, KeyError, TypeError, RuntimeError, subprocess.CalledProcessError) as error:
            if selected is None:
                raise
            print(f"Not reusing additional local progress from {directory}: {error}", flush=True)
            continue
        candidate_profiles = set(prepared[0])
        if selected is None or profiles < candidate_profiles:
            selected, profiles = prepared, candidate_profiles
            start = resume_start(prepared[1])
            if type(start) in (int, float) and 0 < start <= time.time():
                first_start = start if first_start is None else min(first_start, start)
            if profiles == required:
                break
    if selected is None:
        return {}, None, [], {}
    reusable, resumed, groups, store = restore_resume_checks(selected, output)
    if first_start is not None and first_start != resume_start(resumed):
        resumed["earliest_verified_start_at"] = first_start
    return reusable, resumed, groups, store


def try_host_lock(descriptor):
    try:
        if os.name == "nt":
            import msvcrt
            os.lseek(descriptor, 0, os.SEEK_SET)
            msvcrt.locking(descriptor, msvcrt.LK_NBLCK, 1)
        else:
            import fcntl
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return True
    except OSError as error:
        if error.errno in {errno.EACCES, errno.EAGAIN}:
            return False
        raise


def release_host_lock(descriptor):
    if os.name == "nt":
        import msvcrt
        os.lseek(descriptor, 0, os.SEEK_SET)
        with contextlib.suppress(OSError):
            msvcrt.locking(descriptor, msvcrt.LK_UNLCK, 1)


def live_lease_tickets(queue, own):
    """Return waiting tickets in arrival order, removing those whose waiter exited.

    Each waiter locks its own ticket for its lifetime. A ticket whose lock can be
    taken belongs to a process that exited, including after a crash or kill. A
    ticket younger than ten seconds may not be locked yet and is kept.
    """
    live = [own]
    for ticket in queue.glob("*.ticket"):
        if ticket == own:
            continue
        try:
            if time.time() - ticket.stat().st_mtime < 10:
                live.append(ticket)
                continue
            descriptor = os.open(ticket, os.O_RDWR)
        except OSError:
            continue
        try:
            abandoned = try_host_lock(descriptor)
            if abandoned:
                release_host_lock(descriptor)
        finally:
            os.close(descriptor)
        if abandoned:
            with contextlib.suppress(OSError):
                ticket.unlink()
        else:
            live.append(ticket)
    return sorted(live)


def write_host_lease_record(descriptor, record):
    """Bind an acquisition to the existing lock, outside Windows' locked byte."""
    data = canonical(record)
    if len(data) > 4096:
        raise RuntimeError("Host acceptance lease record is too large")
    os.lseek(descriptor, 1, os.SEEK_SET)
    remaining = data
    while remaining:
        written = os.write(descriptor, remaining)
        if not written:
            raise OSError("Could not write the host acceptance lease record")
        remaining = remaining[written:]
    os.ftruncate(descriptor, len(data) + 1)


def read_host_lease_record(descriptor):
    """Read bounded ownership metadata without touching Windows' locked byte."""
    os.lseek(descriptor, 1, os.SEEK_SET)
    data = os.read(descriptor, 4097)
    if len(data) > 4096:
        raise ValueError("Host acceptance lease record is too large")
    return json.loads(data)


def verify_inherited_host_lease(claim, directory):
    """Only a live acquisition owned by this process or an ancestor may nest."""
    descriptor, acquired = None, False
    try:
        import psutil
        if not re.fullmatch(r"v1:[1-9][0-9]*:[0-9a-f]{64}", claim):
            raise ValueError("Unrecognized acquisition claim")
        lock = directory / "host.lock"
        descriptor = os.open(lock, os.O_RDWR)
        record = read_host_lease_record(descriptor)
        if (record.get("version") != 1 or record.get("active") is not True
                or type(record.get("pid")) is not int
                or claim != f"v1:{record['pid']}:{record.get('lease_id')}"):
            raise ValueError("Acquisition has ended or differs from the inherited claim")
        owner = psutil.Process(record["pid"])
        ancestor = psutil.Process()
        while ancestor != owner:
            ancestor = ancestor.parent()
            if ancestor is None:
                raise ValueError("The lease owner is not this process or an ancestor")
        if (not owner.is_running() or owner.status() == psutil.STATUS_ZOMBIE
                or owner.create_time() != record.get("process_created_at")):
            raise ValueError("The lease owner exited or its PID was reused")
        acquired = try_host_lock(descriptor)
        if acquired:
            raise ValueError("The acquisition no longer holds the operating-system lock")
        if (read_host_lease_record(descriptor) != record
                or not os.path.samestat(os.fstat(descriptor), lock.stat())
                or not owner.is_running()):
            raise ValueError("The acquisition changed during inheritance validation")
    except Exception as error:
        raise RuntimeError("Invalid inherited host acceptance lease: " + str(error)
                           + ". Do not invent or copy lease claims; finish the original owner "
                           "before starting a fresh top-level acceptance.") from error
    finally:
        if descriptor is not None:
            if acquired:
                release_host_lock(descriptor)
            os.close(descriptor)


@contextlib.contextmanager
def host_acceptance_lease(attempt, head):
    """Run one fresh reservation at a time on this host, in arrival order.

    Concurrent suites contend for CPU, memory and temporary paths, which turns
    timing budgets into failures that cost whole profile reruns. The operating
    system releases the lock when its holder exits, including after a crash or
    a memory kill. Adjacent fresh workspace and compile-only examples can share a
    reservation with an immediately following fresh resize check. Native services
    and compiler context can share one, as can adjacent fresh
    rendering and browser checks. Between reservations, fresh work rejoins
    the FIFO behind existing waiters. Processes started under
    the holder inherit its acquisition claim. A nested check
    validates the real locked record and process incarnation before proceeding.
    """
    directory = Path.home() / ".nexus/local-acceptance"
    claim = os.environ.get(HOST_LEASE_ENV)
    if claim:
        verify_inherited_host_lease(claim, directory)
        yield
        return
    queue = directory / "queue"
    queue.mkdir(parents=True, exist_ok=True)
    lock, holder = directory / "host.lock", directory / "host-lease.json"
    started = time.monotonic()
    ticket = queue / f"{time.time_ns():020d}-{os.getpid()}.ticket"
    waiting = os.open(ticket, os.O_RDWR | os.O_CREAT, 0o600)
    descriptor = None
    try:
        if not try_host_lock(waiting):
            raise RuntimeError(f"Cannot hold this waiter's acceptance queue ticket: {ticket}")
        descriptor = os.open(lock, os.O_RDWR | os.O_CREAT, 0o600)
        reported = None
        while True:
            tickets = live_lease_tickets(queue, ticket)
            if tickets[0] == ticket and try_host_lock(descriptor):
                break
            if reported is None or time.monotonic() - reported >= 600:
                try:
                    current = holder.read_text(encoding="utf-8").strip()
                except OSError:
                    current = "an unrecorded process"
                print(f"Waiting for this host's local acceptance lease ({lock}), held by {current}. "
                      f"Queue position {tickets.index(ticket) + 1} of {len(tickets)}; "
                      "this invocation rejoins the FIFO between fresh reservations; "
                      "eligible workspace/examples/resize, services/context and rendering/browser groups share a reservation.",
                      flush=True)
                reported = time.monotonic()
            time.sleep(1)
        waited = round(time.monotonic() - started, 3)
        attempt["host_lease_wait_seconds"] = round(attempt.get("host_lease_wait_seconds", 0.0) + waited, 3)
        acquired = time.time()
        attempt.setdefault("host_lease_acquisitions", []).append({
            "profile": attempt.get("active_profile"), "wait_seconds": waited, "acquired_at": acquired,
        })
        members = attempt.get("reservation_profiles", [])
        if len(members) > 1:
            attempt["host_lease_acquisitions"][-1]["profiles"] = list(members)
        import psutil
        record = {"version": 1, "active": True, "pid": os.getpid(),
                  "process_created_at": psutil.Process().create_time(),
                  "lease_id": secrets.token_hex(32), "head": head, "root": str(ROOT),
                  "acquired_at": acquired, "profile": attempt.get("active_profile")}
        if len(members) > 1:
            record["profiles"] = list(members)
        write_host_lease_record(descriptor, record)
        holder.write_bytes(canonical(record))
        print(f"Acquired this host's local acceptance lease after {waited:.3f}s; "
              f"cumulative wait {attempt['host_lease_wait_seconds']:.3f}s",
              flush=True)
        # The holder no longer waits; the next ticket becomes the queue head.
        release_host_lock(waiting)
        os.close(waiting)
        waiting = None
        with contextlib.suppress(OSError):
            ticket.unlink()
        os.environ[HOST_LEASE_ENV] = f"v1:{record['pid']}:{record['lease_id']}"
        try:
            yield
        finally:
            try:
                inactive = {**record, "active": False}
                write_host_lease_record(descriptor, inactive)
                holder.write_bytes(canonical(inactive))
            finally:
                os.environ.pop(HOST_LEASE_ENV, None)
                release_host_lock(descriptor)
    finally:
        if descriptor is not None:
            os.close(descriptor)
        if waiting is not None:
            release_host_lock(waiting)
            os.close(waiting)
            with contextlib.suppress(OSError):
                ticket.unlink()


def check(args):
    """Record failed and successful local attempts, including time before CI."""
    started = getattr(args, "outer_monotonic_started_at", time.monotonic())
    attempt = {"version": 1, "started_at": getattr(args, "outer_started_at", time.time()), "outcome": "running"}
    try:
        perform_check(args, attempt)
        attempt["outcome"] = "success"
    except BaseException as error:
        attempt["outcome"] = "interrupted" if isinstance(error, KeyboardInterrupt) else "failure"
        attempt["error_type"] = type(error).__name__
        raise
    finally:
        attempt.update(finished_at=time.time(), elapsed_seconds=round(time.monotonic() - started, 3))
        # This diagnostic never grants acceptance or replaces signed evidence.
        if "output" in attempt:
            try:
                (Path(attempt["output"]) / "attempt.json").write_bytes(canonical(attempt))
            except OSError as error:
                print(f"Could not save local attempt timing: {error}", file=sys.stderr, flush=True)
        print("LOCAL_ACCEPTANCE_TIMING=" + canonical(attempt).decode(), flush=True)


def scheduled_checks(needed):
    """Keep service bootstrap ahead of expensive profiles after workspace setup."""
    ordered = list(needed)
    if "native-services" in ordered:
        position = next((index for index, name in enumerate(ordered)
                         if name in {"native-release", "rendering", "browser"}), len(ordered))
        if "native-workspace" in ordered:
            position = max(position, ordered.index("native-workspace") + 1)
        service = ordered.index("native-services")
        if position < service:
            ordered.insert(position, ordered.pop(service))
    return ordered


def profile_reservations(ordered, pending):
    """Group eligible fresh profiles while preserving their separate signed results."""
    ordered, pending = list(ordered), set(pending)
    if {"native-services", "native-context"} <= pending:
        # Both build the same native target. Keep the original commands and
        # move only past the independent container benchmark profiles.
        service = ordered.index("native-services")
        context = ordered.index("native-context")
        if service < context and set(ordered[service + 1:context]) <= {"rendering", "browser"}:
            ordered.insert(service + 1, ordered.pop(context))

    shared_pairs = {
        ("native-workspace", "native-examples"),
        ("native-services", "native-context"),
        ("rendering", "browser"),
    }
    reservations, index = [], 0
    while index < len(ordered):
        workspace = ordered[index:index + 3]
        if (workspace == ["native-workspace", "native-examples", "native-resize"]
                and pending.issuperset(workspace)):
            reservations.append(workspace)
            index += len(workspace)
            continue
        pair = ordered[index:index + 2]
        if tuple(pair) in shared_pairs and set(pair) <= pending:
            reservations.append(pair)
            index += 2
        else:
            reservations.append([ordered[index]])
            index += 1
    return reservations


def materialize_profile_module(policy_ref, output):
    """Load protected profile code and its pinned companion from Git objects."""
    import importlib.util

    profiles = output / "protected_profiles.py"
    profiles.write_bytes(tracked(policy_ref, ".github/local-acceptance/checks.py"))
    companion = ".github/local-acceptance/compiler_cache.py"
    if git("ls-tree", "--name-only", policy_ref, "--", companion).stdout.strip():
        (output / "compiler_cache.py").write_bytes(tracked(policy_ref, companion))
    spec = importlib.util.spec_from_file_location("acceptance_checks", profiles)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def preflight_plan_compatibility(plan, names, author, policy_ref, signers_ref):
    """Refuse an unsupported plan before checks; final verification still applies."""
    if policy_ref == signers_ref:
        return
    # The accepted parser receives metadata, never this executor's namespace,
    # credentials or hook selectors. Its temporary source comes from immutable
    # protected Git bytes; no candidate profile or signing operation is loaded.
    driver = '''import json,runpy,sys
protected=runpy.run_path(sys.argv[1],run_name="protected_acceptance_planner")
payload=json.load(sys.stdin)
compatible=protected.get("compatible_acceptance_plan")
supported=(compatible(payload["policy"],payload["names"],payload["author"],payload["plan"])
           if callable(compatible) else
           protected["acceptance_plan"](payload["policy"],payload["names"],payload["author"])==payload["plan"])
if type(supported) is not bool:
    raise TypeError("Protected acceptance planner returned a non-boolean result")
print("LOCAL_ACCEPTANCE_PLAN_PARSER="+json.dumps({"supported":supported}))
'''
    environment = {key: value for key, value in os.environ.items()
                   if key.upper() in {"SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT", "PATH", "TEMP", "TMP"}}
    with tempfile.TemporaryDirectory(prefix="scode-plan-admission-") as temporary:
        protected = Path(temporary) / "protected_planner.py"
        protected.write_bytes(tracked(signers_ref, ".github/scripts/local_acceptance.py"))
        result = subprocess.run([sys.executable, "-I", "-c", driver, str(protected)],
                                input=canonical({"policy": load_policy(policy_ref), "names": names,
                                                 "author": author, "plan": plan}),
                                cwd=temporary, env=environment, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, timeout=30)
    if result.returncode:
        raise RuntimeError("Protected plan parser preview failed before checks: "
                           + result.stderr.decode("utf-8", errors="replace")[-4000:])
    text = result.stdout.decode("utf-8", errors="replace")
    records = [line.split("=", 1)[1] for line in text.splitlines()
               if line.startswith("LOCAL_ACCEPTANCE_PLAN_PARSER=")]
    if len(records) != 1:
        raise RuntimeError("Protected plan parser preview returned malformed metadata")
    verdict = json.loads(records[0])
    if not isinstance(verdict, dict) or set(verdict) != {"supported"} or type(verdict["supported"]) is not bool:
        raise RuntimeError("Protected plan parser preview returned a non-boolean verdict")
    print(text, end="", flush=True)
    supported = verdict["supported"]
    if not supported:
        raise RuntimeError(
            "The protected verifier cannot validate this candidate acceptance plan. "
            "Land a compatible verifier with the production policy unchanged first, "
            "then qualify the policy activation through contract-update. "
            "Candidate source and original receipts were preserved; no checks started.")
    print("LOCAL_ACCEPTANCE_PLAN_ADMISSION: current protected parser accepts the proposed plan; "
          "source, signatures, every selected check and final publication verification remain required.", flush=True)


def perform_check(args, attempt):
    head = value("rev-parse", args.head or "HEAD")
    clean(head)
    policy_ref = value("rev-parse", args.policy_ref)
    base_ref = value("rev-parse", args.base)
    # Older managed parents already pass their protected base and the exact
    # candidate head. Preserve that authority before those parents are upgraded.
    default_authority = base_ref if policy_ref == head else policy_ref
    signers_ref = value("rev-parse", getattr(args, "signers_ref", None) or default_authority)
    # A candidate cannot replace its required command definitions unnoticed.
    check_contract_changes(head, base_ref, policy_ref)
    base = value("merge-base", base_ref, head)
    print(f"Local acceptance inputs: head={head[:12]} base={base[:12]} policy={policy_ref[:12]} "
          f"authority={signers_ref[:12]}", flush=True)
    needed = selected(load_policy(policy_ref), changed_paths(base, head), base=base, head=head)
    if not needed:
        raise RuntimeError("Acceptance selected no actual checks; keep at least one real command before contract-update")
    plan = acceptance_plan(load_policy(policy_ref), needed, host())
    execution_plan = getattr(args, "execution_plan", None)
    if execution_plan is None and read_note(head, PLAN_NOTE_REF)["attestations"]:
        prior = read_execution_plan(head, base_ref, policy_ref, signers_ref)
        execution_plan = next(item for item in read_note(head, PLAN_NOTE_REF)["attestations"].values()
                              if json.loads(base64.b64decode(item["payload"])) == prior)
    if execution_plan is not None:
        launch = verify_execution_plan(execution_plan, head, base_ref, policy_ref, signers_ref)
        if launch["host"] != host():
            raise RuntimeError("Use the original author host for this frozen execution plan")
        plan = launch["plan"]
    report_plan(plan, current_environment=True)
    preflight_plan_compatibility(plan, needed, host(), policy_ref, signers_ref)
    identity = value("config", "localAcceptance.signer")
    key = Path(value("config", "localAcceptance.key")).expanduser()
    registry = json.loads(tracked(policy_ref, SIGNERS))["signers"]
    if identity not in registry or registry[identity]["platform"] != host()["system"]:
        raise RuntimeError("Enroll this host's public signing key on main before running acceptance")
    output = Path(args.output or (Path.home() / ".nexus/local-acceptance/results" / head / str(time.time_ns()))).resolve()
    if output == ROOT or ROOT in output.parents:
        raise RuntimeError("Keep acceptance outputs outside the source checkout")
    output.mkdir(parents=True, exist_ok=False)
    attempt.update(head=head, tree=value("rev-parse", f"{head}^{{tree}}"), base=base,
                   branch=git("symbolic-ref", "--quiet", "--short", "HEAD", check=False).stdout.decode().strip() or None,
                   host=host(), policy_ref=policy_ref, signers_ref=signers_ref, output=str(output))
    (output / "attempt.json").write_bytes(canonical(attempt))
    resume_command = (f'python "{__file__}" check --head {head} --base {base_ref} --policy-ref {policy_ref} '
                      f'--signers-ref {signers_ref} --resume-from "{output}" --publish --remote {args.remote}')
    if getattr(args, "dispatch_publish", None):
        resume_command += f" --dispatch-publish {args.dispatch_publish}"
    print("Local acceptance timing includes this invocation's setup, checks and publication; "
          "signed resume timing retains the earliest recorded start. Older or manual attempts may be unknown.", flush=True)
    (output / "plan.json").write_bytes(canonical(plan))
    module = materialize_profile_module(policy_ref, output)
    # Each invocation loads a fresh module. Keep the dispatcher's existing API
    # so container-only context does not invalidate unrelated native receipts.
    module.run.execution_refs = (policy_ref, head)
    previous = getattr(args, "resume_from", None)
    if previous:
        reusable, resumed, groups, store = resume_checks(previous, head, base_ref, policy_ref, output, needed, signers_ref)
    else:
        reusable, resumed, groups, store = resume_local_outputs(head, base, base_ref, policy_ref, output, needed, signers_ref)
    # Earlier heads of this branch lend profiles that later changes do not select.
    carried, new_groups, store = carry_checks(head, base, base_ref, policy_ref, output,
                                              [name for name in needed if name not in reusable], signers_ref, store)
    for group in new_groups:
        existing = next((item for item in groups if item["head"] == group["head"]
                         and item["envelope_sha256"] == group["envelope_sha256"]), None)
        if existing:
            existing["profiles"] = sorted(existing["profiles"] + group["profiles"])
        else:
            groups.append(group)
    if carried:
        print("CARRIED_PROFILES=" + json.dumps({"head": head, "origins": {
            group["head"]: group["profiles"] for group in new_groups}}), flush=True)
    prior = resumed or {}
    prior_start = prior.get("earliest_verified_start_at", resume_start(prior))
    known_start = type(prior_start) in (int, float) and 0 < prior_start <= attempt["started_at"]
    timing = {"started_at": attempt["started_at"],
              "first_recorded_started_at": prior_start if known_start else attempt["started_at"],
              "prior_timing_available": known_start}
    attempt["first_recorded_started_at"] = timing["first_recorded_started_at"]
    results = {}
    inputs = execution_inputs(policy_ref)
    receipt = {"version": 1, "repository": load_policy(policy_ref)["repository"],
               "head": head, "tree": value("rev-parse", f"{head}^{{tree}}"), "base": base,
               "contract_sha256": contract_hash(policy_ref), "host": host(),
               "commands_sha256": commands_hash(policy_ref, inputs=inputs),
               "commands_ref": value("rev-parse", policy_ref),
               "profile_commands_sha256": profile_commands_hashes(policy_ref, needed, inputs=inputs),
               "reachable_profile_commands_sha256": reachable_profile_commands_hashes(policy_ref, needed, inputs=inputs),
               "finished_at": int(time.time()), "checks": results, "plan": plan, "timing": timing}
    if execution_plan is not None:
        receipt["execution_plan"] = execution_plan
    if resumed is not None:
        receipt["resumed_from"] = resumed

    def sign_carried():
        # A checkpoint lists only the carried profiles it already contains.
        # Version 2 marks carry groups that reference the envelope store.
        current = [{**group, "profiles": [name for name in group["profiles"] if name in results]} for group in groups]
        current = [group for group in current if group["profiles"]]
        if current:
            receipt["carried"], receipt["version"] = current, 2
        else:
            receipt.pop("carried", None)
            receipt["version"] = 1
        return referenced_store(current, store)

    def record_check_time():
        timing["checks_finished_at"] = time.time()
        # Count only completed acquisitions made by this invocation. A resumed
        # receipt can retain an earlier start without inheriting its wait time.
        timing["invocation_host_lease_wait_seconds"] = attempt["host_lease_wait_seconds"]
        timing["invocation_host_lease_acquisitions"] = list(attempt["host_lease_acquisitions"])

    def perform_profile(name):
        if name in reusable:
            print(f"Reusing signed {name} for {head[:12]}; original logs: {resumed['directory']}", flush=True)
            results[name] = reusable[name]
        elif name in carried:
            origin = next(group["head"] for group in groups if name in group["profiles"])
            print(f"Carrying signed {name} from {origin[:12]}; changes since then do not select it", flush=True)
            results[name] = carried[name]
        else:
            print(f"Running {name} on {host()['system']}; logs: {output}", flush=True)
            results[name] = module.run(name, ROOT, output / name, base, run_command)
        record_check_time()
        save_completed_profiles(receipt, output, identity, key, head, base_ref, policy_ref, signers_ref, sign_carried())
        print(f"Saved completed profiles ({len(results)}/{len(needed)}); unfinished profiles remain required. "
              f"If this process exits, resume with: {resume_command}", flush=True)

    serialized = scheduled_checks(name for name in needed if name != "preflight")
    fresh_profiles = set(serialized) - reusable.keys() - carried.keys()
    reservations = profile_reservations(serialized, fresh_profiles)
    serialized = [name for members in reservations for name in members]
    pending = [name for name in serialized if name in fresh_profiles]
    prepare_lint = getattr(module, "prepare_native_workspace_lint", None)
    lint_pending = bool(prepare_lint and "guard-contract" in pending and "native-workspace" in pending
                        and serialized.index("guard-contract") < serialized.index("native-workspace"))
    attempt["host_lease_wait_seconds"] = 0.0
    attempt["host_lease_acquisitions"] = []
    print("LOCAL_ACCEPTANCE_RESOURCE_SCOPE=" + canonical({
        "preflight": "metadata_before_host_lease" if "preflight" in needed else "not_selected",
        "serialized_checks": serialized,
        "pending_serialized_checks": pending,
        "workspace_lint_before_guard": lint_pending,
        "reservation": "fresh_workspace_examples_with_optional_resize_services_context_and_rendering_browser_share_one_each;_others_requeue",
        "pending_reservations": [members for members in reservations if any(name in pending for name in members)],
    }).decode(), flush=True)
    try:
        if "preflight" in needed:
            perform_profile("preflight")
        validate = getattr(module, "validate_before_host_lease", None)
        if validate and pending:
            validate(pending, ROOT, output)
        prepared = False
        if lint_pending:
            attempt["active_profile"] = "native-workspace-lint-preparation"
            with host_acceptance_lease(attempt, head):
                prepare = getattr(module, "prepare_native_checks", None)
                if prepare:
                    module.run.prepared_native = prepare(pending, ROOT, output, run_command)
                prepared = True
                module.run.prepared_workspace = prepare_lint(ROOT, output, base, run_command)
            print("NATIVE_LINT_PREPARATION: lint passed; workspace acceptance remains incomplete. "
                  "Rejoining the existing host FIFO before the contract matrix.", flush=True)
        for members in reservations:
            fresh = members[0] in pending
            attempt["active_profile"] = members[0]
            attempt["reservation_profiles"] = members
            lease = host_acceptance_lease(attempt, head) if fresh else contextlib.nullcontext()
            try:
                with lease:
                    for name in members:
                        attempt["active_profile"] = name
                        if name in pending and not prepared:
                            prepare = getattr(module, "prepare_native_checks", None)
                            if prepare:
                                module.run.prepared_native = prepare(pending, ROOT, output, run_command)
                            prepared = True
                        perform_profile(name)
            finally:
                attempt.pop("reservation_profiles", None)
        attempt.pop("active_profile", None)
        clean(head)
    except BaseException:
        record_check_time()
        if results:
            try:
                save_completed_profiles(receipt, output, identity, key, head, base_ref, policy_ref, signers_ref,
                                        sign_carried())
                print(f"Saved completed profiles; failed/unrun profiles remain required. Resume with: {resume_command}",
                      flush=True)
            except (RuntimeError, OSError, ValueError, subprocess.CalledProcessError) as error:
                print(f"Could not save completed profiles: {error}; original failure remains.", file=sys.stderr, flush=True)
        raise
    record_check_time()
    receipt["finished_at"] = int(time.time())
    referenced = sign_carried()
    envelope = sign_receipt(receipt, output, identity, key)
    verify_envelope(envelope, head, base_ref, policy_ref, signers_ref, store=referenced)
    # envelope.json marks a complete output, so its store is written first.
    if referenced:
        (output / "envelopes.json").write_bytes(canonical(referenced))
    (output / "envelope.json").write_bytes(canonical(envelope))
    if args.publish:
        publish(head, envelope, args.remote, store=referenced)
    print(f"Local acceptance complete: {output / 'receipt.json'}")


def write_managed_file(path, contents, mode=None):
    """Publish complete managed bytes atomically, including idempotent self-install."""
    if (path.is_symlink() or path.parent.is_symlink()
            or (hasattr(path.parent, "is_junction") and path.parent.is_junction())):
        raise RuntimeError(f"Managed acceptance files must not traverse links: {path}")
    permissions = mode if mode is not None else (path.stat().st_mode & 0o777 if path.exists() else 0o644)
    if path.is_file() and path.read_bytes() == contents:
        if mode is not None:
            path.chmod(permissions)
        return
    with tempfile.NamedTemporaryFile(dir=path.parent, prefix="runner-refresh-", suffix=".tmp", delete=False) as stream:
        temporary = Path(stream.name)
        stream.write(contents)
        stream.flush()
        os.fsync(stream.fileno())
    try:
        temporary.chmod(permissions)
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def install(args):
    common = Path(value("rev-parse", "--path-format=absolute", "--git-common-dir")).resolve()
    existing = git("config", "--get", "core.hooksPath", check=False).stdout.decode().strip()
    managed = common / "local-acceptance-hooks"
    if existing and Path(existing).resolve() != managed:
        raise RuntimeError(f"Existing core.hooksPath needs manual composition: {existing}")
    if not existing and (common / "hooks/pre-push").exists():
        raise RuntimeError("Existing pre-push hook needs manual composition; it was preserved")
    managed.mkdir(parents=True, exist_ok=True)
    # Outside the worktree so an older branch cannot silently remove the hook.
    write_managed_file(managed / "local_acceptance.py", Path(__file__).read_bytes())
    interpreter = shlex.quote(sys.executable.replace("\\", "/"))
    helper = shlex.quote(str(managed / "local_acceptance.py").replace("\\", "/"))
    hook = managed / "pre-push"
    write_managed_file(hook, f'#!/bin/sh\nexec {interpreter} {helper} pre-push "$@"\n'.encode(), mode=0o755)
    git("config", "--local", "core.hooksPath", str(managed))
    if args.signer:
        git("config", "--local", "localAcceptance.signer", args.signer)
    if args.key:
        git("config", "--local", "localAcceptance.key", str(Path(args.key).expanduser().resolve()))
    print(f"Installed automatic pre-push acceptance for all worktrees: {managed}")


def refresh_managed_runner(trusted, operation):
    """Atomically refresh only the installed entry from protected Git bytes."""
    current = tracked(trusted, ".github/scripts/local_acceptance.py")
    installed = Path(__file__).resolve()
    protected_code = compile(current, RUNNER_CODE.co_filename, "exec", dont_inherit=True, optimize=sys.flags.optimize)
    if sha(installed.read_bytes()) == sha(current) and RUNNER_CODE == protected_code:
        return None
    common = Path(value("rev-parse", "--path-format=absolute", "--git-common-dir")).resolve()
    managed = common / "local-acceptance-hooks"
    expected = managed / "local_acceptance.py"
    if (installed != expected or managed.is_symlink()
            or (hasattr(managed, "is_junction") and managed.is_junction()) or expected.is_symlink()):
        raise RuntimeError(f"Use the current protected managed runner for {operation}; "
                           f"entry: {sys.executable} {expected}. "
                           "Publish acceptance-rule changes with contract-update. Candidate source was preserved.")
    write_managed_file(installed, current)
    print("LOCAL_ACCEPTANCE_RUNNER_REFRESHED=" + canonical({
        "operation": operation, "entry": str(installed), "protected_commit": value("rev-parse", trusted),
        "sha256": sha(current), "next": "Continue the same frozen source; all selected acceptance remains required.",
    }).decode(), flush=True)
    return current


def deferred_pull(remote, head, preview):
    """The one open, ready PR this branch heads, when its push can be verified
    after it lands. None keeps the original blocking push."""
    if os.environ.get("SCODE_ACCEPTANCE_PUSH") == "blocking" or not preview["ci"]:
        return None
    repository = load_policy("origin/main")["repository"]
    try:
        contract_remote(remote, repository)
    except RuntimeError:
        # Local fixtures and other remotes have no pull request to hold pending.
        return None
    branch = value("symbolic-ref", "--short", "HEAD")
    try:
        pulls = github_api(f"repos/{repository}/pulls?state=open&base=main&head="
                           + urllib.parse.quote(repository.split("/")[0] + ":" + branch, safe=""))
    except RuntimeError as error:
        print(f"LOCAL_ACCEPTANCE_DEFERRED_UNAVAILABLE: {error}", flush=True)
        return None
    if len(pulls) != 1 or pulls[0]["draft"] or pulls[0]["head"]["repo"]["full_name"] != repository:
        return None
    return pulls[0]


def deferred_state(branch):
    """This clone's per-branch state for deferred checks, outside every checkout.
    The name is short: Windows linkers refuse paths over 260 characters, and the
    branch worktree under it builds the whole workspace."""
    # Git may name the common directory relative to the checkout, which is not
    # always this process's working directory.
    common = (ROOT / value("rev-parse", "--git-common-dir")).resolve()
    return Path.home() / ".nexus/local-acceptance/deferred" / sha(f"{common}\0{branch}".encode())[:12]


def stop_superseded(state, head):
    """Stop this branch's earlier deferred check. Returns True when one for the
    same head is still running, so it is kept instead of started twice."""
    import psutil

    try:
        record = json.loads((state / "deferred.json").read_text(encoding="utf-8"))
        process = psutil.Process(record["pid"])
        if process.create_time() != record["started"]:
            return False
    except (OSError, ValueError, KeyError, psutil.NoSuchProcess):
        return False
    if record["head"] == head:
        return True
    if os.name != "nt":
        # start_new_session made the check its own process group leader.
        with contextlib.suppress(OSError):
            os.killpg(process.pid, 9)
    for child in process.children(recursive=True)[::-1] + [process]:
        with contextlib.suppress(psutil.NoSuchProcess):
            child.kill()
    psutil.wait_procs([process], timeout=10)
    with contextlib.suppress(OSError):
        Path(record["log"]).with_suffix(".superseded.json").write_bytes(
            canonical({"superseded_by": head, "at": time.time()}))
    print(f"LOCAL_ACCEPTANCE_DEFERRED: stopped the check for superseded head {record['head'][:12]}", flush=True)
    return False


def defer_check(head, remote, pull, trusted):
    """Publish this head's signed plan, then verify it in the background in the
    branch's own acceptance worktree, so neither the push nor the author's
    checkout waits. Local acceptance stays pending, and merge stays gated,
    until the signed receipt is published."""
    branch = value("symbolic-ref", "--short", "HEAD")
    # The check's own output takes the normal results layout; its plan and log
    # sit beside it.
    output = (Path.home() / ".nexus/local-acceptance/results" / head / str(time.time_ns())).resolve()
    plan_output = output.with_name(output.name + "-plan")
    plan_output.mkdir(parents=True)
    publish_plan(head, trusted, remote, plan_output)
    state = deferred_state(branch)
    state.mkdir(parents=True, exist_ok=True)
    with contextlib.suppress(OSError, ValueError):
        previous = json.loads((state / "deferred.json").read_text(encoding="utf-8"))
        ended, healthy = deferred_status(previous)
        if not healthy:
            print(f"LOCAL_ACCEPTANCE_DEFERRED: the earlier check for {previous['head'][:12]} {ended}", flush=True)
    if stop_superseded(state, head):
        print(f"LOCAL_ACCEPTANCE_DEFERRED: the check for {head[:12]} is already running; "
              f"see {state / 'deferred.json'}", flush=True)
        return
    # Git runs pre-push with selectors for the pushing checkout: from a linked
    # worktree, GIT_DIR names it. The acceptance worktree and its check find
    # their own repository, or they would check out into the author's HEAD and
    # index and judge cleanliness against them.
    own = without_git_selectors(os.environ)
    worktree = state / "w"
    if (worktree / ".git").exists():
        git("checkout", "--detach", "--force", head, root=worktree, env=own)
        git("clean", "-ffd", root=worktree, env=own)
    else:
        git("worktree", "add", "--detach", str(worktree), head, env=own)
    command = [sys.executable, str(Path(__file__).resolve()), "check", "--head", head, "--base", "origin/main",
               "--policy-ref", "origin/main", "--publish", "--remote", remote,
               "--output", str(output), "--dispatch-publish", str(pull["number"])]
    log = output.with_name(output.name + ".log")
    detached = ({"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP | subprocess.CREATE_NO_WINDOW}
                if os.name == "nt" else {"start_new_session": True})
    with log.open("wb") as stream:
        try:
            # Outlive a terminal or agent job that kills its tree on exit.
            breakaway = {"creationflags": detached["creationflags"] | 0x01000000} if os.name == "nt" else detached
            process = subprocess.Popen(command, cwd=worktree, env=own, stdin=subprocess.DEVNULL, stdout=stream,
                                       stderr=subprocess.STDOUT, **breakaway)
        except OSError:
            process = subprocess.Popen(command, cwd=worktree, env=own, stdin=subprocess.DEVNULL, stdout=stream,
                                       stderr=subprocess.STDOUT, **detached)
    import psutil

    (state / "deferred.json").write_bytes(canonical({
        "pid": process.pid, "started": psutil.Process(process.pid).create_time(), "head": head,
        "branch": branch, "pr": pull["number"], "output": str(output), "log": str(log)}))
    print("LOCAL_ACCEPTANCE_DEFERRED=" + canonical({
        "head": head, "pr": pull["number"], "worktree": str(worktree), "log": str(log),
        "status": "Pushed with a signed plan. Local acceptance is running in the background; the required "
                  "Local acceptance status stays pending, and merge stays blocked, until its signed receipt "
                  "is published. Set SCODE_ACCEPTANCE_PUSH=blocking to verify before pushing instead.",
    }).decode(), flush=True)


def deferred_result_path(log):
    return Path(log).with_name(Path(log).name.removesuffix(".log") + ".result.json")


def record_deferred_result(args, outcome, finalization, error=None):
    """Leave the background check's outcome beside its log for doctor and the next push."""
    log = Path(args.output).with_name(Path(args.output).name + ".log")
    deferred_result_path(log).write_bytes(canonical({
        "head": value("rev-parse", args.head or "HEAD"), "pr": args.dispatch_publish, "outcome": outcome,
        "finalization": finalization, "error": None if error is None else str(error)[-2000:],
        "finished_at": time.time()}))


def deferred_status(record):
    """(state, healthy) of the check a deferred.json record started."""
    import psutil

    log = Path(record["log"])
    with contextlib.suppress(psutil.NoSuchProcess, KeyError):
        process = psutil.Process(record["pid"])
        # A finished check stays a zombie only while its starter still runs.
        if process.create_time() == record["started"] and process.status() != psutil.STATUS_ZOMBIE:
            return f"is still running; log {log}", True
    with contextlib.suppress(OSError, ValueError):
        superseded = json.loads(log.with_suffix(".superseded.json").read_text(encoding="utf-8"))
        return f"was stopped for {superseded['superseded_by'][:12]}", True
    try:
        result = json.loads(deferred_result_path(log).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return f"ended without recording a result; Local acceptance stays pending. See {log}", False
    if result["outcome"] != "verified":
        reason = next(reversed((result.get("error") or "unknown error").splitlines()), "unknown error")
        return f"FAILED ({reason}); Local acceptance stays pending until a new head passes. See {log}", False
    if result["finalization"] != "requested":
        return (f"passed and published its receipt, but finalization was not requested ({result['finalization']}). "
                f"Run `gh workflow run local-acceptance-publish.yml -f pr={record['pr']} -f head={record['head']}`"), False
    return "passed; trusted finalization was requested", True


def deferred_findings():
    """One line per branch with a deferred check from this host."""
    findings = []
    for path in sorted((Path.home() / ".nexus/local-acceptance/deferred").glob("*/deferred.json")):
        with contextlib.suppress(OSError, ValueError, KeyError):
            record = json.loads(path.read_text(encoding="utf-8"))
            state, healthy = deferred_status(record)
            findings.append((f"deferred {record['branch']} {record['head'][:12]} (PR #{record['pr']})", healthy, state))
    return findings


def dispatch_publish(pr, head, policy_ref):
    """Ask the trusted publisher to read the receipt just published for `head`."""
    repository = load_policy(policy_ref)["repository"]
    print(f"LOCAL_ACCEPTANCE: receipt published; requesting trusted finalization for PR #{pr} at {head[:12]}",
          flush=True)
    github_api(f"repos/{repository}/actions/workflows/local-acceptance-publish.yml/dispatches",
               {"ref": "main", "inputs": {"pr": str(pr), "head": head}})
    print(f"LOCAL_ACCEPTANCE: trusted finalization requested for PR #{pr}.", flush=True)


def compiler_store_findings(root, capacity=2147483648, stale=86400):
    """Each shared compiler store's size against its cap and its newest write.
    A store at its cap that has stopped taking writes wedged cold compiles on
    2026-11-17; a probe through a scratch store cannot see that."""
    findings = []
    for store in sorted(path for path in root.glob("*") if path.is_dir() and not path.name.endswith(".control")):
        size, newest = 0, 0.0
        for directory, _, files in os.walk(store):
            for name in files:
                with contextlib.suppress(OSError):
                    info = os.stat(os.path.join(directory, name))
                    size, newest = size + info.st_size, max(newest, info.st_mtime)
        age = time.time() - newest if newest else None
        wedged = size >= 0.95 * capacity and (age is None or age > stale)
        written = "never written" if age is None else f"last write {age / 3600:.1f} h ago"
        findings.append((f"compiler store {store.name}", not wedged,
                         f"{size / 1024**3:.2f} of {capacity / 1024**3:.0f} GiB, {written}" + (
                             "; it is full and no longer taking writes, which can hang cold compiles. With no "
                             f"acceptance running, move {store} aside (reversible); the next check starts a fresh store"
                             if wedged else "")))
    return findings


def host_findings(names=None):
    """This host's acceptance prerequisites as (name, ok, detail) with the fix
    in each failing detail. `names` limits it to what those checks use; None
    reports everything. A finding grants no acceptance; the policy's checks
    revalidate what they need before the host queue and at execution."""
    findings = []

    def probe(command, timeout=30, **kwargs):
        try:
            return subprocess.run(command, capture_output=True, timeout=timeout, **kwargs)
        except (OSError, subprocess.TimeoutExpired) as error:
            return error

    native = names is None or any(name.startswith("native-") or name == "guard-contract" for name in names)
    docker = names is None or bool(set(names) & {"rendering", "browser", "guard-contract"})
    if names is None:
        for module in ["psutil", "jsonschema"]:
            try:
                __import__(module)
                findings.append((f"python {module}", True, "importable"))
            except ImportError:
                findings.append((f"python {module}", False, f"run `{sys.executable} -m pip install {module}`"))
        signer = git("config", "localAcceptance.signer", check=False).stdout.decode().strip()
        key = git("config", "localAcceptance.key", check=False).stdout.decode().strip()
        if not signer or not key or not Path(key).expanduser().is_file():
            findings.append(("signing key", False, "run `local_acceptance.py install --signer DEVICE --key KEY` "
                                                   "with a key enrolled in spec/local_acceptance_signers.json"))
        else:
            with tempfile.TemporaryDirectory(prefix="scode-doctor-") as scratch:
                message = Path(scratch) / "probe"
                message.write_bytes(b"doctor")
                public = probe(["ssh-keygen", "-y", "-f", str(Path(key).expanduser())])
                signed = probe(["ssh-keygen", "-Y", "sign", "-f", str(Path(key).expanduser()), "-n", NAMESPACE,
                                str(message)])
                verified = None
                if not isinstance(public, Exception) and not isinstance(signed, Exception) and not signed.returncode:
                    allowed = Path(scratch) / "allowed"
                    allowed.write_bytes(signer.encode() + b" " + public.stdout.strip() + b"\n")
                    verified = probe(["ssh-keygen", "-Y", "verify", "-f", str(allowed), "-I", signer, "-n", NAMESPACE,
                                      "-s", str(message) + ".sig"], input=b"doctor")
                ok = verified is not None and not isinstance(verified, Exception) and not verified.returncode
                findings.append(("signing key", ok, f"{signer} signs and verifies" if ok else
                                 "ssh-keygen could not sign and verify with the configured key; run this outside "
                                 "a sandbox that hides the key or ssh-keygen, and check the key's permissions"))
        gh = shutil.which("gh")
        status = probe([gh, "auth", "status"]) if gh else None
        findings.append(("GitHub CLI", status is not None and not isinstance(status, Exception)
                         and not status.returncode and Path(gh).suffix.lower() not in {".cmd", ".bat"},
                         "authenticated" if status is not None and not isinstance(status, Exception)
                         and not status.returncode else "install the native gh and run `gh auth login`"))
    if os.name == "nt" and native:
        pwsh = shutil.which("pwsh.exe")
        findings.append(("PowerShell 7", bool(pwsh), pwsh or "add an existing PowerShell 7 directory to PATH, or "
                         "`winget install --id Microsoft.PowerShell --exact --source winget`"))
        tools = os.environ.get("VCToolsInstallDir")
        configured = git("config", "localAcceptance.vsdevcmd", check=False).stdout.decode().strip()
        locator = Path(os.environ.get("ProgramFiles(x86)", "C:/Program Files (x86)")) / "Microsoft Visual Studio/Installer/vswhere.exe"
        if tools:
            linker = Path(tools) / "bin/Hostx64/x64/link.exe"
            findings.append(("MSVC linker", linker.is_file(), f"{linker} goes first for compiler commands" if linker.is_file()
                             else f"{linker} is missing; repair the Visual Studio C++ build tools"))
        else:
            ok = bool(configured and Path(configured).is_file()) or locator.is_file()
            findings.append(("MSVC linker", ok, "found through " + (configured or str(locator)) if ok else
                             "install the Visual Studio C++ build tools, or set "
                             "`git config localAcceptance.vsdevcmd <path to VsDevCmd.bat>`"))
    if names is None:
        findings.extend(compiler_store_findings(Path.home() / ".cache/sudocode-local-acceptance/compiler-objects"))
        prepared = sorted((Path.home() / ".cache/sudocode-local-acceptance/tools/sccache").glob("*/*/sccache*"))
        tool = next((path for path in prepared if path.name in {"sccache", "sccache.exe"}), None)
        if tool is None:
            findings.append(("compiler cache", True, "not prepared yet; the first check fetches the pinned sccache"))
        else:
            with tempfile.TemporaryDirectory(prefix="scode-doctor-cache-") as scratch:
                import socket

                with socket.socket() as reservation:
                    reservation.bind(("127.0.0.1", 0))
                    port = reservation.getsockname()[1]
                cache = {**os.environ, "SCCACHE_DIR": str(Path(scratch) / "cache"), "SCCACHE_SERVER_PORT": str(port),
                         "SCCACHE_IDLE_TIMEOUT": "60"}
                (Path(scratch) / "probe.rs").write_text("", encoding="utf-8")
                probe([str(tool), "--start-server"], timeout=20, env=cache)
                compiled = probe([str(tool), "rustc", "--crate-name", "doctor_probe", "--crate-type", "lib", "--emit",
                                  "metadata", "--out-dir", scratch, str(Path(scratch) / "probe.rs")],
                                 timeout=120, env=cache, cwd=scratch)
                probe([str(tool), "--stop-server"], timeout=20, env=cache)
            ok = not isinstance(compiled, Exception) and not compiled.returncode
            findings.append(("compiler cache", ok, f"{tool} compiles through its own server" if ok else
                             f"{tool} did not compile a probe within 120s; acceptance falls back to original Cargo, "
                             "or set any SCCACHE_* variable to skip the cache"))
    if docker:
        server = probe(["docker", "version", "--format", "{{.Server.Version}}"])
        ok = not isinstance(server, Exception) and not server.returncode
        findings.append(("Docker", ok, f"server {server.stdout.decode().strip()}" if ok else
                         "start Docker Desktop (or the Docker daemon) and rerun"))
    return findings


def doctor(args):
    """Report every prerequisite with its fix; runs no acceptance check."""
    findings = host_findings()
    for name, ok, detail in findings:
        print(f"{'ok ' if ok else 'fix'} {name}: {detail}", flush=True)
    # Background checks are reported, not counted as host readiness.
    for name, ok, detail in deferred_findings():
        print(f"{'ok ' if ok else 'see'} {name}: {detail}", flush=True)
    failed = [name for name, ok, _ in findings if not ok]
    if failed:
        raise RuntimeError("fix " + ", ".join(failed) + " before running local acceptance")
    print("This host is ready for local acceptance. A doctor run grants no acceptance.", flush=True)


def pre_push(args):
    global ROOT
    ROOT = Path(value("rev-parse", "--show-toplevel", root=Path.cwd()))
    updates = [line.split() for line in sys.stdin.read().splitlines() if line.strip()]
    heads = [row[1] for row in updates if row[2].startswith("refs/heads/") and set(row[1]) != {"0"}]
    if not heads:
        return
    git("fetch", "--no-tags", "origin", "+refs/heads/main:refs/remotes/origin/main")
    if refresh_managed_runner("origin/main", "pre-push") is not None:
        # Upgrade from the protected default branch, including old worktrees.
        # Do not import or execute the candidate's replacement runner.
        result = subprocess.run([sys.executable, __file__, "pre-push", args.remote, args.url],
                                input=("\n".join(" ".join(row) for row in updates) + "\n").encode())
        if result.returncode:
            raise RuntimeError("Updated local acceptance hook rejected the push")
        return
    for head in set(heads):
        if head != value("rev-parse", "HEAD"):
            raise RuntimeError("Push one checked-out branch at a time so acceptance tests its exact sources")
        fetched = git("fetch", "--no-tags", args.remote, f"+{NOTE_REF}:{NOTE_REF}", check=False)
        if fetched.returncode and "couldn't find remote ref" not in fetched.stderr.decode(errors="replace"):
            raise RuntimeError(fetched.stderr.decode(errors="replace"))
        if changed_contract_files(head, "origin/main") and contract_hash(head) != contract_hash("origin/main"):
            try:
                contract_remote(args.remote, load_policy("origin/main")["repository"])
            except RuntimeError:
                # Local bare fixtures and unrelated remotes cannot carry a
                # GitHub maintainer's authorization for this repository.
                check_contract_changes(head, "origin/main", "origin/main")
        policy_ref = authorized_policy(head, "origin/main", "origin/main")
        try:
            verify(head, "origin/main", policy_ref, signers_ref="origin/main")
        except RuntimeError:
            if policy_ref != "origin/main":
                # The exact candidate executor was authorized by the server.
                run_candidate_checks(head, "origin/main", args.remote)
            else:
                policy = load_policy(policy_ref)
                comparison = value("merge-base", "origin/main", head)
                preview = acceptance_plan(policy,
                    selected(policy, changed_paths(comparison, head), base=comparison, head=head), host())
                pull = deferred_pull(args.remote, head, preview)
                if pull is not None:
                    defer_check(head, args.remote, pull, value("rev-parse", "origin/main"))
                    continue
                if preview["ci"]:
                    print("LOCAL_ACCEPTANCE_PARALLEL_AVAILABLE=" + canonical({
                        "current_push_mode": "serial",
                        "command": [sys.executable, str(Path(__file__).resolve()), "start-pr", "--remote", args.remote],
                        "publishes_frozen_source_before_local_completion": True,
                        "next_frozen_PR": "Use this entry before a long author check to start missing native CI together. "
                                          "The signed launch plan proves no completed tests; full local and native acceptance still gate merge.",
                    }).decode(), flush=True)
                check(argparse.Namespace(head=head, base="origin/main", policy_ref=policy_ref,
                                         output=None, publish=True, remote=args.remote))


def contract_actor(trusted):
    repository = load_policy(trusted)["repository"]
    actor = github_api("user")["login"]
    require_contract_maintainer(repository, actor)
    return repository, actor


def contract_remote(remote, repository):
    configured = value("config", "--get", f"remote.{remote}.url")
    if configured not in {f"https://github.com/{repository}.git", f"git@github.com:{repository}.git",
                          f"ssh://git@github.com/{repository}.git"}:
        raise RuntimeError(f"Contract update remote must point to {repository}. Inspect git remote -v, "
                           "then use contract-update --remote with that repository's remote name.")
    return value("remote", "get-url", "--push", remote)


def contract_pre_push(args):
    """A one-invocation hook: require authority, exact sources and a real signed receipt."""
    clean(args.head)
    repository, actor = contract_actor(args.trusted)
    updates = [line.split() for line in sys.stdin.read().splitlines() if line.strip()]
    branch = value("symbolic-ref", "HEAD")
    expected_url = contract_remote(args.remote, repository)
    already_published = (not updates and value("ls-remote", args.remote, branch).split()[:1] == [args.head])
    valid_update = len(updates) == 1 and len(updates[0]) == 4 and updates[0][:3] == [branch, args.head, branch]
    if (args.url != expected_url or branch == "refs/heads/main" or not (valid_update or already_published)):
        raise RuntimeError("Contract update must push exactly the checked-out non-main branch to its protected GitHub repository")
    # Recheck authority and the signed input immediately at the actual push.
    verify(args.head, args.trusted, args.head, signers_ref=args.trusted)
    print(f"Exact-head contract push authorized for {actor}; native CI is still required.", flush=True)


def plan_pre_push(args):
    """Publish only one frozen feature branch under a signed scheduling plan."""
    clean(args.head)
    repository = load_policy(args.trusted)["repository"]
    updates = [line.split() for line in sys.stdin.read().splitlines() if line.strip()]
    branch = value("symbolic-ref", "HEAD")
    expected_url = contract_remote(args.remote, repository)
    already_published = (not updates and value("ls-remote", args.remote, branch).split()[:1] == [args.head])
    valid_update = len(updates) == 1 and len(updates[0]) == 4 and updates[0][:3] == [branch, args.head, branch]
    if args.url != expected_url or branch == "refs/heads/main" or not (valid_update or already_published):
        raise RuntimeError("Execution-plan publication requires exactly the frozen non-main branch and configured repository")
    read_execution_plan(args.head, args.trusted, args.trusted, signers_ref=args.trusted)
    print("LOCAL_ACCEPTANCE_PARALLEL_SOURCE: signed frozen source published for scheduling only; "
          "complete author checks and native CI still gate merge.", flush=True)


def publish_plan(head, trusted, remote, output):
    """Sign and publish this head's execution plan, or reuse the one already
    published from this host. The plan schedules native CI; it proves no test."""
    repository = load_policy(trusted)["repository"]
    contract_remote(remote, repository)
    base = value("merge-base", trusted, head)
    policy = load_policy(trusted)
    names = selected(policy, changed_paths(base, head), base=base, head=head)
    plan = acceptance_plan(policy, names, host())
    for note_ref in [NOTE_REF, PLAN_NOTE_REF]:
        fetched = git("fetch", "--no-tags", remote, f"+{note_ref}:{note_ref}", check=False)
        if fetched.returncode and "couldn't find remote ref" not in fetched.stderr.decode(errors="replace"):
            raise RuntimeError(fetched.stderr.decode(errors="replace"))
    prior = None
    if read_note(head, PLAN_NOTE_REF)["attestations"]:
        prior = read_execution_plan(head, trusted, trusted)
        if prior["host"] != host():
            raise RuntimeError("Use the original author host for this frozen plan, or publish a new source head")
        plan = prior["plan"]
    if not plan["ci"]:
        raise RuntimeError("No complementary native hosts are selected; use normal git push for this source")
    identity = value("config", "localAcceptance.signer")
    key = Path(value("config", "localAcceptance.key")).expanduser()
    receipt = {"version": 1, "kind": "execution-plan", "repository": repository, "head": head,
               "tree": value("rev-parse", f"{head}^{{tree}}"), "base": base, "policy": trusted,
               "contract_sha256": contract_hash(trusted), "host": host(), "plan": plan, "checks": {}}
    if prior is None:
        envelope = sign_receipt(receipt, output / "execution-plan", identity, key, namespace=NAMESPACE + "-plan")
    else:
        envelope = next(item for item in read_note(head, PLAN_NOTE_REF)["attestations"].values()
                        if json.loads(base64.b64decode(item["payload"])) == prior)
    verify_execution_plan(envelope, head, trusted, trusted)
    if prior is None:
        publish(head, envelope, remote, note_ref=PLAN_NOTE_REF)
    git("fetch", "--no-tags", remote, f"+{PLAN_NOTE_REF}:{PLAN_NOTE_REF}")
    return plan, envelope


def start_pr(args, api=None, context=None):
    """Start native CI and mandatory author checks together for frozen source."""
    api = api or github_api
    started, wall_start = ((time.monotonic(), time.time()) if context is None
                           else (context["started"], context["started_at"]))
    head = value("rev-parse", "HEAD")
    clean(head)
    branch = value("symbolic-ref", "--short", "HEAD")
    if branch == "main":
        raise RuntimeError("Parallel acceptance requires a frozen non-main source branch")
    output = Path(args.output or (Path.home() / ".nexus/local-acceptance/results" / head / ("parallel-" + str(time.time_ns())))).resolve()
    if output == ROOT or ROOT in output.parents:
        raise RuntimeError("Keep parallel acceptance outputs outside the source checkout")
    if context is None:
        output.mkdir(parents=True, exist_ok=False)
        context = {"started": started, "started_at": wall_start, "head": head,
                   "branch": branch, "output": str(output), "refreshes": 0}
    elif (context["head"] != head or context["branch"] != branch or context["output"] != str(output)
            or not output.is_dir() or output.is_symlink()):
        raise RuntimeError("Protected runner refresh changed the frozen source or original output")
    args.output = str(output)
    journal = {"version": 1, "operation": "start-pr", "head": head, "started_at": wall_start,
               "outcome": "running", "runner_refreshes": context["refreshes"]}
    delegated = False
    try:
        git("fetch", "--no-tags", args.remote, "+refs/heads/main:refs/remotes/origin/main")
        trusted = value("rev-parse", "origin/main")
        current_runner = refresh_managed_runner(trusted, "start-pr")
        if current_runner is not None:
            if context["refreshes"] >= 3:
                raise RuntimeError("Protected runner advanced repeatedly; retry this updated managed entry. Original timing remains recorded.")
            # Execute the exact bytes just read from protected main, retaining
            # the real API boundary and original output/start across refresh.
            namespace = {"__name__": "protected_start_pr_runner", "__file__": str(Path(__file__).resolve()),
                         "__package__": None}
            exec(compile(current_runner, namespace["__file__"], "exec"), namespace)
            namespace["ROOT"] = ROOT
            delegated = True
            return namespace["start_pr"](args, api=api, context={**context, "refreshes": context["refreshes"] + 1})
        repository = load_policy(trusted)["repository"]
        plan, envelope = publish_plan(head, trusted, args.remote, output)
        with tempfile.TemporaryDirectory(prefix="scode-plan-push-") as temporary:
            hook = Path(temporary) / "pre-push"
            interpreter = shlex.quote(sys.executable.replace("\\", "/"))
            runner = shlex.quote(str(Path(__file__).resolve()).replace("\\", "/"))
            hook.write_text(f'#!/bin/sh\nexec {interpreter} {runner} plan-pre-push --head {head} --trusted {trusted} "$@"\n',
                            encoding="utf-8", newline="\n")
            hook.chmod(0o755)
            git("-c", f"core.hooksPath={temporary}", "push", "--set-upstream", args.remote,
                f"refs/heads/{branch}:refs/heads/{branch}")
        owner = repository.split("/")[0]
        pulls = api(f"repos/{repository}/pulls?state=open&base=main&head=" + urllib.parse.quote(owner + ":" + branch, safe=""))
        if len(pulls) > 1:
            raise RuntimeError("Source branch has multiple current PRs; inspect GitHub before continuing")
        if pulls:
            pull = pulls[0]
        else:
            body = (Path(args.body_file).read_text(encoding="utf-8") if args.body_file else
                    "Frozen source. Mandatory author acceptance and complementary native CI are running in parallel; Local acceptance remains required before merge.")
            pull = api(f"repos/{repository}/pulls", {"base": "main", "head": branch, "draft": True,
                "title": args.title or value("log", "-1", "--format=%s"), "body": body})
        if pull["head"]["sha"] != head or pull["head"]["repo"]["full_name"] != repository:
            raise RuntimeError("PR advanced or names another source repository; preserve the execution-plan evidence")
        journal.update(pr=pull["number"], source_published=True, policy=trusted)
        if pull["draft"]:
            response = api("graphql", {"query": "mutation($id:ID!){markPullRequestReadyForReview(input:{pullRequestId:$id}){pullRequest{isDraft}}}",
                                      "variables": {"id": pull["node_id"]}})
            if response.get("errors") or response["data"]["markPullRequestReadyForReview"]["pullRequest"]["isDraft"]:
                raise RuntimeError("GitHub did not start ready source CI; no local field started")
        print("LOCAL_ACCEPTANCE_PARALLEL=" + canonical({"head": head, "pr": pull["number"],
            "ci": plan["ci"], "author_checks": plan["local_checks"], "output": str(output),
            "completion": "Frozen source is ready for tests, not merge. A scheduling plan cannot satisfy Local acceptance; all author checks and native jobs remain required."}).decode(), flush=True)
        try:
            existing = verify(head, trusted, trusted)
        except RuntimeError:
            check(argparse.Namespace(head=head, base=trusted, policy_ref=trusted, signers_ref=trusted,
                output=str(output / "author"), resume_from=None, publish=True, remote=args.remote,
                dispatch_publish=pull["number"],
                outer_started_at=wall_start, outer_monotonic_started_at=started, execution_plan=envelope))
        else:
            if existing["host"] != host():
                raise RuntimeError("Existing acceptance belongs to another author host; retain that responsibility and use normal git push")
            print("LOCAL_ACCEPTANCE_PARALLEL: existing exact-source author signature reused; no completed local profile reran.", flush=True)
        clean(head)
        api(f"repos/{repository}/actions/workflows/local-acceptance-publish.yml/dispatches",
            {"ref": "main", "inputs": {"pr": str(pull["number"]), "head": head}})
        journal["outcome"] = "success"
        print(f"LOCAL_ACCEPTANCE_PARALLEL: author checks passed for PR #{pull['number']}; trusted finalization requested. "
              "Existing native jobs are retained. Wait for the required Local acceptance status, then use the normal merge queue.", flush=True)
    except BaseException:
        journal["outcome"] = "failure"
        raise
    finally:
        if not delegated or not (output / "parallel-attempt.json").is_file():
            journal.update(finished_at=time.time(), elapsed_seconds=round(time.monotonic() - started, 3))
            (output / "parallel-attempt.json").write_bytes(canonical(journal))


def contract_update(args):
    head = value("rev-parse", "HEAD")
    clean(head)
    git("fetch", "--no-tags", args.remote, "+refs/heads/main:refs/remotes/origin/main")
    trusted = value("rev-parse", "origin/main")
    repository, actor = contract_actor(trusted)
    if load_policy(head)["repository"] != repository:
        raise RuntimeError("Contract authorization cannot change this repository's identity")
    branch = value("symbolic-ref", "--short", "HEAD")
    contract_remote(args.remote, repository)
    if branch == "main":
        raise RuntimeError("Use a non-main branch for contract-update")
    shared_base = value("merge-base", trusted, head)
    if contract_hash(shared_base) != contract_hash(trusted):
        raise RuntimeError("Trusted acceptance contract advanced. Integrate origin/main while preserving other changes, "
                           "then rerun contract-update. Unrelated main updates do not require a cosmetic merge.")
    if not changed_contract_files(head, trusted):
        raise RuntimeError("No protected contract files changed. Next: git push --set-upstream origin HEAD")
    signer = value("config", "localAcceptance.signer")
    if signer not in json.loads(tracked(trusted, SIGNERS))["signers"]:
        raise RuntimeError("Use an already enrolled signing device to authorize a contract update, including new device enrollment.")
    print(f"Contract update by {actor}: head {head}, trusted main {trusted}; all selected local checks remain required.", flush=True)
    fetched = git("fetch", "--no-tags", args.remote, f"+{NOTE_REF}:{NOTE_REF}", check=False)
    if fetched.returncode and "couldn't find remote ref" not in fetched.stderr.decode(errors="replace"):
        raise RuntimeError(fetched.stderr.decode(errors="replace"))
    try:
        verify(head, trusted, head, signers_ref=trusted)
    except RuntimeError:
        run_candidate_checks(head, trusted, args.remote, args.output)
    with tempfile.TemporaryDirectory(prefix="scode-contract-push-") as temporary:
        hook = Path(temporary) / "pre-push"
        interpreter = shlex.quote(sys.executable.replace("\\", "/"))
        helper = shlex.quote(str(Path(__file__).resolve()).replace("\\", "/"))
        hook.write_text(f'#!/bin/sh\nexec {interpreter} {helper} contract-pre-push --head {head} --trusted {trusted} "$@"\n',
                        encoding="utf-8", newline="\n")
        hook.chmod(0o755)
        git("-c", f"core.hooksPath={temporary}", "push", "--set-upstream", args.remote,
            f"refs/heads/{branch}:refs/heads/{branch}")
    try:
        status = github_api(f"repos/{repository}/statuses/{head}", {
            "state": "success", "context": AUTHORIZATION_CONTEXT,
            "description": authorization_description(head, trusted),
            "target_url": f"https://github.com/{repository}/commit/{head}",
        })
        if status["creator"]["login"] != actor:
            raise RuntimeError("GitHub authorization actor changed during publication")
    except RuntimeError as error:
        raise RuntimeError("Branch was pushed, but contract authorization is incomplete. Rerun contract-update; "
                           "the existing exact-head signed receipt will be reused. " + str(error)) from error
    print(f"Contract authorization published for {head}. Local acceptance and all complementary CI checks must still pass.\n"
          f"Next: gh pr create --draft --base main --head {branch} (or inspect the existing PR).\n"
          f"When ready to land: gh pr merge {shlex.quote(branch)} --merge --auto. "
          "Request queueing before waiting; GitHub still enforces required checks and reviews.", flush=True)


def run_candidate_checks(head, trusted, remote, output=None):
    # A contract update can change run_command itself. Execute that exact
    # checked-out runner after authority verification, not this older copy.
    command = [sys.executable, str(ROOT / ".github/scripts/local_acceptance.py"), "check",
               "--head", head, "--base", trusted, "--policy-ref", head, "--signers-ref", trusted,
               "--publish", "--remote", remote]
    if output:
        command += ["--output", output]
    subprocess.run(command, cwd=ROOT, check=True)
    git("fetch", "--no-tags", remote, f"+{NOTE_REF}:{NOTE_REF}")
    verify(head, trusted, head, signers_ref=trusted)


def main():
    global ROOT
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="operation", required=True)
    for name in ["check", "verify", "plan"]:
        command = sub.add_parser(name)
        command.add_argument("--head")
        command.add_argument("--base", default="origin/main")
        command.add_argument("--policy-ref", default="origin/main")
        if name == "plan":
            command.add_argument("--host", choices=["linux", "macos", "windows"])
            command.add_argument("--architecture")
        if name == "check":
            command.add_argument("--output")
            command.add_argument("--signers-ref", help=argparse.SUPPRESS)
            command.add_argument("--resume-from", type=Path,
                                 help="Reuse unchanged checks from a signed exact-source output directory")
            command.add_argument("--publish", action="store_true")
            command.add_argument("--remote", default="origin")
            command.add_argument("--dispatch-publish", type=int, metavar="PR",
                                 help="After publishing, ask the trusted publisher to finalize this PR")
    sub.add_parser("doctor", help="Report this host's acceptance prerequisites and how to fix each; runs no check")
    command = sub.add_parser("install")
    command.add_argument("--signer")
    command.add_argument("--key")
    command = sub.add_parser("index-output", help="Discover existing signed local progress without running or waiving checks")
    command.add_argument("--directory", type=Path, required=True)
    command.add_argument("--policy-ref", default="origin/main")
    command = sub.add_parser("pre-push")
    command.add_argument("remote")
    command.add_argument("url")
    command = sub.add_parser("contract-update", help="Run and publish an exact-head contract update as a repository maintainer")
    command.add_argument("--remote", default="origin")
    command.add_argument("--output")
    command = sub.add_parser("contract-pre-push", help=argparse.SUPPRESS)
    command.add_argument("--head", required=True)
    command.add_argument("--trusted", required=True)
    command.add_argument("remote")
    command.add_argument("url")
    command = sub.add_parser("start-pr", help="Freeze/publish source, start complementary native CI, and run mandatory local acceptance in parallel")
    command.add_argument("--remote", default="origin")
    command.add_argument("--output")
    command.add_argument("--title")
    command.add_argument("--body-file")
    command = sub.add_parser("plan-pre-push", help=argparse.SUPPRESS)
    command.add_argument("--head", required=True)
    command.add_argument("--trusted", required=True)
    command.add_argument("remote")
    command.add_argument("url")
    args = parser.parse_args()
    timed = args.operation in {"check", "pre-push", "contract-update", "contract-pre-push", "start-pr", "plan-pre-push"}
    started = time.monotonic()
    operation = {"version": 1, "operation": args.operation, "started_at": time.time(), "outcome": "failure"}
    try:
        # Installed runners live outside a worktree. Their caller's checkout,
        # not the installation directory, supplies the exact tested sources.
        ROOT = Path(value("rev-parse", "--show-toplevel", root=Path.cwd()))
        if args.operation == "plan":
            head = value("rev-parse", args.head or "HEAD")
            base = value("merge-base", args.base, head)
            author = host()
            if args.host:
                author["system"] = args.host
            if args.architecture:
                author["architecture"] = args.architecture
            report_plan(acceptance_plan(load_policy(args.policy_ref),
                        selected(load_policy(args.policy_ref), changed_paths(base, head), base=base, head=head), author),
                        current_environment=author["system"] == host()["system"])
            print("Preview only: a plan is not a successful or signed test result.")
        elif args.operation == "verify":
            head = value("rev-parse", args.head or "HEAD")
            trusted = args.policy_ref
            policy_ref = authorized_policy(head, args.base, trusted) if trusted == "origin/main" else trusted
            verify(head, args.base, policy_ref, signers_ref=trusted)
        elif args.operation == "check":
            if args.policy_ref == "origin/main":
                args.policy_ref = authorized_policy(value("rev-parse", args.head or "HEAD"), args.base, args.policy_ref)
            if not args.dispatch_publish:
                check(args)
            else:
                try:
                    check(args)
                except BaseException as error:
                    record_deferred_result(args, "failed", "not requested", error)
                    raise
                try:
                    dispatch_publish(args.dispatch_publish, value("rev-parse", args.head or "HEAD"), args.policy_ref)
                except BaseException as error:
                    record_deferred_result(args, "verified", f"failed: {error}")
                    raise
                record_deferred_result(args, "verified", "requested")
        elif args.operation == "doctor":
            doctor(args)
        elif args.operation == "index-output":
            index_existing_output(args.directory, value("rev-parse", args.policy_ref))
        elif args.operation == "install":
            install(args)
        elif args.operation == "contract-update":
            contract_update(args)
        elif args.operation == "contract-pre-push":
            contract_pre_push(args)
        elif args.operation == "start-pr":
            start_pr(args)
        elif args.operation == "plan-pre-push":
            plan_pre_push(args)
        else:
            pre_push(args)
        operation["outcome"] = "success"
    except KeyboardInterrupt:
        operation["outcome"] = "interrupted"
        raise
    except (RuntimeError, subprocess.CalledProcessError, OSError, ValueError, KeyError) as error:
        print(f"Local acceptance blocked: {error}", file=sys.stderr)
        return 1
    finally:
        if timed:
            operation.update(finished_at=time.time(), elapsed_seconds=round(time.monotonic() - started, 3))
            directory = Path(args.output).resolve() if getattr(args, "output", None) else None
            if directory and directory.is_dir() and directory != ROOT and ROOT not in directory.parents:
                try:
                    (directory / "operation.json").write_bytes(canonical(operation))
                except OSError as error:
                    print(f"Could not save local operation timing: {error}", file=sys.stderr, flush=True)
            print("LOCAL_ACCEPTANCE_OPERATION_TIMING=" + canonical(operation).decode(), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
