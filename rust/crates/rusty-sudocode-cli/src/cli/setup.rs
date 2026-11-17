//! Runtime choice for a fresh CLI, sharing the existing frontend service owner.

use std::error::Error;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use runtime::nexus_mailbox::{credential_present, Config, NodeRecord};
use runtime::{FsBackend, StdFsBackend};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use service_host::AgentEnrollment;

use super::args::CliOutputFormat;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeMode {
    Local,
    Shared,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preference {
    #[serde(rename = "schemaVersion")]
    version: u8,
    mode: RuntimeMode,
    #[serde(rename = "ownerRoot", default, skip_serializing_if = "Option::is_none")]
    owner_root: Option<PathBuf>,
}

fn path() -> PathBuf {
    runtime::config::default_config_home().join("runtime-profile.json")
}

fn preference() -> Result<Option<Preference>> {
    let bytes = match std::fs::read(path()) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let preference: Preference = serde_json::from_slice(&bytes)?;
    if preference.version != 1 {
        return Err("Unsupported runtime profile version".into());
    }
    Ok(Some(preference))
}

fn choose() -> Result<Option<RuntimeMode>> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("Noninteractive setup requires --mode local or --mode shared".into());
    }
    let choice = dialoguer::Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt("Choose how to run scode")
        .items(&[
            "Single agent (skip Nexus setup)",
            "Multi-agent (reuse configured Box; otherwise prepare local Nexus)",
            "Decide later",
        ])
        .default(0)
        .interact_opt()?;
    Ok(match choice {
        Some(0) => Some(RuntimeMode::Local),
        Some(1) => Some(RuntimeMode::Shared),
        _ => None,
    })
}

fn activate(mode: RuntimeMode, owner_root: Option<&Path>) -> Result<Value> {
    if mode == RuntimeMode::Shared {
        if let Some(record) = NodeRecord::load()? {
            let service = service_host::ensure_local_box(record.endpoint(), owner_root)
                .map_err(|error| error as Box<dyn Error>)?;
            return Ok(json!({"mode": mode, "service": service,
                "box": {"source": "node_record", "endpoint": record.endpoint()},
                "domainJoinAccepted": false}));
        }
        if owner_root.is_some() {
            return Err(
                "A selected local owner requires the original configured Box record".into(),
            );
        }
        let service =
            service_host::start_local_services().map_err(|error| error as Box<dyn Error>)?;
        return Ok(json!({"mode": mode, "service": service, "box": null,
            "domainJoinAccepted": false}));
    }
    Ok(json!({"mode": mode, "service": null, "box": null, "domainJoinAccepted": false}))
}

pub(crate) fn selected_owner_root() -> Result<Option<PathBuf>> {
    Ok(preference()?.and_then(|preference| preference.owner_root))
}

/// The workspace a session started here would use, spelled as sessions spell it.
fn workspace() -> Result<PathBuf> {
    Ok(std::fs::canonicalize(std::env::current_dir()?)?)
}

fn is_home(path: &Path) -> bool {
    runtime::user_paths::home_dir()
        .and_then(|home| std::fs::canonicalize(home).ok())
        .is_some_and(|home| home == path)
}

/// The actual credential state of `agent` on the configured node, with the
/// recovery text a missing credential carries.
fn agent_status(record: &NodeRecord, agent: &str) -> Value {
    match Config::from_node_record(agent) {
        Ok(Some(_)) => json!({"name": agent, "credential": "ready"}),
        Ok(None) => json!({"name": agent, "credential": "unavailable"}),
        Err(error) if !credential_present(record.agents_dir(), agent) => {
            json!({"name": agent, "credential": "missing", "recovery": error})
        }
        Err(error) => json!({"name": agent, "credential": "invalid", "error": error}),
    }
}

fn enroll(
    record: &NodeRecord,
    agent: &str,
    owner_root: Option<&Path>,
) -> std::result::Result<AgentEnrollment, String> {
    service_host::enroll_local_agent(agent, record.endpoint(), record.agents_dir(), owner_root)
        .map_err(|error| error.to_string())
}

/// The [`engine_host::peer::Enroller`] for a user who chose shared mode: issue a
/// session's own Agent through this user's local Box.
fn enroll_own_agent(agent: &str) -> std::result::Result<(), String> {
    let record =
        NodeRecord::load()?.ok_or_else(|| "this machine has no node record".to_string())?;
    let owner_root = selected_owner_root().map_err(|error| error.to_string())?;
    enroll(&record, agent, owner_root.as_deref()).map(|_| ())
}

