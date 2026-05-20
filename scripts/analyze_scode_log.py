#!/usr/bin/env python3
"""
Scode Log Analyzer

Analyzes scode.log files and generates formatted reports organized by:
- Session (chronological order)
- Each request within a session (with unique request_id)
- Request details: status, body, response, tokens
- Session summary with total token consumption

Usage:
    python analyze_scode_log.py [log_file_path]

Default log path: ~/.nexus/logs/scode.log
"""

import json
import sys
from collections import defaultdict
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any


@dataclass
class RequestInfo:
    """Information about a single request."""

    request_id: str
    started_at: datetime | None = None
    succeeded_at: datetime | None = None
    failed_at: datetime | None = None
    status: int | None = None
    duration_ms: int | None = None
    attempt: int = 1
    error: str | None = None
    retryable: bool = False
    provider_request_id: str | None = None

    # Request details
    url: str | None = None
    method: str | None = None
    path: str | None = None
    headers: dict = field(default_factory=dict)
    body: dict | None = None

    # Response details
    response_headers: dict = field(default_factory=dict)
    response_body: dict | None = None
    # Provider request ID from response headers (e.g., x-oneapi-request-id, request-id)
    provider_response_request_id: str | None = None
    input_tokens: int = 0
    output_tokens: int = 0
    cache_creation_input_tokens: int = 0
    cache_read_input_tokens: int = 0


@dataclass
class SessionInfo:
    """Information about a session."""

    session_id: str
    started_at: datetime | None = None
    ended_at: datetime | None = None
    version: str | None = None
    cwd: str | None = None
    mode: str | None = None
    model: str | None = None
    total_turns: int = 0
    duration_ms: int = 0

    # Requests indexed by request_id
    requests: dict[str, RequestInfo] = field(default_factory=dict)

    # Token totals
    total_input_tokens: int = 0
    total_output_tokens: int = 0
    total_cache_creation_tokens: int = 0
    total_cache_read_tokens: int = 0


def parse_timestamp(ts_str: str) -> datetime | None:
    """Parse ISO 8601 timestamp string."""
    if not ts_str:
        return None
    try:
        # Handle format: 2026-05-20T10:00:00.000+08:00
        dt = datetime.fromisoformat(ts_str)
        # Convert to UTC and make naive for comparison
        if dt.tzinfo is not None:
            dt = dt.replace(tzinfo=None)
        return dt
    except ValueError:
        return None


def parse_log_entry(line: str) -> dict[str, Any] | None:
    """Parse a single log line as JSON."""
    line = line.strip()
    if not line:
        return None
    try:
        return json.loads(line)
    except json.JSONDecodeError:
        return None


