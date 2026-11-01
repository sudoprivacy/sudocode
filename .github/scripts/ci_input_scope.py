#!/usr/bin/env python3
"""Select expensive provider/daemon acceptance from actual changed Git inputs."""

import fnmatch
import json
import os
from pathlib import Path
import posixpath
import re
import subprocess

from local_acceptance import POLICY, SIGNERS, production_build_paths


def git(*args):
    return subprocess.check_output(["git", *args]).decode().strip()


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
    ignored_product_inputs = []
    ignored_cohost_cli_sources = []
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text(encoding="utf-8"))
    name = os.environ["GITHUB_EVENT_NAME"]
    if name == "workflow_dispatch":
        paths, live, daemon, cohost, clippy = [], True, True, True, True
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
            paths, live, daemon, cohost, clippy = [], True, True, True, True
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
                             ".github/scripts/vfs_search_cache.py", ".github/actions/vfs-search-cache/**",
                             ".github/workflows/ci-vfs-search-cache.yml"]
            # Acceptance metadata changes execution authorization, not provider
            # requests. Keep these identities with the acceptance authority.
            live = any(fnmatch.fnmatchcase(path, rule)
                       for path in relevant if path not in {POLICY, SIGNERS}
                       for rule in live_inputs)
            # Test modules remain part of all-target/all-feature linting even
            # when they do not rebuild or exercise the daemon binary.
            clippy = any(path.startswith("rust/") for path in relevant)
            # CLI integration modules are not inputs to the daemon/cohost
            # binaries or shell journeys. Native PTYs and live model scope
            # remain unchanged; manifests, src and moved source still qualify.
            normal_inputs = production_build_paths(relevant, base, head)
            ignored_product_inputs = [path for path in relevant if path not in normal_inputs]
            daemon = any(fnmatch.fnmatchcase(path, rule)
                         for path in normal_inputs
                         if not path.startswith("rust/crates/rusty-sudocode-cli/tests/")
                         for rule in daemon_inputs)
            # The CLI package has only a binary; the co-host and VFS search
            # fixture do not import its source. Keep the actual CLI daemon
            # journeys above, and retain all shared/build/harness inputs here.
            cli_sources = [path for path in normal_inputs if path.startswith("rust/crates/rusty-sudocode-cli/src/")]
            if cli_sources and independent_cli_sources(base, head):
                ignored_cohost_cli_sources = cli_sources
            cohost = any(fnmatch.fnmatchcase(path, rule)
                         for path in normal_inputs
                         if not path.startswith("rust/crates/rusty-sudocode-cli/tests/")
                         and path not in ignored_cohost_cli_sources
                         for rule in daemon_inputs)
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
    clippy_executor = any(path in {".github/workflows/rust-ci.yml", ".github/scripts/ci_input_scope.py",
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
              "cohost_required": str(cohost).lower(),
              "benchmark_required": str(benchmark).lower(), "container_required": str(container).lower(),
              "clippy_required": str(clippy).lower(), "clippy_executor_required": str(clippy_executor).lower(),
              "gzip_codec_required": str(gzip_codec).lower()}
    result["vfs_cache_required"] = str(vfs_cache).lower()
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
        for key, value in result.items():
            stream.write(f"{key}={value}\n")
    print(json.dumps({**result, "changed_paths": paths, "normal_product_ignored": ignored_product_inputs,
                      "cohost_cli_source_ignored": ignored_cohost_cli_sources}))


if __name__ == "__main__":
    main()
