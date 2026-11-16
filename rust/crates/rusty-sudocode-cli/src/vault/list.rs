//! `vault_list`: every configured operation, labelled configured-unqualified,
//! with whether its secret is reachable through this agent's credential.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;

use nexus_vfs_client::NexusVfsClient;
use serde_json::{json, Map, Value};
use service_host::vault_client;

use super::credential;
use super::settings::{self, Operation, VaultSettings};
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

/// One credential snapshot and one connection, then one metadata listing per
/// configured namespace. Only whether each configured key is live leaves this
/// process; the rest of a listing is discarded.
fn availability(settings: &VaultSettings) -> Vec<&'static str> {
    let client = match connect(settings) {
        Ok(client) => client,
        Err(code) => return vec![code; settings.operations.len()],
    };
    let mut listings = BTreeMap::new();
    settings
        .operations
        .iter()
        .map(|operation| {
            let secret_ref = &operation.secret_ref;
            let listing = listings
                .entry(secret_ref.namespace.as_str())
                .or_insert_with(|| live_keys(&client, &secret_ref.namespace));
            match listing {
                Ok(keys) if keys.contains(&secret_ref.key) => "available",
                Ok(_) => "secret_absent",
                Err(code) => *code,
            }
        })
        .collect()
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

fn live_keys(client: &NexusVfsClient, namespace: &str) -> Result<BTreeSet<String>, &'static str> {
    vault_client::live_keys(client, namespace).map_err(|error| {
        match error.downcast_ref::<io::Error>().map(io::Error::kind) {
            Some(io::ErrorKind::PermissionDenied) => "access_denied",
            _ => VAULT_UNAVAILABLE,
        }
    })
}
