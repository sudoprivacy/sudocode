//! MCP headers_helper module — executes an optional helper script and merges
//! its dynamic JSON headers with the static headers from transport config.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use crate::mcp_client::McpRemoteTransport;

/// Maximum time to wait for the headers_helper script to finish.
const HELPER_TIMEOUT: Duration = Duration::from_secs(10);

/// Environment variable injected into the helper process carrying the MCP
/// server name.
const ENV_SERVER_NAME: &str = "SUDOCODE_MCP_SERVER_NAME";

/// Environment variable injected into the helper process carrying the MCP
/// server URL.
const ENV_SERVER_URL: &str = "SUDOCODE_MCP_SERVER_URL";

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can arise while resolving MCP request headers.
#[derive(Debug)]
pub enum HeadersHelperError {
    /// The headers_helper script exited with a non-zero status or could not
    /// be spawned.
    HelperExecution {
        program: String,
        source: std::io::Error,
    },
    /// The helper's stdout could not be parsed as a JSON object whose keys and
    /// values are all strings.
    JsonParse {
        source: serde_json::Error,
        raw_output: String,
    },
    /// A key/value pair returned by the helper contained bytes that are not
    /// valid HTTP header values.
    InvalidHeaderValue {
        key: String,
        value: String,
        source: reqwest::header::InvalidHeaderValue,
    },
}

impl fmt::Display for HeadersHelperError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HelperExecution { program, source } => {
                write!(f, "headers_helper `{program}` failed: {source}")
            }
            Self::JsonParse { source, raw_output } => {
                write!(
                    f,
                    "headers_helper returned invalid JSON: {source}\noutput: {raw_output}"
                )
            }
            Self::InvalidHeaderValue { key, value, source } => {
                write!(
                    f,
                    "invalid header value for key `{key}`: {value:?}: {source}"
                )
            }
        }
    }
}

