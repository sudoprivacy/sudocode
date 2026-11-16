//! `vault_list`: every configured operation, labelled configured-unqualified,
//! with whether its secret is reachable through this agent's credential.

use std::io;
use std::path::Path;

use nexus_vfs_client::NexusVfsClient;
use serde_json::{json, Map, Value};
use service_host::vault_client;

use super::credential;
use super::settings::{self, Operation, SecretRef, VaultSettings};
use super::Refusal;

const NOTE: &str = "operations are configured locally, not authorized by the backend; \
                    per-agent grants: https://github.com/nexi-lab/nexus-vfs/issues/579";

pub(super) fn run(
    cwd: &Path,
    input: Result<Map<String, Value>, Refusal>,
) -> Result<Value, Refusal> {
    let settings = settings::load(cwd)?;
    if !input?.is_empty() {
        return Err(Refusal::invalid_input("vault_list takes no input"));
    }
    let operations: Vec<Value> = settings
        .operations
        .iter()
        .zip(availability(&settings))
        .map(|(operation, availability)| describe(operation, availability))
        .collect();
    Ok(json!({
        "status": "configured-unqualified",
        "note": NOTE,
        "operations": operations,
    }))
}

fn describe(operation: &Operation, availability: &str) -> Value {
    json!({
        "id": operation.id,
        "kind": operation.kind,
        "ref": operation.secret_ref.to_string(),
        "target": operation.target,
        "availability": availability,
    })
}

/// One credential snapshot and one connection, then a metadata-only lookup of
/// each exact ref. A credential or connection failure applies to every operation.
fn availability(settings: &VaultSettings) -> Vec<&'static str> {
    match connect(settings) {
        Ok(client) => settings
            .operations
            .iter()
            .map(|operation| secret_state(&client, &operation.secret_ref))
            .collect(),
        Err(code) => vec![code; settings.operations.len()],
    }
}

fn connect(settings: &VaultSettings) -> Result<NexusVfsClient, &'static str> {
    let bundle = credential::snapshot(&settings.credential_dir, &settings.owner)
        .map_err(credential::CredentialError::code)?;
    NexusVfsClient::connect_tls(
        &settings.endpoint,
        bundle.ca_pem,
        bundle.cert_pem,
        bundle.key_pem,
        &bundle.server_name,
    )
    .map_err(|_| VAULT_UNAVAILABLE)
}

/// The endpoint could not be reached, or the Vault service on it gave no
/// usable answer. The client reports transport failures and unclassified
/// backend errors alike, so this does not claim which one it was.
const VAULT_UNAVAILABLE: &str = "vault_unavailable";

fn secret_state(client: &NexusVfsClient, secret_ref: &SecretRef) -> &'static str {
    match vault_client::secret_versions(client, &secret_ref.namespace, &secret_ref.key) {
        Ok(versions) if versions.is_empty() => "secret_absent",
        Ok(_) => "available",
        Err(error) => match error.downcast_ref::<io::Error>().map(io::Error::kind) {
            Some(io::ErrorKind::NotFound) => "secret_absent",
            Some(io::ErrorKind::PermissionDenied) => "access_denied",
            _ => VAULT_UNAVAILABLE,
        },
    }
}
