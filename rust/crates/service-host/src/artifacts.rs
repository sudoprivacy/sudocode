//! Versioned runtime artifacts installed by Sudocode. The manifest owns pins
//! and checksums; kernel and plugin code remain independent Nexus components.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::Result;

#[derive(Deserialize, serde::Serialize)]
struct Manifest {
    versions: BTreeMap<String, String>,
    sha256: BTreeMap<String, String>,
}

pub fn root() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("SCODE_SERVICE_ROOT") {
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            return Err("SCODE_SERVICE_ROOT must be absolute".into());
        }
        return Ok(root);
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .ok_or("cannot locate the current OS user's home")?;
    Ok(PathBuf::from(home).join(".nexus-vfs"))
}

pub(crate) fn owner_root(explicit: Option<&Path>) -> Result<PathBuf> {
    match explicit {
        Some(path) if path.is_absolute() => Ok(path.to_path_buf()),
        Some(_) => Err("Local Box owner root must be absolute".into()),
        None => root(),
    }
}

pub fn version() -> String {
    manifest()
        .expect("valid embedded artifact manifest")
        .versions["nexusd-cluster"]
        .clone()
}

pub fn assembly(root: &Path) -> PathBuf {
    root.join("runtimes").join(format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&manifest().expect("valid embedded artifact manifest"))
                .expect("serialize artifact manifest")
        )
    ))
}

pub fn plugins(root: &Path) -> PathBuf {
    assembly(root).join("plugins")
}

pub fn fuse_available() -> bool {
    !cfg!(windows)
        && (!cfg!(target_os = "macos")
            || [
                "/usr/local/lib/libfuse3.4.dylib",
                "/Library/Application Support/fuse-t/lib/libfuse3.4.dylib",
            ]
            .iter()
            .any(|path| Path::new(path).is_file()))
}

pub fn binary(root: &Path) -> PathBuf {
    assembly(root).join("bin").join(if cfg!(windows) {
        "nexusd-cluster.exe"
    } else {
        "nexusd-cluster"
    })
}

fn platform() -> Result<(&'static str, &'static str)> {
    let os = match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "macos",
        "linux" => "linux",
        _ => return Err("unsupported service platform".into()),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => return Err("unsupported service architecture".into()),
    };
    Ok((os, arch))
}

fn manifest() -> Result<Manifest> {
    let mut manifest: Manifest = serde_json::from_str(
        include_str!("../runtime-artifacts.json").trim_start_matches('\u{feff}'),
    )?;
    manifest.versions.insert(
        "nexusd-cluster".into(),
        env!("NEXUS_RUNTIME_VERSION").into(),
    );
    Ok(manifest)
}

fn release_tag(prefix: &str, version: &str) -> Result<String> {
    Ok(match prefix {
        "nexusd-cluster" => format!("v{version}"),
        "nexus-vault" => format!("vault-v{version}"),
        "nexus-local-connector" => format!("local-connector-v{version}"),
        "nexus-fuse-plugin" => format!("fuse-v{version}"),
        _ => return Err("unknown runtime component".into()),
    })
}

