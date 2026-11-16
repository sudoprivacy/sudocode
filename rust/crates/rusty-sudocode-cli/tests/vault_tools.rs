//! The builtin Vault tools through the real `scode` binary: settings refusals,
//! input checks, the unconditional `vault_use` refusal, and credential checks
//! on bundles with nexus's agent certificate profile. Every run must leave
//! stderr empty and print exactly one JSON object.

use std::fs;
use std::io::ErrorKind;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use lib::transport_primitives::{user_runtime_server_name, AgentCredential};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use vault_tool::ToolRun;

#[path = "support/vault_tool.rs"]
mod vault_tool;

const OWNER: &str = "vault-test-owner";
const UNAVAILABLE: &str = "vault_use is unavailable: the backend has no per-agent \
                           credential-use authority yet \
                           (https://github.com/nexi-lab/nexus-vfs/issues/579)";
const NOTE: &str = "operations are configured locally, not authorized by the backend; \
                    per-agent grants: https://github.com/nexi-lab/nexus-vfs/issues/579";
const DEPLOY: &str = r#"{"operation":"deploy-api","body":{"service":"billing"}}"#;
const MIGRATE: &str = r#"{"operation":"db-migrate","params":{"target":"orders_v2"}}"#;

/// One isolated scode environment: home, config home, project, tool and
/// credential directories under one temporary root.
struct Env {
    root: TempDir,
}

impl Env {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temporary root");
        for dir in ["home", "config", "project", "tools"] {
            fs::create_dir_all(root.path().join(dir)).expect("fixture directory");
        }
        fs::write(root.path().join("tools").join("db-tool"), b"tool").expect("executable");
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn credentials(&self) -> PathBuf {
        self.path("credentials")
    }

    /// A valid section with one http and one exec operation.
    fn vault(&self, endpoint: &str) -> Value {
        json!({
            "endpoint": endpoint,
            "owner": OWNER,
            "credentialDir": self.credentials(),
            "operations": [
                {
                    "id": "deploy-api", "kind": "http", "ref": "deploy/api-token",
                    "method": "POST", "origin": "https://127.0.0.1:8443", "path": "/v1/exact",
                    "header": "Authorization", "scheme": "Bearer", "redirect": "deny",
                    "body": {"service": "^[a-z]{1,16}$"},
                    "response": {
                        "headers": ["X-Request-Id"],
                        "jsonFields": ["/status", "/data/id"],
                        "maxBytes": 4096
                    }
                },
                {
                    "id": "db-migrate", "kind": "exec", "ref": "db/migrator",
                    "executable": self.path("tools").join("db-tool"),
                    "args": ["--fixed", "{target}"],
                    "params": {"target": "^[A-Za-z0-9_.-]{1,64}$"},
                    "cwd": self.path("project"), "env": "DB_PASSWORD", "passEnv": ["LANG"]
                }
            ]
        })
    }

    fn write_user_vault(&self, vault: &Value) {
        write_json(
            &self.path("config").join("settings.json"),
            &json!({ "vault": vault }),
        );
    }

    /// Run `scode vault tool <tool>` from the project with `input` on stdin.
    fn run(&self, tool: &str, input: &str) -> ToolRun {
        vault_tool::run(
            &self.path("project"),
            &self.path("home"),
            &self.path("config"),
            tool,
            input,
        )
    }

    fn list_availability(&self) -> Vec<String> {
        availability(&self.run("list", "{}").result)
    }
}

/// CA, certificate and key PEMs of one credential bundle.
struct Bundle {
    ca: Vec<u8>,
    cert: Vec<u8>,
    key: Vec<u8>,
}

/// A session agent credential for `owner`, valid throughout this century.
fn mint(owner: &str) -> Bundle {
    mint_between(owner, 2000, 2099)
}

