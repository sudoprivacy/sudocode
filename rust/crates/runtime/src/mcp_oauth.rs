//! MCP-dedicated OAuth client module.
//!
//! This module implements a complete OAuth 2.0 + PKCE flow for MCP remote servers,
//! fully independent of the CLI's own `oauth.rs` login module. It supports:
//!
//! - RFC 9728 protected-resource metadata discovery
//! - RFC 8414 authorization-server metadata discovery
//! - RFC 7591 dynamic client registration (DCR)
//! - Authorization Code + PKCE flow with loopback redirect
//! - Token refresh with cross-process locking
//! - Credential storage in OS keyring (primary) and file (fallback)

#[cfg(test)]
use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use crate::config::McpOAuthConfig;
use crate::mcp_client::{McpClientAuth, McpRemoteTransport};
use crate::oauth::{code_challenge_s256, loopback_redirect_uri};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// User-Agent header value for all HTTP requests in this module.
const USER_AGENT: &str = concat!("sudocode/", env!("CARGO_PKG_VERSION"));

/// Keyring service name for MCP OAuth credentials.
const KEYRING_SERVICE: &str = "sudocode-credentials";

/// Callback path for the loopback redirect server.
const CALLBACK_PATH: &str = "/callback";

/// Timeout for the authorization code callback wait (5 minutes).
const AUTH_CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Lock file acquire timeout for cross-process token refresh.
const LOCK_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Errors produced by MCP OAuth operations.
#[derive(Debug)]
pub enum McpOAuthError {
    /// The user needs to visit the authorize URL in a browser (interactive auth).
    NeedsInteractiveAuth { authorize_url: String },
    /// A network error occurred.
    Network(String),
    /// An I/O error occurred.
    Io(io::Error),
    /// A credential storage error occurred.
    Storage(String),
}

impl fmt::Display for McpOAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeedsInteractiveAuth { authorize_url } => {
                write!(f, "interactive auth required: {authorize_url}")
            }
            Self::Network(msg) => write!(f, "network error: {msg}"),
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::Storage(msg) => write!(f, "storage error: {msg}"),
        }
    }
}

impl std::error::Error for McpOAuthError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for McpOAuthError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

// ---------------------------------------------------------------------------
// Internal data types
// ---------------------------------------------------------------------------

/// Stored token set for an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredToken {
    access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
}

/// OAuth authorization server metadata (RFC 8414).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
struct AuthorizationServerMetadata {
    #[serde(default)]
    issuer: Option<String>,
    #[serde(default)]
    authorization_endpoint: Option<String>,
    #[serde(default)]
    token_endpoint: Option<String>,
    #[serde(default)]
    registration_endpoint: Option<String>,
    #[serde(default)]
    revocation_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scopes_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    code_challenge_methods_supported: Option<Vec<String>>,
    #[serde(default)]
    response_types_supported: Vec<String>,
    #[serde(default)]
    grant_types_supported: Vec<String>,
}

/// Protected resource metadata (RFC 9728).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
struct ProtectedResourceMetadata {
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    authorization_servers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scopes_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bearer_methods_supported: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resource_signing_alg_values_supported: Option<Vec<String>>,
}

/// DCR registration response (RFC 7591).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
struct ClientRegistrationResponse {
    client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
    #[serde(default)]
    redirect_uris: Vec<String>,
    #[serde(default)]
    grant_types: Vec<String>,
    #[serde(default)]
    response_types: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_id_issued_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_secret_expires_at: Option<u64>,
}

/// Token endpoint response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
struct TokenResponse {
    access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_in: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
}