def analyze_log(log_path: Path) -> dict[str, SessionInfo]:
    """Analyze log file and return sessions with their requests."""

    sessions: dict[str, SessionInfo] = {}

    with open(log_path, encoding="utf-8") as f:
        for line in f:
            entry = parse_log_entry(line)
            if not entry:
                continue

            session_id = entry.get("session_id", "")
            event = entry.get("event", "")
            attrs = entry.get("attributes", {})
            timestamp = parse_timestamp(entry.get("timestamp", ""))

            if not session_id:
                continue

            # Get or create session
            if session_id not in sessions:
                sessions[session_id] = SessionInfo(session_id=session_id)

            session = sessions[session_id]

            if event == "session_started":
                session.started_at = timestamp
                session.version = attrs.get("version")
                session.cwd = attrs.get("cwd")
                session.mode = attrs.get("mode")
                session.model = attrs.get("model")

            elif event == "session_ended":
                session.ended_at = timestamp
                session.total_turns = attrs.get("total_turns", 0)
                session.total_input_tokens = attrs.get("total_input_tokens", 0)
                session.total_output_tokens = attrs.get("total_output_tokens", 0)
                session.duration_ms = attrs.get("duration_ms", 0)

            elif event == "request_started":
                request_id = attrs.get("request_id", "")
                if not request_id:
                    # Generate a synthetic request_id for old log format
                    request_id = f"legacy_{session_id}_{timestamp.isoformat() if timestamp else 'unknown'}"

                if request_id not in session.requests:
                    session.requests[request_id] = RequestInfo(request_id=request_id)

                req = session.requests[request_id]
                req.started_at = timestamp
                req.attempt = attrs.get("attempt", 1)
                req.method = attrs.get("method")
                req.path = attrs.get("path")

            elif event == "request_debug":
                request_id = attrs.get("request_id", "")
                if not request_id:
                    # Skip debug events without request_id (old format)
                    continue

                if request_id not in session.requests:
                    session.requests[request_id] = RequestInfo(request_id=request_id)

                req = session.requests[request_id]
                req.url = attrs.get("url")
                req.method = attrs.get("method")
                req.headers = attrs.get("headers", {})
                req.body = attrs.get("body")

            elif event == "request_succeeded":
                request_id = attrs.get("request_id", "")
                if not request_id:
                    # Generate a synthetic request_id for old log format
                    request_id = f"legacy_{session_id}_{timestamp.isoformat() if timestamp else 'unknown'}"

                if request_id not in session.requests:
                    session.requests[request_id] = RequestInfo(request_id=request_id)

                req = session.requests[request_id]
                req.succeeded_at = timestamp
                req.status = attrs.get("status")
                req.duration_ms = attrs.get("duration_ms")
                req.attempt = attrs.get("attempt", 1)
                req.provider_request_id = attrs.get("provider_request_id")

            elif event == "request_failed":
                request_id = attrs.get("request_id", "")
                if not request_id:
                    # Generate a synthetic request_id for old log format
                    request_id = f"legacy_{session_id}_{timestamp.isoformat() if timestamp else 'unknown'}"

                if request_id not in session.requests:
                    session.requests[request_id] = RequestInfo(request_id=request_id)

                req = session.requests[request_id]
                req.failed_at = timestamp
                req.error = attrs.get("error")
                req.retryable = attrs.get("retryable", False)
                req.duration_ms = attrs.get("duration_ms")
                req.attempt = attrs.get("attempt", 1)

            elif event == "response_usage":
                request_id = attrs.get("request_id", "")
                input_tokens = attrs.get("input_tokens", 0)
                output_tokens = attrs.get("output_tokens", 0)
                cache_creation_input_tokens = attrs.get("cache_creation_input_tokens", 0)
                cache_read_input_tokens = attrs.get("cache_read_input_tokens", 0)

                # If request_id is a specific request, update that request
                if request_id and request_id in session.requests:
                    req = session.requests[request_id]
                    req.input_tokens = input_tokens
                    req.output_tokens = output_tokens
                    req.cache_creation_input_tokens = cache_creation_input_tokens
                    req.cache_read_input_tokens = cache_read_input_tokens

                # Always update session totals
                session.total_input_tokens += input_tokens
                session.total_output_tokens += output_tokens
                session.total_cache_creation_tokens += cache_creation_input_tokens
                session.total_cache_read_tokens += cache_read_input_tokens

            elif event == "response_debug":
                request_id = attrs.get("request_id", "")
                if not request_id:
                    continue

                if request_id not in session.requests:
                    session.requests[request_id] = RequestInfo(request_id=request_id)

                req = session.requests[request_id]
                req.status = attrs.get("status")
                req.response_headers = attrs.get("headers", {})
                req.response_body = attrs.get("body")

                # Extract provider request ID from response headers
                # Common header names: x-oneapi-request-id, x-request-id, request-id
                headers = req.response_headers
                for header_name in [
                    "x-oneapi-request-id",
                    "x-request-id",
                    "request-id",
                    "cf-ray",  # Cloudflare
                ]:
                    if header_name in headers:
                        req.provider_response_request_id = headers[header_name]
                        break

    return sessions


def format_datetime(dt: datetime | None) -> str:
    """Format datetime for display."""
    if not dt:
        return "N/A"
    return dt.strftime("%Y-%m-%d %H:%M:%S")


def format_duration(ms: int | None) -> str:
    """Format duration in milliseconds to human readable."""
    if ms is None:
        return "N/A"
    if ms < 1000:
        return f"{ms}ms"
    seconds = ms / 1000
    if seconds < 60:
        return f"{seconds:.2f}s"
    minutes = seconds / 60
    return f"{minutes:.2f}m"