/// A zone CA and a session agent certificate for `owner` that it signed, valid
/// from `from_year` to `to_year`, with the agent and owner identity SANs nexus
/// issues. Built with rcgen: nexus's certgen crate needs `protoc`, so it is a
/// Linux-only dev-dependency, and these cases run on every platform.
fn mint_between(owner: &str, from_year: i32, to_year: i32) -> Bundle {
    let ca_key = KeyPair::generate().expect("CA key");
    let mut ca = CertificateParams::default();
    ca.distinguished_name
        .push(DnType::CommonName, "vault-test zone CA");
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_pem = ca.self_signed(&ca_key).expect("CA certificate").pem();
    let issuer = Issuer::from_ca_cert_pem(&ca_pem, ca_key).expect("issuer");
    let key = KeyPair::generate().expect("agent key");
    let mut params = CertificateParams::default();
    params.subject_alt_names = vec![
        SanType::URI(
            lib::agent_identity::agent_identity_uri("vault-agent")
                .try_into()
                .expect("agent URI"),
        ),
        SanType::URI(
            lib::agent_identity::owner_uri(owner)
                .try_into()
                .expect("owner URI"),
        ),
    ];
    params.not_before = rcgen::date_time_ymd(from_year, 1, 1);
    params.not_after = rcgen::date_time_ymd(to_year, 1, 1);
    let cert = params.signed_by(&key, &issuer).expect("certificate");
    Bundle {
        ca: ca_pem.into_bytes(),
        cert: cert.pem().into_bytes(),
        key: key.serialize_pem().into_bytes(),
    }
}

/// Write `bundle` with the mint's own manifest layout, naming `server_name`.
fn write_bundle(dir: &Path, bundle: &Bundle, server_name: &str) {
    fs::create_dir_all(dir).expect("bundle directory");
    let mut manifest = AgentCredential::for_bundle("vault-agent");
    server_name.clone_into(&mut manifest.server_name);
    fs::write(dir.join(&manifest.ca), &bundle.ca).expect("CA");
    fs::write(dir.join(&manifest.cert), &bundle.cert).expect("certificate");
    fs::write(dir.join(&manifest.key), &bundle.key).expect("key");
    manifest.write_to(dir).expect("manifest");
}

fn runtime_name() -> String {
    user_runtime_server_name(OWNER).expect("runtime server name")
}

fn write_json(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().expect("parent")).expect("settings directory");
    fs::write(path, value.to_string()).expect("settings file");
}

/// A listener the test owns, as an https endpoint.
fn listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let endpoint = format!("https://{}", listener.local_addr().expect("address"));
    (listener, endpoint)
}

fn assert_never_contacted(listener: &TcpListener) {
    match listener.accept() {
        Err(error) if error.kind() == ErrorKind::WouldBlock => {}
        other => panic!("the endpoint was contacted: {other:?}"),
    }
}

/// An https endpoint on a port nothing listens on.
fn closed_endpoint() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    drop(listener);
    format!("https://{address}")
}

fn code(result: &Value) -> &str {
    result["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a refusal: {result}"))
}

fn message(result: &Value) -> &str {
    result["error"]["message"].as_str().expect("message")
}

fn availability(result: &Value) -> Vec<String> {
    result["operations"]
        .as_array()
        .unwrap_or_else(|| panic!("expected operations: {result}"))
        .iter()
        .map(|operation| {
            operation["availability"]
                .as_str()
                .expect("availability")
                .to_owned()
        })
        .collect()
}

/// Set `pointer` in `target`, adding an object member or replacing an array item.
fn set(target: &mut Value, pointer: &str, value: Value) {
    let (parent, key) = pointer.rsplit_once('/').expect("pointer");
    match target.pointer_mut(parent).expect("parent") {
        Value::Object(members) => {
            members.insert(key.to_owned(), value);
        }
        Value::Array(items) => items[key.parse::<usize>().expect("index")] = value,
        other => panic!("cannot set {pointer} in {other}"),
    }
}

fn remove(target: &mut Value, pointer: &str) {
    let (parent, key) = pointer.rsplit_once('/').expect("pointer");
    target
        .pointer_mut(parent)
        .and_then(Value::as_object_mut)
        .expect("parent object")
        .remove(key);
}

#[test]
fn without_a_vault_section_both_tools_refuse_as_not_configured() {
    let env = Env::new();
    for (tool, input) in [("list", "{}"), ("use", DEPLOY)] {
        let result = env.run(tool, input).result;
        assert_eq!(code(&result), "not_configured", "{result}");
        assert!(message(&result).contains("settings.json"), "{result}");
    }
}