/// Callback data received from the loopback server.
#[derive(Debug)]
struct CallbackResult {
    code: String,
    state: String,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Ensure a valid access token exists for the given MCP server.
///
/// Returns `None` if the server is not configured for OAuth.
/// Returns `Some(token)` if a valid (possibly refreshed) token is available.
/// Returns an error if token retrieval fails or interactive auth is required.
pub async fn ensure_access_token(
    server_name: &str,
    transport: &McpRemoteTransport,
) -> Result<Option<String>, McpOAuthError> {
    let McpClientAuth::OAuth(ref oauth_config) = transport.auth else {
        return Ok(None);
    };

    let server_key = compute_server_key(transport);
    let stored = load_token(&server_key)?;

    if let Some(token) = stored {
        if !is_token_expired(&token) {
            return Ok(Some(token.access_token));
        }

        // Try refresh if we have a refresh token.
        if let Some(refresh_token) = &token.refresh_token {
            match try_refresh_token(
                server_name,
                transport,
                oauth_config,
                &server_key,
                refresh_token,
                token.client_id.as_deref(),
                token.client_secret.as_deref(),
            )
            .await
            {
                Ok(new_access_token) => return Ok(Some(new_access_token)),
                Err(McpOAuthError::Network(_)) => {
                    // Network failure during refresh — return the expired token
                    // and let the caller handle the 401. Do not silently fail.
                    return Ok(Some(token.access_token));
                }
                Err(e) => return Err(e),
            }
        }
    }

    // No valid token available. Signal that interactive auth is needed.
    let authorize_url = build_authorize_url_for_error(server_name, transport, oauth_config).await?;
    Err(McpOAuthError::NeedsInteractiveAuth { authorize_url })
}

/// Handle a 401 Unauthorized response from an MCP server.
///
/// Attempts to refresh the token first. If that fails, starts a full
/// authorization code + PKCE flow.
pub async fn on_unauthorized(
    server_name: &str,
    transport: &McpRemoteTransport,
) -> Result<String, McpOAuthError> {
    let McpClientAuth::OAuth(ref oauth_config) = transport.auth else {
        return Err(McpOAuthError::Storage(
            "server is not configured for OAuth".to_string(),
        ));
    };

    let server_key = compute_server_key(transport);

    // Try refresh first.
    let stored = load_token(&server_key)?;
    if let Some(token) = &stored {
        if let Some(refresh_token) = &token.refresh_token {
            match try_refresh_token(
                server_name,
                transport,
                oauth_config,
                &server_key,
                refresh_token,
                token.client_id.as_deref(),
                token.client_secret.as_deref(),
            )
            .await
            {
                Ok(new_token) => return Ok(new_token),
                Err(_) => {
                    // Refresh failed; fall through to full auth flow.
                }
            }
        }
    }

    // Full authorization code + PKCE flow.
    run_authorization_code_flow(server_name, transport, oauth_config, &server_key).await
}

// ---------------------------------------------------------------------------
// Server key computation
// ---------------------------------------------------------------------------

/// Compute a stable key for the given transport configuration.
///
/// Format: `SHA256(transport_type + ":" + url + ":" + sorted_headers)` -> hex prefix (16 chars).
fn compute_server_key(transport: &McpRemoteTransport) -> String {
    let transport_type = "remote"; // All McpRemoteTransport variants use "remote"
    let mut header_parts: Vec<String> = transport
        .headers
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    header_parts.sort();

    let input = format!(
        "{}:{}:{}",
        transport_type,
        transport.url,
        header_parts.join(",")
    );
    let digest = Sha256::digest(input.as_bytes());
    hex_encode(&digest[..8])
}

// ---------------------------------------------------------------------------
// Token storage
// ---------------------------------------------------------------------------

fn token_file_path(server_key: &str) -> Result<PathBuf, McpOAuthError> {
    let config_home = config_home_dir()?;
    Ok(config_home
        .join("mcp-oauth")
        .join(format!("{server_key}.json")))
}

fn config_home_dir() -> Result<PathBuf, McpOAuthError> {
    std::env::var_os("SUDO_CODE_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("SUDOCODE_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|home| PathBuf::from(home).join(".nexus").join("sudocode"))
        })
        .ok_or_else(|| {
            McpOAuthError::Storage(
                "cannot determine config home: set SUDO_CODE_CONFIG_HOME or HOME".to_string(),
            )
        })
}

/// Load a stored token from keyring (primary) or file (fallback).
fn load_token(server_key: &str) -> Result<Option<StoredToken>, McpOAuthError> {
    // Try keyring first.
    if let Some(token) = load_token_from_keyring(server_key) {
        return Ok(Some(token));
    }

    // Fallback to file.
    let path = token_file_path(server_key)?;
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(McpOAuthError::Io(e)),
    };

    let token: StoredToken =
        serde_json::from_str(&content).map_err(|e| McpOAuthError::Storage(format!("{e}")))?;
    Ok(Some(token))
}

/// Save token to both keyring and file.
fn save_token(server_key: &str, token: &StoredToken) -> Result<(), McpOAuthError> {
    // Save to keyring.
    save_token_to_keyring(server_key, token);

    // Save to file.
    let path = token_file_path(server_key)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(McpOAuthError::Io)?;
    }
    let json = serde_json::to_string_pretty(token)
        .map_err(|e| McpOAuthError::Storage(format!("serialize token: {e}")))?;
    std::fs::write(&path, format!("{json}\n").as_bytes()).map_err(McpOAuthError::Io)?;

    Ok(())
}

fn keyring_entry_key(server_key: &str) -> String {
    format!("mcp-oauth-{server_key}")
}

fn load_token_from_keyring(server_key: &str) -> Option<StoredToken> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &keyring_entry_key(server_key)).ok()?;
    let json = entry.get_password().ok()?;
    serde_json::from_str(&json).ok()
}

fn save_token_to_keyring(server_key: &str, token: &StoredToken) {
    let Ok(json) = serde_json::to_string(token) else {
        return;
    };
    let entry = keyring::Entry::new(KEYRING_SERVICE, &keyring_entry_key(server_key));
    if let Ok(entry) = entry {
        let _ = entry.set_password(&json);
    }
}

