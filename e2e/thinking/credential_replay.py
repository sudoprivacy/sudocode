"""Verify copied-history credential redaction through the funded Read/resume journey."""

import argparse
import hashlib
import json
import subprocess
import sys
import tempfile
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request, urlopen

parser = argparse.ArgumentParser()
parser.add_argument("binary", type=Path)
parser.add_argument("--profile", default="sudorouter")
parser.add_argument("--model", default="claude-sonnet-4-6")
parser.add_argument(
    "--config", type=Path, default=Path.home() / ".nexus/sudocode/sudocode.json"
)
parser.add_argument("--artifacts", type=Path, required=True)
parser.add_argument("--upstream-channel", type=int)
parser.add_argument("--drop-global-cache-scope", action="store_true")
args = parser.parse_args()
profile = json.loads(args.config.read_text(encoding="utf-8-sig"))["auth_modes"][
    "proxy"
][args.profile]
sentinel = "DIAGNOSTIC_LEGACY_SECRET_20261012"
directory = Path(tempfile.mkdtemp(prefix="scode-credential-replay-")).resolve()
assert directory.parent == Path(tempfile.gettempdir()).resolve()
config = directory / "config.json"
fixture = directory / "fixture.jsonl"
source = Path(__file__).resolve().parent / "fixtures/empty-legacy-signature.jsonl"
rows = [json.loads(line) for line in source.read_text(encoding="utf-8").splitlines()]
rows[-1]["message"]["blocks"][-1]["text"] += (
    " A saved config value was " + sentinel + "."
)
try:
    config.write_text(
        json.dumps(
            {
                "auth_modes": {
                    "proxy": {
                        args.profile: profile,
                        "test-inactive-replay": {
                            "apiKey": sentinel,
                            "baseUrl": "https://unused.invalid/v1",
                        },
                    }
                }
            }
        ),
        encoding="utf-8",
    )
    fixture.write_text(
        "\n".join(json.dumps(row) for row in rows) + "\n", encoding="utf-8"
    )
    source_hash = hashlib.sha256(fixture.read_bytes()).hexdigest()
    command = [
        sys.executable,
        str(source.parent.parent / "run.py"),
        str(args.binary.resolve()),
        "--config",
        str(config),
        "--profile",
        args.profile,
        "--model",
        args.model,
        "--thinking-display",
        "summarized",
        "--effort",
        "high",
        "--check-context",
        "--history-fixture",
        str(fixture),
        "--artifacts",
        str(args.artifacts.resolve()),
    ]
    if args.upstream_channel is not None:
        command.extend(["--upstream-channel", str(args.upstream_channel)])
    if args.drop_global_cache_scope:
        command.append("--drop-global-cache-scope")
    with subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
    ) as process:
        first_line = process.stdout.readline()
        startup = json.loads(first_line)
        local_config = json.loads(
            (Path(startup["workspace"]) / "config/sudocode.json").read_text()
        )
        local_url = local_config["auth_modes"]["proxy"]["audit"]["baseUrl"]
        request = Request(
            local_url + "/messages",
            data=json.dumps({"diagnostic_credential": sentinel}).encode(),
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        rejected = False
        try:
            with urlopen(request, timeout=10) as response:
                response.read()
        except HTTPError as error:
            rejected = error.code == 400 and (
                b"configured API key found in request body" in error.read()
            )
        remaining, _ = process.communicate()
        output = first_line + remaining
        print(output, end="", flush=True)
        assert rejected, "credential-bearing body was not rejected locally"
        assert process.returncode == 0, "funded Read/resume workflow failed"
    records = [json.loads(line) for line in output.splitlines() if line.startswith("{")]
    assert sum(record.get("credential_body_rejected", False) for record in records) == 1
    assert startup["history_credential_redactions"] == 1
    assert hashlib.sha256(fixture.read_bytes()).hexdigest() == source_hash
    destination = Path(
        next(record["artifacts"] for record in records if "artifacts" in record)
    )
    requests = list(destination.glob("request-*-client.json"))
    assert len(requests) == 3
    for request in requests + list(destination.glob("request-*-live.json")):
        body = request.read_bytes()
        assert sentinel.encode() not in body
        assert profile["apiKey"].encode() not in body
        assert b"[REDACTED_PROFILE_KEY]" in body
        assert b"The earlier task completed." in body
    print(
        json.dumps(
            {
                "history_redaction_live_pass": True,
                "credential_body_rejected_locally": True,
                "inactive_configured_key_absent": True,
                "source_fixture_unchanged": True,
                "requests": len(requests),
            }
        ),
        flush=True,
    )
finally:
    for path in (config, fixture):
        assert path.resolve().parent == directory
        path.unlink(missing_ok=True)
    directory.rmdir()
