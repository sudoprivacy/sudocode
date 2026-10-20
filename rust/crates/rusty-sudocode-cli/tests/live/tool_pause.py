"""Real model -> main and child shells -> human pause -> fresh resumed turn.

Run with SCODE_TOOL_PAUSE_LIVE=1, SCODE_TEST_BIN, OPENAI_API_KEY,
OPENAI_BASE_URL and SCODE_LIVE_MODEL. Credentials stay in the environment.
Pass an empty, owned evidence directory with --evidence.
"""

import argparse
import asyncio
import json
import os
import time
from pathlib import Path


class ACP:
    def __init__(self, process):
        self.process = process
        self.next_id = 0
        self.pending = {}
        self.updates = []
        self.permission_count = 0

    async def read(self):
        while line := await self.process.stdout.readline():
            message = json.loads(line)
            if "id" in message and "method" not in message:
                future = self.pending.pop(message["id"], None)
                if future and not future.done():
                    future.set_result(message)
            else:
                self.updates.append(message)
                if message.get("method") == "session/request_permission":
                    request = message["params"]
                    command = (
                        request.get("toolCall", {}).get("rawInput", {}).get("command")
                    )
                    assert command in ("sh child-loop.sh", "sh main-loop.sh"), (
                        "Live fixture approval is limited to its two prepared shell commands"
                    )
                    choice = next(
                        option
                        for option in request["options"]
                        if option["kind"] == "allow_once"
                    )
                    self.process.stdin.write(
                        (
                            json.dumps(
                                {
                                    "jsonrpc": "2.0",
                                    "id": message["id"],
                                    "result": {
                                        "outcome": {
                                            "outcome": "selected",
                                            "optionId": choice["optionId"],
                                        }
                                    },
                                }
                            )
                            + "\n"
                        ).encode()
                    )
                    await self.process.stdin.drain()
                    self.permission_count += 1
                update = message.get("params", {}).get("update", {})
                if update.get("sessionUpdate") in ("tool_call", "tool_call_update"):
                    print(
                        json.dumps(
                            {
                                "tool": update.get("title"),
                                "status": update.get("status"),
                                "child": bool(
                                    update.get("_meta", {})
                                    .get("sudocode", {})
                                    .get("subagent")
                                ),
                            }
                        ),
                        flush=True,
                    )
        for future in self.pending.values():
            if not future.done():
                future.set_exception(RuntimeError("Agent exited before its response"))

    async def request(self, method, params, timeout=180, expect_error=False):
        self.next_id += 1
        request_id = self.next_id
        future = asyncio.get_running_loop().create_future()
        self.pending[request_id] = future
        self.process.stdin.write(
            (
                json.dumps(
                    {
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "method": method,
                        "params": params,
                    }
                )
                + "\n"
            ).encode()
        )
        await self.process.stdin.drain()
        response = await asyncio.wait_for(future, timeout)
        assert ("error" in response) == expect_error, response
        return response.get("result", response.get("error"))

    async def prompt(self, session_id, text, **kwargs):
        return await self.request(
            "session/prompt",
            {"sessionId": session_id, "prompt": [{"type": "text", "text": text}]},
            **kwargs,
        )


