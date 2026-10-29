#!/usr/bin/env python3
"""Validate the public cache usage contract, including rejected observations."""

from copy import deepcopy
import json
from pathlib import Path

from jsonschema import Draft202012Validator


def main():
    root = Path(__file__).resolve().parents[1]
    schema = json.loads((root / "spec/cache_usage_fixtures.schema.json").read_text("utf-8"))
    fixtures = json.loads((root / "spec/cache_usage_fixtures.json").read_text("utf-8"))
    Draft202012Validator.check_schema(schema)
    validator = Draft202012Validator(schema)
    validator.validate(fixtures)

    for invalid_kind in ("negative_usage", "unknown_ttl", "fabricated_savings"):
        invalid = deepcopy(fixtures)
        if invalid_kind == "negative_usage":
            invalid["fixtures"][0]["snapshots"][0]["usage"]["input_tokens"] = -1
        elif invalid_kind == "unknown_ttl":
            invalid["fixtures"][0]["snapshots"][0]["requested_ttls_seconds"] = [600]
        else:
            transition = next(f for f in invalid["fixtures"] if "ttl_transition" in f)
            transition["ttl_transition"]["savings_claimed"] = True
        if validator.is_valid(invalid):
            raise SystemExit(f"Schema accepted invalid observation: {invalid_kind}")
    print(f"Validated {len(fixtures['fixtures'])} cache usage fixtures and rejected 3 invalid variants")


if __name__ == "__main__":
    main()
