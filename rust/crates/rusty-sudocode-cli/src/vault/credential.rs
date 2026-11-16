//! A consistent snapshot of the owner-bound agent credential bundle that dials
//! the owner's nexus `serve-user` runtime, checked before anything is dialled.

use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read};
use std::path::{Component, Path};
use std::time::{SystemTime, UNIX_EPOCH};

use lib::transport_primitives::authorship::owner_from_x509;
use lib::transport_primitives::{user_runtime_server_name, AgentCredential, CREDENTIAL_MANIFEST};
use rcgen::{KeyPair, PublicKeyData};
use x509_parser::certificate::X509Certificate;
use x509_parser::pem::{parse_x509_pem, Pem};

/// The bundle manifest version this build understands.
const MANIFEST_VERSION: u32 = 1;

/// Far above any manifest, CA, certificate or key a mint writes.
const MAX_FILE_BYTES: u64 = 64 * 1024;

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
    Inconsistent,
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
            Self::Inconsistent => "credential_inconsistent",
            Self::Expired => "credential_expired",
            Self::NotYetValid => "credential_not_yet_valid",
            Self::Replaced => "credential_replaced",
            Self::OwnerMismatch => "owner_mismatch",
            Self::ServerNameMismatch => "server_name_mismatch",
        }
    }
}

/// Read the bundle in `dir` and check it is one current credential that
/// belongs to `owner` and names `owner`'s runtime as its server.
///
/// `dir` is resolved once, so a bundle reached through a link that is switched
/// to another generation directory is read from one of them. Within it, a read
/// that straddles a rewrite shows up as a changed manifest or as files that are
/// not one credential; that read is repeated once, then refused.
pub(super) fn snapshot(dir: &Path, owner: &str) -> Result<Credential, CredentialError> {
    let dir = std::fs::canonicalize(dir)?;
    let mut last = CredentialError::Replaced;
    for _ in 0..2 {
        match read(&dir).and_then(|(manifest, credential)| {
            check(&manifest, &credential, owner)?;
            Ok(credential)
        }) {
            Err(error @ (CredentialError::Replaced | CredentialError::Inconsistent)) => {
                last = error;
            }
            outcome => return outcome,
        }
    }
    Err(last)
}

/// The manifest, the files it names, then the manifest again.
fn read(dir: &Path) -> Result<(AgentCredential, Credential), CredentialError> {
    let before = read_file(dir, CREDENTIAL_MANIFEST)?;
    let manifest: AgentCredential =
        serde_json::from_slice(&before).map_err(|_| CredentialError::Invalid)?;
    let credential = Credential {
        ca_pem: read_file(dir, &manifest.ca)?,
        cert_pem: read_file(dir, &manifest.cert)?,
        key_pem: read_file(dir, &manifest.key)?,
        server_name: manifest.server_name.clone(),
    };
    if read_file(dir, CREDENTIAL_MANIFEST)? == before {
        Ok((manifest, credential))
    } else {
        Err(CredentialError::Replaced)
    }
}

/// A bundle file: one name directly in the bundle directory, opened without
/// following a symbolic link or reparse point. A hard link is not detected:
/// the directory belongs to the writer that publishes the bundle.
fn read_file(dir: &Path, name: &str) -> Result<Vec<u8>, CredentialError> {
    let mut components = Path::new(name).components();
    let (Some(Component::Normal(_)), None) = (components.next(), components.next()) else {
        return Err(CredentialError::Invalid);
    };
    let file = open_without_following(&dir.join(name))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || is_link(&metadata) || metadata.len() > MAX_FILE_BYTES {
        return Err(CredentialError::Invalid);
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Open `path` for reading, failing on a link instead of following it, and
/// without waiting on a FIFO.
#[cfg(unix)]
fn open_without_following(path: &Path) -> io::Result<File> {
    use nix::fcntl::OFlag;
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
        .open(path)
}

/// Open `path` for reading; a link is opened as itself, not its target.
#[cfg(windows)]
fn open_without_following(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(unix)]
fn is_link(_metadata: &Metadata) -> bool {
    false
}

#[cfg(windows)]
fn is_link(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

impl From<io::Error> for CredentialError {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound => Self::Missing,
            _ => Self::Invalid,
        }
    }
}

/// The checks the maintained writer makes before it publishes a bundle (the
/// key is the certificate's, and the bundle CA signed the certificate), then
/// that the certificate is current, belongs to `owner` and the manifest names
/// `owner`'s runtime.
fn check(
    manifest: &AgentCredential,
    credential: &Credential,
    owner: &str,
) -> Result<(), CredentialError> {
    if manifest.version != MANIFEST_VERSION {
        return Err(CredentialError::Invalid);
    }
    let ca = first_pem(&credential.ca_pem)?;
    let cert = first_pem(&credential.cert_pem)?;
    if ca.label != "CERTIFICATE" || cert.label != "CERTIFICATE" {
        return Err(CredentialError::Invalid);
    }
    let key = std::str::from_utf8(&credential.key_pem)
        .ok()
        .and_then(|pem| KeyPair::from_pem(pem).ok())
        .ok_or(CredentialError::Invalid)?;
    let ca = ca.parse_x509().map_err(|_| CredentialError::Invalid)?;
    let cert = cert.parse_x509().map_err(|_| CredentialError::Invalid)?;
    if cert.public_key().raw != key.subject_public_key_info()
        || cert.verify_signature(Some(ca.public_key())).is_err()
    {
        return Err(CredentialError::Inconsistent);
    }
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
