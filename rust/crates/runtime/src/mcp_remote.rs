//! Helpers shared by the remote MCP transports (legacy SSE, Streamable HTTP).
//!
//! `resolve_headers` merges a server's static `headers` with the dynamic
//! output of its optional `headersHelper` script. These helpers are
//! transport-agnostic, so both [`crate::mcp_sse`] and [`crate::mcp_http`]
//! reuse them through this module rather than each keeping a private copy.

use std::collections::BTreeMap;
use std::io;
use std::time::Duration;

use tokio::time::timeout;

use crate::mcp_client::McpRemoteTransport;

/// Best-effort cap on a `headersHelper` invocation. A helper that cannot
/// produce headers within this window is treated as failure (static headers
/// still apply).
const HEADERS_HELPER_TIMEOUT: Duration = Duration::from_secs(10);

/// Merge static `headers` with dynamic `headersHelper` output (dynamic wins).
/// Helper failures (missing script, non-zero exit, malformed JSON, timeout)
/// are absorbed — the caller proceeds with static headers alone, matching the
/// best-effort contract of the helper. No trust gating is applied: this is on
/// par with the stdio transport, which spawns `command` from the same config
/// source without a trust check.
pub(crate) async fn resolve_headers(
    transport: &McpRemoteTransport,
    server_name: &str,
) -> BTreeMap<String, String> {
    let mut headers = transport.headers.clone();
    if let Some(helper) = transport.headers_helper.as_deref() {
        if let Ok(dynamic) = run_headers_helper(helper, server_name, &transport.url).await {
            for (key, value) in dynamic {
                headers.insert(key, value);
            }
        }
    }
    headers
}

/// Execute the `headersHelper` script and parse its stdout as a JSON object of
/// string→string. Injects `MCP_SERVER_NAME` / `MCP_SERVER_URL` so a single
/// helper can serve multiple servers (git credential-helper style).
pub(crate) async fn run_headers_helper(
    helper: &str,
    server_name: &str,
    server_url: &str,
) -> io::Result<BTreeMap<String, String>> {
    let output = timeout(HEADERS_HELPER_TIMEOUT, async {
        tokio::process::Command::new(helper)
            .env("MCP_SERVER_NAME", server_name)
            .env("MCP_SERVER_URL", server_url)
            .output()
            .await
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "headersHelper timed out"))??;

    if !output.status.success() {
        return Err(io::Error::other(format!(
            "headersHelper exited with status {}",
            output.status
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let headers: BTreeMap<String, String> =
        serde_json::from_str(stdout.trim()).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("headersHelper stdout is not a JSON object of string→string: {error}"),
            )
        })?;
    Ok(headers)
}