pub(crate) fn run(
    mode: Option<RuntimeMode>,
    owner_root: Option<PathBuf>,
    check: bool,
    format: CliOutputFormat,
) -> Result<()> {
    let previous = preference()?;
    let owner_root = owner_root.or_else(|| {
        previous
            .as_ref()
            .and_then(|preference| preference.owner_root.clone())
    });
    if owner_root.as_ref().is_some_and(|root| !root.is_absolute()) {
        return Err("Local Box owner root must be absolute".into());
    }
    let output = if check {
        let record = NodeRecord::load()?;
        let agent = match &record {
            Some(record) => {
                let name = engine_host::peek_agent_name_under(&workspace()?);
                Some(agent_status(record, &name))
            }
            None => None,
        };
        let configured_box = record.as_ref().map(|record| {
            json!({"source": "node_record", "endpoint": record.endpoint(),
                "agentAuthentication": agent.as_ref().map(|agent| agent["credential"].clone())})
        });
        json!({"mode": previous.map(|preference| preference.mode), "ownerRoot": owner_root,
            "box": configured_box, "agent": agent,
            "services": service_host::inspect_local_services_at(owner_root.as_deref()).map_err(|error| error as Box<dyn Error>)?,
            "domainJoinAccepted": false})
    } else {
        let selected = match mode {
            Some(mode) => Some(mode),
            None => choose()?,
        };
        let Some(mode) = selected else {
            return Ok(());
        };
        let mut output = activate(mode, owner_root.as_deref())?;
        output["ownerRoot"] = json!(owner_root);
        if mode == RuntimeMode::Shared {
            if let Some(record) = NodeRecord::load()? {
                let cwd = workspace()?;
                // Not persisted here: the session derives and saves the same name.
                let name = engine_host::peek_agent_name_under(&cwd);
                let derived = runtime::mailbox::local_agent_name(None, &cwd);
                let joined = if is_home(&cwd) {
                    Err(
                        "the home directory's settings apply to every workspace without its \
                         own, so its Agent is not joined; run `scode setup --mode shared` in \
                         a workspace"
                            .to_string(),
                    )
                } else if name != derived {
                    // A configured name may come from the repository or be inherited
                    // from the user's settings by every unconfigured workspace.
                    let source = engine_host::agent_name_source(&cwd).map_or_else(
                        || "its settings".to_string(),
                        |path| path.display().to_string(),
                    );
                    Err(format!(
                        "this workspace answers to '{name}' from {source}, not from its own \
                         path, and scode does not claim a configured name. Ask the node owner \
                         to mint '{name}', or remove `agentName` from {source} so this \
                         workspace joins as '{derived}'"
                    ))
                } else {
                    enroll(&record, &name, owner_root.as_deref())
                };
                let mut agent = agent_status(&record, &name);
                match joined {
                    Ok(outcome) => agent["joined"] = json!(outcome == AgentEnrollment::Joined),
                    Err(error) => agent["joinError"] = json!(error),
                }
                if output["box"].is_object() {
                    output["box"]["agentAuthentication"] = agent["credential"].clone();
                }
                output["agent"] = agent;
            }
        }
        let path = path();
        std::fs::create_dir_all(path.parent().ok_or("missing configuration directory")?)?;
        StdFsBackend.write_atomic(
            &path.to_string_lossy(),
            &serde_json::to_vec(&Preference {
                version: 1,
                mode,
                owner_root,
            })?,
        )?;
        output
    };
    if format == CliOutputFormat::Json {
        println!("{}", serde_json::to_string(&output)?);
    } else if check {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Runtime preference saved.");
        if let Some(endpoint) = output["box"]["endpoint"].as_str() {
            println!("Using configured Box at {endpoint}.");
            let agent = &output["agent"];
            let name = agent["name"].as_str().unwrap_or_default();
            match (agent["credential"].as_str(), agent["joinError"].as_str()) {
                (Some("ready"), _) if agent["joined"] == true => {
                    println!("Joined A2A as '{name}'.");
                }
                (Some("ready"), _) => println!("A2A agent '{name}' is ready."),
                (Some("invalid"), None) => println!(
                    "A2A agent '{name}' has an unusable credential: {}",
                    agent["error"].as_str().unwrap_or_default()
                ),
                (_, Some(error)) => {
                    println!("A2A agent '{name}' is not joined: {error}");
                    if let Some(recovery) = agent["recovery"].as_str() {
                        println!("{recovery}.");
                    }
                }
                _ => {}
            }
        } else if output["mode"] == "shared" {
            println!("Local services are ready. Collaboration enrollment is a separate step.");
        }
    }
    Ok(())
}

/// Honor an existing runtime choice without reading input or writing protocol
/// output. CLI and ACP share the same service owner and preparation policy.
/// A missing preference never grants permission to provision services.
pub(crate) fn prepare_selected_runtime() -> Result<bool> {
    if let Some(preference) = preference()? {
        if preference.mode == RuntimeMode::Shared {
            engine_host::peer::set_enroller(enroll_own_agent);
            let configured = runtime::nexus_mailbox::node_record_present();
            if let Err(error) = activate(preference.mode, preference.owner_root.as_deref()) {
                if !configured {
                    return Err(error);
                }
                // The mailbox keeps this selected namespace and its actual
                // error. Automatic preparation must not block ordinary work;
                // explicit `setup`/`--check` still report their failures.
                telemetry::RetryDiagnostics::new(
                    "nexus-setup",
                    "configured Box",
                    "preparation unavailable",
                    "ready",
                )
                .with_notices(telemetry::RetryNotices::LogOnly)
                .failed("automatic_shared_setup", &error.to_string());
            }
        }
        return Ok(true);
    }
    Ok(false)
}

/// Existing installations retain their current behavior until explicit setup.
/// Interactive new and resumed sessions share the same first-run choice.
/// Only an interactive fresh configuration offers the runtime-choice picker.
pub(crate) fn startup_prompt() -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(());
    }
    if !prepare_selected_runtime()? && !runtime::config::default_config_home().exists() {
        run(None, None, false, CliOutputFormat::Text)?;
    }
    Ok(())
}
