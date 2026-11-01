"""Exercise the production power guard around real native fixture processes.

Only this isolated fixture replaces pmset with a native probe process. It does
not change the machine's power settings or produce product acceptance receipts.
"""

import argparse
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import threading


SOURCE = Path(__file__).resolve().parents[2]


def scenario(directory, name):
    spec = importlib.util.spec_from_file_location("power_profiles", SOURCE / ".github/local-acceptance/checks.py")
    profiles = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(profiles)
    profiles.platform.system = lambda: "Linux" if name == "other-platform" else "Darwin"
    state = directory / "state.json"
    initial = {"source": "AC Power", "mode": 0}
    if name == "battery":
        initial = {"source": "Battery Power", "mode": 1}
    elif name == "low-power":
        initial["mode"] = 1
    elif name == "unknown-source":
        initial["source"] = "Unknown Power"
    elif name in {"missing-setting", "probe-failure"}:
        initial["problem"] = name
    state.write_text(json.dumps(initial))
    probe = directory / "pmset.py"
    probe.write_text(
        'import json,pathlib,sys\n'
        'path=pathlib.Path(sys.argv[1]); state=json.loads(path.read_text())\n'
        'if state.get("problem")=="probe-failure":\n'
        '    (path.parent/"transition-observed").touch(); sys.exit(9)\n'
        'if sys.argv[-1]=="ps":\n'
        '    if state["source"]!="AC Power": (path.parent/"transition-observed").touch()\n'
        '    print("Now drawing from " + repr(state["source"]))\n'
        'elif state.get("problem")!="missing-setting":\n'
        '    if state["mode"]: (path.parent/"transition-observed").touch()\n'
        '    print(state["mode"])\n'
    )
    original = subprocess.check_output
    calls = []

    def native_probe(command, **kwargs):
        assert command[:2] == ["/usr/bin/pmset", "-g"], command
        calls.append(command)
        return original([sys.executable, str(probe), str(state), *command[1:]], **kwargs)

    profiles.subprocess.check_output = native_probe
    profiles.performance_low_power_mode = lambda: bool(int(original(
        [sys.executable, str(probe), str(state), "low-power"], text=True, timeout=5).strip()))
    marker = directory / "workload-started"
    workload = directory / "workload.py"
    workload.write_text(
        'import json,pathlib,sys,time\n'
        'root=pathlib.Path(sys.argv[1]); name=sys.argv[2]\n'
        '(root/"workload-started").write_text("real native workload")\n'
        'if name.startswith("during-") or name=="original-failure":\n'
        '    change={"source":"AC Power","mode":0}\n'
        '    if name in {"during-battery","original-failure"}: change["source"]="Battery Power"\n'
        '    elif name=="during-low-power": change["mode"]=1\n'
        '    else: change["problem"]="probe-failure"\n'
        '    state=root/"state.json"; staged=root/"next-state.json"\n'
        '    staged.write_text(json.dumps(change)); staged.replace(state)\n'
        '    deadline=time.monotonic()+15\n'
        '    while not (root/"transition-observed").exists():\n'
        '        assert time.monotonic()<deadline, "Native monitor never observed the transition"\n'
        '        time.sleep(.01)\n'
        '    if name=="original-failure": sys.exit(17)\n'
        '    staged.write_text(json.dumps({"source":"AC Power","mode":0})); staged.replace(state)\n'
    )
    caught = None
    try:
        with profiles.stable_performance_power(directory):
            subprocess.run([sys.executable, str(workload), str(directory), name], check=True)
    except (RuntimeError, subprocess.CalledProcessError) as error:
        caught = error
    assert not any(worker.name == "acceptance-power" for worker in threading.enumerate()), "Monitor outlived its window"
    if name == "other-platform":
        assert caught is None and marker.exists() and not calls
        assert not (directory / "host-power.json").exists()
        return
    evidence = json.loads((directory / "host-power.json").read_text())
    if name == "stable":
        assert caught is None and marker.exists() and not evidence["invalid"]
        assert len(evidence["observations"]) >= 2
    else:
        assert caught is not None and evidence["invalid"], (name, caught, evidence)
        if name == "original-failure":
            assert isinstance(caught, subprocess.CalledProcessError) and caught.returncode == 17
        else:
            assert isinstance(caught, RuntimeError) and "Power evidence:" in str(caught)
        assert marker.exists() == (name.startswith("during-") or name == "original-failure")
        if name.startswith("during-"):
            assert evidence["observations"][-1].get("source") == "AC Power"
            assert not evidence["observations"][-1].get("low_power_mode")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--scenario")
    args = parser.parse_args()
    output = args.output.resolve()
    if output == SOURCE or SOURCE in output.parents:
        raise RuntimeError("Keep fixture output outside the source checkout")
    if args.scenario:
        scenario(output, args.scenario)
        return
    output.mkdir(parents=True, exist_ok=False)
    names = ["stable", "battery", "low-power", "unknown-source", "missing-setting", "probe-failure",
             "during-battery", "during-low-power", "during-probe-failure", "original-failure", "other-platform"]
    results = []
    for name in names:
        directory = output / name
        directory.mkdir()
        command = [sys.executable, str(Path(__file__).resolve()), "--output", str(directory), "--scenario", name]
        with (directory / "driver.log").open("w") as stream:
            completed = subprocess.run(command, stdout=stream, stderr=subprocess.STDOUT, timeout=30)
        assert completed.returncode == 0, f"Power fixture failed without retry: {directory / 'driver.log'}"
        results.append({"scenario": name, "exit_code": completed.returncode})
    (output / "result.json").write_text(json.dumps({
        "kind": "native-infrastructure-fixtures", "scenarios": results,
        "product_or_real_power_acceptance_claimed": False,
    }, indent=2) + "\n")
    print(f"PASS: native power admission, transitions, probe errors and original failure preservation; {output}")


if __name__ == "__main__":
    main()