async def run(root):
    assert os.environ.get("SCODE_TOOL_PAUSE_LIVE") == "1", "Real API opt-in required"
    for name in (
        "SCODE_TEST_BIN",
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "SCODE_LIVE_MODEL",
    ):
        assert os.environ.get(name), f"Missing {name}"
    assert not root.exists() or not any(root.iterdir()), (
        "Use an empty evidence directory"
    )
    root.mkdir(parents=True, exist_ok=True)
    workspace, config = root / "workspace", root / "config"
    workspace.mkdir()
    config.mkdir()
    for actor in ("child", "main"):
        (workspace / f"{actor}-loop.sh").write_text(
            f"printf {actor} > {actor}-start.txt\n"
            "i=0\n"
            "while [ $i -lt 1200 ] && [ ! -f release.txt ]; do\n"
            f"  printf x >> {actor}-ticks.txt\n"
            "  i=$((i+1))\n  sleep 0.1\ndone\n"
        )
    repository = Path(__file__).resolve().parents[4]
    sample = json.loads(
        (repository / "crates/runtime/src/sudocode.sample.json").read_text()
    )
    sample["models"]["pause-live"] = {
        "alias": "pause-live",
        "name": "Tool pause acceptance",
        "input": ["text"],
        "providers": {
            "api-key": {
                "provider": "openai",
                "model": os.environ["SCODE_LIVE_MODEL"],
                "api": "openai-completions",
            }
        },
    }
    sample["auth_modes"]["api-key"]["openai"] = {
        "baseUrl": os.environ["OPENAI_BASE_URL"],
        "apiKeyEnv": "OPENAI_API_KEY",
    }
    (config / "sudocode.json").write_text(json.dumps(sample))
    env = os.environ.copy()
    for name in list(env):
        if name.startswith(("SUDO_CODE_", "CLAUDE_", "ANTHROPIC_")):
            env.pop(name)
    env.update(
        SUDO_CODE_CONFIG_HOME=str(config),
        CLAUDE_CONFIG_DIR=str(config),
        NO_PROXY="127.0.0.1,localhost,::1",
    )
    with (root / "agent.private.stderr").open("wb") as stderr:
        process = await asyncio.create_subprocess_exec(
            os.environ["SCODE_TEST_BIN"],
            "--auth",
            "api-key",
            "--model",
            "pause-live",
            "--dangerously-skip-permissions",
            "acp",
            cwd=workspace,
            env=env,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=stderr,
        )
        acp = ACP(process)
        reader = asyncio.create_task(acp.read())
        prompt = None
        try:
            initialized = await acp.request(
                "initialize",
                {
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "_meta": {"sudocode": {"subagentEvents": {"version": 1}}}
                    },
                },
            )
            assert initialized["_meta"]["sudocode"]["subagentEvents"]["version"] == 1
            assert initialized["_meta"]["sudocode"]["toolPause"]["version"] == 1
            session = (
                await acp.request(
                    "session/new", {"cwd": str(workspace), "mcpServers": []}
                )
            )["sessionId"]
            prompt = asyncio.create_task(
                acp.prompt(
                    session,
                    "Acceptance test in a disposable workspace. main-loop.sh and child-loop.sh are prepared fixtures, bounded to 120 seconds and 1200 bytes per shell; the harness also releases them during cleanup. First spawn exactly one general-purpose background Agent. Its only task is to run Bash with command 'sh child-loop.sh', timeout 180000 and run_in_background false, then finish when it returns. Then in the main agent run Bash with command 'sh main-loop.sh', timeout 180000 and run_in_background false. The harness pauses both agents after their shells start. Do not edit the scripts, replace commands, append punctuation to commands, or launch separate background shell jobs. All paths are relative to the supplied workspace.",
                )
            )
            deadline = time.monotonic() + 150
            while not all(
                (workspace / name).exists()
                for name in ("child-ticks.txt", "main-ticks.txt")
            ):
                if reader.done():
                    await reader
                if prompt.done():
                    await prompt
                    raise AssertionError(
                        "The model ended before both real shells started"
                    )
                assert time.monotonic() < deadline, (
                    "Both main and child shells must actually start"
                )
                await asyncio.sleep(0.1)
            started = time.monotonic()
            paused = await acp.request(
                "_sudocode/session/pause_tools", {"sessionId": session}, timeout=45
            )
            pause_seconds = time.monotonic() - started
            assert paused["isQuiescent"] is True
            sizes = {
                name: (workspace / name).stat().st_size
                for name in ("child-ticks.txt", "main-ticks.txt")
            }
            await asyncio.sleep(2)
            assert sizes == {
                name: (workspace / name).stat().st_size for name in sizes
            }, "AI wrote after human control was acknowledged"
            await prompt
            manifests = [
                json.loads(path.read_text())
                for path in (workspace / ".sudocode-agents").glob("*.json")
            ]
            assert manifests and all(
                value["status"] == "cancelled" for value in manifests
            ), manifests
            await acp.prompt(
                session, "! printf forbidden > forbidden.txt", expect_error=True
            )
            assert not (workspace / "forbidden.txt").exists()
            (workspace / "human-note.txt").write_text("HUMAN_EDIT_AFTER_PAUSE")
            await acp.request(
                "_sudocode/session/resume_tools",
                {"sessionId": session, "pauseEpoch": paused["pauseEpoch"] + 1},
                expect_error=True,
            )
            await acp.request(
                "_sudocode/session/resume_tools",
                {"sessionId": session, "pauseEpoch": paused["pauseEpoch"]},
            )
            await acp.prompt(
                session,
                "Control is returned. Do not restart any cancelled work. Use a tool to read human-note.txt, then copy its exact contents to resumed-note.txt using a tool. Finish after that.",
            )
            assert (
                workspace / "resumed-note.txt"
            ).read_text() == "HUMAN_EDIT_AFTER_PAUSE"
            assert sizes == {
                name: (workspace / name).stat().st_size for name in sizes
            }, "Old work revived after return"
            receipt = {
                "sessionId": session,
                "pauseEpoch": paused["pauseEpoch"],
                "pauseSeconds": pause_seconds,
                "stoppedShellBytes": sizes,
                "childStatuses": [value["status"] for value in manifests],
                "resumedObservation": "HUMAN_EDIT_AFTER_PAUSE",
                "updates": len(acp.updates),
                "fixturePermissionApprovals": acp.permission_count,
            }
            (root / "evidence.json").write_text(json.dumps(receipt, indent=2))
            print(json.dumps(receipt))
        finally:
            (workspace / "release.txt").write_text("owned fixture cleanup")
            (root / "updates.private.json").write_text(
                json.dumps(acp.updates, indent=2)
            )
            if process.returncode is None:
                process.terminate()
                await asyncio.wait_for(process.wait(), 15)
            if prompt and not prompt.done():
                prompt.cancel()
            reader.cancel()
            await asyncio.gather(
                reader, *([prompt] if prompt else []), return_exceptions=True
            )


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence", type=Path, required=True)
    arguments = parser.parse_args()
    asyncio.run(run(arguments.evidence.resolve()))
