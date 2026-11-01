"""Exercise genuine saved SSH evidence through the production resume path.

Requires a complete author-local acceptance directory with its original logs.
This live diagnostic never signs, publishes or authorizes a source change.
"""

import argparse
import base64
from datetime import datetime, timezone
import importlib.util
import json
from pathlib import Path
import shutil


SOURCE = Path(__file__).resolve().parents[2]


def run(saved, policy_ref, profiles, refused_profiles, output):
    if (output == SOURCE or SOURCE in output.parents or output == saved
            or output in saved.parents or saved in output.parents):
        raise RuntimeError("Keep diagnostic output outside the source and original saved evidence")
    output.mkdir(parents=True, exist_ok=False)
    spec = importlib.util.spec_from_file_location("saved_profile_authority", SOURCE / ".github/scripts/local_acceptance.py")
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    helper.ROOT = SOURCE
    envelope_path, receipt_path = saved / "envelope.json", saved / "receipt.json"
    envelope = json.loads(envelope_path.read_text(encoding="utf-8"))
    payload = base64.b64decode(envelope["payload"], validate=True)
    if payload != receipt_path.read_bytes():
        raise RuntimeError("Saved receipt differs from its original signed payload")
    receipt = json.loads(payload)
    policy = helper.value("rev-parse", policy_ref + "^{commit}")
    head, base = receipt["head"], receipt["base"]
    result = {"started_at": datetime.now(timezone.utc).isoformat(), "saved": str(saved),
              "original_head": head, "original_policy": receipt["commands_ref"], "policy": policy,
              "requested_profiles": profiles, "required_refusals": refused_profiles,
              "scope": "Real SSH signatures and original command-log resumption. No product rerun, new signature, push authorization or cross-head reuse."}
    (output / "attempt.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")

    def resume(directory, name, requested, expected_head=head):
        destination = output / name
        destination.mkdir()
        return helper.resume_checks(directory, expected_head, base, policy, destination, requested)

    def reject(name, operation):
        print(f"Expected negative control: {name} must be refused", flush=True)
        try:
            operation()
        except RuntimeError as error:
            return {"status": "refused", "error": str(error)}
        raise AssertionError(f"Invalid saved evidence passed: {name}")

    def copy_evidence(name):
        destination = output / name
        destination.mkdir()
        for path in [envelope_path, receipt_path]:
            if path.is_symlink():
                raise RuntimeError("Original evidence files must be regular files")
            shutil.copyfile(path, destination / path.name)
        for profile in profiles:
            folder = destination / profile
            folder.mkdir()
            for path in (saved / profile).glob("*.log"):
                if path.is_symlink():
                    raise RuntimeError("Original evidence logs must be regular files")
                shutil.copyfile(path, folder / path.name)
        return destination

    with helper.host_acceptance_lease(result, helper.value("rev-parse", "HEAD")):
        reused, provenance = resume(saved, "retained", profiles)
        assert set(reused) == set(profiles), "Requested completed profiles require fresh execution under this policy"
        assert all(reused[name] == receipt["checks"][name] for name in profiles)
        result["retained"] = provenance
        if refused_profiles:
            declined, _ = resume(saved, "changed-criteria", refused_profiles)
            assert not declined, "A profile with changed criteria inherited another profile's approval"
            result["changed_criteria_remain_required"] = refused_profiles
        result["different_head"] = reject("different head", lambda: resume(saved, "wrong-head", profiles, "0" * 40))
        tampered = copy_evidence("tampered-signature-input")
        changed = {**receipt, "finished_at": receipt["finished_at"] + 1}
        changed_bytes = helper.canonical(changed)
        (tampered / "receipt.json").write_bytes(changed_bytes)
        (tampered / "envelope.json").write_bytes(helper.canonical({
            **envelope, "payload": base64.b64encode(changed_bytes).decode(),
        }))
        result["invalid_signature"] = reject("invalid signature", lambda: resume(tampered, "wrong-signature", profiles))
        tampered = copy_evidence("tampered-log-input")
        for profile in profiles:
            for path in (tampered / profile).glob("*.log"):
                path.write_bytes(path.read_bytes() + b"\nTAMPERED\n")
        result["changed_logs"] = reject("changed logs", lambda: resume(tampered, "wrong-logs", profiles))
        partial, _ = resume(saved, "missing-profile", [*profiles, "unexecuted-profile"])
        assert set(partial) == set(profiles) and "unexecuted-profile" not in partial
        result["unexecuted_profile_remains_required"] = True
    result.update(status="PASS", finished_at=datetime.now(timezone.utc).isoformat())
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--saved", type=Path, required=True)
    parser.add_argument("--policy-ref", required=True)
    parser.add_argument("--profiles", nargs="+", required=True)
    parser.add_argument("--refuse-profiles", nargs="*", default=[])
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    run(args.saved.resolve(strict=True), args.policy_ref, args.profiles, args.refuse_profiles, args.output.resolve())