#[test]
fn a_vault_section_in_project_or_local_settings_is_refused_naming_the_file() {
    let env = Env::new();
    let (listener, endpoint) = listener();
    let vault = env.vault(&endpoint);
    env.write_user_vault(&vault);
    let project = env.path("project");
    let settings = project.join(".nexus").join("sudocode");
    for path in [
        settings.join("settings.json"),
        settings.join("settings.local.json"),
        project.join(".scode.json"),
    ] {
        write_json(&path, &json!({ "vault": vault }));
        for (tool, input) in [("list", "{}"), ("use", DEPLOY)] {
            let result = env.run(tool, input).result;
            assert_eq!(code(&result), "project_config_refused", "{result}");
            assert!(
                message(&result).contains(&path.display().to_string()),
                "names {}: {result}",
                path.display()
            );
        }
        fs::remove_file(&path).expect("remove project settings");
    }
    assert_never_contacted(&listener);
}

/// Each case breaks one rule of an otherwise valid section and must be refused
/// with a message that names the settings file and contains `expected`.
fn assert_each_refused(env: &Env, valid: &Value, cases: &[(&str, Value, &str)]) {
    for (pointer, value, expected) in cases {
        let mut vault = valid.clone();
        set(&mut vault, pointer, value.clone());
        env.write_user_vault(&vault);
        let result = env.run("list", "{}").result;
        assert_eq!(
            code(&result),
            "invalid_settings",
            "{pointer} = {value}: {result}"
        );
        let message = message(&result);
        assert!(message.contains(expected), "{pointer} = {value}: {message}");
        assert!(
            message.contains("settings.json"),
            "names the file: {message}"
        );
    }
}

#[test]
fn each_invalid_section_rule_is_refused_with_its_own_message() {
    let env = Env::new();
    let (listener, endpoint) = listener();
    let valid = env.vault(&endpoint);
    let cases = [
        ("/endpoint", json!("http://127.0.0.1:1"), "endpoint"),
        ("/owner", json!(""), "owner"),
        (
            "/credentialDir",
            json!("relative/credentials"),
            "credentialDir",
        ),
        ("/operations/0/id", json!("Deploy API"), "operation id"),
        ("/operations/1/id", json!("deploy-api"), "not unique"),
        (
            "/operations/0/ref",
            json!("deploy/api/token"),
            "ref must be",
        ),
        (
            "/operations/1/kind",
            json!("shell"),
            "unknown variant `shell`",
        ),
        ("/extra", json!(true), "unknown field `extra`"),
        ("/operations/0/timeout", json!(5), "unknown field `timeout`"),
        (
            "/operations/0/response/body",
            json!(true),
            "unknown field `body`",
        ),
    ];
    assert_each_refused(&env, &valid, &cases);
    let mut without_response = valid.clone();
    remove(&mut without_response, "/operations/0/response");
    env.write_user_vault(&without_response);
    let result = env.run("list", "{}").result;
    assert_eq!(code(&result), "invalid_settings", "{result}");
    assert!(
        message(&result).contains("missing field `response`"),
        "{result}"
    );
    assert_never_contacted(&listener);
}

#[test]
fn each_invalid_http_rule_is_refused_with_its_own_message() {
    let env = Env::new();
    let (listener, endpoint) = listener();
    let cases = [
        (
            "/operations/0/origin",
            json!("https://127.0.0.1:8443/v1"),
            "origin",
        ),
        (
            "/operations/0/origin",
            json!("http://example.com"),
            "origin",
        ),
        ("/operations/0/method", json!("TRACE"), "method"),
        ("/operations/0/path", json!("/v1/../admin"), "path"),
        ("/operations/0/header", json!("Host"), "header must"),
        ("/operations/0/scheme", json!("Bearer token"), "scheme"),
        ("/operations/0/redirect", json!("follow"), "redirect"),
        (
            "/operations/0/body/service",
            json!("[a-z]+"),
            "body `service`",
        ),
        (
            "/operations/0/body/service",
            json!("^[a-z$"),
            "does not compile",
        ),
        (
            "/operations/0/response/headers/0",
            json!("Set-Cookie"),
            "response.headers",
        ),
        (
            "/operations/0/response/headers/0",
            json!("X Request"),
            "response.headers",
        ),
        (
            "/operations/0/response/jsonFields/0",
            json!("status"),
            "response.jsonFields",
        ),
        (
            "/operations/0/response/jsonFields/1",
            json!("/a~2b"),
            "response.jsonFields",
        ),
        (
            "/operations/0/response/maxBytes",
            json!(0),
            "response.maxBytes",
        ),
        (
            "/operations/0/response/maxBytes",
            json!(65_537),
            "response.maxBytes",
        ),
    ];
    assert_each_refused(&env, &env.vault(&endpoint), &cases);
    assert_never_contacted(&listener);
}