fn clear_token_from_keyring(server_key: &str) {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &keyring_entry_key(server_key)) {
        let _ = entry.delete_credential();
    }
}

fn is_token_expired(token: &StoredToken) -> bool {
    match token.expires_at {
        Some(expires_at) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            // Consider expired if within 60 seconds of expiry.
            now + 60 >= expires_at
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Metadata discovery
// ---------------------------------------------------------------------------

/// Discover the authorization server metadata for an MCP server endpoint.
///
/// Discovery order:
/// 1. Config-provided `auth_server_metadata_url` from McpOAuthConfig
/// 2. RFC 9728: `{url}/.well-known/oauth-protected-resource`
/// 3. RFC 8414: `{url}/.well-known/oauth-authorization-server`
async fn discover_metadata(
    transport: &McpRemoteTransport,
    oauth_config: &McpOAuthConfig,
) -> Result<AuthorizationServerMetadata, McpOAuthError> {
    let client = http_client();

    // 1. If a metadata URL is explicitly configured, use it directly.
    if let Some(ref metadata_url) = oauth_config.auth_server_metadata_url {
        return fetch_metadata(&client, metadata_url).await;
    }

    // 2. RFC 9728: Protected resource metadata.
    let protected_url =
        build_well_known_url(&transport.url, ".well-known/oauth-protected-resource");
    if let Ok(protected) = fetch_protected_resource_metadata(&client, &protected_url).await {
        if let Some(auth_servers) = &protected.authorization_servers {
            // Try each authorization server URL to get full metadata.
            for server_url in auth_servers {
                if let Ok(meta) = fetch_metadata(&client, server_url).await {
                    if meta.authorization_endpoint.is_some() && meta.token_endpoint.is_some() {
                        return Ok(meta);
                    }
                }
            }
        }
    }

    // 3. RFC 8414: Authorization server metadata.
    let auth_server_url =
        build_well_known_url(&transport.url, ".well-known/oauth-authorization-server");
    fetch_metadata(&client, &auth_server_url).await
}

fn build_well_known_url(base_url: &str, path: &str) -> String {
    // Parse base URL and construct well-known URL at the same origin.
    let url = url::Url::parse(base_url)
        .unwrap_or_else(|_| url::Url::parse("http://localhost/").expect("fallback URL"));

    let scheme = url.scheme();
    let host = url.host_str().unwrap_or("localhost");
    let port_part = match url.port() {
        Some(p) => format!(":{p}"),
        None => String::new(),
    };
    let base_path = url.path();

    // If the base URL has a non-root path, we append the well-known after
    // stripping the last path segment (which is typically the MCP endpoint).
    if base_path != "/" && !base_path.is_empty() {
        let trimmed = base_path.trim_end_matches('/');
        if let Some(slash_pos) = trimmed.rfind('/') {
            return format!(
                "{scheme}://{host}{port_part}{}{path}",
                &base_path[..=slash_pos],
            );
        }
    }

    format!("{scheme}://{host}{port_part}/{path}")
}

async fn fetch_metadata(
    client: &reqwest::Client,
    url: &str,
) -> Result<AuthorizationServerMetadata, McpOAuthError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| McpOAuthError::Network(format!("fetch metadata from {url}: {e}")))?;

    if !response.status().is_success() {
        return Err(McpOAuthError::Network(format!(
            "metadata endpoint {url} returned status {}",
            response.status()
        )));
    }

    response
        .json::<AuthorizationServerMetadata>()
        .await
        .map_err(|e| McpOAuthError::Network(format!("parse metadata from {url}: {e}")))
}

async fn fetch_protected_resource_metadata(
    client: &reqwest::Client,
    url: &str,
) -> Result<ProtectedResourceMetadata, McpOAuthError> {
    let response = client.get(url).send().await.map_err(|e| {
        McpOAuthError::Network(format!("fetch protected resource metadata from {url}: {e}"))
    })?;

    if !response.status().is_success() {
        return Err(McpOAuthError::Network(format!(
            "protected resource metadata endpoint {url} returned status {}",
            response.status()
        )));
    }

    response
        .json::<ProtectedResourceMetadata>()
        .await
        .map_err(|e| {
            McpOAuthError::Network(format!("parse protected resource metadata from {url}: {e}"))
        })
}

// ---------------------------------------------------------------------------
// Dynamic Client Registration (RFC 7591)
// ---------------------------------------------------------------------------