def truncate_text(text: str, max_len: int = 200) -> str:
    """Truncate text with ellipsis."""
    if len(text) <= max_len:
        return text
    return text[:max_len] + "..."


def format_body(body: dict | None, indent: int = 4) -> str:
    """Format request body for display."""
    if not body:
        return "N/A"

    # Create a summary version
    result = []

    # Model
    if "model" in body:
        result.append(f"Model: {body['model']}")

    # Max tokens
    if "max_tokens" in body:
        result.append(f"Max Tokens: {body['max_tokens']}")

    # Messages count
    if "messages" in body:
        messages = body["messages"]
        result.append(f"Messages: {len(messages)} message(s)")

        # Separate messages by role for better readability
        user_messages = []
        assistant_messages = []
        system_messages = []
        other_messages = []

        for msg in messages:
            role = msg.get("role", "unknown")
            if role == "user":
                user_messages.append(msg)
            elif role == "assistant":
                assistant_messages.append(msg)
            elif role == "system":
                system_messages.append(msg)
            else:
                other_messages.append(msg)

        # Show user messages prominently
        if user_messages:
            result.append("")
            result.append("  USER REQUESTS:")
            for i, msg in enumerate(user_messages, 1):
                content = msg.get("content", "")
                content_preview = format_message_content(content, 200)
                result.append(f"    [{i}] {content_preview}")

        # Show assistant messages
        if assistant_messages:
            result.append("")
            result.append(f"  ASSISTANT RESPONSES: {len(assistant_messages)} message(s)")
            for i, msg in enumerate(assistant_messages[:3], 1):  # Show first 3
                content = msg.get("content", "")
                content_preview = format_message_content(content, 150)
                result.append(f"    [{i}] {content_preview}")
            if len(assistant_messages) > 3:
                result.append(f"    ... and {len(assistant_messages) - 3} more")

        # Show system messages
        if system_messages:
            result.append("")
            result.append(f"  SYSTEM MESSAGES: {len(system_messages)} message(s)")
            for i, msg in enumerate(system_messages[:2], 1):  # Show first 2
                content = msg.get("content", "")
                content_preview = format_message_content(content, 100)
                result.append(f"    [{i}] {content_preview}")
            if len(system_messages) > 2:
                result.append(f"    ... and {len(system_messages) - 2} more")

        # Show other messages (tool_use, tool_result, etc.)
        if other_messages:
            result.append("")
            result.append(f"  OTHER MESSAGES: {len(other_messages)} message(s)")
            for i, msg in enumerate(other_messages[:3], 1):
                role = msg.get("role", "unknown")
                content = msg.get("content", "")
                content_preview = format_message_content(content, 100)
                result.append(f"    [{i}] {role}: {content_preview}")
            if len(other_messages) > 3:
                result.append(f"    ... and {len(other_messages) - 3} more")

    # Tools
    if "tools" in body:
        tools = body["tools"]
        result.append(f"Tools: {len(tools)} tool(s) available")

    # System prompt info
    if "system" in body:
        system = body["system"]
        if isinstance(system, str):
            result.append(f"System: {truncate_text(system[:100], 100)}")
        elif isinstance(system, list):
            result.append(f"System: {len(system)} part(s)")

    return "\n".join(" " * indent + line for line in result)


def format_message_content(content, max_len: int = 100) -> str:
    """Format message content for display."""
    if isinstance(content, str):
        # Clean up the content
        cleaned = content.replace("\n", " ").strip()
        # Remove system-reminder tags for cleaner display
        if "<system-reminder>" in cleaned:
            # Just show that it contains system-reminder
            return truncate_text(cleaned[:max_len], max_len)
        return truncate_text(cleaned, max_len)
    elif isinstance(content, list):
        # Handle multi-part content
        parts = []
        for part in content[:3]:
            if isinstance(part, dict):
                part_type = part.get("type", "unknown")
                if part_type == "text":
                    text = part.get("text", "")
                    parts.append(truncate_text(text[:50], 50))
                elif part_type == "image":
                    parts.append("[image]")
                elif part_type == "tool_use":
                    parts.append(f"[tool:{part.get('name', 'unknown')}]")
                else:
                    parts.append(f"[{part_type}]")
            else:
                parts.append(str(part)[:50])
        return ", ".join(parts)
    else:
        return truncate_text(str(content), max_len)