#[test]
fn each_invalid_exec_rule_is_refused_with_its_own_message() {
    let env = Env::new();
    let (listener, endpoint) = listener();
    let interpreter = env.path("tools").join("python3");
    fs::write(&interpreter, b"").expect("interpreter stand-in");
    let output = |max_bytes: u64| json!({"stdout": {"maxBytes": max_bytes}});
    let cases = [
        ("/operations/1/executable", json!("db-tool"), "executable"),
        (
            "/operations/1/executable",
            json!(interpreter),
            "interpreter",
        ),
        (
            "/operations/1/args/1",
            json!("--target={target}"),
            "args must",
        ),
        ("/operations/1/args/1", json!("{other}"), "undeclared param"),
        ("/operations/1/params/unused", json!("^x$"), "not used"),
        (
            "/operations/1/params/target",
            json!("[a-z]+"),
            "params `target`",
        ),
        ("/operations/1/cwd", json!(env.path("missing")), "cwd"),
        ("/operations/1/env", json!("PATH"), "env `PATH`"),
        (
            "/operations/1/env",
            json!("db_password"),
            "env `db_password`",
        ),
        (
            "/operations/1/passEnv/0",
            json!("DB_PASSWORD"),
            "passEnv must not",
        ),
        (
            "/operations/1/passEnv/0",
            json!("LD_PRELOAD"),
            "passEnv `LD_PRELOAD`",
        ),
        ("/operations/1/output", json!("stdout"), "output must"),
        ("/operations/1/output", json!({"stdout": {}}), "output must"),
        ("/operations/1/output", output(0), "output.stdout.maxBytes"),
        (
            "/operations/1/output",
            output(70_000),
            "output.stdout.maxBytes",
        ),
    ];
    assert_each_refused(&env, &env.vault(&endpoint), &cases);
    assert_never_contacted(&listener);
}

#[test]
fn optional_response_size_and_both_exec_output_forms_are_accepted() {
    let env = Env::new();
    let mut vault = env.vault(&closed_endpoint());
    remove(&mut vault, "/operations/0/response/maxBytes");
    for output in [json!("exit-code"), json!({"stdout": {"maxBytes": 65_536}})] {
        set(&mut vault, "/operations/1/output", output);
        env.write_user_vault(&vault);
        let result = env.run("use", MIGRATE).result;
        assert_eq!(code(&result), "use_unavailable", "{result}");
    }
}

