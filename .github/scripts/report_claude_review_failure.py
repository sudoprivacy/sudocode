#!/usr/bin/env python3
"""Describe a failed review from SDK records without publishing message contents."""

import argparse
import json
from pathlib import Path
import re

MAX_LOG_BYTES = 16 * 1024 * 1024
MAX_ERROR_CHARS = 16_384


def classify(records):
    results = [record for record in records if isinstance(record, dict) and record.get("type") == "result"]
    if not results:
        return {"category": "missing_cli_result"}
    result = results[-1]
    if result.get("is_error") is False and result.get("subtype") == "success":
        return {"category": "action_failed_after_cli_success"}
    fragments = [result.get("result")]
    errors = result.get("errors")
    if isinstance(errors, list):
        fragments.extend(errors)
    text = "\n".join(value[:MAX_ERROR_CHARS] for value in fragments if isinstance(value, str))[:MAX_ERROR_CHARS]
    lower = text.casefold()
    status_match = re.search(r"\b(?:api error|http(?: status)?)\s*:?\s*([1-5][0-9]{2})\b", lower)
    status = int(status_match[1]) if status_match else None
    category = "unclassified_cli_failure"
    provider_code = None
    if status == 403 and re.search(r"\berror code\s*:\s*1010\b", lower):
        category, provider_code = "edge_request_denied", "1010"
    elif any(value in lower for value in ["insufficient_quota", "insufficient balance", "credit balance", "余额不足", "额度不足", "预扣费", "prepayment", "pre-payment"]):
        category = "billing_or_quota_refused"
    elif status == 401 or any(value in lower for value in ["authentication_error", "invalid api key", "not logged in"]):
        category = "authentication_refused"
    elif any(value in lower for value in ["model_not_found", "model not found", "no available channel"]):
        category = "model_route_unavailable"
    elif any(value in lower for value in ["unknown skill", "unknown slash command", "plugin not found"]):
        category = "plugin_command_unavailable"
    elif result.get("permission_denials"):
        category = "tool_permission_refused"
    elif status == 429:
        category = "rate_limited"
    elif status is not None and status >= 500:
        category = "upstream_service_failure"
    elif status is not None:
        category = "http_request_refused"
    return {"category": category, "http_status": status, "provider_code": provider_code,
            "is_model_usage_present": bool(result.get("modelUsage"))}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("execution_file", type=Path)
    parser.add_argument("--summary-file", type=Path)
    args = parser.parse_args()
    try:
        if args.execution_file.is_symlink():
            raise ValueError("symlink")
        with args.execution_file.open("rb") as stream:
            raw = stream.read(MAX_LOG_BYTES + 1)
        if len(raw) > MAX_LOG_BYTES:
            raise ValueError("size")
        records = json.loads(raw)
        if not isinstance(records, list):
            raise ValueError("shape")
        summary = classify(records)
    except (OSError, ValueError, RecursionError):
        summary = {"category": "execution_log_unavailable"}
    # Emit only fixed categories, numeric HTTP status and presence flags. Never
    # print provider messages, credentials, prompts or review tool output.
    encoded = json.dumps(summary, sort_keys=True)
    print(encoded)
    if args.summary_file:
        with args.summary_file.open("a", encoding="utf-8") as output:
            output.write("\n### Review failure diagnostics\n\n```json\n" + encoded + "\n```\n")


if __name__ == "__main__":
    main()
