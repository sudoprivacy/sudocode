"""Run both production benchmark builders with real Cargo and native drivers.

Tiny fixture executables exercise compilation, feature unification and binary
identity. This infrastructure acceptance does not claim product browser coverage.
"""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def run(output, profiles):
    if sys.platform != "linux":
        raise RuntimeError("Browser benchmark executors require Linux. Run this acceptance in local Docker or Linux CI.")
    output.mkdir(parents=True, exist_ok=False)
    inputs = output / "inputs"
    checks = load("benchmark_profiles", profiles)
    executor = load("benchmark_executor", SOURCE / ".github/scripts/local_acceptance.py")
    commands = []

    def command(*args, cwd=inputs, extra=None):
        log = output / "commands" / f"{len(commands):03d}.log"
        environment = {"npm_config_audit": "false", "npm_config_fund": "false", **(extra or {})}
        commands.append(executor.run_command(args, cwd, log, environment))
        return log

    driver = '''#[test]
#[ignore]
fn {name}() {{
    let binary = std::env::var("SCODE_TEST_BIN").unwrap();
    let output = std::process::Command::new(binary).output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "candidate:normal");
    assert_eq!(shared_support::flavor(), "dev");
}}
'''
    measurement = '''import argparse,hashlib,json,os
from pathlib import Path
import subprocess
p=argparse.ArgumentParser()
for name in ["driver","base","candidate","output"]: p.add_argument("--"+name,required=True)
p.add_argument("--policy");p.add_argument("--profile");a=p.parse_args()
assert subprocess.check_output([a.base]).decode().strip()=="baseline:normal"
assert subprocess.check_output([a.candidate]).decode().strip()=="candidate:normal"
case="rendering_driver_runs_release" if a.policy else "browser_terminal_links_and_interaction"
if a.policy: assert a.profile=="core" and json.loads(Path(a.policy).read_text())=={"fixture":True}
subprocess.run([a.driver,"--ignored","--exact",case],env={**os.environ,"SCODE_TEST_BIN":a.candidate},check=True)
out=Path(a.output);out.mkdir()
(out/"fixture.json").write_text(json.dumps({"candidate_sha256":hashlib.sha256(Path(a.candidate).read_bytes()).hexdigest()}))
'''
    for label in ["baseline", "candidate", "trusted"]:
        checkout = inputs / label
        files = {
            "rust/Cargo.toml": '[package]\nname="rusty-sudocode-cli"\nversion="0.0.1"\nedition="2021"\n'
                '[workspace]\nmembers=["support"]\nresolver="2"\n'
                '[[bin]]\nname="scode"\npath="src/main.rs"\n'
                '[dependencies]\nshared-support={path="support",features=["normal"]}\n'
                '[dev-dependencies]\nshared-support={path="support",features=["dev"]}\n',
            "rust/src/main.rs": f'fn main() {{ println!("{label}:{{}}", shared_support::flavor()); }}\n',
            "rust/support/Cargo.toml": '[package]\nname="shared-support"\nversion="0.0.1"\nedition="2021"\n'
                '[features]\nnormal=[]\ndev=[]\n',
            "rust/support/src/lib.rs": 'pub fn flavor() -> &\'static str { if cfg!(feature="dev") { "dev" } else { "normal" } }\n',
            "rust/tests/pty_hyperlinks.rs": driver.format(name="browser_terminal_links_and_interaction"),
            "rust/tests/pty_prompt_viewport.rs": driver.format(name="viewport_browser_interaction"),
            "rust/tests/pty_render_performance.rs": driver.format(name="rendering_driver_runs_release"),
            "e2e/terminal-resize/package.json": '{"name":"browser-builder-acceptance","version":"1.0.0"}\n',
            "e2e/terminal-resize/package-lock.json": '{"name":"browser-builder-acceptance","version":"1.0.0",'
                '"lockfileVersion":3,"packages":{"":{"name":"browser-builder-acceptance","version":"1.0.0"}}}\n',
            "e2e/terminal-resize/gui-benchmark.py": measurement,
            "e2e/terminal-resize/benchmark.py": measurement,
            "e2e/terminal-resize/performance-policy.json": '{"fixture":true}\n',
            "scripts/benchmark_prompt_viewport.py": '''import argparse,hashlib,json,os
from pathlib import Path
import subprocess
p=argparse.ArgumentParser()
for name in ["driver","binary","output"]: p.add_argument("--"+name,required=True)
p.add_argument("--observer",default="headless");a=p.parse_args()
assert subprocess.check_output([a.binary]).decode().strip()=="candidate:normal"
subprocess.run([a.driver,"--ignored","--exact","viewport_browser_interaction"],
    env={**os.environ,"SCODE_TEST_BIN":a.binary},check=True)
out=Path(a.output);out.mkdir()
(out/"fixture.json").write_text(json.dumps({"observer":a.observer,
    "candidate_sha256":hashlib.sha256(Path(a.binary).read_bytes()).hexdigest()}))
''',
        }
        for relative, content in files.items():
            path = checkout / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content, encoding="utf-8")
        if label == "trusted":
            (checkout / "rust/src/main.rs").write_text('compile_error!("Browser must build the candidate, not trusted source");\n')
        command("cargo", "generate-lockfile", "--manifest-path", str(checkout / "rust/Cargo.toml"))

    # Replace only the storage location with private state. Every production
    # command and target/checkout decision executes unmodified; no global cache
    # or another workline's lease is mounted or written.
    checks.benchmark_storage = lambda name: (output / "state" / name, output / "state/target" / name)
    evidence = output / "browser"
    evidence.mkdir()
    locks = output / "locks"
    locks.mkdir()
    with checks.exclusive_benchmark_state(locks):
        checks.benchmark_locked("browser", inputs / "candidate", evidence, "fixture-base", command, output / "unused-target")

    viewport = [output / "commands" / f"{index:03d}.log" for index, row in enumerate(commands)
                if "pty_prompt_viewport" in row["command"] and "--no-run" in row["command"]]
    assert len(viewport) == 1
    artifacts = [json.loads(line) for line in viewport[0].read_text(encoding="utf-8").splitlines()
                 if line.startswith("{")]
    for name in ["shared_support", "scode"]:
        rows = [row for row in artifacts if row.get("reason") == "compiler-artifact"
                and row["target"]["name"] == name]
        assert len(rows) == 1 and rows[0]["fresh"], f"Viewport redundantly rebuilt {name}: {rows}"
    digest = hashlib.sha256((evidence / "scode-candidate").read_bytes()).hexdigest()
    for relative in ["measurements", "viewport-headless", "viewport-browser"]:
        data = json.loads((evidence / relative / "fixture.json").read_text())
        assert data["candidate_sha256"] == digest, "A driver replaced the production binary"
    candidate_build = next(row["command"] for row in commands if "build" in row["command"]
                           and row["command"][row["command"].index("--manifest-path") + 1].endswith("/candidate/rust/Cargo.toml"))
    candidate_target = Path(candidate_build[candidate_build.index("--target-dir") + 1])
    assert subprocess.check_output([str(candidate_target / "release/scode")]).decode().strip() == "candidate:dev"
    assert len(commands) == 12, "A production build/driver/observer stage was omitted"

    # First retain the browser-only guard against accidentally compiling trusted
    # source. Then exercise the usual rendering -> browser order in the same
    # private state, including normal/dev feature switching in real Cargo.
    (inputs / "trusted/rust/src/main.rs").write_text('fn main() {}\n', encoding="utf-8")
    stage_digests = []
    for name in ["rendering", "browser"]:
        stage = output / ("subsequent-" + name)
        stage.mkdir()
        with checks.exclusive_benchmark_state(locks):
            checks.benchmark_locked(name, inputs / "candidate", stage, "fixture-base", command, output / "unused-target")
        stage_digests += [hashlib.sha256((stage / "scode-candidate").read_bytes()).hexdigest(),
                          json.loads((stage / "measurements/fixture.json").read_text())["candidate_sha256"]]
    candidate_builds = [(index, row["command"]) for index, row in enumerate(commands)
                        if "build" in row["command"] and "--manifest-path" in row["command"]
                        and row["command"][row["command"].index("--manifest-path") + 1].endswith("/candidate/rust/Cargo.toml")]
    assert len(candidate_builds) == 3
    assert len({args[args.index("--manifest-path") + 1] for _, args in candidate_builds}) == 1, "Candidate source path must stay canonical across profiles"
    assert len({args[args.index("--target-dir") + 1] for _, args in candidate_builds}) == 1, "Candidate outputs must stay with the same source path"
    assert all(value == digest for value in stage_digests), "Production candidate changed between profiles"
    final_build_log = (output / "commands" / f"{candidate_builds[-1][0]:03d}.log").read_text(encoding="utf-8")
    assert "Compiling " not in final_build_log, "Browser redundantly compiled the candidate after rendering"
    target_trees = sorted({row["command"][row["command"].index("--target-dir") + 1]
                           for row in commands if "--target-dir" in row["command"]})
    assert len(target_trees) == 4, target_trees
    result = {"status": "PASS", "commands": commands, "candidate_sha256": digest,
              "target_trees": target_trees, "cross_profile_candidate_build_reused": True,
              "profiles_sha256": hashlib.sha256(profiles.read_bytes()).hexdigest(),
              "scope": "Real production browser build executor, Cargo and npm; distinct baseline/candidate and native integration drivers, reusable dev artifacts, immutable production binary. Tiny infrastructure fixtures, no product browser coverage credit."}
    (output / "result.json").write_text(json.dumps(result, indent=2), encoding="utf-8")
    print(json.dumps({"status": "PASS", "evidence": str(output), "candidate_sha256": digest}), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--profiles", type=Path, default=SOURCE / ".github/local-acceptance/checks.py")
    arguments = parser.parse_args()
    run(arguments.output.resolve(), arguments.profiles.resolve())
