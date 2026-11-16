//! The `vault` settings section. It is read only from user settings, parsed
//! strictly (unknown keys anywhere are refused) and validated rule by rule, so
//! each refusal names the file, the operation and the field at fault.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use lib::transport_primitives::user_runtime_server_name;
use regex::Regex;
use reqwest::Url;
use runtime::{ConfigLoader, ConfigSource};
use serde::Deserialize;
use serde_json::Value;

use super::Refusal;

const HTTP_METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];
const RESERVED_HEADERS: [&str; 4] = ["host", "content-length", "transfer-encoding", "connection"];
const CREDENTIAL_RESPONSE_HEADERS: [&str; 4] = [
    "set-cookie",
    "authorization",
    "proxy-authorization",
    "cookie",
];
const INTERPRETERS: [&str; 15] = [
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "fish",
    "cmd",
    "powershell",
    "pwsh",
    "python",
    "python3",
    "perl",
    "ruby",
    "node",
    "env",
];
const RESERVED_ENV: [&str; 6] = [
    "PATH",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "HOME",
];
/// Upper bound of any declared output size.
const MAX_OUTPUT_BYTES: u64 = 65_536;

/// Validated settings, keeping only what the tools act on.
pub(super) struct VaultSettings {
    pub(super) endpoint: String,
    pub(super) owner: String,
    pub(super) credential_dir: PathBuf,
    pub(super) operations: Vec<Operation>,
}

pub(super) struct Operation {
    pub(super) id: String,
    pub(super) kind: &'static str,
    pub(super) secret_ref: SecretRef,
    /// `METHOD origin+path` for http, the executable for exec.
    pub(super) target: String,
    /// The exact params (exec) and body fields (http) a use must supply.
    pub(super) params: BTreeMap<String, Regex>,
    pub(super) body: BTreeMap<String, Regex>,
}

/// One exact secret: `namespace/key`.
pub(super) struct SecretRef {
    pub(super) namespace: String,
    pub(super) key: String,
}

impl fmt::Display for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.namespace, self.key)
    }
}

pub(super) enum SettingsError {
    NotConfigured { settings: PathBuf },
    ProjectConfigRefused { path: PathBuf },
    Invalid(String),
}

