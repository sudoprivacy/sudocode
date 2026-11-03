//! Exercise the production TLS client through real scode processes and a local
//! HTTPS gateway. The native loader's per-process file override keeps platform
//! trust stores and developer credentials untouched.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use serde_json::Value;

struct HttpsGateway {
    child: Child,
    ca: PathBuf,
    port: u16,
}

impl HttpsGateway {
    fn start(env: &common::TestEnv) -> Self {
        let root = env.workspace_root();
        let (ca, certificate, key) = write_certificates(root);
        let config: Value = serde_json::from_slice(
            &std::fs::read(env.config_home().join("sudocode.json")).unwrap(),
        )
        .unwrap();
        let backend = config["auth_modes"]["api-key"]["anthropic"]["baseUrl"]
            .as_str()
            .unwrap();
        let ready = root.join("tls-ready.json");
        let child = Command::new(common::resolve_python())
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/https_gateway.py"))
            .args([certificate.as_os_str(), key.as_os_str()])
            .arg(backend)
            .arg(&ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start local HTTPS gateway");
        // Own the child before readiness assertions so failure also closes it.
        let mut gateway = Self { child, ca, port: 0 };
        let deadline = Instant::now() + env.timeout();
        loop {
            if let Ok(bytes) = std::fs::read(&ready) {
                if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                    gateway.port = u16::try_from(value["port"].as_u64().unwrap()).unwrap();
                    return gateway;
                }
            }
            assert!(
                gateway.child.try_wait().unwrap().is_none(),
                "HTTPS gateway exited before readiness"
            );
            assert!(
                Instant::now() < deadline,
                "HTTPS gateway did not become ready"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn configure(&self, env: &common::TestEnv, host: &str) {
        let path = env.config_home().join("sudocode.json");
        let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        config["auth_modes"]["api-key"]["anthropic"]["baseUrl"] =
            format!("https://{host}:{}", self.port).into();
        std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    }
}

impl Drop for HttpsGateway {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write_certificates(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_key = KeyPair::generate().unwrap();
    let ca = params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(params, ca_key);
    let mut leaf = CertificateParams::new(vec!["localhost".into()]).unwrap();
    leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let key = KeyPair::generate().unwrap();
    let leaf = leaf.signed_by(&key, &issuer).unwrap();
    let paths = (
        root.join("ca.pem"),
        root.join("server.pem"),
        root.join("server-key.pem"),
    );
    std::fs::write(&paths.0, ca.pem()).unwrap();
    std::fs::write(&paths.1, leaf.pem()).unwrap();
    std::fs::write(&paths.2, key.serialize_pem()).unwrap();
    paths
}

#[test]
fn native_ca_trust_reaches_https_model_gateway_after_fresh_launch() {
    let env = common::TestEnv::new_mock("native-ca-trusted");
    let gateway = HttpsGateway::start(&env);
    gateway.configure(&env, "localhost");
    let ca = gateway.ca.to_str().unwrap();
    let prompt = env.prompt("Compute two plus two.", "single_turn_text");
    for expected_requests in 1..=2 {
        let mut cli = env.spawn_with_env(
            &["-p", &prompt],
            &[
                ("SSL_CERT_FILE", ca),
                ("SSL_CERT_DIR", ""),
                ("NO_PROXY", "*"),
                ("no_proxy", "*"),
            ],
        );
        cli.expect("The answer is 4").expect("HTTPS model response");
        assert_eq!(cli.expect_eof().unwrap(), 0);
        assert_eq!(env.captured_message_count(), expected_requests);
    }
}

fn assert_tls_refusal(host: &str, trusted: bool, cause: &str) {
    let env = common::TestEnv::new_mock("native-ca-refusal");
    let gateway = HttpsGateway::start(&env);
    gateway.configure(&env, host);
    let unrelated = env.workspace_root().join("unrelated-ca.pem");
    let unrelated_ca =
        rcgen::generate_simple_self_signed(vec!["unrelated.invalid".into()]).unwrap();
    std::fs::write(&unrelated, unrelated_ca.cert.pem()).unwrap();
    let ca = if trusted { &gateway.ca } else { &unrelated };
    let prompt = env.prompt("Compute two plus two.", "single_turn_text");
    let mut cli = env.spawn_with_env(
        &["-p", &prompt],
        &[
            ("SSL_CERT_FILE", ca.to_str().unwrap()),
            ("SSL_CERT_DIR", ""),
            ("NO_PROXY", "*"),
            ("no_proxy", "*"),
        ],
    );
    cli.expect(cause)
        .expect("the actual certificate verification failure must be visible");
    assert_eq!(
        env.captured_message_count(),
        0,
        "TLS refusal must precede the model request"
    );
    // The production network retry budget is independent of trust validation.
    // The PTY guard closes this owned child after its observed TLS refusal.
}

#[test]
fn native_ca_trust_does_not_accept_an_unknown_issuer() {
    assert_tls_refusal("localhost", false, "UnknownIssuer");
}

#[test]
fn native_ca_trust_still_checks_the_gateway_hostname() {
    assert_tls_refusal("127.0.0.1", true, "NotValidForName");
}