def format_response_body(body: dict | None, indent: int = 4) -> str:
    """Format response body for display."""
    if not body:
        return "N/A"

    result = []

    # Message ID
    if "id" in body:
        result.append(f"Message ID: {body['id']}")

    # Model
    if "model" in body:
        result.append(f"Model: {body['model']}")

    # Stop reason
    if "stop_reason" in body:
        result.append(f"Stop Reason: {body['stop_reason']}")

    # Content
    if "content" in body:
        content = body["content"]
        if isinstance(content, list):
            result.append(f"Content: {len(content)} block(s)")
            for i, block in enumerate(content[:5]):  # Show first 5 blocks
                block_type = block.get("type", "unknown")
                if block_type == "text":
                    text = block.get("text", "")
                    result.append(f"  [{i+1}] text: {truncate_text(text[:100], 100)}")
                elif block_type == "tool_use":
                    result.append(f"  [{i+1}] tool_use: {block.get('name', 'unknown')}")
                else:
                    result.append(f"  [{i+1}] {block_type}")
            if len(content) > 5:
                result.append(f"  ... and {len(content) - 5} more block(s)")
        else:
            result.append(f"Content: {truncate_text(str(content)[:100], 100)}")

    # Usage
    if "usage" in body:
        usage = body["usage"]
        result.append("Usage:")
        result.append(f"  Input Tokens: {usage.get('input_tokens', 'N/A')}")
        result.append(f"  Output Tokens: {usage.get('output_tokens', 'N/A')}")
        if "cache_creation_input_tokens" in usage:
            result.append(f"  Cache Creation: {usage['cache_creation_input_tokens']}")
        if "cache_read_input_tokens" in usage:
            result.append(f"  Cache Read: {usage['cache_read_input_tokens']}")

    return "\n".join(" " * indent + line for line in result)


