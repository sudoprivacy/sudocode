#!/usr/bin/env python3
"""Explicit local/manual model acceptance; shared commands, no automatic spending."""

import argparse
import datetime
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parents[2]


def cli(target, case=None, *, exact=False, ignored=False):
    command = ["cargo", "test", "--locked", "-p", "rusty-sudocode-cli", "--test", target]
    if case:
        command.append(case)
    command += ["--", "--test-threads=1", "--nocapture"]
    if exact:
        command.append("--exact")
    if ignored:
        command.append("--ignored")
    return {"command": command}


# This registry owns local and Actions command lists. Existing native fixture
# acceptance remains independent; listing a paid suite never runs its commands.
SUITES = {
    "smoke": [cli("pty_headless", "interactive_file_reads_updated_contents_after_resume", exact=True),
              cli("pty_headless", "print_completes_tool_roundtrip_in_each_output_format", exact=True)],
    "acp": [cli("acp_live_smoke")],
    "native-acp": [cli("acp_live_smoke", "live_subagent_smoke_stdio", exact=True)],
    "mailbox": [{"command": ["cargo", "test", "--locked", "-p", "engine-acp", "--features", "mailbox",
                              "--test", "session_mailbox_live", "--", "--ignored", "--nocapture"]}],
    "offline-nexus": [{"command": ["bash", "e2e/nexus-a2a/run-live-offline.sh"], "cwd": "."}],
    "pty": [
        cli("pty_step_budget", "live_step_budget_preserves_results_across_turns"),
        cli("pty_edit_uniqueness", "live_model_recovers_with_unique_context"),
        cli("pty_headless"),
        {**cli("pty_azure_inline_reasoning", "azure_live_file_roundtrip_has_a_final_answer"),
         "azure": True},
        cli("pty_model_flag_priority", "configured_model_reads_updated_file_after_resume"),
        cli("pty_offline_delivery", "persisted_message_is_consumed_when_the_receiver_first_starts", exact=True),
        cli("pty_a2a_overlay", "queued_and_offline_messages_survive_receiver_restart"),
        cli("pty_a2a_overlay", "pending_batch_and_two_runtimes_share_one_durable_reader"),
        cli("pty_shared_session", "live_independent_runtimes_preserve_tools_and_outputs_in_one_session",
            exact=True, ignored=True),
        cli("pty_agent_permission_bubble"),
        *[cli(target) for target in ["pty_bash_process_tree", "pty_input_batch", "pty_hook_progress", "pty_arrow_keys"]],
        cli("pty_background_tasks", "inspecting_running_task_does_not_acknowledge_its_future_result"),
        cli("pty_background_tasks", "exiting_during_a_foreground_tool_reaps_both_process_groups"),
        {**cli("pty_file_operations", "windows_verbatim_paths_read_the_same_file"), "platform": "windows"},
        {**cli("pty_file_operations", "edit_then_verify_on_disk"), "platform": "windows"},
        cli("pty_block_spacing"),
    ],
    "subagent": [cli(target) for target in [
        "pty_subagent_workflow", "pty_presets_e2e", "pty_custom_agents", "pty_agent_memory_scoping",
        "pty_send_message_multi_turn", "pty_verification_streak", "pty_agent_summary", "pty_agent_permission_bubble"]],
    "compat": [cli("pty_model_compat")],
}
TIME_BUDGET = {name: 1800 for name in SUITES}
TIME_BUDGET["subagent"] = 2700
TIME_BUDGET["compat"] = 900


def failure_kind(text):
    lowered = text.lower()
    for category, markers in [
        ("quota", ["insufficient_quota", "quota exceeded", "quota is not enough", "余额不足", "额度不足"]),
        ("authentication", ["invalid api key", "invalid_api_key", "unauthorized", "401 unauthorized"]),
        ("rate_limit", ["429 too many requests", "rate_limit_exceeded"]),
        ("upstream_unavailable", ["502 bad gateway", "503 service unavailable", "connection refused", "dns error"]),
        ("build_failure", ["could not compile", "failed to run custom build command"]),
        ("assertion_failure", ["test result: failed", "assertion failed", "assertion `"]),
    ]:
        if any(marker in lowered for marker in markers):
            return category
    return "unclassified_failure"


