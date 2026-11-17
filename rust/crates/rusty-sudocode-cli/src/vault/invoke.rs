//! `vault_use`: checks a request against its configured operation, then
//! refuses, because the backend has no per-agent credential-use authority yet
//! (nexi-lab/nexus-vfs#579). It reads no credential and opens no connection.

use std::collections::BTreeMap;
use std::path::Path;

use regex::Regex;
use serde::Deserialize;
use serde_json::{Map, Value};

use super::settings;
use super::Refusal;

const UNAVAILABLE: &str = "vault_use is unavailable: the backend has no per-agent \
                           credential-use authority yet \
                           (https://github.com/nexi-lab/nexus-vfs/issues/579)";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UseInput {
    operation: String,
    #[serde(default)]
    params: BTreeMap<String, String>,
    #[serde(default)]
    body: BTreeMap<String, String>,
}

/// Always a refusal: the first problem with the request, else `use_unavailable`.
pub(super) fn run(cwd: &Path, input: Result<Map<String, Value>, Refusal>) -> Refusal {
    admit(cwd, input)
        .err()
        .unwrap_or_else(|| Refusal::new("use_unavailable", UNAVAILABLE))
}

fn admit(cwd: &Path, input: Result<Map<String, Value>, Refusal>) -> Result<(), Refusal> {
    let settings = settings::load(cwd)?;
    let input: UseInput = serde_json::from_value(Value::Object(input?))
        .map_err(|error| Refusal::invalid_input(format!("vault_use input: {error}")))?;
    let operation = settings
        .operations
        .iter()
        .find(|operation| operation.id == input.operation)
        .ok_or_else(|| {
            Refusal::new(
                "unknown_operation",
                format!("no Vault operation `{}` is configured", input.operation),
            )
        })?;
    exactly(&operation.id, "params", &operation.params, &input.params)?;
    exactly(&operation.id, "body", &operation.body, &input.body)
}

/// `given` names exactly the `declared` fields, each matching its pattern.
fn exactly(
    id: &str,
    field: &str,
    declared: &BTreeMap<String, Regex>,
    given: &BTreeMap<String, String>,
) -> Result<(), Refusal> {
    if let Some(name) = given.keys().find(|name| !declared.contains_key(*name)) {
        return Err(Refusal::invalid_input(format!(
            "operation `{id}` declares no {field} `{name}`"
        )));
    }
    for (name, pattern) in declared {
        let Some(value) = given.get(name) else {
            return Err(Refusal::invalid_input(format!(
                "operation `{id}` requires {field} `{name}`"
            )));
        };
        if !pattern.is_match(value) {
            return Err(Refusal::invalid_input(format!(
                "operation `{id}` {field} `{name}` does not match its declared pattern"
            )));
        }
    }
    Ok(())
}