def print_report(sessions: dict[str, SessionInfo]) -> None:
    """Print formatted report."""

    print("=" * 80)
    print("SCODE LOG ANALYSIS REPORT")
    print("=" * 80)
    print()

    # Sort sessions by start time
    sorted_sessions = sorted(
        sessions.values(),
        key=lambda s: s.started_at or datetime.min,
    )

    for session in sorted_sessions:
        print("-" * 80)
        print(f"SESSION: {session.session_id}")
        print("-" * 80)
        print(f"  Started:    {format_datetime(session.started_at)}")
        print(f"  Ended:      {format_datetime(session.ended_at)}")
        print(f"  Version:    {session.version or 'N/A'}")
        print(f"  Mode:       {session.mode or 'N/A'}")
        print(f"  Model:      {session.model or 'N/A'}")
        print(f"  Working Dir: {session.cwd or 'N/A'}")
        print(f"  Duration:   {format_duration(session.duration_ms)}")
        print(f"  Total Turns: {session.total_turns}")
        print()

        # Sort requests by start time
        sorted_requests = sorted(
            session.requests.values(),
            key=lambda r: r.started_at or datetime.min,
        )

        for i, req in enumerate(sorted_requests, 1):
            print(f"  {'=' * 70}")
            print(f"  REQUEST #{i}: {req.request_id}")
            print(f"  {'=' * 70}")
            print()

            # Status
            if req.status:
                status_emoji = "✅" if req.status == 200 else "❌"
                print(f"  Status: {status_emoji} {req.status}")
            elif req.error:
                print(f"  Status: ❌ FAILED")
            else:
                print(f"  Status: ⏳ In Progress")

            print(f"  Attempt:    {req.attempt}")
            print(f"  Method:     {req.method or 'N/A'}")
            print(f"  Path:       {req.path or 'N/A'}")
            print(f"  Started:    {format_datetime(req.started_at)}")

            if req.succeeded_at:
                print(f"  Succeeded:  {format_datetime(req.succeeded_at)}")
            if req.failed_at:
                print(f"  Failed:     {format_datetime(req.failed_at)}")

            print(f"  Duration:   {format_duration(req.duration_ms)}")

            # Request IDs for tracking and reconciliation
            print(f"  Client Request ID: {req.request_id}")
            if req.provider_request_id:
                print(f"  Provider Request ID (header): {req.provider_request_id}")
            if req.provider_response_request_id:
                print(f"  Provider Request ID (response): {req.provider_response_request_id}")

            if req.error:
                print(f"  Error:      {req.error}")
                print(f"  Retryable:  {'Yes' if req.retryable else 'No'}")

            print()

            # Request body
            if req.body:
                print("  Request Body:")
                print(format_body(req.body))
                print()

            # Response body
            if req.response_body:
                print("  Response Body:")
                print(format_response_body(req.response_body))
                print()

            # Token usage
            if req.input_tokens or req.output_tokens:
                print("  Token Usage:")
                print(f"    Input Tokens:              {req.input_tokens:,}")
                print(f"    Output Tokens:             {req.output_tokens:,}")
                if req.cache_creation_input_tokens:
                    print(f"    Cache Creation Tokens:     {req.cache_creation_input_tokens:,}")
                if req.cache_read_input_tokens:
                    print(f"    Cache Read Tokens:         {req.cache_read_input_tokens:,}")
                print()

        # Session summary
        print("-" * 80)
        print("  SESSION SUMMARY")
        print("-" * 80)
        print(f"  Total Requests:        {len(session.requests)}")

        # Count successful/failed
        successful = sum(1 for r in session.requests.values() if r.status == 200)
        failed = sum(1 for r in session.requests.values() if r.error)
        print(f"  Successful Requests:   {successful}")
        print(f"  Failed Requests:       {failed}")

        print()
        print("  Token Consumption:")
        print(f"    Total Input Tokens:          {session.total_input_tokens:,}")
        print(f"    Total Output Tokens:         {session.total_output_tokens:,}")
        if session.total_cache_creation_tokens:
            print(f"    Total Cache Creation Tokens: {session.total_cache_creation_tokens:,}")
        if session.total_cache_read_tokens:
            print(f"    Total Cache Read Tokens:     {session.total_cache_read_tokens:,}")

        total_tokens = (
            session.total_input_tokens
            + session.total_output_tokens
            + session.total_cache_creation_tokens
            + session.total_cache_read_tokens
        )
        print(f"    ─────────────────────────────────────")
        print(f"    Grand Total:                 {total_tokens:,}")

        print()

    # Final summary
    print("=" * 80)
    print("OVERALL SUMMARY")
    print("=" * 80)
    print(f"  Total Sessions:     {len(sessions)}")

    total_requests = sum(len(s.requests) for s in sessions.values())
    print(f"  Total Requests:     {total_requests}")

    grand_input = sum(s.total_input_tokens for s in sessions.values())
    grand_output = sum(s.total_output_tokens for s in sessions.values())
    grand_cache_creation = sum(s.total_cache_creation_tokens for s in sessions.values())
    grand_cache_read = sum(s.total_cache_read_tokens for s in sessions.values())

    print()
    print("  Total Token Consumption (All Sessions):")
    print(f"    Input Tokens:          {grand_input:,}")
    print(f"    Output Tokens:         {grand_output:,}")
    print(f"    Cache Creation Tokens: {grand_cache_creation:,}")
    print(f"    Cache Read Tokens:     {grand_cache_read:,}")
    print(f"    ─────────────────────────────────────")
    print(f"    Grand Total:           {grand_input + grand_output + grand_cache_creation + grand_cache_read:,}")
    print()


def main() -> None:
    """Main entry point."""
    if len(sys.argv) > 1:
        log_path = Path(sys.argv[1])
    else:
        # Default path
        log_path = Path.home() / ".nexus" / "logs" / "scode.log"

    if not log_path.exists():
        print(f"Error: Log file not found: {log_path}")
        sys.exit(1)

    print(f"Analyzing log file: {log_path}")
    print()

    sessions = analyze_log(log_path)

    if not sessions:
        print("No sessions found in log file.")
        sys.exit(0)

    print_report(sessions)


if __name__ == "__main__":
    main()