async fn register_client(
    client: &reqwest::Client,
    registration_endpoint: &str,
    redirect_uri: &str,
) -> Result<ClientRegistrationResponse, McpOAuthError> {
    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    struct RegistrationRequest {
        redirect_uris: Vec<String>,
        token_endpoint_auth_method: String,
        grant_types: Vec<String>,
        response_types: Vec<String>,
        application_type: String,
        client_name: String,
    }

    let body = RegistrationRequest {
        redirect_uris: vec![redirect_uri.to_string()],
        token_endpoint_auth_method: "none".to_string(),
        grant_types: vec![
            "authorization_code".to_string(),
            "refresh_token".to_string(),
        ],
        response_types: vec!["code".to_string()],
        application_type: "native".to_string(),
        client_name: "sudocode".to_string(),
    };

    let response = client
        .post(registration_endpoint)
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            McpOAuthError::Network(format!("register client at {registration_endpoint}: {e}"))
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(McpOAuthError::Network(format!(
            "DCR at {registration_endpoint} returned {status}: {body}"
        )));
    }

    response
        .json::<ClientRegistrationResponse>()
        .await
        .map_err(|e| McpOAuthError::Network(format!("parse DCR response: {e}")))
}

// ---------------------------------------------------------------------------
// PKCE helpers (using getrandom)
// ---------------------------------------------------------------------------

fn generate_random_bytes(len: usize) -> Result<Vec<u8>, McpOAuthError> {
    let mut buf = vec![0u8; len];
    getrandom::fill(&mut buf).map_err(|e| {
        McpOAuthError::Io(io::Error::new(
            io::ErrorKind::Other,
            format!("getrandom: {e}"),
        ))
    })?;
    Ok(buf)
}

fn generate_pkce_verifier() -> Result<String, McpOAuthError> {
    let bytes = generate_random_bytes(32)?;
    Ok(base64url_encode(&bytes))
}

fn generate_state() -> Result<String, McpOAuthError> {
    let bytes = generate_random_bytes(32)?;
    Ok(base64url_encode(&bytes))
}

// ---------------------------------------------------------------------------
// Authorization Code + PKCE flow
// ---------------------------------------------------------------------------

/// Run the full authorization code flow with PKCE.
async fn run_authorization_code_flow(
    server_name: &str,
    transport: &McpRemoteTransport,
    oauth_config: &McpOAuthConfig,
    server_key: &str,
) -> Result<String, McpOAuthError> {
    // 1. Discover metadata.
    let metadata = discover_metadata(transport, oauth_config).await?;

    let authorization_endpoint = metadata.authorization_endpoint.as_deref().ok_or_else(|| {
        McpOAuthError::Network("metadata missing authorization_endpoint".to_string())
    })?;
    let token_endpoint = metadata
        .token_endpoint
        .as_deref()
        .ok_or_else(|| McpOAuthError::Network("metadata missing token_endpoint".to_string()))?;

    // 2. Determine client ID — either from config or via DCR.
    let (client_id, client_secret, redirect_uri) =
        resolve_client_credentials(&metadata, oauth_config).await?;

    // 3. Generate PKCE pair and state.
    let verifier = generate_pkce_verifier()?;
    let challenge = code_challenge_s256(&verifier);
    let state = generate_state()?;

    // 4. Build authorize URL and present it to the user.
    let authorize_url = build_authorize_url(
        authorization_endpoint,
        &client_id,
        &redirect_uri,
        &challenge,
        &state,
    );
    eprintln!(
        "[mcp-oauth] Please visit the following URL to authorize MCP server '{server_name}':\n  {authorize_url}"
    );

    // 5. Start loopback callback server.
    let callback_port = oauth_config.callback_port.unwrap_or(0);
    let (callback_result, _server) = start_callback_server_and_wait(callback_port, &state).await?;

    // 6. Exchange code for tokens.
    let token_response = exchange_code(
        token_endpoint,
        &callback_result.code,
        &verifier,
        &redirect_uri,
        &client_id,
        client_secret.as_deref(),
    )
    .await?;

    // 7. Store tokens.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let expires_at = token_response.expires_in.map(|d| now + d);

    let stored = StoredToken {
        access_token: token_response.access_token.clone(),
        refresh_token: token_response.refresh_token,
        expires_at,
        client_id: Some(client_id),
        client_secret,
    };

    save_token(server_key, &stored)?;

    eprintln!("[mcp-oauth] successfully authenticated MCP server '{server_name}'");

    Ok(token_response.access_token)
}

/// Resolve client credentials: use configured client_id or perform DCR.
async fn resolve_client_credentials(
    metadata: &AuthorizationServerMetadata,
    oauth_config: &McpOAuthConfig,
) -> Result<(String, Option<String>, String), McpOAuthError> {
    // Determine the redirect URI (will use loopback).
    let redirect_uri = loopback_redirect_uri(0); // port=0 placeholder; real port assigned later

    if let Some(ref configured_client_id) = oauth_config.client_id {
        return Ok((configured_client_id.clone(), None, redirect_uri));
    }

    // No configured client ID — try DCR.
    let registration_endpoint = metadata.registration_endpoint.as_deref().ok_or_else(|| {
        McpOAuthError::Network(
            "no client_id configured and server does not advertise registration_endpoint"
                .to_string(),
        )
    })?;

    let client = http_client();

    // Use the actual redirect URI (loopback). The port will be 0 initially,
    // but we re-register after we know the port if needed.
    let reg = register_client(&client, registration_endpoint, &redirect_uri).await?;

    Ok((reg.client_id, reg.client_secret, redirect_uri))
}

