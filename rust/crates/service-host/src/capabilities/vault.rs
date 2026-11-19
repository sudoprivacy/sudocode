use super::required;
use crate::vault_client::{call, proto, secret_versions};
use crate::Result;
use nexus_vfs_client::NexusVfsClient;
use serde_json::{json, Value};

fn timestamp(value: Option<prost_types::Timestamp>) -> Option<i64> {
    value.map(|value| value.seconds * 1000 + i64::from(value.nanos) / 1_000_000)
}

fn metadata(value: &proto::SecretMetadata) -> Value {
    json!({"namespace": value.namespace, "key": value.key, "description": value.description,
        "currentVersion": value.current_version, "deleted": value.deleted,
        "createdAt": timestamp(value.created_at), "updatedAt": timestamp(value.updated_at)})
}

fn get_request(params: &Value) -> Result<proto::GetSecretRequest> {
    Ok(proto::GetSecretRequest {
        namespace: required(params, "namespace")?.into(),
        key: required(params, "key")?.into(),
        version: params
            .get("version")
            .and_then(Value::as_i64)
            .map(i32::try_from)
            .transpose()?,
    })
}

fn put_request(params: &Value) -> Result<proto::PutSecretRequest> {
    Ok(proto::PutSecretRequest {
        namespace: required(params, "namespace")?.into(),
        key: required(params, "key")?.into(),
        value: required(params, "value")?.into(),
        description: params
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

pub(super) fn dispatch(client: &NexusVfsClient, method: &str, params: &Value) -> Result<Value> {
    match method {
        "vault.get" => get_secret(client, params),
        "vault.put" => {
            let response: proto::PutSecretResponse =
                call(client, "secret_put", &put_request(params)?)?;
            Ok(metadata(
                &response.metadata.ok_or("vault put returned no metadata")?,
            ))
        }
        "vault.list" => list_secrets(client, params),
        "vault.delete" => {
            let request = get_request(params)?;
            let response: proto::DeleteSecretResponse = call(
                client,
                "secret_delete",
                &proto::DeleteSecretRequest {
                    namespace: request.namespace,
                    key: request.key,
                },
            )?;
            Ok(json!(response.deleted))
        }
        "vault.restore" => {
            let request = get_request(params)?;
            let response: proto::RestoreSecretResponse = call(
                client,
                "secret_restore",
                &proto::RestoreSecretRequest {
                    namespace: request.namespace,
                    key: request.key,
                },
            )?;
            Ok(json!(response.restored))
        }
        "vault.versions" => {
            let request = get_request(params)?;
            let versions = secret_versions(client, &request.namespace, &request.key)?;
            Ok(json!(versions.into_iter().map(|value| json!({"version": value.version, "createdAt": timestamp(value.created_at)})).collect::<Vec<_>>()))
        }
        "vault.batchGet" => batch_get_secrets(client, params),
        "vault.batchPut" => batch_put_secrets(client, params),
        "vault.deleteVersion" => {
            let request = get_request(params)?;
            let response: proto::DeleteSecretVersionResponse = call(
                client,
                "secret_delete_version",
                &proto::DeleteSecretVersionRequest {
                    namespace: request.namespace,
                    key: request.key,
                    version: request.version.ok_or("missing version")?,
                },
            )?;
            Ok(json!(response.deleted))
        }
        "vault.updateDescription" => {
            let request = get_request(params)?;
            let response: proto::UpdateSecretDescriptionResponse = call(
                client,
                "secret_update_description",
                &proto::UpdateSecretDescriptionRequest {
                    namespace: request.namespace,
                    key: request.key,
                    description: required(params, "description")?.into(),
                },
            )?;
            Ok(json!(
                response.description == required(params, "description")?
            ))
        }
        _ => Err(format!("unknown service capability: {method}").into()),
    }
}

fn get_secret(client: &NexusVfsClient, params: &Value) -> Result<Value> {
    let request = get_request(params)?;
    if request.version.is_some() {
        let response: proto::GetSecretResponse = call(client, "secret_get", &request)?;
        return Ok(json!(response.value));
    }
    let key = format!("{}:{}", request.namespace, request.key);
    let response: proto::BatchGetSecretsResponse = call(
        client,
        "secret_batch_get",
        &proto::BatchGetSecretsRequest {
            queries: vec![request],
        },
    )?;
    Ok(json!(response.secrets.get(&key)))
}

fn list_secrets(client: &NexusVfsClient, params: &Value) -> Result<Value> {
    let response: proto::ListSecretsResponse = call(
        client,
        "secret_list",
        &proto::ListSecretsRequest {
            namespace: params
                .get("namespace")
                .and_then(Value::as_str)
                .map(str::to_owned),
            include_deleted: params
                .get("includeDeleted")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
    )?;
    Ok(Value::Array(
        response
            .secrets
            .into_iter()
            .map(|value| metadata(&value))
            .collect(),
    ))
}

fn batch_get_secrets(client: &NexusVfsClient, params: &Value) -> Result<Value> {
    let queries = params
        .get("queries")
        .and_then(Value::as_array)
        .ok_or("missing queries array")?
        .iter()
        .map(get_request)
        .collect::<Result<Vec<_>>>()?;
    let response: proto::BatchGetSecretsResponse = call(
        client,
        "secret_batch_get",
        &proto::BatchGetSecretsRequest { queries },
    )?;
    Ok(json!(response.secrets))
}

fn batch_put_secrets(client: &NexusVfsClient, params: &Value) -> Result<Value> {
    let secrets = params
        .get("secrets")
        .and_then(Value::as_array)
        .ok_or("missing secrets array")?
        .iter()
        .map(put_request)
        .collect::<Result<Vec<_>>>()?;
    let response: proto::BatchPutSecretsResponse = call(
        client,
        "secret_batch_put",
        &proto::BatchPutSecretsRequest { secrets },
    )?;
    Ok(Value::Array(
        response
            .results
            .into_iter()
            .map(|value| metadata(&value))
            .collect(),
    ))
}
