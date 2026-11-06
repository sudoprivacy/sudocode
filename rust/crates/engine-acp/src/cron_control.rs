//! `_sudocode/cron/*`: the cron service over ACP, so a desktop host's UI
//! manages the same jobs `scode cron`, `/cron` and the agent's tools do. The
//! host renders; the store, validation and scheduling stay here.
//!
//! Requests take `cronId` and, for create and update, a job object in the
//! stored entry's own field names, the shape `list` returns. When the store
//! changes, from this connection or any other process (a fire records its
//! run, the CLI adds a job), the client gets `_sudocode/cron/changed` with the
//! full list.

use std::time::Duration;

use agent_client_protocol::role::acp::Client;
use agent_client_protocol::{ConnectionTo, Error};
use agent_client_protocol_schema::{AgentNotification, ExtNotification};
use runtime::cron_registry::{CronCreateParams, CronCreator, CronKind, CronTarget};
use runtime::cron_service;
use serde::Deserialize;
use serde_json::{json, Value};

const PREFIX: &str = "_sudocode/cron/";
const CHANGED: &str = "_sudocode/cron/changed";
const WATCH_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) fn matches(method: &str) -> bool {
    method.starts_with(PREFIX)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ById {
    cron_id: String,
    #[serde(rename = "_meta", default)]
    _meta: Option<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Update {
    cron_id: String,
    changes: cron_service::CronUpdate,
    #[serde(rename = "_meta", default)]
    _meta: Option<serde::de::IgnoredAny>,
}

/// A new job in the stored entry's field names.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewJob {
    schedule: String,
    #[serde(default)]
    kind: CronKind,
    prompt: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    tz: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    target: CronTarget,
    #[serde(default)]
    delivery: Option<runtime::turn_input::InputDelivery>,
    #[serde(default)]
    created_by: CronCreator,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Create {
    cron: NewJob,
    #[serde(rename = "_meta", default)]
    _meta: Option<serde::de::IgnoredAny>,
}

fn parse<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, Error> {
    serde_json::from_value(params).map_err(|error| Error::invalid_params().data(error.to_string()))
}

fn refused(error: String) -> Error {
    Error::invalid_params().data(error)
}

fn list() -> Value {
    json!({ "crons": cron_service::list() })
}

/// Serve one `_sudocode/cron/*` request.
pub(crate) fn call(method: &str, params: Value) -> Result<Value, Error> {
    match method.strip_prefix(PREFIX).unwrap_or_default() {
        "list" => Ok(list()),
        "get" => {
            let ById { cron_id, .. } = parse(params)?;
            Ok(json!({ "cron": cron_service::get(&cron_id) }))
        }
        "create" => {
            let Create { cron: job, .. } = parse(params)?;
            let entry = cron_service::create(CronCreateParams {
                schedule: job.schedule,
                kind: job.kind,
                prompt: job.prompt,
                description: job.description,
                name: job.name,
                tz: job.tz,
                cwd: job.cwd,
                target: job.target,
                delivery: job.delivery,
                created_by: job.created_by,
            })
            .map_err(refused)?;
            Ok(json!({ "cron": entry }))
        }
        "update" => {
            let Update {
                cron_id, changes, ..
            } = parse(params)?;
            let entry = cron_service::update(&cron_id, changes).map_err(refused)?;
            Ok(json!({ "cron": entry }))
        }
        "remove" => {
            let ById { cron_id, .. } = parse(params)?;
            let entry = cron_service::remove(&cron_id).map_err(refused)?;
            Ok(json!({ "cron": entry }))
        }
        "trigger" => {
            let ById { cron_id, .. } = parse(params)?;
            let status = runtime::cron_scheduler::trigger_here(&cron_id).map_err(refused)?;
            Ok(json!({ "status": status, "cron": cron_service::get(&cron_id) }))
        }
        _ => Err(Error::method_not_found()),
    }
}

/// Send `_sudocode/cron/changed` whenever the store's contents change, for as
/// long as the connection accepts it.
pub(crate) async fn watch(cx: ConnectionTo<Client>) {
    let mut last = list();
    loop {
        tokio::time::sleep(WATCH_INTERVAL).await;
        let current = list();
        if current == last {
            continue;
        }
        let Ok(payload) = serde_json::value::to_raw_value(&current) else {
            continue;
        };
        let notification =
            AgentNotification::ExtNotification(ExtNotification::new(CHANGED, payload.into()));
        if cx.send_notification(notification).is_err() {
            return;
        }
        last = current;
    }
}
