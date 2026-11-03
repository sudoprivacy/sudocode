"""Generate the legacy field-name projection from canonical input_accounting.

The public file keeps its existing schema and fields for pinned consumers.
Only provider_semantics is generated; defaults, paths and scenarios remain
source data. --check never rewrites the contract.
"""
import argparse
import json
from pathlib import Path
import re


SPEC = Path(__file__).resolve().parents[1] / "spec/prompt_cache_observations.json"


def projection(contract):
    """Keep the historical primary field names, with aliases owned by accounting."""
    providers = {}
    for name, rules in contract["input_accounting"].items():
        if not isinstance(rules, dict) or "usage_object" not in rules:
            continue
        fields = {}
        for kind in ("read", "write"):
            paths = rules[f"{kind}_paths"]
            if not isinstance(paths, list) or any(not isinstance(path, str) or not path for path in paths):
                raise ValueError(f"Invalid canonical {name}.{kind}_paths")
            fields[f"{kind}_field"] = paths[0] if paths else None
        providers[name] = fields
    if not providers:
        raise ValueError("No canonical provider accounting definitions")
    return providers


def check_spec(path=SPEC):
    """Reject manually diverging compatibility metadata in the real public file."""
    contract = json.loads(path.read_text(encoding="utf-8"))
    if contract["provider_semantics"] != projection(contract):
        raise ValueError("provider_semantics drifted from input_accounting; run python scripts/generate_cache_semantics.py")
    print("Public cache field projection matches canonical input_accounting")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    if args.check:
        check_spec()
        return
    raw = SPEC.read_text(encoding="utf-8")
    expected = projection(json.loads(raw))
    # Replace only the owned top-level value; preserve the public fixtures and
    # their formatting rather than serializing a second copy of the contract.
    match = re.search(r'(?m)^  "provider_semantics":\s*', raw)
    if match is None:
        raise ValueError("Missing provider_semantics projection")
    _, length = json.JSONDecoder().raw_decode(raw[match.end():])
    generated = json.dumps(expected, indent=2).replace("\n", "\n  ")
    SPEC.write_text(raw[:match.end()] + generated + raw[match.end() + length:], encoding="utf-8")
    check_spec()


if __name__ == "__main__":
    main()