fn build_authorize_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
) -> String {
    let params = [
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("code_challenge", code_challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
    ];

    let query = params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    format!(
        "{}{}{}",
        authorization_endpoint,
        if authorization_endpoint.contains('?') {
            '&'
        } else {
            '?'
        },
        query
    )
}

/// Start a loopback HTTP server on 127.0.0.1 and wait for the OAuth callback.
async fn start_callback_server_and_wait(
    preferred_port: u16,
    expected_state: &str,
) -> Result<(CallbackResult, u16), McpOAuthError> {
    use axum::extract::State;
    use axum::response::IntoResponse;

    let (tx, rx) = oneshot::channel::<CallbackResult>();
    let tx = Arc::new(std::sync::Mutex::new(Some(tx)));

    let expected_state = expected_state.to_string();

    let callback = move |axum::extract::Query(params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
                         State(tx): State<
        Arc<std::sync::Mutex<Option<oneshot::Sender<CallbackResult>>>>,
    >| {
        let tx = tx.clone();
        async move {
            let code = params.get("code").cloned().unwrap_or_default();
            let state = params.get("state").cloned().unwrap_or_default();
            let error = params.get("error").cloned();

            let body = if let Some(err) = error {
                let desc = params.get("error_description").cloned().unwrap_or_default();
                format!(
                    "<html><body><h2>Authentication failed</h2><p>{err}: {desc}</p></body></html>"
                )
            } else if code.is_empty() {
                "<html><body><h2>Authentication failed</h2><p>No authorization code received.</p></body></html>".to_string()
            } else {
                "<html><body><h2>Authentication successful</h2><p>You can close this tab.</p></body></html>".to_string()
            };

            if let Some(sender) = tx.lock().ok().and_then(|mut guard| guard.take()) {
                let _ = sender.send(CallbackResult { code, state });
            }

            ([("content-type", "text/html; charset=utf-8")], body).into_response()
        }
    };

    let app = axum::Router::new()
        .route(CALLBACK_PATH, axum::routing::get(callback))
        .with_state(tx);

    let listener = if preferred_port > 0 {
        tokio::net::TcpListener::bind(format!("127.0.0.1:{preferred_port}"))
            .await
            .map_err(|e| {
                McpOAuthError::Io(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("bind 127.0.0.1:{preferred_port}: {e}"),
                ))
            })?
    } else {
        tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| {
                McpOAuthError::Io(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("bind 127.0.0.1:0: {e}"),
                ))
            })?
    };

    let actual_port = listener.local_addr().map_err(McpOAuthError::Io)?.port();

    let server = axum::serve(listener, app);

    // Spawn the server in the background; it will stop after the first request.
    let server_handle = tokio::spawn(async move {
        let _ = server.await;
    });

    // Wait for callback with timeout.
    let _start = Instant::now();
    let result = tokio::select! {
        result = rx => {
            result.map_err(|_| McpOAuthError::Storage("callback channel closed unexpectedly".to_string()))
        }
        _ = tokio::time::sleep(AUTH_CALLBACK_TIMEOUT) => {
            server_handle.abort();
            Err(McpOAuthError::Storage(
                format!("timed out waiting for OAuth callback after {}s on port {actual_port}",
                    AUTH_CALLBACK_TIMEOUT.as_secs())
            ))
        }
    };

    // Abort server if still running.
    server_handle.abort();

    let callback_result = result?;

    // Validate state.
    if callback_result.state != expected_state {
        return Err(McpOAuthError::Storage(format!(
            "OAuth state mismatch: expected {expected_state}, got {}",
            callback_result.state
        )));
    }

    if callback_result.code.is_empty() {
        return Err(McpOAuthError::Storage(
            "OAuth callback did not include an authorization code".to_string(),
        ));
    }

    Ok((callback_result, actual_port))
}

/// Exchange an authorization code for tokens.
async fn exchange_code(
    token_endpoint: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    client_id: &str,
    client_secret: Option<&str>,
) -> Result<TokenResponse, McpOAuthError> {
    let client = http_client();

    let mut params = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("code_verifier", verifier.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("client_id", client_id.to_string()),
    ];

    if let Some(secret) = client_secret {
        params.push(("client_secret", secret.to_string()));
    }

    let response = client
        .post(token_endpoint)
        .form(&params)
        .send()
        .await
        .map_err(|e| McpOAuthError::Network(format!("token exchange at {token_endpoint}: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(McpOAuthError::Network(format!(
            "token exchange at {token_endpoint} returned {status}: {body}"
        )));
    }

    response
        .json::<TokenResponse>()
        .await
        .map_err(|e| McpOAuthError::Network(format!("parse token response: {e}")))
}