pub fn expected_plugins() -> Result<Vec<String>> {
    Ok(selected(Path::new(""))?
        .into_iter()
        .filter(|(prefix, _, _)| prefix != "nexusd-cluster")
        .filter_map(|(_, _, files)| {
            files
                .first()?
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .collect())
}

fn selected(root: &Path) -> Result<Vec<(String, String, Vec<PathBuf>)>> {
    let (os, arch) = platform()?;
    let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
    let mut items = vec![(
        "nexusd-cluster".into(),
        format!("nexusd-cluster-{os}-{arch}.{ext}"),
        vec![root.join("bin").join(if cfg!(windows) {
            "nexusd-cluster.exe"
        } else {
            "nexusd-cluster"
        })],
    )];
    let plugin_arch = if os == "macos" && arch == "aarch64" {
        "arm64"
    } else {
        arch
    };
    for (prefix, library) in [
        ("nexus-vault", "nexus_vault"),
        ("nexus-local-connector", "nexus_local_connector"),
        ("nexus-fuse-plugin", "nexus_fuse_plugin"),
    ] {
        if prefix == "nexus-fuse-plugin" && !fuse_available() {
            continue;
        }
        let name = if cfg!(windows) {
            format!("{library}.dll")
        } else if cfg!(target_os = "macos") {
            format!("lib{library}.dylib")
        } else {
            format!("lib{library}.so")
        };
        let dest = root.join("plugins").join(name);
        let signature = dest.with_file_name(format!(
            "{}.sig",
            dest.file_name()
                .ok_or("missing library filename")?
                .to_string_lossy()
        ));
        items.push((
            prefix.into(),
            format!("{prefix}-{os}-{plugin_arch}.{ext}"),
            vec![dest, signature],
        ));
    }
    Ok(items)
}

fn marker(root: &Path, prefix: &str) -> PathBuf {
    root.join("run").join(format!("{prefix}-installation.json"))
}

fn verified(root: &Path, prefix: &str, version: &str, paths: &[PathBuf]) -> bool {
    let Ok(raw) = fs::read(marker(root, prefix)) else {
        return false;
    };
    let Ok(record) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return false;
    };
    if record["version"].as_str() != Some(version) {
        return false;
    }
    paths.iter().all(|path| {
        let Ok(bytes) = fs::read(path) else {
            return false;
        };
        let Some(name) = path.file_name() else {
            return false;
        };
        record["files"][name.to_string_lossy().as_ref()].as_str()
            == Some(format!("{:x}", Sha256::digest(&bytes)).as_str())
    })
}

pub fn installed(root: &Path) -> Result<bool> {
    let manifest = manifest()?;
    let assembly = assembly(root);
    Ok(selected(&assembly)?
        .iter()
        .all(|(prefix, _, files)| verified(&assembly, prefix, &manifest.versions[prefix], files)))
}

/// Download and validate every component before publishing its files.
pub fn install(root: &Path) -> Result<()> {
    let manifest = manifest()?;
    if installed(root)? {
        return Ok(());
    }
    let destination = assembly(root);
    let runtime_dir = root.join("runtimes");
    fs::create_dir_all(&runtime_dir)?;
    let staged_assembly = tempfile::tempdir_in(&runtime_dir)?;
    let staged_root = staged_assembly.path();
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_mins(2))
        .user_agent("scode-service-installer")
        .build()?;
    for (prefix, archive, files) in selected(staged_root)? {
        let version = &manifest.versions[&prefix];
        let release_tag = release_tag(&prefix, version)?;
        let checksum_key = format!("{release_tag}/{archive}");
        let checksum = manifest.sha256.get(&checksum_key).ok_or_else(|| {
            format!("service artifact is unavailable for this release and platform: {checksum_key}")
        })?;
        let cos_prefix = if prefix == "nexusd-cluster" {
            "nexus-vfs"
        } else {
            prefix.as_str()
        };
        let urls = [format!("https://sudowork-runtime-1309794936.cos.accelerate.myqcloud.com/{cos_prefix}/release/v{version}/{archive}"), format!("https://github.com/{}/{release_tag}/{archive}", if prefix == "nexusd-cluster" { "nexi-lab/nexus-vfs/releases/download" } else { "nexi-lab/nexus/releases/download" })];
        let mut download = None;
        for url in urls {
            if let Ok(response) = client
                .get(url)
                .send()
                .and_then(reqwest::blocking::Response::error_for_status)
            {
                let bytes = response.bytes()?;
                if format!("{:x}", Sha256::digest(&bytes)) != *checksum {
                    return Err(format!("checksum mismatch for {archive}").into());
                }
                download = Some(bytes.to_vec());
                break;
            }
        }
        let bytes =
            download.ok_or_else(|| format!("could not download service component: {archive}"))?;
        let stage = tempfile::tempdir()?;
        let mut hashes = BTreeMap::new();
        for file in &files {
            let name = file
                .file_name()
                .ok_or("missing artifact filename")?
                .to_string_lossy();
            let data = extract_file(&archive, &bytes, &name)?;
            hashes.insert(name.to_string(), format!("{:x}", Sha256::digest(&data)));
            fs::write(stage.path().join(name.as_ref()), data)?;
        }
        for file in &files {
            fs::create_dir_all(file.parent().ok_or("missing artifact parent")?)?;
            atomic_copy(
                &stage
                    .path()
                    .join(file.file_name().ok_or("missing artifact filename")?),
                file,
            )?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(file, fs::Permissions::from_mode(0o700))?;
            }
        }
        atomic_write(
            &marker(staged_root, &prefix),
            &serde_json::to_vec(&serde_json::json!({"version": version, "files": hashes}))?,
        )?;
    }
    if destination.exists() {
        fs::rename(
            &destination,
            runtime_dir.join(format!("quarantine-{}", uuid::Uuid::new_v4())),
        )?;
    }
    fs::rename(staged_root, &destination)?;
    Ok(())
}

fn extract_file(archive: &str, bytes: &[u8], name: &str) -> Result<Vec<u8>> {
    if Path::new(archive)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
        for index in 0..archive.len() {
            let mut file = archive.by_index(index)?;
            if file
                .enclosed_name()
                .and_then(|path| path.file_name().map(std::ffi::OsStr::to_owned))
                .as_deref()
                == Some(std::ffi::OsStr::new(name))
            {
                let mut data = Vec::new();
                file.read_to_end(&mut data)?;
                return Ok(data);
            }
        }
    } else {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
        for file in archive.entries()? {
            let mut file = file?;
            if file.path()?.file_name() == Some(std::ffi::OsStr::new(name))
                && file.header().entry_type().is_file()
            {
                let mut data = Vec::new();
                file.read_to_end(&mut data)?;
                return Ok(data);
            }
        }
    }
    Err(format!("artifact contains no {name}").into())
}

fn atomic_copy(source: &Path, dest: &Path) -> Result<()> {
    atomic_write(dest, &fs::read(source)?)
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or("missing destination parent")?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_are_bound_to_the_selected_component_releases() {
        let manifest = manifest().unwrap();
        for (key, digest) in &manifest.sha256 {
            let (tag, archive) = key.split_once('/').expect("versioned artifact checksum");
            let prefix = manifest
                .versions
                .keys()
                .find(|prefix| archive.starts_with(&format!("{prefix}-")))
                .expect("known runtime component");
            assert_eq!(
                tag,
                release_tag(prefix, &manifest.versions[prefix]).unwrap()
            );
            assert_eq!(digest.len(), 64);
            assert!(digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
        }
        for (prefix, archive, _) in selected(Path::new("")).unwrap() {
            let tag = release_tag(&prefix, &manifest.versions[&prefix]).unwrap();
            assert!(manifest.sha256.contains_key(&format!("{tag}/{archive}")));
            assert!(!manifest.sha256.contains_key(&archive));
        }
    }
}