#[test]
fn an_unknown_operation_id_is_refused() {
    let env = Env::new();
    let (listener, endpoint) = listener();
    env.write_user_vault(&env.vault(&endpoint));
    let result = env.run("use", r#"{"operation":"rotate-keys"}"#).result;
    assert_eq!(code(&result), "unknown_operation", "{result}");
    assert!(message(&result).contains("rotate-keys"), "{result}");
    assert_never_contacted(&listener);
}

#[test]
fn use_input_outside_the_declared_params_or_body_is_refused() {
    let env = Env::new();
    let (listener, endpoint) = listener();
    env.write_user_vault(&env.vault(&endpoint));
    let cases = [
        ("use", r#"{"operation":"db-migrate"}"#),
        (
            "use",
            r#"{"operation":"db-migrate","params":{"target":"orders v2"}}"#,
        ),
        (
            "use",
            r#"{"operation":"db-migrate","params":{"target":"a","extra":"b"}}"#,
        ),
        ("use", r#"{"operation":"db-migrate","params":{"target":7}}"#),
        (
            "use",
            r#"{"operation":"deploy-api","body":{"service":"Billing"}}"#,
        ),
        (
            "use",
            r#"{"operation":"deploy-api","body":{"service":"billing"},"params":{"x":"y"}}"#,
        ),
        (
            "use",
            r#"{"operation":"deploy-api","body":{"service":"billing"},"extra":1}"#,
        ),
        ("use", r#"{"body":{"service":"billing"}}"#),
        ("use", "not json"),
        ("list", r#"{"operation":"deploy-api"}"#),
    ];
    for (tool, input) in cases {
        let result = env.run(tool, input).result;
        assert_eq!(code(&result), "invalid_input", "{tool} {input}: {result}");
    }
    assert_never_contacted(&listener);
}

#[test]
fn a_valid_use_is_refused_with_the_579_diagnostic_and_touches_no_credential_or_network() {
    let env = Env::new();
    let (listener, endpoint) = listener();
    env.write_user_vault(&env.vault(&endpoint));
    assert!(!env.credentials().exists(), "no credential to read");
    for input in [DEPLOY, MIGRATE] {
        let result = env.run("use", input).result;
        assert_eq!(code(&result), "use_unavailable", "{result}");
        assert_eq!(message(&result), UNAVAILABLE);
    }
    assert_never_contacted(&listener);
}

/// List against a test-owned endpoint after `prepare` shapes the bundle, and
/// check the credential problem stopped the call before any dial.
fn credential_availability(prepare: impl FnOnce(&Path)) -> Vec<String> {
    let env = Env::new();
    let (listener, endpoint) = listener();
    env.write_user_vault(&env.vault(&endpoint));
    prepare(&env.credentials());
    let availability = env.list_availability();
    assert_never_contacted(&listener);
    availability
}

#[test]
fn a_missing_credential_directory_or_file_is_credential_missing() {
    assert_eq!(credential_availability(|_| {}), ["credential_missing"; 2]);
    let missing_key = credential_availability(|dir| {
        write_bundle(dir, &mint(OWNER), &runtime_name());
        fs::remove_file(dir.join("agent-key.pem")).expect("remove key");
    });
    assert_eq!(missing_key, ["credential_missing"; 2]);
}

#[test]
fn a_malformed_manifest_or_certificate_is_credential_invalid() {
    let manifest = credential_availability(|dir| {
        write_bundle(dir, &mint(OWNER), &runtime_name());
        fs::write(dir.join("credential.json"), "{not json").expect("manifest");
    });
    assert_eq!(manifest, ["credential_invalid"; 2]);
    let certificate = credential_availability(|dir| {
        write_bundle(dir, &mint(OWNER), &runtime_name());
        fs::write(dir.join("agent.pem"), "not a certificate").expect("certificate");
    });
    assert_eq!(certificate, ["credential_invalid"; 2]);
    let outside = credential_availability(|dir| {
        let elsewhere = dir.with_file_name("elsewhere");
        write_bundle(&elsewhere, &mint(OWNER), &runtime_name());
        fs::create_dir_all(dir).expect("bundle directory");
        let mut manifest = AgentCredential::for_bundle("vault-agent");
        manifest.server_name = runtime_name();
        manifest.ca = "../elsewhere/ca.pem".to_string();
        manifest.cert = "../elsewhere/agent.pem".to_string();
        manifest.key = "../elsewhere/agent-key.pem".to_string();
        manifest.write_to(dir).expect("manifest");
    });
    assert_eq!(outside, ["credential_invalid"; 2]);
    let nested = credential_availability(|dir| {
        write_bundle(&dir.join("generation"), &mint(OWNER), &runtime_name());
        let mut manifest = AgentCredential::for_bundle("vault-agent");
        manifest.server_name = runtime_name();
        manifest.ca = "generation/ca.pem".to_string();
        manifest.cert = "generation/agent.pem".to_string();
        manifest.key = "generation/agent-key.pem".to_string();
        manifest.write_to(dir).expect("manifest");
    });
    assert_eq!(nested, ["credential_invalid"; 2]);
}

/// Two generations of one agent's bundle, each with its own CA and key, as an
/// in-place rewrite produces them. Every mix of their files is what a read
/// that straddles the rewrite can see; none of them may be dialled with.
#[test]
fn every_mix_of_two_generations_is_credential_inconsistent() {
    let generations = [mint(OWNER), mint(OWNER)];
    for mix in 0..8_usize {
        let pick = |file: usize| &generations[(mix >> file) & 1];
        let bundle = Bundle {
            ca: pick(0).ca.clone(),
            cert: pick(1).cert.clone(),
            key: pick(2).key.clone(),
        };
        let env = Env::new();
        env.write_user_vault(&env.vault(&closed_endpoint()));
        write_bundle(&env.credentials(), &bundle, &runtime_name());
        let expected = if mix == 0 || mix == 7 {
            "vault_unavailable"
        } else {
            "credential_inconsistent"
        };
        assert_eq!(env.list_availability(), [expected; 2], "mix {mix:03b}");
    }
}

/// A bundle file that is a link is refused even when it points at a valid
/// bundle, so "inside the bundle" holds on disk and not only in the name. The
/// credential directory itself may be a link, as a `current` link switched
/// between generation directories is.
#[test]
fn a_linked_bundle_file_is_refused_and_a_linked_bundle_directory_is_followed() {
    let env = Env::new();
    let generation = env.path("generation-1");
    write_bundle(&generation, &mint(OWNER), &runtime_name());
    let mut vault = env.vault(&closed_endpoint());
    link(&generation, &env.credentials());
    env.write_user_vault(&vault);
    assert_eq!(env.list_availability(), ["vault_unavailable"; 2]);

    let linked = env.path("linked-certificate");
    fs::create_dir_all(&linked).expect("bundle directory");
    for name in ["credential.json", "ca.pem", "agent-key.pem"] {
        fs::copy(generation.join(name), linked.join(name)).expect("bundle file");
    }
    link(&generation.join("agent.pem"), &linked.join("agent.pem"));
    set(&mut vault, "/credentialDir", json!(linked));
    env.write_user_vault(&vault);
    assert_eq!(env.list_availability(), ["credential_invalid"; 2]);
}

fn link(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).expect("link");
    #[cfg(windows)]
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link).expect("directory link");
    } else {
        std::os::windows::fs::symlink_file(target, link).expect("file link");
    }
}

/// An endpoint that accepts connections and never answers must not hold the
/// agent's turn: the connection and every listing share one budget.
#[test]
fn a_silent_endpoint_is_vault_unavailable_within_the_list_budget() {
    let env = Env::new();
    let (listener, endpoint) = listener();
    env.write_user_vault(&env.vault(&endpoint));
    write_bundle(&env.credentials(), &mint(OWNER), &runtime_name());
    let started = Instant::now();
    assert_eq!(env.list_availability(), ["vault_unavailable"; 2]);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(18),
        "the listing took {elapsed:?}"
    );
    drop(listener);
}

#[test]
fn an_expired_certificate_is_credential_expired() {
    let expired = credential_availability(|dir| {
        write_bundle(dir, &mint_between(OWNER, 2000, 2001), &runtime_name());
    });
    assert_eq!(expired, ["credential_expired"; 2]);
}

#[test]
fn a_future_certificate_is_credential_not_yet_valid() {
    let future = credential_availability(|dir| {
        write_bundle(dir, &mint_between(OWNER, 2099, 2100), &runtime_name());
    });
    assert_eq!(future, ["credential_not_yet_valid"; 2]);
}

#[test]
fn another_owners_certificate_is_owner_mismatch() {
    let other = credential_availability(|dir| {
        write_bundle(dir, &mint("someone-else"), &runtime_name());
    });
    assert_eq!(other, ["owner_mismatch"; 2]);
}

#[test]
fn a_bundle_naming_another_server_is_server_name_mismatch() {
    let cluster = credential_availability(|dir| {
        write_bundle(dir, &mint(OWNER), "nexus-node");
    });
    assert_eq!(cluster, ["server_name_mismatch"; 2]);
}

#[test]
fn a_closed_endpoint_is_vault_unavailable_rather_than_a_credential_problem() {
    let env = Env::new();
    env.write_user_vault(&env.vault(&closed_endpoint()));
    write_bundle(&env.credentials(), &mint(OWNER), &runtime_name());
    assert_eq!(env.list_availability(), ["vault_unavailable"; 2]);
}

#[test]
fn list_labels_every_operation_unqualified_and_prints_no_key_or_cert_material() {
    let env = Env::new();
    env.write_user_vault(&env.vault(&closed_endpoint()));
    let bundle = mint(OWNER);
    write_bundle(&env.credentials(), &bundle, &runtime_name());
    let run = env.run("list", "{}");
    assert_eq!(run.result["status"], "configured-unqualified");
    assert_eq!(run.result["note"], NOTE);
    assert_eq!(
        run.result["operations"],
        json!([
            {
                "id": "deploy-api", "kind": "http", "ref": "deploy/api-token",
                "target": "POST https://127.0.0.1:8443/v1/exact",
                "availability": "vault_unavailable"
            },
            {
                "id": "db-migrate", "kind": "exec", "ref": "db/migrator",
                "target": env.path("tools").join("db-tool"),
                "availability": "vault_unavailable"
            }
        ])
    );
    assert!(!run.stdout.contains("-----BEGIN"), "{}", run.stdout);
    for pem in [&bundle.ca, &bundle.cert, &bundle.key] {
        for line in String::from_utf8_lossy(pem)
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with("-----"))
        {
            assert!(!run.stdout.contains(line), "PEM material in the output");
        }
    }
}