impl From<SettingsError> for Refusal {
    fn from(error: SettingsError) -> Self {
        match error {
            SettingsError::NotConfigured { settings } => Refusal::new(
                "not_configured",
                format!(
                    "Vault is not configured: add a `vault` section to user settings ({})",
                    settings.display()
                ),
            ),
            SettingsError::ProjectConfigRefused { path } => Refusal::new(
                "project_config_refused",
                format!(
                    "`vault` is read only from user settings; remove it from {}",
                    path.display()
                ),
            ),
            SettingsError::Invalid(message) => Refusal::new("invalid_settings", message),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawVault {
    endpoint: String,
    owner: String,
    credential_dir: String,
    operations: Vec<RawOperation>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum RawOperation {
    Http(RawHttp),
    Exec(RawExec),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHttp {
    id: String,
    #[serde(rename = "ref")]
    secret_ref: String,
    method: String,
    origin: String,
    path: String,
    header: String,
    scheme: Option<String>,
    redirect: String,
    body: Option<BTreeMap<String, String>>,
    response: RawResponse,
}

/// What an http operation may return: named headers and JSON fields, never a body.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawResponse {
    headers: Vec<String>,
    json_fields: Vec<String>,
    max_bytes: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawExec {
    id: String,
    #[serde(rename = "ref")]
    secret_ref: String,
    executable: String,
    args: Vec<String>,
    params: BTreeMap<String, String>,
    cwd: String,
    env: String,
    pass_env: Vec<String>,
    output: Option<Value>,
}

/// The kind-specific part of a validated operation.
struct Shape {
    kind: &'static str,
    target: String,
    params: BTreeMap<String, Regex>,
    body: BTreeMap<String, Regex>,
}

/// Load the `vault` section for `cwd` from the settings files scode discovers
/// there. A project or local file that carries one is refused by path; between
/// user files, the later one wins, as in the merged configuration. Files are
/// read directly rather than through `ConfigLoader::load`, which reports
/// warnings on stderr.
pub(super) fn load(cwd: &Path) -> Result<VaultSettings, SettingsError> {
    let loader = ConfigLoader::default_for(cwd);
    let entries = loader.discover();
    let user_files: Vec<&Path> = entries
        .iter()
        .filter(|entry| entry.source == ConfigSource::User)
        .map(|entry| entry.path.as_path())
        .collect();
    for entry in &entries {
        if !user_files.contains(&entry.path.as_path()) && vault_section(&entry.path)?.is_some() {
            return Err(SettingsError::ProjectConfigRefused {
                path: entry.path.clone(),
            });
        }
    }
    let mut section = None;
    for path in user_files {
        if let Some(value) = vault_section(path)? {
            section = Some((path, value));
        }
    }
    let Some((path, value)) = section else {
        return Err(SettingsError::NotConfigured {
            settings: loader.config_home().join("settings.json"),
        });
    };
    serde_json::from_value(value)
        .map_err(|error| error.to_string())
        .and_then(validate)
        .map_err(|message| SettingsError::Invalid(format!("{}: vault: {message}", path.display())))
}

fn vault_section(path: &Path) -> Result<Option<Value>, SettingsError> {
    let unreadable = |error: String| SettingsError::Invalid(format!("{}: {error}", path.display()));
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unreadable(error.to_string())),
    };
    let text = text.trim_start_matches('\u{feff}');
    if text.trim().is_empty() {
        return Ok(None);
    }
    match serde_json::from_str::<Value>(text).map_err(|error| unreadable(error.to_string()))? {
        Value::Object(mut root) => Ok(root.remove("vault")),
        _ => Ok(None),
    }
}

fn validate(raw: RawVault) -> Result<VaultSettings, String> {
    let endpoint = endpoint(raw.endpoint)?;
    let owner = owner(raw.owner)?;
    let credential_dir = credential_dir(raw.credential_dir)?;
    let mut seen = BTreeSet::new();
    let operations = raw
        .operations
        .iter()
        .map(|operation| validate_operation(operation, &mut seen))
        .collect::<Result<_, _>>()?;
    Ok(VaultSettings {
        endpoint,
        owner,
        credential_dir,
        operations,
    })
}

fn endpoint(raw: String) -> Result<String, String> {
    match Url::parse(&raw) {
        Ok(url)
            if url.scheme() == "https" && url.host_str().is_some_and(|host| !host.is_empty()) =>
        {
            Ok(raw)
        }
        _ => Err("endpoint must be an https URL with a host".into()),
    }
}

fn owner(raw: String) -> Result<String, String> {
    if raw.is_empty() {
        return Err("owner must not be empty".into());
    }
    user_runtime_server_name(&raw).map_err(|error| format!("owner is not accepted: {error}"))?;
    Ok(raw)
}

fn credential_dir(raw: String) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err("credentialDir must be an absolute path".into())
    }
}

fn validate_operation(
    raw: &RawOperation,
    seen: &mut BTreeSet<String>,
) -> Result<Operation, String> {
    let (id, secret_ref) = match raw {
        RawOperation::Http(http) => (&http.id, &http.secret_ref),
        RawOperation::Exec(exec) => (&exec.id, &exec.secret_ref),
    };
    let id = operation_id(id, seen)?;
    let checked = parse_secret_ref(secret_ref).and_then(|secret_ref| {
        let shape = match raw {
            RawOperation::Http(http) => http_shape(http),
            RawOperation::Exec(exec) => exec_shape(exec),
        }?;
        Ok((secret_ref, shape))
    });
    let (secret_ref, shape) = checked.map_err(|message| format!("operation `{id}`: {message}"))?;
    Ok(Operation {
        id,
        kind: shape.kind,
        secret_ref,
        target: shape.target,
        params: shape.params,
        body: shape.body,
    })
}

fn operation_id(id: &str, seen: &mut BTreeSet<String>) -> Result<String, String> {
    let mut chars = id.chars();
    let is_valid = id.len() <= 64
        && chars
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !is_valid {
        return Err(format!(
            "operation id `{id}` must match ^[a-z0-9][a-z0-9_-]{{0,63}}$"
        ));
    }
    if !seen.insert(id.to_owned()) {
        return Err(format!("operation id `{id}` is not unique"));
    }
    Ok(id.to_owned())
}

fn parse_secret_ref(raw: &str) -> Result<SecretRef, String> {
    match raw.split_once('/') {
        Some((namespace, key))
            if !namespace.is_empty() && !key.is_empty() && !key.contains('/') =>
        {
            Ok(SecretRef {
                namespace: namespace.to_owned(),
                key: key.to_owned(),
            })
        }
        _ => Err("ref must be exactly `namespace/key`".into()),
    }
}

fn http_shape(raw: &RawHttp) -> Result<Shape, String> {
    origin(&raw.origin)?;
    if !HTTP_METHODS.contains(&raw.method.as_str()) {
        return Err(format!("method must be one of {}", HTTP_METHODS.join(", ")));
    }
    if !raw.path.starts_with('/') || raw.path.contains("..") || raw.path.contains(['?', '#']) {
        return Err("path must start with `/` and contain no `..`, `?` or `#`".into());
    }
    if !is_token(&raw.header)
        || RESERVED_HEADERS.contains(&raw.header.to_ascii_lowercase().as_str())
    {
        return Err(
            "header must be an HTTP token other than Host, Content-Length, Transfer-Encoding \
             or Connection"
                .into(),
        );
    }
    if raw
        .scheme
        .as_deref()
        .is_some_and(|scheme| !is_token(scheme))
    {
        return Err("scheme must be an HTTP token".into());
    }
    if raw.redirect != "deny" {
        return Err("redirect must be \"deny\"".into());
    }
    response(&raw.response)?;
    Ok(Shape {
        kind: "http",
        target: format!("{} {}{}", raw.method, raw.origin, raw.path),
        params: BTreeMap::new(),
        body: patterns("body", raw.body.iter().flatten())?,
    })
}

/// Scheme, host and optional port only: https, or http to a loopback host.
fn origin(raw: &str) -> Result<(), String> {
    let is_allowed = Url::parse(raw).is_ok_and(|url| {
        let is_bare = url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && !raw.ends_with('/')
            && url.query().is_none()
            && url.fragment().is_none();
        let host = url.host_str().unwrap_or_default();
        let is_secure = match url.scheme() {
            "https" => !host.is_empty(),
            "http" => is_loopback(host),
            _ => false,
        };
        is_bare && is_secure
    });
    if is_allowed {
        Ok(())
    } else {
        Err(
            "origin must be scheme://host[:port] using https, or http only to a loopback host"
                .into(),
        )
    }
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// An RFC 7230 `token`.
fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

fn response(raw: &RawResponse) -> Result<(), String> {
    if let Some(header) = raw.headers.iter().find(|header| {
        !is_token(header)
            || CREDENTIAL_RESPONSE_HEADERS.contains(&header.to_ascii_lowercase().as_str())
    }) {
        return Err(format!(
            "response.headers `{header}` must be an HTTP token other than Set-Cookie, \
             Authorization, Proxy-Authorization or Cookie"
        ));
    }
    if let Some(field) = raw.json_fields.iter().find(|field| !is_json_pointer(field)) {
        return Err(format!(
            "response.jsonFields `{field}` must be a JSON Pointer (RFC 6901) starting with `/`"
        ));
    }
    raw.max_bytes.as_ref().map_or(Ok(()), |max_bytes| {
        byte_limit("response.maxBytes", max_bytes)
    })
}

/// An RFC 6901 pointer below the root: `/`-separated tokens in which `~` only
/// begins the escapes `~0` and `~1`.
fn is_json_pointer(pointer: &str) -> bool {
    pointer.starts_with('/')
        && pointer
            .split('~')
            .skip(1)
            .all(|escaped| escaped.starts_with(['0', '1']))
}

fn byte_limit(field: &str, value: &Value) -> Result<(), String> {
    match value.as_u64() {
        Some(1..=MAX_OUTPUT_BYTES) => Ok(()),
        _ => Err(format!(
            "{field} must be an integer from 1 to {MAX_OUTPUT_BYTES}"
        )),
    }
}

fn exec_shape(raw: &RawExec) -> Result<Shape, String> {
    executable(&raw.executable)?;
    let params = patterns("params", &raw.params)?;
    placeholders(&raw.args, &params)?;
    let cwd = Path::new(&raw.cwd);
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err("cwd must be an absolute path to an existing directory".into());
    }
    env_name("env", &raw.env)?;
    for name in &raw.pass_env {
        env_name("passEnv", name)?;
        if *name == raw.env {
            return Err(format!(
                "passEnv must not include the secret's env `{name}`"
            ));
        }
    }
    raw.output.as_ref().map_or(Ok(()), output)?;
    Ok(Shape {
        kind: "exec",
        target: raw.executable.clone(),
        params,
        body: BTreeMap::new(),
    })
}

fn executable(raw: &str) -> Result<(), String> {
    let path = Path::new(raw);
    if !path.is_absolute() || !path.is_file() {
        return Err("executable must be an absolute path to an existing regular file".into());
    }
    let stem = path.file_stem().map_or_else(String::new, |stem| {
        stem.to_string_lossy().to_ascii_lowercase()
    });
    if INTERPRETERS.contains(&stem.as_str()) {
        return Err(format!(
            "executable must not be a shell or interpreter (`{stem}`)"
        ));
    }
    Ok(())
}

/// Each arg is a literal without braces or exactly `{param}`; every declared
/// param is used.
fn placeholders(args: &[String], params: &BTreeMap<String, Regex>) -> Result<(), String> {
    let mut used = BTreeSet::new();
    for arg in args {
        if !arg.contains(['{', '}']) {
            continue;
        }
        let name = arg
            .strip_prefix('{')
            .and_then(|rest| rest.strip_suffix('}'))
            .filter(|name| !name.contains(['{', '}']))
            .ok_or("args must be literals without `{` or `}`, or exactly `{param}`")?;
        if !params.contains_key(name) {
            return Err(format!("args use undeclared param `{name}`"));
        }
        used.insert(name);
    }
    match params.keys().find(|name| !used.contains(name.as_str())) {
        Some(name) => Err(format!("params `{name}` is declared but not used in args")),
        None => Ok(()),
    }
}

fn env_name(field: &str, name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let is_valid = name.len() <= 64
        && chars
            .next()
            .is_some_and(|first| first.is_ascii_uppercase() || first == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if !is_valid {
        return Err(format!(
            "{field} `{name}` must match ^[A-Z_][A-Z0-9_]{{0,63}}$"
        ));
    }
    if RESERVED_ENV.contains(&name) {
        return Err(format!("{field} `{name}` is reserved"));
    }
    Ok(())
}

/// `"exit-code"`, the default, or `{"stdout": {"maxBytes": N}}`.
fn output(value: &Value) -> Result<(), String> {
    if value.as_str() == Some("exit-code") {
        return Ok(());
    }
    let max_bytes = value
        .as_object()
        .filter(|output| output.len() == 1)
        .and_then(|output| output.get("stdout"))
        .and_then(Value::as_object)
        .filter(|stdout| stdout.len() == 1)
        .and_then(|stdout| stdout.get("maxBytes"))
        .ok_or("output must be \"exit-code\" or {\"stdout\": {\"maxBytes\": N}}")?;
    byte_limit("output.stdout.maxBytes", max_bytes)
}

fn patterns<'a>(
    field: &str,
    raw: impl IntoIterator<Item = (&'a String, &'a String)>,
) -> Result<BTreeMap<String, Regex>, String> {
    raw.into_iter()
        .map(|(name, pattern)| {
            anchored(pattern)
                .map(|regex| (name.clone(), regex))
                .map_err(|message| format!("{field} `{name}`: {message}"))
        })
        .collect()
}

/// A pattern written as `^...$`, compiled to match a whole value even when it
/// alternates between its anchors.
fn anchored(pattern: &str) -> Result<Regex, String> {
    if !pattern.starts_with('^') || !pattern.ends_with('$') {
        return Err("pattern must start with `^` and end with `$`".into());
    }
    Regex::new(pattern)
        .and_then(|_| Regex::new(&format!("^(?:{pattern})$")))
        .map_err(|error| format!("pattern does not compile: {error}"))
}