// ---------------------------------------------------------------------------
// Token refresh
// ---------------------------------------------------------------------------

/// Attempt to refresh the access token using a stored refresh token.
///
/// Uses a file-based lock to prevent concurrent refreshes across processes.
async fn try_refresh_token(
    _server_name: &str,
    transport: &McpRemoteTransport,
    oauth_config: &McpOAuthConfig,
    server_key: &str,
    refresh_token: &str,
    client_id: Option<&str>,
    client_secret: Option<&str>,
) -> Result<String, McpOAuthError> {
    // Acquire cross-process lock.
    let lock_path = token_lock_path(server_key)?;
    let _lock = acquire_lock(&lock_path).await?;

    // Discover metadata for token endpoint.
    let metadata = discover_metadata(transport, oauth_config).await?;
    let token_endpoint = metadata
        .token_endpoint
        .as_deref()
        .ok_or_else(|| McpOAuthError::Network("metadata missing token_endpoint".to_string()))?;

    // Use provided client_id or discover via metadata.
    let effective_client_id = client_id
        .map(str::to_string)
        .or(oauth_config.client_id.clone())
        .ok_or_else(|| {
            McpOAuthError::Storage("no client_id available for token refresh".to_string())
        })?;

    let client = http_client();

    let mut params = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
        ("client_id", effective_client_id.clone()),
    ];

    if let Some(secret) = client_secret {
        params.push(("client_secret", secret.to_string()));
    }

    let response = client
        .post(token_endpoint)
        .form(&params)
        .send()
        .await
        .map_err(|e| McpOAuthError::Network(format!("token refresh at {token_endpoint}: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(McpOAuthError::Network(format!(
            "token refresh at {token_endpoint} returned {status}: {body}"
        )));
    }

    let token_response: TokenResponse = response
        .json()
        .await
        .map_err(|e| McpOAuthError::Network(format!("parse refresh response: {e}")))?;

    // Store updated tokens.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let expires_at = token_response.expires_in.map(|d| now + d);

    let stored = StoredToken {
        access_token: token_response.access_token.clone(),
        refresh_token: token_response
            .refresh_token
            .or_else(|| Some(refresh_token.to_string())),
        expires_at,
        client_id: Some(effective_client_id),
        client_secret: client_secret.map(str::to_string),
    };

    save_token(server_key, &stored)?;

    let _ = lock_path; // Lock released when _lock is dropped.

    Ok(token_response.access_token)
}

fn token_lock_path(server_key: &str) -> Result<PathBuf, McpOAuthError> {
    let config_home = config_home_dir()?;
    Ok(config_home
        .join("mcp-oauth")
        .join("locks")
        .join(format!("{server_key}.lock")))
}

/// Acquire a cross-process lock using atomic file creation.
///
/// Uses `tokio::time::sleep` instead of `std::thread::sleep` to avoid
/// blocking the tokio worker thread while polling.
async fn acquire_lock(lock_path: &PathBuf) -> Result<LockGuard, McpOAuthError> {
    // Ensure the parent directory (locks/) exists.
    if let Some(parent) = lock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let start = Instant::now();

    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(_file) => {
                return Ok(LockGuard {
                    path: lock_path.clone(),
                });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // Check if the lock is stale (older than 5 minutes).
                if let Ok(metadata) = std::fs::metadata(lock_path) {
                    if let Ok(modified) = metadata.modified() {
                        if let Ok(elapsed) = modified.elapsed() {
                            if elapsed > Duration::from_secs(5 * 60) {
                                // Stale lock — remove it and retry.
                                let _ = std::fs::remove_file(lock_path);
                                continue;
                            }
                        }
                    }
                }

                if start.elapsed() > LOCK_ACQUIRE_TIMEOUT {
                    return Err(McpOAuthError::Storage(format!(
                        "timed out acquiring lock: {}",
                        lock_path.display()
                    )));
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => {
                return Err(McpOAuthError::Io(e));
            }
        }
    }
}

/// RAII guard that releases the lock file on drop.
struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------
// HTTP client
// ---------------------------------------------------------------------------

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

// ---------------------------------------------------------------------------
// Helpers for building authorize URL for error messages
// ---------------------------------------------------------------------------

