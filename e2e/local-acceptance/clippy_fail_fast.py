"""Exercise the CI lint command with real independent Cargo packages."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import time

import psutil


SOURCE = Path(__file__).resolve().parents[2]


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    workflow = (SOURCE / ".github/workflows/rust-ci.yml").read_text(encoding="utf-8")
    job = workflow.split("  clippy-workspace:\n", 1)[1].split("\n  integrated-acceptance:", 1)[0]
    commands = [shlex.split(line.strip()[5:]) for line in job.splitlines()
                if line.strip().startswith("run: python ")]
    assert commands == [["python", "../.github/scripts/ci_clippy.py"]], \
        "CI lint must invoke the shared queue selector; update this fixture if its interface changes"
    project = output / "project/rust"
    project.mkdir(parents=True)
    command = [sys.executable, str(SOURCE / ".github/scripts/ci_clippy.py"),
               "--repository", str(project.parent), "--message-format", "json"]
    (project / "Cargo.toml").write_text('[workspace]\nmembers=["crates/rusty-sudocode-cli", "second", "third"]\nresolver="2"\n')
    package_paths = {"first": project / "crates/rusty-sudocode-cli",
                     "second": project / "second", "third": project / "third"}
    for name in ["first", "second", "third"]:
        package = package_paths[name]
        (package / "src").mkdir(parents=True)
        (package / "Cargo.toml").write_text(f'[package]\nname="lint-{name}"\nversion="0.0.0"\nedition="2021"\n'
            '[features]\nextra=[]\n')
        (package / "src/lib.rs").write_text('#[cfg(feature="extra")]\npub fn feature_path() { let unused_value = 42; }\n')
    environment = dict(os.environ, CARGO_BUILD_JOBS="1", CARGO_INCREMENTAL="0",
                       CARGO_PROFILE_DEV_DEBUG="0", CARGO_PROFILE_TEST_DEBUG="0", CARGO_TERM_COLOR="always")
    environment.pop("SCODE_QUEUE_BASE", None)
    subprocess.run(["cargo", "generate-lockfile", "--offline"], cwd=project, env=environment,
                   check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    # Lint compilation runs no product/test executable. Keep this fixture's
    # Python assertions Normal while its Cargo/compiler process stays below it.
    def invoke(label, arguments, target, compiler_expected=True):
        environment["CARGO_TARGET_DIR"] = str(target)
        if os.name != "nt":
            arguments = ["nice", "-n", "10", *arguments]
        started = time.monotonic()
        stdout, stderr = output / (label + ".stdout.log"), output / (label + ".stderr.log")
        observed = {}
        with stdout.open("wb") as out, stderr.open("wb") as err:
            process = subprocess.Popen(arguments, cwd=project, env=environment, stdout=out, stderr=err,
                creationflags=subprocess.BELOW_NORMAL_PRIORITY_CLASS if os.name == "nt" else 0)
            while process.poll() is None:
                try:
                    for child in [psutil.Process(process.pid), *psutil.Process(process.pid).children(recursive=True)]:
                        try:
                            name = Path(child.exe()).name.lower()
                            if name in {"rustc", "rustc.exe", "clippy-driver", "clippy-driver.exe"}:
                                observed[child.pid, name] = child.nice()
                        except (psutil.NoSuchProcess, psutil.AccessDenied):
                            continue
                except (psutil.NoSuchProcess, psutil.AccessDenied):
                    pass
                time.sleep(0.01)
        # The executor's bounded shape: {compiler name: {priority: distinct children}}.
        priorities = {}
        for (_pid, name), priority in observed.items():
            counts = priorities.setdefault(name, {})
            counts[str(priority)] = counts.get(str(priority), 0) + 1
        if compiler_expected:
            assert priorities, "Did not observe the actual lint compiler; inspect the retained process logs"
        expected = psutil.BELOW_NORMAL_PRIORITY_CLASS if os.name == "nt" else 10
        assert all(set(counts) == {str(expected)} for counts in priorities.values()), priorities
        lines = stdout.read_text(encoding="utf-8").splitlines()
        messages = [json.loads(line) for line in lines if line.startswith("{")]
        scopes = [json.loads(line.split("=", 1)[1]) for line in lines if line.startswith("QUEUE_CLIPPY_SCOPE=")]
        errors = [row for row in messages if row.get("reason") == "compiler-message"
                  and row["message"]["level"] == "error" and row["message"].get("code", {}).get("code") == "unused_variables"]
        artifacts = [row for row in messages if row.get("reason") == "compiler-artifact"]
        return {"command": arguments, "exit_code": process.returncode, "compiler_priorities": priorities,
                "elapsed_seconds": round(time.monotonic() - started, 3),
                "failed_packages": sorted({row["package_id"] for row in errors}),
                "artifacts": artifacts, "scope": scopes[0] if scopes else None,
                "errors": errors}

    fast = invoke("first-failure", command, output / "fast-target")
    selected = fast["scope"]["command"]
    assert selected[:2] == ["cargo", "clippy"]
    assert {"--workspace", "--locked", "--all-targets", "--all-features"}.issubset(selected)
    assert selected[-3:] == ["--", "-D", "warnings"]
    assert "--keep-going" not in selected, "CI must stop after its first lint error"
    previous = [*selected[:2], "--keep-going", *selected[2:]]
    continuing = invoke("previous-keep-going", previous, output / "previous-target")
    assert fast["exit_code"] != 0 and len(fast["failed_packages"]) == 1, fast
    assert continuing["exit_code"] != 0 and len(continuing["failed_packages"]) == 3, continuing
    # The warning exists only under the extra feature: the actual negative
    # proves all-features and -D warnings remain active in the selected command.
    for package in package_paths.values():
        source = package / "src/lib.rs"
        if source.is_file():
            source.write_text(source.read_text().replace("let unused_value", "let _unused_value"))
    passing = invoke("clean-workspace", command, output / "fast-target")
    assert passing["exit_code"] == 0, passing
    artifacts = passing["artifacts"]
    assert len({row["package_id"] for row in artifacts}) == 3, artifacts
    assert all("extra" in row["features"] for row in artifacts), artifacts
    assert {row["profile"]["test"] for row in artifacts} == {False, True}, artifacts

    # Actual Git commits select only a changed original and its containing
    # group. Ordinary/shared changes restore all original enabled targets.
    spec = importlib.util.spec_from_file_location("clippy_fixture_groups", SOURCE / ".github/local-acceptance/checks.py")
    recipes = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(recipes)
    first = package_paths["first"]
    (first / "tests/acceptance_groups").mkdir(parents=True)
    (first / "tests/support").mkdir()
    manifest = first / "Cargo.toml"
    manifest.write_text(manifest.read_text() + '\n[[test]]\nname="acceptance_first"\n'
                        'path="tests/acceptance_groups/acceptance_first.rs"\ntest=false\n'
                        '\n[[test]]\nname="manual_driver"\npath="tests/support/manual_driver.rs"\ntest=false\n')
    for name in ["covered_a", "covered_b"]:
        (first / f"tests/{name}.rs").write_text(f'#[test]\nfn {name}() {{ lint_first::feature_path(); }}\n')
    wrapper = first / "tests/acceptance_groups/acceptance_first.rs"
    wrapper.write_text(recipes.test_group_source(["covered_a", "covered_b"]), encoding="utf-8")
    manual = first / "tests/support/manual_driver.rs"
    manual.write_text('compile_error!("manual driver must stay excluded");\n')
    (first / "tests/unrelated.rs").write_text('#[path="support/shared.rs"]\nmod shared;\n#[test]\nfn unrelated() { shared::value(); }\n')
    shared = first / "tests/support/shared.rs"
    shared.write_text('pub fn value() {}\n')
    (first / "examples").mkdir()
    (first / "benches").mkdir()
    example, bench = first / "examples/demo.rs", first / "benches/sample.rs"
    example.write_text('fn main() { lint_first::feature_path(); }\n')
    bench.write_text('fn main() { lint_first::feature_path(); }\n')
    binary = project / "second/src/main.rs"
    binary.write_text('fn main() {}\n')
    # A normal dependency's extra API is enabled by another workspace member.
    # Retaining --workspace must preserve that graph even with few test roots.
    manifest.write_text(manifest.read_text() + '\n[dependencies]\nlint-second={path="../../second"}\n')
    third_manifest = project / "third/Cargo.toml"
    third_manifest.write_text(third_manifest.read_text() + '\n[dependencies]\nlint-second={path="../second",features=["extra"]}\n')
    first_library = first / "src/lib.rs"
    first_library.write_text(first_library.read_text() + '\n#[cfg(feature="extra")]\npub fn require_shared_extra() { lint_second::feature_path(); }\n')
    subprocess.run(["cargo", "generate-lockfile", "--offline"], cwd=project, env=environment,
                   check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def workspace_git(*arguments):
        return subprocess.check_output(["git", *arguments], cwd=project.parent, env=environment,
                                       stderr=subprocess.PIPE).decode().strip()

    def commit_workspace(message):
        workspace_git("add", ".")
        workspace_git("commit", "--quiet", "-m", message)
        return workspace_git("rev-parse", "HEAD")

    workspace_git("init", "--quiet")
    workspace_git("config", "user.name", "CI fixture")
    workspace_git("config", "user.email", "fixture@example.invalid")
    workspace_git("config", "core.autocrlf", "false")
    workspace_git("config", "commit.gpgsign", "false")
    lint_base = commit_workspace("Original workspace with test groups")
    changed_test = first / "tests/covered_a.rs"
    changed_test.write_text(changed_test.read_text() + "\n// Changed isolated test.\n")
    commit_workspace("Change one CLI test")
    isolated_command = [*command, "--base", lint_base]
    grouped = invoke("isolated-clean", isolated_command, output / "fast-target")
    assert grouped["exit_code"] == 0, grouped
    scope = grouped["scope"]
    assert scope["mode"] == "changed-cli-tests" and scope["changed_tests"] == ["covered_a"], scope
    assert scope["integration_targets_declared"] == 5, scope
    assert scope["integration_targets_before"] == 3 and scope["integration_targets_selected"] == 2, scope
    targets = {row["target"]["name"] for row in grouped["artifacts"]}
    assert {"acceptance_first", "covered_a", "lint_first", "lint_second"} <= targets, targets
    assert not {"covered_b", "unrelated", "manual_driver", "demo", "sample", "lint-second", "lint_third"} & targets, targets
    assert all("extra" in row["features"] for row in grouped["artifacts"]), grouped
    assert {row["profile"]["test"] for row in grouped["artifacts"]} == {False, True}, grouped
    wrong_package = invoke("lost-workspace-feature-refused",
        ["cargo", "clippy", "--locked", "-p", "lint-first", "--all-features", "--test", "covered_a",
         "--message-format=json", "--", "-D", "warnings"], output / "fast-target")
    assert wrong_package["exit_code"] != 0, wrong_package
    assert "feature_path" in (output / "lost-workspace-feature-refused.stdout.log").read_text(encoding="utf-8")

    # Inspect real old/new compiler artifacts: declaration count alone includes
    # disabled groups/manual drivers and overstates the former lint inventory.
    manual_source = manual.read_text()
    manual.write_text('compile_error!("manual driver must stay excluded");\n')
    original_targets = invoke("original-all-targets", selected, output / "fast-target")
    assert original_targets["exit_code"] == 0, original_targets
    originals = {row["target"]["name"] for row in original_targets["artifacts"]}
    assert {"covered_a", "covered_b", "unrelated"} <= originals, originals
    assert not {"acceptance_first", "manual_driver"} & originals, originals
    manual_excluded = invoke("isolated-manual-excluded", isolated_command, output / "fast-target", compiler_expected=False)
    assert manual_excluded["exit_code"] == 0, manual_excluded
    assert manual_excluded["artifacts"] and all(row["fresh"] for row in manual_excluded["artifacts"]), manual_excluded
    manual.write_text(manual_source)
    controls = []
    for label, path, cfg in [
        ("group-first-module", first / "tests/covered_a.rs", "feature=\"extra\""),
        ("group-second-module", first / "tests/covered_b.rs", "feature=\"extra\""),
        ("example", example, "feature=\"extra\""),
        ("bench", bench, "feature=\"extra\""),
        ("library-test-configuration", first / "src/lib.rs", "all(test, feature=\"extra\")"),
        ("binary-test-configuration", binary, "all(test, feature=\"extra\")"),
        ("shared-helper", shared, "feature=\"extra\""),
    ]:
        original = path.read_text()
        path.write_text(original + f'\n#[cfg({cfg})]\npub fn lint_poison() {{ let unused_value = 42; }}\n')
        commit_workspace("Inject " + label)
        negative = invoke(label, isolated_command, output / "fast-target")
        assert negative["exit_code"] != 0 and negative["errors"], negative
        expected_mode = "changed-cli-tests" if label.startswith("group-") else "all-targets"
        assert negative["scope"]["mode"] == expected_mode, negative["scope"]
        assert any(any(Path(span["file_name"]).name == path.name for span in error["message"]["spans"])
                   for error in negative["errors"]), negative
        controls.append({"case": label, "exit_code": negative["exit_code"],
                         "elapsed_seconds": negative["elapsed_seconds"]})
        path.write_text(original)
        commit_workspace("Restore " + label)
    # Invalid wrapper declarations must refuse selection before lint begins.
    wrapper.write_text(wrapper.read_text(encoding="utf-8") + '\nfn hidden_code() {}\n', encoding="utf-8")
    invalid = invoke("invalid-wrapper", command, output / "fast-target", compiler_expected=False)
    assert invalid["exit_code"] != 0 and invalid["scope"] is None, invalid
    assert "canonical path modules" in (output / "invalid-wrapper.stderr.log").read_text(encoding="utf-8")
    wrapper.write_text(recipes.test_group_source(["covered_a", "covered_b"]), encoding="utf-8")
    grouped_final = invoke("isolated-restored", isolated_command, output / "fast-target")
    assert grouped_final["exit_code"] == 0, grouped_final
    ordinary = invoke("ordinary-full-targets", command, output / "fast-target")
    assert ordinary["exit_code"] == 0 and ordinary["scope"]["mode"] == "all-targets", ordinary
    assert {"covered_a", "covered_b", "unrelated"} <= {row["target"]["name"] for row in ordinary["artifacts"]}
    unavailable = invoke("unavailable-base", [*command, "--base", "0" * 40], output / "fast-target", compiler_expected=False)
    assert unavailable["exit_code"] == 0 and unavailable["scope"]["mode"] == "all-targets", unavailable
    untracked = first / "tests/extra_untracked.rs"
    untracked.write_text('#[test]\nfn extra_untracked() {}\n')
    dirty = invoke("untracked-input-full", isolated_command, output / "fast-target")
    assert dirty["exit_code"] == 0 and dirty["scope"]["mode"] == "all-targets", dirty
    assert "extra_untracked" in {row["target"]["name"] for row in dirty["artifacts"]}
    untracked.unlink()
    # Actual committed Git events select the real three-platform queue lint
    # when this entry changes. Documentation alone must not allocate it.
    repository = output / "scope-repository"
    repository.mkdir()

    def git(*arguments):
        return subprocess.check_output(["git", *arguments], cwd=repository, env=environment,
                                       stderr=subprocess.PIPE).decode().strip()

    git("init", "--quiet")
    git("config", "user.name", "CI fixture")
    git("config", "user.email", "fixture@example.invalid")
    (repository / "README.md").write_text("Original docs\n")
    git("add", ".")
    git("commit", "--quiet", "-m", "Initial fixture")
    base = git("rev-parse", "HEAD")
    (repository / "README.md").write_text("Updated docs\n")
    git("commit", "--quiet", "-am", "Update docs")
    docs = git("rev-parse", "HEAD")
    (repository / ".github/scripts").mkdir(parents=True)
    (repository / ".github/scripts/ci_clippy.py").write_text("# Changed queue entry\n")
    git("add", ".")
    git("commit", "--quiet", "-m", "Update queue entry")
    changed = git("rev-parse", "HEAD")
    selections = []
    for head, required in [(docs, "false"), (changed, "true")]:
        for event_name in ["pull_request", "push", "merge_group"]:
            event = {"pull_request": {"base": {"sha": base}, "head": {"sha": head}}} if event_name == "pull_request" else (
                {"before": base, "after": head} if event_name == "push" else
                {"merge_group": {"base_sha": base, "head_sha": head}})
            label = f"scope-{required}-{event_name}"
            event_file, result_file = output / (label + ".json"), output / (label + ".txt")
            event_file.write_text(json.dumps(event), encoding="utf-8")
            value = subprocess.check_output([sys.executable, str(SOURCE / ".github/scripts/ci_input_scope.py")],
                cwd=repository, env={**environment, "GITHUB_EVENT_NAME": event_name,
                                     "GITHUB_EVENT_PATH": str(event_file), "GITHUB_OUTPUT": str(result_file)})
            (output / (label + ".stdout.log")).write_bytes(value)
            selection = json.loads(value)
            assert selection["clippy_required"] == required, selection
            assert selection["clippy_executor_required"] == required, selection
            assert selection["daemon_required"] == "false" and selection["live_required"] == "false", selection
            selections.append({"event": event_name, "head": head, "clippy_required": required})
    result = {"status": "PASS", "first_failure": fast, "previous_keep_going": continuing,
              "clean_workspace": passing,
              "grouped_workspace": grouped, "grouped_restored": grouped_final, "controls": controls,
              "original_all_targets": original_targets, "grouped_manual_excluded": manual_excluded,
              "ordinary_full_targets": ordinary, "unavailable_base": unavailable,
              "dirty_worktree_full_targets": dirty,
              "isolated_package_loses_required_feature": wrong_package,
              "real_git_selections": selections,
              "scope": "Actual Git/workflow/Cargo/Clippy processes. Isolated originals plus groups, ordinary/shared/full fallback, all-feature first-error behavior, examples/benches/lib-bin test configurations and manual exclusions are exercised. Only unchanged unrelated integration targets are omitted for isolated committed CLI test edits. Infrastructure only; no product PTY or production latency credit."}
    (output / "result.json").write_text(json.dumps(result, indent=2), encoding="utf-8")
    print(json.dumps({"status": "PASS", "failed_packages_before": 3, "failed_packages_after": 1,
                      "passing_packages": 3, "evidence": str(output)}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    run(parser.parse_args().output.resolve())