def stop_process(process):
    import psutil

    if process.poll() is not None:
        return
    try:
        owner = psutil.Process(process.pid)
        descendants = [(child, child.create_time()) for child in owner.children(recursive=True)]
    except psutil.NoSuchProcess:
        return
    if os.name == "nt":
        subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"], capture_output=True)
    else:
        # A real PTY and its tool processes can have separate process groups.
        # Retain descendant identity before terminating the supervisor.
        for child, created in reversed(descendants):
            try:
                if child.create_time() == created:
                    child.terminate()
            except psutil.NoSuchProcess:
                pass
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        if os.name != "nt":
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        process.wait()
    _, alive = psutil.wait_procs([child for child, _ in descendants], timeout=5)
    identities = {child.pid: created for child, created in descendants}
    for child in alive:
        try:
            if child.create_time() == identities[child.pid] and child.status() != psutil.STATUS_ZOMBIE:
                child.kill()
        except psutil.NoSuchProcess:
            pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--suite", choices=sorted(SUITES), default="smoke")
    parser.add_argument("--execute", action="store_true", help="Authorize this invocation's paid model calls")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--tests", default="", help="Optional comma-separated test targets within the selected suite")
    parser.add_argument("--command-index", type=int, help=argparse.SUPPRESS)
    args = parser.parse_args()
    plan = [item for item in SUITES[args.suite]
            if item.get("platform", "windows" if os.name == "nt" else "unix") == ("windows" if os.name == "nt" else "unix")]
    if args.tests:
        requested = {target.strip() for target in args.tests.split(",") if target.strip()}
        available = {item["command"][item["command"].index("--test") + 1]
                     for item in plan if "--test" in item["command"]}
        if not requested or requested - available:
            parser.error("Requested test targets must belong to the selected suite")
        plan = [item for item in plan if "--test" in item["command"]
                and item["command"][item["command"].index("--test") + 1] in requested]
    if not args.execute:
        print(json.dumps({"suite": args.suite, "paid_calls_started": False,
                          "seconds_budget": TIME_BUDGET[args.suite], "commands": plan}, indent=2))
        return 0
    environment = {**os.environ, "SCODE_TEST_BACKEND": "live"}
    required = {"acp": "PROXY_AUTH_TOKEN", "native-acp": "PROXY_AUTH_TOKEN",
                "mailbox": "ANTHROPIC_API_KEY"}.get(args.suite)
    if required and not environment.get(required, "").strip():
        parser.error(f"{required} is required; missing credentials cannot count as paid acceptance")
    if any(item.get("azure") for item in plan) and not environment.get("SCODE_AZURE_MODEL"):
        parser.error("The full PTY suite requires SCODE_AZURE_MODEL and its configured auth profile; use smoke for the local default provider")
    if args.command_index is not None:
        # Reuse the native executor's compiler priority/MSVC handling. A
        # separate process lets the parent bound and clean up the whole tree.
        from local_acceptance import run_command
        if not args.output or not args.output.is_dir() or not 0 <= args.command_index < len(plan):
            parser.error("Invalid internal command execution")
        item = plan[args.command_index]
        if item.get("azure"):
            environment.update(SCODE_LIVE_AUTH_PROFILE="sudorouter-azure",
                               SCODE_LIVE_MODEL=environment["SCODE_AZURE_MODEL"])
        if environment.get("GITHUB_ACTIONS") == "true":
            environment["CARGO_BUILD_JOBS"] = str(os.cpu_count() or 1)
        result = run_command(item["command"], ROOT / item.get("cwd", "rust"),
                             args.output / f"{args.command_index:03}.log", env=environment)
        (args.output / f"{args.command_index:03}.execution.json").write_text(json.dumps(result, indent=2) + "\n")
        return result["exit_code"]
    output = args.output or Path(tempfile.mkdtemp(prefix="scode-model-acceptance-")) / args.suite
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    started = time.monotonic()
    report = {"suite": args.suite, "head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
              "working_tree_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT)),
              "product_binary_override": environment.get("SCODE_TEST_BIN"),
              "scope": "Process test results; provider traces are separate and this is not a signed source receipt",
              "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
              "seconds_budget": TIME_BUDGET[args.suite], "attempts_per_command": 1,
              "platform": os.name, "commands": [], "outcome": "running"}
    (output / "result.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    code = 0
    try:
        for index, item in enumerate(plan):
            remaining = TIME_BUDGET[args.suite] - (time.monotonic() - started)
            if remaining <= 0:
                raise subprocess.TimeoutExpired(item["command"], TIME_BUDGET[args.suite])
            log = output / f"{index:03}.log"
            print(f"{args.suite}: {' '.join(item['command'])}; log={log}", flush=True)
            with (output / f"{index:03}.supervisor.log").open("wb") as stream:
                process = subprocess.Popen([sys.executable, __file__, "--suite", args.suite, "--execute",
                    "--output", str(output), "--tests", args.tests, "--command-index", str(index)], cwd=ROOT,
                    env=environment, stdout=stream, stderr=subprocess.STDOUT,
                    start_new_session=os.name != "nt")
                try:
                    code = process.wait(timeout=remaining)
                except BaseException:
                    stop_process(process)
                    raise
            report["commands"].append({"command": item["command"], "exit_code": code, "log": log.name})
            if code == 0 and item["command"][0] == "cargo":
                text = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", log.read_text(errors="replace"))
                if not re.search(r"test result: ok\. [1-9][0-9]* passed", text):
                    code = 1
                    report.update(outcome="failed", failure_kind="empty_test_selection")
                    break
            if code:
                report.update(outcome="failed", failure_kind=failure_kind(log.read_text(errors="replace"))
                              if log.exists() else "runner_failure")
                break
        else:
            report["outcome"] = "passed"
    except subprocess.TimeoutExpired:
        code = 124
        report.update(outcome="failed", failure_kind="timeout")
    except KeyboardInterrupt:
        code = 130
        report.update(outcome="interrupted", failure_kind="interrupted")
    except Exception:
        code = 1
        report.update(outcome="failed", failure_kind="runner_failure")
        raise
    finally:
        report.update(elapsed_seconds=round(time.monotonic() - started, 3), exit_code=code)
        (output / "result.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        print(f"Model acceptance {report['outcome']}; evidence={output / 'result.json'}", flush=True)
    return code


if __name__ == "__main__":
    raise SystemExit(main())