impl std::error::Error for HeadersHelperError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::HelperExecution { source, .. } => Some(source),
            Self::JsonParse { source, .. } => Some(source),
            Self::InvalidHeaderValue { source, .. } => Some(source),
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Build a complete set of HTTP headers for an MCP remote transport.
///
/// 1. Starts from the static `transport.headers` map.
/// 2. If `transport.headers_helper` is `Some`, the helper script is executed
///    with `SUDOCODE_MCP_SERVER_NAME` and `SUDOCODE_MCP_SERVER_URL` injected
///    into its environment. On Windows the helper runs via `cmd /C`; on Unix
///    via `sh -c`. The helper's stdout must be a JSON object of string
///    key/value pairs; these dynamic headers **override** any static headers
///    with the same name.
/// 3. If the helper fails for any reason, a warning is logged and the static
///    headers are returned unchanged — the MCP connection is not blocked.
/// 4. **Trust gating**: when `scope` is `Project` or `Local`, the helper is
///    only executed if `workspace_is_trusted` is `true`. Otherwise the helper
///    is silently skipped and only static headers are returned. This prevents
///    untrusted workspaces from executing arbitrary shell commands via
///    `headers_helper`.
pub async fn build_request_headers(
    server_name: &str,
    transport: &McpRemoteTransport,
    scope: crate::config::ConfigSource,
    workspace_is_trusted: bool,
) -> Result<reqwest::header::HeaderMap, HeadersHelperError> {
    // Start from static headers.
    let mut headers = static_headers(&transport.headers)?;

    // If a helper is configured, run it and merge the result.
    if let Some(ref helper) = transport.headers_helper {
        // Trust gate: Project/Local scoped helpers require workspace trust.
        if matches!(
            scope,
            crate::config::ConfigSource::Project | crate::config::ConfigSource::Local
        ) && !workspace_is_trusted
        {
            eprintln!(
                "warning: headers_helper for server `{server_name}` skipped: \
                 workspace is not trusted for {scope:?}-scoped helper execution"
            );
            return Ok(headers);
        }

        match run_helper(helper, server_name, &transport.url).await {
            Ok(dynamic) => {
                merge_dynamic_headers(&mut headers, dynamic)?;
            }
            Err(e) => {
                // Non-blocking: log and continue with static headers only.
                eprintln!(
                    "warning: headers_helper failed for server `{server_name}`: {e}; \
                     using static headers only"
                );
            }
        }
    }

    Ok(headers)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Convert the static BTreeMap into a HeaderMap.
fn static_headers(
    map: &BTreeMap<String, String>,
) -> Result<reqwest::header::HeaderMap, HeadersHelperError> {
    let mut headers = reqwest::header::HeaderMap::with_capacity(map.len());
    for (key, value) in map {
        let name = reqwest::header::HeaderName::from_bytes(key.as_bytes())
            .unwrap_or_else(|_| reqwest::header::HeaderName::from_static("x-unknown"));
        let hv = match reqwest::header::HeaderValue::from_str(value) {
            Ok(v) => v,
            Err(source) => {
                return Err(HeadersHelperError::InvalidHeaderValue {
                    key: key.clone(),
                    value: value.clone(),
                    source,
                })
            }
        };
        headers.insert(name, hv);
    }
    Ok(headers)
}

/// Execute the headers_helper script and return the parsed dynamic headers.
async fn run_helper(
    helper: &str,
    server_name: &str,
    server_url: &str,
) -> Result<BTreeMap<String, String>, HeadersHelperError> {
    let (program, args) = if cfg!(windows) {
        ("cmd", vec!["/C", helper])
    } else {
        ("sh", vec!["-c", helper])
    };

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(&args)
        .env(ENV_SERVER_NAME, server_name)
        .env(ENV_SERVER_URL, server_url);

    let output = tokio::time::timeout(HELPER_TIMEOUT, cmd.output())
        .await
        .map_err(|_| HeadersHelperError::HelperExecution {
            program: helper.to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("headers_helper timed out after {HELPER_TIMEOUT:?}"),
            ),
        })?
        .map_err(|source| HeadersHelperError::HelperExecution {
            program: program.to_string(),
            source,
        })?;

    if !output.status.success() {
        return Err(HeadersHelperError::HelperExecution {
            program: helper.to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "exited with status {}; stderr: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: BTreeMap<String, String> =
        serde_json::from_str(stdout.trim()).map_err(|source| HeadersHelperError::JsonParse {
            source,
            raw_output: stdout.trim().to_string(),
        })?;

    Ok(parsed)
}

/// Merge dynamic headers into the existing HeaderMap, overwriting duplicates.
fn merge_dynamic_headers(
    headers: &mut reqwest::header::HeaderMap,
    dynamic: BTreeMap<String, String>,
) -> Result<(), HeadersHelperError> {
    for (key, value) in dynamic {
        let name = reqwest::header::HeaderName::from_bytes(key.as_bytes())
            .unwrap_or_else(|_| reqwest::header::HeaderName::from_static("x-unknown"));
        let hv = match reqwest::header::HeaderValue::from_str(&value) {
            Ok(v) => v,
            Err(source) => {
                return Err(HeadersHelperError::InvalidHeaderValue {
                    key: key.clone(),
                    value: value.clone(),
                    source,
                })
            }
        };
        headers.insert(name, hv);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_transport(
        headers: BTreeMap<String, String>,
        headers_helper: Option<&str>,
    ) -> McpRemoteTransport {
        McpRemoteTransport {
            url: "https://example.com/mcp".to_string(),
            headers,
            headers_helper: headers_helper.map(String::from),
            auth: crate::mcp_client::McpClientAuth::None,
        }
    }

    #[test]
    fn static_headers_converted_to_header_map() {
        let map = BTreeMap::from([
            ("authorization".to_string(), "Bearer token".to_string()),
            ("x-custom".to_string(), "value".to_string()),
        ]);
        let result = static_headers(&map).unwrap();
        assert_eq!(result.get("authorization").unwrap(), "Bearer token");
        assert_eq!(result.get("x-custom").unwrap(), "value");
    }

    #[test]
    fn empty_static_headers_produce_empty_map() {
        let map = BTreeMap::new();
        let result = static_headers(&map).unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn build_request_headers_without_helper() {
        let transport = make_transport(
            BTreeMap::from([("x-test".to_string(), "static".to_string())]),
            None,
        );
        let headers = build_request_headers(
            "test-server",
            &transport,
            crate::config::ConfigSource::User,
            true,
        )
        .await
        .unwrap();
        assert_eq!(headers.get("x-test").unwrap(), "static");
    }

    #[tokio::test]
    async fn helper_failure_returns_static_headers() {
        let transport = make_transport(
            BTreeMap::from([("x-fallback".to_string(), "yes".to_string())]),
            Some("nonexistent_helper_script_12345"),
        );
        let headers = build_request_headers(
            "test-server",
            &transport,
            crate::config::ConfigSource::User,
            true,
        )
        .await
        .unwrap();
        // Helper will fail but we should still get static headers.
        assert_eq!(headers.get("x-fallback").unwrap(), "yes");
    }

    #[tokio::test]
    async fn helper_success_merges_and_overrides() {
        // Use a helper that outputs a JSON object via echo.
        let helper = if cfg!(windows) {
            "echo {\"x-dynamic\": \"from-helper\"}"
        } else {
            "echo '{\"x-dynamic\": \"from-helper\"}'"
        };
        let transport = make_transport(
            BTreeMap::from([
                ("x-static".to_string(), "keep".to_string()),
                ("x-dynamic".to_string(), "overridden".to_string()),
            ]),
            Some(helper),
        );
        let headers = build_request_headers(
            "test-server",
            &transport,
            crate::config::ConfigSource::User,
            true,
        )
        .await
        .unwrap();
        assert_eq!(headers.get("x-static").unwrap(), "keep");
        assert_eq!(headers.get("x-dynamic").unwrap(), "from-helper");
    }

    #[tokio::test]
    async fn helper_receives_env_vars() {
        let helper = if cfg!(windows) {
            // Windows: write the env vars as a JSON object.
            // We rely on cmd not interpreting single quotes.
            r#"echo {"server": "%SUDOCODE_MCP_SERVER_NAME%", "url": "%SUDOCODE_MCP_SERVER_URL%"}"#
        } else {
            // Unix: use printenv to dump our specific vars, then format as JSON.
            r#"printf '{"server":"%s","url":"%s"}' "$SUDOCODE_MCP_SERVER_NAME" "$SUDOCODE_MCP_SERVER_URL""#
        };
        let transport = make_transport(BTreeMap::new(), Some(helper));
        let headers = build_request_headers(
            "my-server",
            &transport,
            crate::config::ConfigSource::User,
            true,
        )
        .await
        .unwrap();
        assert_eq!(headers.get("server").unwrap(), "my-server");
        assert_eq!(headers.get("url").unwrap(), "https://example.com/mcp");
    }

    #[tokio::test]
    async fn project_scope_untrusted_skips_helper() {
        let helper = if cfg!(windows) {
            "echo {\"x-dynamic\": \"should-not-appear\"}"
        } else {
            "echo '{\"x-dynamic\": \"should-not-appear\"}'"
        };
        let transport = make_transport(
            BTreeMap::from([("x-static".to_string(), "kept".to_string())]),
            Some(helper),
        );
        // Project scope, NOT trusted → helper should be skipped.
        let headers = build_request_headers(
            "test-server",
            &transport,
            crate::config::ConfigSource::Project,
            false,
        )
        .await
        .unwrap();
        assert_eq!(headers.get("x-static").unwrap(), "kept");
        assert!(
            headers.get("x-dynamic").is_none(),
            "helper should have been skipped"
        );
    }

    #[tokio::test]
    async fn project_scope_trusted_executes_helper() {
        let helper = if cfg!(windows) {
            "echo {\"x-dynamic\": \"from-helper\"}"
        } else {
            "echo '{\"x-dynamic\": \"from-helper\"}'"
        };
        let transport = make_transport(BTreeMap::new(), Some(helper));
        // Project scope, trusted → helper should execute.
        let headers = build_request_headers(
            "test-server",
            &transport,
            crate::config::ConfigSource::Project,
            true,
        )
        .await
        .unwrap();
        assert_eq!(headers.get("x-dynamic").unwrap(), "from-helper");
    }

    #[test]
    fn invalid_header_value_returns_error() {
        let map = BTreeMap::from([("x-bad".to_string(), "\0null".to_string())]);
        let result = static_headers(&map);
        assert!(result.is_err());
        match result.unwrap_err() {
            HeadersHelperError::InvalidHeaderValue { key, .. } => {
                assert_eq!(key, "x-bad");
            }
            other => panic!("expected InvalidHeaderValue, got {other:?}"),
        }
    }
}
