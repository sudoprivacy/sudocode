//! The builtin Vault tools through the real `scode` binary: settings refusals,
//! input checks, the unconditional `vault_use` refusal, and credential checks
//! on bundles minted by nexus's own certgen. Every run must leave stderr empty
//! and print exactly one JSON object.

use std::fs;
use std::io::{ErrorKind, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use nexus_raft::transport::{
    generate_session_agent_cert, generate_zone_ca, user_runtime_server_name, AgentCredential,
};
use rcgen::{CertificateParams, Issuer, KeyPair, SanType};
use serde_json::{json, Value};
use tempfile::TempDir;

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

/// One tool run: its single JSON result and the raw streams.
struct ToolRun {
    result: Value,
    stdout: String,
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
        let home = self.path("home");
        let mut child = Command::new(env!("CARGO_BIN_EXE_scode"))
            .args(["vault", "tool", tool])
            .current_dir(self.path("project"))
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("SCODE_GLOBAL_CONFIG_DIR", self.path("config"))
            .env_remove("SUDO_CODE_CONFIG_HOME")
            .env_remove("SCODE_PROJECT_CONFIG_DIR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("launch scode");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write the tool input");
        let output = child.wait_with_output().expect("scode exits");
        let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "vault tool {tool} exited {}",
            output.status
        );
        assert!(
            stderr.is_empty(),
            "vault tool {tool} wrote stderr: {stderr}"
        );
        let result: Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|error| panic!("expected one JSON object ({error}): {stdout}"));
        assert!(result.is_object(), "{stdout}");
        ToolRun { result, stdout }
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

/// A session agent credential for `owner`, from nexus's own certgen.
fn mint(owner: &str, validity_secs: u64) -> Bundle {
    let (ca, ca_key) = generate_zone_ca("vault-test").expect("zone CA");
    let (cert, key) =
        generate_session_agent_cert("vault-agent", owner, validity_secs, &ca, &ca_key)
            .expect("session agent certificate");
    Bundle { ca, cert, key }
}

/// A certificate that starts in 2099. Nexus certgen always starts at the time
/// of minting, so this one is signed with rcgen by a nexus zone CA, carrying
/// the same owner SAN.
fn mint_not_yet_valid(owner: &str) -> Bundle {
    let (ca, ca_key) = generate_zone_ca("vault-test").expect("zone CA");
    let ca_key_pair =
        KeyPair::from_pem(std::str::from_utf8(&ca_key).expect("PEM")).expect("CA key");
    let issuer = Issuer::from_ca_cert_pem(std::str::from_utf8(&ca).expect("PEM"), ca_key_pair)
        .expect("issuer");
    let key = KeyPair::generate().expect("agent key");
    let mut params = CertificateParams::default();
    params.subject_alt_names = vec![SanType::URI(
        lib::agent_identity::owner_uri(owner)
            .try_into()
            .expect("owner URI"),
    )];
    params.not_before = rcgen::date_time_ymd(2099, 1, 1);
    params.not_after = rcgen::date_time_ymd(2100, 1, 1);
    let cert = params.signed_by(&key, &issuer).expect("certificate");
    Bundle {
        ca,
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
        write_bundle(dir, &mint(OWNER, 300), &runtime_name());
        fs::remove_file(dir.join("agent-key.pem")).expect("remove key");
    });
    assert_eq!(missing_key, ["credential_missing"; 2]);
}

#[test]
fn a_malformed_manifest_or_certificate_is_credential_invalid() {
    let manifest = credential_availability(|dir| {
        write_bundle(dir, &mint(OWNER, 300), &runtime_name());
        fs::write(dir.join("credential.json"), "{not json").expect("manifest");
    });
    assert_eq!(manifest, ["credential_invalid"; 2]);
    let certificate = credential_availability(|dir| {
        write_bundle(dir, &mint(OWNER, 300), &runtime_name());
        fs::write(dir.join("agent.pem"), "not a certificate").expect("certificate");
    });
    assert_eq!(certificate, ["credential_invalid"; 2]);
    let outside = credential_availability(|dir| {
        let elsewhere = dir.with_file_name("elsewhere");
        write_bundle(&elsewhere, &mint(OWNER, 300), &runtime_name());
        fs::create_dir_all(dir).expect("bundle directory");
        let mut manifest = AgentCredential::for_bundle("vault-agent");
        manifest.server_name = runtime_name();
        manifest.ca = "../elsewhere/ca.pem".to_string();
        manifest.cert = "../elsewhere/agent.pem".to_string();
        manifest.key = "../elsewhere/agent-key.pem".to_string();
        manifest.write_to(dir).expect("manifest");
    });
    assert_eq!(outside, ["credential_invalid"; 2]);
}

#[test]
fn an_expired_certificate_is_credential_expired() {
    let expired = credential_availability(|dir| {
        write_bundle(dir, &mint(OWNER, 1), &runtime_name());
        std::thread::sleep(Duration::from_millis(2_500));
    });
    assert_eq!(expired, ["credential_expired"; 2]);
}

#[test]
fn a_future_certificate_is_credential_not_yet_valid() {
    let future = credential_availability(|dir| {
        write_bundle(dir, &mint_not_yet_valid(OWNER), &runtime_name());
    });
    assert_eq!(future, ["credential_not_yet_valid"; 2]);
}

#[test]
fn another_owners_certificate_is_owner_mismatch() {
    let other = credential_availability(|dir| {
        write_bundle(dir, &mint("someone-else", 300), &runtime_name());
    });
    assert_eq!(other, ["owner_mismatch"; 2]);
}

#[test]
fn a_bundle_naming_another_server_is_server_name_mismatch() {
    let cluster = credential_availability(|dir| {
        write_bundle(dir, &mint(OWNER, 300), "nexus-node");
    });
    assert_eq!(cluster, ["server_name_mismatch"; 2]);
}

#[test]
fn a_closed_endpoint_is_vault_unavailable_rather_than_a_credential_problem() {
    let env = Env::new();
    env.write_user_vault(&env.vault(&closed_endpoint()));
    write_bundle(&env.credentials(), &mint(OWNER, 300), &runtime_name());
    assert_eq!(env.list_availability(), ["vault_unavailable"; 2]);
}

#[test]
fn list_labels_every_operation_unqualified_and_prints_no_key_or_cert_material() {
    let env = Env::new();
    env.write_user_vault(&env.vault(&closed_endpoint()));
    let bundle = mint(OWNER, 300);
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
