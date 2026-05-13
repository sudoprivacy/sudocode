//! Stdio-based ACP server.
//!
//! Thin wrapper that runs the shared ACP handler chain over stdin/stdout.

use std::sync::{Arc, Mutex};

use agent_client_protocol_tokio::Stdio;

use crate::acp_sdk_server::{
    new_abort_registry, run_acp_on_transport, SdkAcpConfig, SdkAcpDelegate, SharedDelegate,
};

/// Run the ACP server on stdin/stdout.
///
/// # Errors
///
/// Returns an error if the transport or handler chain fails.
pub async fn run_acp_stdio_server(
    config: SdkAcpConfig,
    delegate: Box<dyn SdkAcpDelegate>,
) -> Result<(), Box<dyn std::error::Error>> {
    let delegate: SharedDelegate = Arc::new(Mutex::new(delegate));
    run_acp_on_transport(&config, delegate, new_abort_registry(), Stdio::new()).await
}