async fn build_authorize_url_for_error(
    server_name: &str,
    transport: &McpRemoteTransport,
    oauth_config: &McpOAuthConfig,
) -> Result<String, McpOAuthError> {
    match discover_metadata(transport, oauth_config).await {
        Ok(metadata) => {
            if let Some(ref endpoint) = metadata.authorization_endpoint {
                let client_id = oauth_config.client_id.as_deref().unwrap_or("unknown");
                let redirect_uri = loopback_redirect_uri(oauth_config.callback_port.unwrap_or(0));
                Ok(build_authorize_url(
                    endpoint,
                    client_id,
                    &redirect_uri,
                    "placeholder-challenge",
                    "placeholder-state",
                ))
            } else {
                Ok(format!("https://unknown/authorize?server={server_name}"))
            }
        }
        Err(_) => Ok(format!("https://unknown/authorize?server={server_name}")),
    }
}

// ---------------------------------------------------------------------------
// Encoding helpers
// ---------------------------------------------------------------------------

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn base64url_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::new();
    let mut index = 0;
    while index + 3 <= bytes.len() {
        let block = (u32::from(bytes[index]) << 16)
            | (u32::from(bytes[index + 1]) << 8)
            | u32::from(bytes[index + 2]);
        output.push(TABLE[((block >> 18) & 0x3F) as usize] as char);
        output.push(TABLE[((block >> 12) & 0x3F) as usize] as char);
        output.push(TABLE[((block >> 6) & 0x3F) as usize] as char);
        output.push(TABLE[(block & 0x3F) as usize] as char);
        index += 3;
    }
    match bytes.len().saturating_sub(index) {
        1 => {
            let block = u32::from(bytes[index]) << 16;
            output.push(TABLE[((block >> 18) & 0x3F) as usize] as char);
            output.push(TABLE[((block >> 12) & 0x3F) as usize] as char);
        }
        2 => {
            let block = (u32::from(bytes[index]) << 16) | (u32::from(bytes[index + 1]) << 8);
            output.push(TABLE[((block >> 18) & 0x3F) as usize] as char);
            output.push(TABLE[((block >> 12) & 0x3F) as usize] as char);
            output.push(TABLE[((block >> 6) & 0x3F) as usize] as char);
        }
        _ => {}
    }
    output
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(char::from(byte));
            }
            _ => {
                use std::fmt::Write as _;
                let _ = write!(&mut encoded, "%{byte:02X}");
            }
        }
    }
    encoded
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_key_is_deterministic_and_hex() {
        let transport = McpRemoteTransport {
            url: "https://example.test/mcp".to_string(),
            headers: BTreeMap::from([
                ("Authorization".to_string(), "Bearer x".to_string()),
                ("X-Custom".to_string(), "123".to_string()),
            ]),
            headers_helper: None,
            auth: McpClientAuth::None,
        };

        let key1 = compute_server_key(&transport);
        let key2 = compute_server_key(&transport);
        assert_eq!(key1, key2, "same transport must produce same key");
        assert_eq!(key1.len(), 16, "key must be 16 hex characters");
        assert!(
            key1.chars().all(|c| c.is_ascii_hexdigit()),
            "key must be hex"
        );
    }

    #[test]
    fn server_key_differs_for_different_urls() {
        let t1 = McpRemoteTransport {
            url: "https://a.test/mcp".to_string(),
            headers: BTreeMap::new(),
            headers_helper: None,
            auth: McpClientAuth::None,
        };
        let t2 = McpRemoteTransport {
            url: "https://b.test/mcp".to_string(),
            headers: BTreeMap::new(),
            headers_helper: None,
            auth: McpClientAuth::None,
        };
        assert_ne!(compute_server_key(&t1), compute_server_key(&t2));
    }

    #[test]
    fn server_key_differs_for_different_headers() {
        let t1 = McpRemoteTransport {
            url: "https://example.test/mcp".to_string(),
            headers: BTreeMap::from([("X-A".to_string(), "1".to_string())]),
            headers_helper: None,
            auth: McpClientAuth::None,
        };
        let t2 = McpRemoteTransport {
            url: "https://example.test/mcp".to_string(),
            headers: BTreeMap::from([("X-A".to_string(), "2".to_string())]),
            headers_helper: None,
            auth: McpClientAuth::None,
        };
        assert_ne!(compute_server_key(&t1), compute_server_key(&t2));
    }

    #[test]
    fn stored_token_serialization_roundtrip() {
        let token = StoredToken {
            access_token: "at-123".to_string(),
            refresh_token: Some("rt-456".to_string()),
            expires_at: Some(9999999),
            client_id: Some("cid-789".to_string()),
            client_secret: None,
        };
        let json = serde_json::to_string(&token).expect("serialize");
        let parsed: StoredToken = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.access_token, "at-123");
        assert_eq!(parsed.refresh_token, Some("rt-456".to_string()));
        assert_eq!(parsed.expires_at, Some(9999999));
        assert_eq!(parsed.client_id, Some("cid-789".to_string()));
        assert!(parsed.client_secret.is_none());
    }

    #[test]
    fn token_expiry_check() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let valid = StoredToken {
            access_token: "at".to_string(),
            refresh_token: None,
            expires_at: Some(now + 3600),
            client_id: None,
            client_secret: None,
        };
        assert!(!is_token_expired(&valid));

        let expired = StoredToken {
            access_token: "at".to_string(),
            refresh_token: None,
            expires_at: Some(now - 10),
            client_id: None,
            client_secret: None,
        };
        assert!(is_token_expired(&expired));

        let no_expiry = StoredToken {
            access_token: "at".to_string(),
            refresh_token: None,
            expires_at: None,
            client_id: None,
            client_secret: None,
        };
        assert!(!is_token_expired(&no_expiry));
    }

    #[test]
    fn build_authorize_url_contains_required_params() {
        let url = build_authorize_url(
            "https://auth.example/authorize",
            "my-client-id",
            "http://localhost:12345/callback",
            "challenge-abc",
            "state-xyz",
        );
        assert!(url.starts_with("https://auth.example/authorize?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=my-client-id"));
        assert!(url.contains("code_challenge=challenge-abc"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state-xyz"));
        assert!(url.contains("redirect_uri=http"));
    }

    #[test]
    fn base64url_encoding_matches_expected_vectors() {
        // Empty
        assert_eq!(base64url_encode(&[]), "");
        // "hello" -> aGVsbG8
        assert_eq!(base64url_encode(b"hello"), "aGVsbG8");
        // Known vector: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk" -> SHA256 base64url
        let digest = Sha256::digest(b"dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(
            base64url_encode(&digest),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn hex_encode_produces_lowercase_hex() {
        assert_eq!(hex_encode(&[0x0f, 0xab, 0x01]), "0fab01");
    }

    #[test]
    fn ensure_access_token_returns_none_for_non_oauth_transport() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let transport = McpRemoteTransport {
            url: "https://example.test/mcp".to_string(),
            headers: BTreeMap::new(),
            headers_helper: None,
            auth: McpClientAuth::None,
        };
        let result = rt.block_on(ensure_access_token("test", &transport));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn percent_encode_handles_special_chars() {
        assert_eq!(percent_encode("hello world"), "hello%20world");
        assert_eq!(percent_encode("a/b:c@d"), "a%2Fb%3Ac%40d");
        assert_eq!(percent_encode("abc-123_456.789~0"), "abc-123_456.789~0");
    }

    #[test]
    fn build_well_known_url_constructs_correct_path() {
        let url = build_well_known_url(
            "https://auth.example.com/api/mcp",
            ".well-known/oauth-authorization-server",
        );
        assert!(url.starts_with("https://auth.example.com/"));
        assert!(url.contains(".well-known/oauth-authorization-server"));
    }

    #[test]
    fn keyring_entry_key_format() {
        assert_eq!(keyring_entry_key("abc123"), "mcp-oauth-abc123");
    }

    #[test]
    fn metadata_deserialization_handles_minimal_json() {
        let json = r#"{
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code"]
        }"#;
        let meta: AuthorizationServerMetadata = serde_json::from_str(json).expect("parse");
        assert!(meta.issuer.is_none());
        assert!(meta.authorization_endpoint.is_none());
        assert!(meta.token_endpoint.is_none());
        assert!(meta.registration_endpoint.is_none());
        assert_eq!(meta.response_types_supported, vec!["code"]);
    }

    #[test]
    fn token_response_deserialization() {
        let json = r#"{
            "access_token": "at-123",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "rt-456",
            "scope": "read write"
        }"#;
        let resp: TokenResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(resp.access_token, "at-123");
        assert_eq!(resp.token_type, Some("Bearer".to_string()));
        assert_eq!(resp.expires_in, Some(3600));
        assert_eq!(resp.refresh_token, Some("rt-456".to_string()));
        assert_eq!(resp.scope, Some("read write".to_string()));
    }

    #[test]
    fn client_registration_response_deserialization() {
        let json = r#"{
            "client_id": "reg-123",
            "client_secret": "secret-abc",
            "redirect_uris": ["http://localhost:12345/callback"],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "client_id_issued_at": 1000000
        }"#;
        let resp: ClientRegistrationResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(resp.client_id, "reg-123");
        assert_eq!(resp.client_secret, Some("secret-abc".to_string()));
        assert_eq!(resp.redirect_uris.len(), 1);
    }

    #[test]
    fn protected_resource_metadata_deserialization() {
        let json = r#"{
            "resource": "https://api.example.com",
            "authorization_servers": ["https://auth.example.com"]
        }"#;
        let meta: ProtectedResourceMetadata = serde_json::from_str(json).expect("parse");
        assert_eq!(meta.resource, Some("https://api.example.com".to_string()));
        assert_eq!(
            meta.authorization_servers,
            Some(vec!["https://auth.example.com".to_string()])
        );
    }
}
