#!/usr/bin/env python3
"""Select expensive provider/daemon acceptance from actual changed Git inputs."""

import fnmatch
import json
import os
from pathlib import Path
import re
import subprocess

from local_acceptance import POLICY, SIGNERS, production_build_paths


def git(*args):
    return subprocess.check_output(["git", *args]).decode().strip()


def main():
    ignored_product_inputs = []
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
            daemon_inputs = ["rust/**", "e2e/nexus-a2a/**", "e2e/shared-services/**", ".github/ci/**"]
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
            cohost = any(fnmatch.fnmatchcase(path, rule)
                         for path in normal_inputs
                         if not path.startswith(("rust/crates/rusty-sudocode-cli/src/",
                                                 "rust/crates/rusty-sudocode-cli/tests/"))
                         for rule in daemon_inputs)
    executor_inputs = {".github/local-acceptance/checks.py", ".github/local-acceptance/Dockerfile",
                       ".github/scripts/local_acceptance.py", ".github/scripts/ci_input_scope.py",
                       ".github/workflows/rust-ci.yml"}
    benchmark = any(path in executor_inputs | {"e2e/local-acceptance/benchmark_targets_acceptance.py"}
                    or path.startswith("rust/crates/scode-benchmark-drivers/")
                    for path in paths)
    container = any(path in executor_inputs | {"e2e/local-acceptance/container_inputs_acceptance.py"}
                    for path in paths)
    clippy_executor = any(path in {".github/workflows/rust-ci.yml", ".github/scripts/ci_input_scope.py",
                                  "e2e/local-acceptance/clippy_fail_fast.py"} for path in paths)
    gzip_codec = any(path.startswith("e2e/response-gzip/")
                     or path in executor_inputs | {"rust/crates/runtime/src/response_archive.rs",
                                                   "rust/crates/runtime/src/response_archive/gzip.rs"}
                     for path in paths)
    result = {"live_required": str(live).lower(), "daemon_required": str(daemon).lower(),
              "cohost_required": str(cohost).lower(),
              "benchmark_required": str(benchmark).lower(), "container_required": str(container).lower(),
              "clippy_required": str(clippy).lower(), "clippy_executor_required": str(clippy_executor).lower(),
              "gzip_codec_required": str(gzip_codec).lower()}
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
        for key, value in result.items():
            stream.write(f"{key}={value}\n")
    print(json.dumps({**result, "changed_paths": paths, "normal_product_ignored": ignored_product_inputs}))


if __name__ == "__main__":
    main()
