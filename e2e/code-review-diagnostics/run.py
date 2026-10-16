#!/usr/bin/env python3
"""Drive the real diagnostic CLI with SDK log files and verify safe CI output."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile

REPORTER = Path(__file__).resolve().parents[2] / ".github/scripts/report_claude_review_failure.py"


def main():
    with tempfile.TemporaryDirectory(prefix="review-diagnostic-") as directory:
        root = Path(directory)
        private = "sk-fixture-credential-must-never-appear"

        def run_case(label, records, expected):
            source = root / "execution.json"
            source.write_text(json.dumps(records), encoding="utf-8")
            result = subprocess.run([sys.executable, str(REPORTER), str(source)],
                                    capture_output=True, text=True, encoding="utf-8", check=True)
            assert not result.stderr, label + result.stderr
            assert private not in result.stdout and "private-prompt" not in result.stdout, label
            output = json.loads(result.stdout)
            assert set(output) <= {"category", "http_status", "provider_code", "is_model_usage_present"}, label
            assert output.get("provider_code") in [None, "1010"], label
            assert "123.456" not in result.stdout, label
            for key, value in expected.items():
                assert output.get(key) == value, label + result.stdout
            print("PASS: " + label)

        def failure(message, **extra):
            return [{"type": "user", "message": "private-prompt " + private},
                    {"type": "result", "subtype": "success", "is_error": True,
                     "result": message, "modelUsage": {}, **extra}]

        run_case("edge rejection without credential or prompt output",
                 failure("API Error: 403 error code: 1010 " + private),
                 {"category": "edge_request_denied", "http_status": 403, "provider_code": "1010"})
        run_case("billing refusal without publishing the balance",
                 failure('API Error: 400 {"error":{"message":"预扣费失败，账户余额不足：123.456"}}'),
                 {"category": "billing_or_quota_refused", "http_status": 400})
        run_case("authentication failure", failure("API Error: 401 invalid api key " + private),
                 {"category": "authentication_refused"})
        run_case("missing model route", failure("API Error: 503 model_not_found"),
                 {"category": "model_route_unavailable", "http_status": 503})
        run_case("plugin failure before model use", failure("Unknown skill: code-review:code-review"),
                 {"category": "plugin_command_unavailable", "http_status": None})
        run_case("tool permission failure", failure("tool denied", permission_denials=[{"tool_name": "Bash"}]),
                 {"category": "tool_permission_refused"})
        run_case("last top-level result wins over an earlier or nested result",
                 failure("API Error: 401") + [{"type": "assistant", "nested": failure("API Error: 503")},
                 {"type": "result", "subtype": "success", "is_error": True,
                  "result": "API Error: 429 rate limited", "modelUsage": {"example-model": {"inputTokens": 10}}}],
                 {"category": "rate_limited", "http_status": 429, "is_model_usage_present": True})
        run_case("action failure after a successful CLI result is distinct",
                 [{"type": "result", "subtype": "success", "is_error": False}],
                 {"category": "action_failed_after_cli_success"})
        run_case("unknown private error stays private", failure(private),
                 {"category": "unclassified_cli_failure"})
        run_case("no completed CLI result", [{"type": "system", "subtype": "init"}],
                 {"category": "missing_cli_result"})
        run_case("unexpected execution log shape", {"secret": private},
                 {"category": "execution_log_unavailable"})
        missing = subprocess.run([sys.executable, str(REPORTER), str(root / "missing.json")],
                                 capture_output=True, text=True, encoding="utf-8", check=True)
        assert json.loads(missing.stdout) == {"category": "execution_log_unavailable"}
        print("PASS: a missing execution file preserves the original failure")
        invalid = root / "invalid.json"
        invalid.write_text("malformed " + private, encoding="utf-8")
        oversized = root / "oversized.json"
        oversized.write_bytes(b" " * (16 * 1024 * 1024 + 1))
        for label, source in [("malformed log", invalid), ("bounded oversized log", oversized)]:
            result = subprocess.run([sys.executable, str(REPORTER), str(source)],
                                    capture_output=True, text=True, encoding="utf-8", check=True)
            assert json.loads(result.stdout) == {"category": "execution_log_unavailable"}, label
            assert private not in result.stdout and not result.stderr, label
            print("PASS: " + label)
        summary = root / "summary.md"
        source = root / "execution.json"
        source.write_text(json.dumps(failure("API Error: 403 error code: 1010 " + private)), encoding="utf-8")
        result = subprocess.run([sys.executable, str(REPORTER), str(source), "--summary-file", str(summary)],
                                capture_output=True, text=True, encoding="utf-8", check=True)
        rendered = summary.read_text(encoding="utf-8")
        assert "### Review failure diagnostics" in rendered and result.stdout.strip() in rendered
        assert private not in rendered and "private-prompt" not in rendered
        print("PASS: the job summary contains only the safe diagnostic")


if __name__ == "__main__":
    main()
