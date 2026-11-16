//! A consistent snapshot of the owner-bound agent credential bundle that dials
//! the owner's nexus `serve-user` runtime, checked before anything is dialled.

use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use lib::transport_primitives::authorship::owner_from_x509;
use lib::transport_primitives::{user_runtime_server_name, AgentCredential, CREDENTIAL_MANIFEST};
use x509_parser::certificate::X509Certificate;
use x509_parser::pem::{parse_x509_pem, Pem};

/// The bundle manifest version this build understands.
const MANIFEST_VERSION: u32 = 1;

/// PEM material ready for `NexusVfsClient::connect_tls`.
pub(super) struct Credential {
    pub(super) ca_pem: Vec<u8>,
    pub(super) cert_pem: Vec<u8>,
    pub(super) key_pem: Vec<u8>,
    pub(super) server_name: String,
}

/// Why the credential cannot be used.
#[derive(Clone, Copy)]
pub(super) enum CredentialError {
    Missing,
    Invalid,
    Expired,
    NotYetValid,
    Replaced,
    OwnerMismatch,
    ServerNameMismatch,
}

impl CredentialError {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::Missing => "credential_missing",
            Self::Invalid => "credential_invalid",
            Self::Expired => "credential_expired",
            Self::NotYetValid => "credential_not_yet_valid",
            Self::Replaced => "credential_replaced",
            Self::OwnerMismatch => "owner_mismatch",
            Self::ServerNameMismatch => "server_name_mismatch",
        }
    }
}

/// Read the bundle in `dir` and check it is current, belongs to `owner` and
/// names `owner`'s runtime as its server.
pub(super) fn snapshot(dir: &Path, owner: &str) -> Result<Credential, CredentialError> {
    let (manifest, credential) = read_consistent(dir)?;
    check(&manifest, &credential, owner)?;
    Ok(credential)
}

/// Read the manifest, the files it names, then the manifest again. A bundle
/// replaced mid-read is read once more and refused if it changes again.
fn read_consistent(dir: &Path) -> Result<(AgentCredential, Credential), CredentialError> {
    let manifest_path = dir.join(CREDENTIAL_MANIFEST);
    for _ in 0..2 {
        let before = read(&manifest_path)?;
        let manifest: AgentCredential =
            serde_json::from_slice(&before).map_err(|_| CredentialError::Invalid)?;
        let credential = Credential {
            ca_pem: read_named(dir, &manifest.ca)?,
            cert_pem: read_named(dir, &manifest.cert)?,
            key_pem: read_named(dir, &manifest.key)?,
            server_name: manifest.server_name.clone(),
        };
        if read(&manifest_path)? == before {
            return Ok((manifest, credential));
        }
    }
    Err(CredentialError::Replaced)
}

/// A file the manifest names, relative to the bundle directory.
fn read_named(dir: &Path, name: &str) -> Result<Vec<u8>, CredentialError> {
    let relative = Path::new(name);
    if relative.is_absolute() || relative.has_root() {
        return Err(CredentialError::Invalid);
    }
    read(&dir.join(relative))
}

fn read(path: &Path) -> Result<Vec<u8>, CredentialError> {
    std::fs::read(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => CredentialError::Missing,
        _ => CredentialError::Invalid,
    })
}

fn check(
    manifest: &AgentCredential,
    credential: &Credential,
    owner: &str,
) -> Result<(), CredentialError> {
    if manifest.version != MANIFEST_VERSION {
        return Err(CredentialError::Invalid);
    }
    let ca = first_pem(&credential.ca_pem)?;
    let key = first_pem(&credential.key_pem)?;
    let cert = first_pem(&credential.cert_pem)?;
    if ca.label != "CERTIFICATE"
        || ca.parse_x509().is_err()
        || !key.label.ends_with("PRIVATE KEY")
        || cert.label != "CERTIFICATE"
    {
        return Err(CredentialError::Invalid);
    }
    let cert = cert.parse_x509().map_err(|_| CredentialError::Invalid)?;
    current(&cert)?;
    if owner_from_x509(&cert).as_deref() != Some(owner) {
        return Err(CredentialError::OwnerMismatch);
    }
    if user_runtime_server_name(owner).ok().as_deref() != Some(manifest.server_name.as_str()) {
        return Err(CredentialError::ServerNameMismatch);
    }
    Ok(())
}

fn first_pem(bytes: &[u8]) -> Result<Pem, CredentialError> {
    parse_x509_pem(bytes)
        .map(|(_, pem)| pem)
        .map_err(|_| CredentialError::Invalid)
}

fn current(cert: &X509Certificate<'_>) -> Result<(), CredentialError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        });
    let validity = cert.validity();
    if now < validity.not_before.timestamp() {
        Err(CredentialError::NotYetValid)
    } else if now > validity.not_after.timestamp() {
        Err(CredentialError::Expired)
    } else {
        Ok(())
    }
}
