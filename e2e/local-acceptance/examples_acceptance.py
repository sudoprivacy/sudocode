"""Exercise the production compile-only example profile with real Cargo processes."""
import argparse
import importlib.util
import json
import os
from pathlib import Path


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    root = output / "source"
    rust = root / "rust"
    (rust / "src").mkdir(parents=True)
    (rust / "examples").mkdir()
    (rust / "Cargo.toml").write_text(
        '[workspace]\n[package]\nname="engine-acp"\nversion="0.0.0"\nedition="2021"\n'
        '[features]\nmailbox=[]\n', encoding="utf-8")
    (rust / "src/lib.rs").write_text('pub fn marker() -> &\'static str { "example-retained" }\n', encoding="utf-8")
    example = rust / "examples/canary.rs"
    # Profile compilation must not execute this program's observable side effect.
    example.write_text('fn main() {\n    std::fs::write("example-ran", "ran").unwrap();\n'
                       '    println!("{}", engine_acp::marker());\n}\n', encoding="utf-8")
    helper = load("example_executor", SOURCE / ".github/scripts/local_acceptance.py")
    profiles = load("example_profiles", SOURCE / ".github/local-acceptance/checks.py")
    helper.ROOT = SOURCE
    commands = [helper.run_command(["cargo", "generate-lockfile", "--offline"], rust, output / "lock.log")]
    previous = os.environ.get("CARGO_TARGET_DIR")
    os.environ["CARGO_TARGET_DIR"] = str(output / "target")
    try:
        commands += profiles.run("native-examples", root, output / "good", "HEAD", helper.run_command)
        assert not (rust / "example-ran").exists(), "Compile-only profile ran an example entry point"
        binary = output / "target/debug/examples" / ("canary.exe" if os.name == "nt" else "canary")
        assert binary.is_file(), "Native profile omitted the normal example binary"
        observed = output / "actual-example.log"
        commands.append(helper.run_command([str(binary)], rust, observed))
        assert observed.read_text(encoding="utf-8").strip() == "example-retained"
        assert (rust / "example-ran").read_text() == "ran"
        example.write_text('compile_error!("BROKEN_COMPILE_ONLY_EXAMPLE");\nfn main() {}\n', encoding="utf-8")
        try:
            profiles.run("native-examples", root, output / "broken", "HEAD", helper.run_command)
        except RuntimeError:
            failed = json.loads((output / "broken/000.command.json").read_text())
            assert failed["exit_code"] != 0
            assert "BROKEN_COMPILE_ONLY_EXAMPLE" in (output / "broken/000.log").read_text(encoding="utf-8")
        else:
            raise AssertionError("The production profile accepted a broken compile-only example")
    finally:
        if previous is None:
            os.environ.pop("CARGO_TARGET_DIR", None)
        else:
            os.environ["CARGO_TARGET_DIR"] = previous
    result = {"status": "PASS", "normal_example_built": True, "profile_did_not_run_example": True,
              "actual_example_process_passed": True, "broken_example_refused": True,
              "commands": commands, "expected_failure": failed,
              "scope": "Real production dispatcher, Cargo compilation/linking and native example execution. Tiny infrastructure fixture; no product/provider coverage credit."}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(f"Native example profile passed normal-link/compile-only/runtime-canary/broken-example checks: {output}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    run(parser.parse_args().output.resolve())
