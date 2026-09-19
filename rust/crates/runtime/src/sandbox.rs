use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FilesystemIsolationMode {
    Off,
    #[default]
    WorkspaceOnly,
    AllowList,
}

impl FilesystemIsolationMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::WorkspaceOnly => "workspace-only",
            Self::AllowList => "allow-list",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SandboxConfig {
    pub enabled: Option<bool>,
    pub namespace_restrictions: Option<bool>,
    pub network_isolation: Option<bool>,
    pub filesystem_mode: Option<FilesystemIsolationMode>,
    pub allowed_mounts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SandboxRequest {
    pub enabled: bool,
    pub namespace_restrictions: bool,
    pub network_isolation: bool,
    pub filesystem_mode: FilesystemIsolationMode,
    pub allowed_mounts: Vec<String>,
    /// Whether the caller asked for confinement in so many words (a
    /// `sandbox.enabled` / `filesystemMode` / `networkIsolation` setting or
    /// the per-call bash overrides) rather than inheriting the defaults.
    /// The macOS Seatbelt backend engages only on an explicit request — see
    /// [`SandboxBackend::MacosSeatbelt`].
    #[serde(default)]
    pub explicit: bool,
}

/// Which process-confinement mechanism the platform provides for `bash`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxBackend {
    /// No confinement: the command runs as a plain child process. HOME and
    /// TMPDIR are still redirected into the workspace when the filesystem
    /// mode asks for it, but nothing is enforced.
    #[default]
    None,
    /// Linux user namespaces via `unshare` (mount/ipc/pid/uts, optionally
    /// net). Filesystem mode is advisory: it is exported as
    /// `SUDOCODE_SANDBOX_FILESYSTEM_MODE`, not enforced by the kernel.
    LinuxNamespaces,
    /// macOS Seatbelt via `/usr/bin/sandbox-exec`, which enforces the
    /// filesystem mode (writes outside the workspace and a few scratch
    /// locations are denied) and network isolation.
    ///
    /// Opt-in: a fresh install has `sandbox.enabled` unset, and every macOS
    /// user has been running unconfined so far, so engaging Seatbelt on the
    /// default config would start denying writes to caches, global installs
    /// and other out-of-tree paths overnight. Set `sandbox.enabled: true`
    /// (or pass `filesystemMode` / `isolateNetwork` on the bash call) to
    /// turn it on; `allowedMounts` widens the writable set.
    MacosSeatbelt,
}

impl SandboxBackend {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::LinuxNamespaces => "linux-namespaces",
            Self::MacosSeatbelt => "macos-seatbelt",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ContainerEnvironment {
    pub in_container: bool,
    pub markers: Vec<String>,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct SandboxStatus {
    pub enabled: bool,
    pub requested: SandboxRequest,
    pub supported: bool,
    pub active: bool,
    pub namespace_supported: bool,
    pub namespace_active: bool,
    pub network_supported: bool,
    pub network_active: bool,
    pub filesystem_mode: FilesystemIsolationMode,
    pub filesystem_active: bool,
    pub allowed_mounts: Vec<String>,
    pub in_container: bool,
    pub container_markers: Vec<String>,
    pub fallback_reason: Option<String>,
    /// The confinement mechanism that will actually wrap the command.
    #[serde(default)]
    pub backend: SandboxBackend,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxDetectionInputs<'a> {
    pub env_pairs: Vec<(String, String)>,
    pub dockerenv_exists: bool,
    pub containerenv_exists: bool,
    pub proc_1_cgroup: Option<&'a str>,
}

/// A launcher program plus arguments that wraps the user's command in the
/// platform's confinement mechanism.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Former name of [`SandboxCommand`], kept for callers that predate the
/// macOS backend.
pub type LinuxSandboxCommand = SandboxCommand;

impl SandboxConfig {
    #[must_use]
    pub fn resolve_request(
        &self,
        enabled_override: Option<bool>,
        namespace_override: Option<bool>,
        network_override: Option<bool>,
        filesystem_mode_override: Option<FilesystemIsolationMode>,
        allowed_mounts_override: Option<Vec<String>>,
    ) -> SandboxRequest {
        SandboxRequest {
            enabled: enabled_override.unwrap_or(self.enabled.unwrap_or(true)),
            namespace_restrictions: namespace_override
                .unwrap_or(self.namespace_restrictions.unwrap_or(true)),
            network_isolation: network_override.unwrap_or(self.network_isolation.unwrap_or(false)),
            filesystem_mode: filesystem_mode_override
                .or(self.filesystem_mode)
                .unwrap_or_default(),
            allowed_mounts: allowed_mounts_override.unwrap_or_else(|| self.allowed_mounts.clone()),
            explicit: enabled_override.is_some()
                || network_override.is_some()
                || filesystem_mode_override.is_some()
                || self.enabled.is_some()
                || self.network_isolation.is_some()
                || self.filesystem_mode.is_some(),
        }
    }
}

#[must_use]
pub fn detect_container_environment() -> ContainerEnvironment {
    let proc_1_cgroup = fs::read_to_string("/proc/1/cgroup").ok();
    detect_container_environment_from(SandboxDetectionInputs {
        env_pairs: env::vars().collect(),
        dockerenv_exists: Path::new("/.dockerenv").exists(),
        containerenv_exists: Path::new("/run/.containerenv").exists(),
        proc_1_cgroup: proc_1_cgroup.as_deref(),
    })
}

#[must_use]
pub fn detect_container_environment_from(
    inputs: SandboxDetectionInputs<'_>,
) -> ContainerEnvironment {
    let mut markers = Vec::new();
    if inputs.dockerenv_exists {
        markers.push("/.dockerenv".to_string());
    }
    if inputs.containerenv_exists {
        markers.push("/run/.containerenv".to_string());
    }
    for (key, value) in inputs.env_pairs {
        let normalized = key.to_ascii_lowercase();
        if matches!(
            normalized.as_str(),
            "container" | "docker" | "podman" | "kubernetes_service_host"
        ) && !value.is_empty()
        {
            markers.push(format!("env:{key}={value}"));
        }
    }
    if let Some(cgroup) = inputs.proc_1_cgroup {
        for needle in ["docker", "containerd", "kubepods", "podman", "libpod"] {
            if cgroup.contains(needle) {
                markers.push(format!("/proc/1/cgroup:{needle}"));
            }
        }
    }
    markers.sort();
    markers.dedup();
    ContainerEnvironment {
        in_container: !markers.is_empty(),
        markers,
    }
}

#[must_use]
pub fn resolve_sandbox_status(config: &SandboxConfig, cwd: &Path) -> SandboxStatus {
    let request = config.resolve_request(None, None, None, None, None);
    resolve_sandbox_status_for_request(&request, cwd)
}

#[must_use]
pub fn resolve_sandbox_status_for_request(request: &SandboxRequest, cwd: &Path) -> SandboxStatus {
    let container = detect_container_environment();
    let namespace_supported = cfg!(target_os = "linux") && unshare_user_namespace_works();
    let seatbelt_supported = cfg!(target_os = "macos") && seatbelt_available();
    // Seatbelt engages only on an explicit request — see
    // `SandboxBackend::MacosSeatbelt` for why the default config stays
    // unconfined on macOS.
    let seatbelt_engaged = seatbelt_supported && request.enabled && request.explicit;
    let network_supported = namespace_supported || seatbelt_supported;
    let filesystem_active =
        request.enabled && request.filesystem_mode != FilesystemIsolationMode::Off;
    let mut fallback_reasons = Vec::new();

    if request.enabled && request.namespace_restrictions && !namespace_supported {
        fallback_reasons.push(if seatbelt_engaged {
            "namespace isolation unavailable (Linux only); Seatbelt confines files and network instead".to_string()
        } else {
            "namespace isolation unavailable (requires Linux with `unshare`)".to_string()
        });
    }
    if request.enabled && request.network_isolation && !network_supported {
        fallback_reasons.push(
            "network isolation unavailable (requires Linux with `unshare` or macOS with `sandbox-exec`)"
                .to_string(),
        );
    }
    if request.enabled && seatbelt_supported && !request.explicit {
        fallback_reasons.push(
            "macOS Seatbelt is opt-in: set sandbox.enabled=true in .scode.json (or pass filesystemMode / isolateNetwork on the bash call) to confine commands"
                .to_string(),
        );
    }
    if request.enabled
        && request.filesystem_mode == FilesystemIsolationMode::AllowList
        && request.allowed_mounts.is_empty()
    {
        fallback_reasons
            .push("filesystem allow-list requested without configured mounts".to_string());
    }

    let backend = if seatbelt_engaged {
        SandboxBackend::MacosSeatbelt
    } else if request.enabled && namespace_supported {
        SandboxBackend::LinuxNamespaces
    } else {
        SandboxBackend::None
    };
    let network_active = request.enabled
        && request.network_isolation
        && match backend {
            SandboxBackend::LinuxNamespaces => namespace_supported,
            SandboxBackend::MacosSeatbelt => true,
            SandboxBackend::None => false,
        };
    let active = match backend {
        SandboxBackend::MacosSeatbelt => filesystem_active || network_active,
        SandboxBackend::LinuxNamespaces | SandboxBackend::None => {
            request.enabled
                && (!request.namespace_restrictions || namespace_supported)
                && (!request.network_isolation || network_supported)
        }
    };

    let allowed_mounts = normalize_mounts(&request.allowed_mounts, cwd);

    SandboxStatus {
        enabled: request.enabled,
        requested: request.clone(),
        supported: namespace_supported || seatbelt_supported,
        active,
        namespace_supported,
        namespace_active: request.enabled && request.namespace_restrictions && namespace_supported,
        network_supported,
        network_active,
        filesystem_mode: request.filesystem_mode,
        filesystem_active,
        allowed_mounts,
        in_container: container.in_container,
        container_markers: container.markers,
        fallback_reason: (!fallback_reasons.is_empty()).then(|| fallback_reasons.join("; ")),
        backend,
    }
}

/// Wrap `command` in whichever confinement backend `status` resolved to, or
/// `None` when the command should run as a plain child process.
#[must_use]
pub fn build_sandbox_command(
    command: &str,
    cwd: &Path,
    status: &SandboxStatus,
) -> Option<SandboxCommand> {
    match status.backend {
        SandboxBackend::LinuxNamespaces => build_linux_sandbox_command(command, cwd, status),
        SandboxBackend::MacosSeatbelt => build_macos_sandbox_command(command, cwd, status),
        SandboxBackend::None => None,
    }
}

/// The environment every backend hands the confined shell: HOME and TMPDIR
/// inside the workspace, the advisory filesystem policy, and the host PATH.
fn sandbox_env(cwd: &Path, status: &SandboxStatus) -> Vec<(String, String)> {
    let sandbox_home = cwd.join(".sandbox-home");
    let sandbox_tmp = cwd.join(".sandbox-tmp");
    let mut env = vec![
        ("HOME".to_string(), sandbox_home.display().to_string()),
        ("TMPDIR".to_string(), sandbox_tmp.display().to_string()),
        (
            "SUDOCODE_SANDBOX_FILESYSTEM_MODE".to_string(),
            status.filesystem_mode.as_str().to_string(),
        ),
        (
            "SUDOCODE_SANDBOX_ALLOWED_MOUNTS".to_string(),
            status.allowed_mounts.join(":"),
        ),
    ];
    if let Ok(path) = env::var("PATH") {
        env.push(("PATH".to_string(), path));
    }
    env
}

/// `sandbox-exec -p <profile> sh -lc <command>` for the macOS Seatbelt
/// backend. `None` unless the status resolved to that backend with something
/// to enforce.
#[must_use]
pub fn build_macos_sandbox_command(
    command: &str,
    cwd: &Path,
    status: &SandboxStatus,
) -> Option<SandboxCommand> {
    if status.backend != SandboxBackend::MacosSeatbelt
        || (!status.filesystem_active && !status.network_active)
    {
        return None;
    }
    let profile = seatbelt_profile(cwd, status);
    Some(SandboxCommand {
        program: SEATBELT_LAUNCHER.to_string(),
        args: vec![
            "-p".to_string(),
            profile,
            "sh".to_string(),
            "-lc".to_string(),
            command.to_string(),
        ],
        env: sandbox_env(cwd, status),
    })
}

const SEATBELT_LAUNCHER: &str = "/usr/bin/sandbox-exec";

fn seatbelt_available() -> bool {
    Path::new(SEATBELT_LAUNCHER).exists()
}

/// Render the Seatbelt profile (SBPL) for `status`.
///
/// Everything is allowed except what the resolved status confines:
///
/// - filesystem mode `workspace-only` / `allow-list`: `file-write*` is denied
///   outside the workspace, the git common dir (a worktree's `.git` lives in
///   the primary checkout), the system and per-user temp trees, the
///   configured `allowedMounts`, and the tty/null devices a shell needs;
/// - network isolation: `network*` is denied.
///
/// Reads are never restricted: the model already has read access to the
/// host through `read_file`, and a read-only profile would only break the
/// toolchains it needs to run.
#[must_use]
pub fn seatbelt_profile(cwd: &Path, status: &SandboxStatus) -> String {
    let mut profile = String::from("(version 1)\n(allow default)\n");
    if status.filesystem_active {
        profile.push_str("(deny file-write*)\n(allow file-write*\n");
        for path in seatbelt_writable_roots(cwd, status) {
            profile.push_str(&format!("  (subpath \"{}\")\n", sbpl_escape(&path)));
        }
        profile.push_str(
            "  (literal \"/dev/null\")\n  (literal \"/dev/stdout\")\n  (literal \"/dev/stderr\")\n  (regex #\"^/dev/tty\")\n  (regex #\"^/dev/fd/\")\n  (regex #\"^/dev/pty\")\n)\n",
        );
    }
    if status.network_active {
        profile.push_str("(deny network*)\n");
    }
    profile
}

/// Directories the confined shell may write to, canonicalised because
/// Seatbelt matches real paths (`/tmp` is `/private/tmp`, `/var` is
/// `/private/var`). Order is irrelevant to the profile; duplicates are
/// dropped so the profile stays readable in `scode sandbox --status`.
fn seatbelt_writable_roots(cwd: &Path, status: &SandboxStatus) -> Vec<String> {
    let mut roots: Vec<String> = Vec::new();
    let mut push = |path: PathBuf| {
        let real = fs::canonicalize(&path).unwrap_or(path);
        let display = real.display().to_string();
        if !display.is_empty() && !roots.contains(&display) {
            roots.push(display);
        }
    };
    push(cwd.to_path_buf());
    if let Some(common) = git_common_dir(cwd) {
        push(common);
    }
    push(env::temp_dir());
    push(PathBuf::from("/private/tmp"));
    push(PathBuf::from("/private/var/folders"));
    for mount in &status.allowed_mounts {
        push(PathBuf::from(mount));
    }
    roots
}

/// The repository's shared `.git` directory when `cwd` is inside a git
/// checkout: a linked worktree's `.git` is a file pointing into the primary
/// checkout, so `git commit` from a worktree writes outside `cwd`.
fn git_common_dir(cwd: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(&raw);
    Some(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

/// Escape a path for an SBPL string literal.
fn sbpl_escape(path: &str) -> String {
    path.replace('\\', "\\\\").replace('"', "\\\"")
}

#[must_use]
pub fn build_linux_sandbox_command(
    command: &str,
    cwd: &Path,
    status: &SandboxStatus,
) -> Option<SandboxCommand> {
    if !cfg!(target_os = "linux")
        || !status.enabled
        || (!status.namespace_active && !status.network_active)
    {
        return None;
    }

    let mut args = vec![
        "--user".to_string(),
        "--map-root-user".to_string(),
        "--mount".to_string(),
        "--ipc".to_string(),
        "--pid".to_string(),
        "--uts".to_string(),
        "--fork".to_string(),
    ];
    if status.network_active {
        args.push("--net".to_string());
    }
    args.push("sh".to_string());
    args.push("-lc".to_string());
    args.push(command.to_string());

    Some(SandboxCommand {
        program: "unshare".to_string(),
        args,
        env: sandbox_env(cwd, status),
    })
}

fn normalize_mounts(mounts: &[String], cwd: &Path) -> Vec<String> {
    let cwd = cwd.to_path_buf();
    mounts
        .iter()
        .map(|mount| {
            let path = PathBuf::from(mount);
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        })
        .map(|path| path.display().to_string())
        .collect()
}

fn command_exists(command: &str) -> bool {
    env::var_os("PATH")
        .is_some_and(|paths| env::split_paths(&paths).any(|path| path.join(command).exists()))
}

/// Check whether `unshare --user` actually works on this system.
/// On some CI environments (e.g. GitHub Actions), the binary exists but
/// user namespaces are restricted, causing silent failures.
fn unshare_user_namespace_works() -> bool {
    use std::sync::OnceLock;
    static RESULT: OnceLock<bool> = OnceLock::new();
    *RESULT.get_or_init(|| {
        if !command_exists("unshare") {
            return false;
        }
        std::process::Command::new("unshare")
            .args(["--user", "--map-root-user", "true"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::{
        build_linux_sandbox_command, build_sandbox_command, detect_container_environment_from,
        resolve_sandbox_status_for_request, seatbelt_profile, FilesystemIsolationMode,
        SandboxBackend, SandboxConfig, SandboxDetectionInputs,
    };
    use std::path::Path;

    #[test]
    fn default_config_is_not_an_explicit_request() {
        let request = SandboxConfig::default().resolve_request(None, None, None, None, None);
        assert!(request.enabled, "sandboxing stays on by default");
        assert!(
            !request.explicit,
            "…but nobody asked for it in so many words"
        );

        let by_config = SandboxConfig {
            enabled: Some(true),
            ..SandboxConfig::default()
        }
        .resolve_request(None, None, None, None, None);
        assert!(by_config.explicit);

        let by_call = SandboxConfig::default().resolve_request(
            None,
            None,
            None,
            Some(FilesystemIsolationMode::WorkspaceOnly),
            None,
        );
        assert!(by_call.explicit);
    }

    #[test]
    fn seatbelt_profile_confines_writes_to_workspace_and_denies_network() {
        let cwd = Path::new("/workspace/project");
        let mut status = resolve_sandbox_status_for_request(
            &SandboxConfig::default().resolve_request(
                Some(true),
                Some(false),
                Some(true),
                Some(FilesystemIsolationMode::WorkspaceOnly),
                Some(vec!["/opt/cache".to_string()]),
            ),
            cwd,
        );
        // Force the backend so the profile is testable on every host.
        status.backend = SandboxBackend::MacosSeatbelt;
        status.filesystem_active = true;
        status.network_active = true;

        let profile = seatbelt_profile(cwd, &status);
        assert!(profile.starts_with("(version 1)\n(allow default)\n"));
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains("(subpath \"/workspace/project\")"));
        assert!(profile.contains("(subpath \"/opt/cache\")"));
        assert!(profile.contains("(subpath \"/private/tmp\")"));
        assert!(profile.contains("(literal \"/dev/null\")"));
        assert!(profile.contains("(deny network*)"));

        status.network_active = false;
        assert!(!seatbelt_profile(cwd, &status).contains("deny network"));
        status.filesystem_active = false;
        assert_eq!(
            seatbelt_profile(cwd, &status),
            "(version 1)\n(allow default)\n"
        );
    }

    #[test]
    fn seatbelt_profile_escapes_quotes_in_paths() {
        let cwd = Path::new("/tmp/we\"ird");
        let mut status = resolve_sandbox_status_for_request(
            &SandboxConfig::default().resolve_request(
                Some(true),
                None,
                None,
                Some(FilesystemIsolationMode::WorkspaceOnly),
                None,
            ),
            cwd,
        );
        status.backend = SandboxBackend::MacosSeatbelt;
        status.filesystem_active = true;
        assert!(seatbelt_profile(cwd, &status).contains("(subpath \"/tmp/we\\\"ird\")"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_default_config_stays_unconfined_and_says_how_to_opt_in() {
        let status = resolve_sandbox_status_for_request(
            &SandboxConfig::default().resolve_request(None, None, None, None, None),
            Path::new("/tmp"),
        );
        assert_eq!(status.backend, SandboxBackend::None);
        assert!(!status.active);
        assert!(status.supported, "sandbox-exec ships with macOS");
        assert!(status
            .fallback_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("Seatbelt is opt-in")));
        assert!(build_sandbox_command("true", Path::new("/tmp"), &status).is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_explicit_request_engages_seatbelt_and_enforces_it() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let cwd = std::env::temp_dir().join(format!("scode-seatbelt-{unique}"));
        std::fs::create_dir_all(&cwd).expect("workspace");
        let outside = std::env::temp_dir().join(format!("scode-seatbelt-outside-{unique}"));
        // Both live under the temp tree, which the profile allows, so pick an
        // "outside" target that no writable root covers: the user's home.
        let home = std::env::var("HOME").expect("HOME");
        let outside_home = Path::new(&home).join(format!(".scode-seatbelt-probe-{unique}"));

        let status = resolve_sandbox_status_for_request(
            &SandboxConfig::default().resolve_request(
                Some(true),
                Some(false),
                Some(true),
                Some(FilesystemIsolationMode::WorkspaceOnly),
                None,
            ),
            &cwd,
        );
        assert_eq!(status.backend, SandboxBackend::MacosSeatbelt);
        assert!(status.active);
        assert!(status.filesystem_active);
        assert!(status.network_active);

        let command = format!(
            "echo inside > {cwd}/inside.txt; echo outside > {outside_home} 2>/dev/null && echo outside-ok || echo outside-denied",
            cwd = cwd.display(),
            outside_home = outside_home.display(),
        );
        let launcher = build_sandbox_command(&command, &cwd, &status).expect("seatbelt launcher");
        assert_eq!(launcher.program, "/usr/bin/sandbox-exec");
        let output = std::process::Command::new(&launcher.program)
            .args(&launcher.args)
            .envs(launcher.env.iter().cloned())
            .current_dir(&cwd)
            .output()
            .expect("sandbox-exec should run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("outside-denied"),
            "write outside the workspace must be denied; stdout={stdout} stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(cwd.join("inside.txt"))
                .expect("inside write")
                .trim(),
            "inside"
        );
        assert!(!outside_home.exists());

        let _ = std::fs::remove_dir_all(&cwd);
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_file(&outside_home);
    }

    #[test]
    fn detects_container_markers_from_multiple_sources() {
        let detected = detect_container_environment_from(SandboxDetectionInputs {
            env_pairs: vec![("container".to_string(), "docker".to_string())],
            dockerenv_exists: true,
            containerenv_exists: false,
            proc_1_cgroup: Some("12:memory:/docker/abc"),
        });

        assert!(detected.in_container);
        assert!(detected
            .markers
            .iter()
            .any(|marker| marker == "/.dockerenv"));
        assert!(detected
            .markers
            .iter()
            .any(|marker| marker == "env:container=docker"));
        assert!(detected
            .markers
            .iter()
            .any(|marker| marker == "/proc/1/cgroup:docker"));
    }

    #[test]
    fn resolves_request_with_overrides() {
        let config = SandboxConfig {
            enabled: Some(true),
            namespace_restrictions: Some(true),
            network_isolation: Some(false),
            filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
            allowed_mounts: vec!["logs".to_string()],
        };

        let request = config.resolve_request(
            Some(true),
            Some(false),
            Some(true),
            Some(FilesystemIsolationMode::AllowList),
            Some(vec!["tmp".to_string()]),
        );

        assert!(request.enabled);
        assert!(!request.namespace_restrictions);
        assert!(request.network_isolation);
        assert_eq!(request.filesystem_mode, FilesystemIsolationMode::AllowList);
        assert_eq!(request.allowed_mounts, vec!["tmp"]);
    }

    #[test]
    fn builds_linux_launcher_with_network_flag_when_requested() {
        let config = SandboxConfig::default();
        let status = super::resolve_sandbox_status_for_request(
            &config.resolve_request(
                Some(true),
                Some(true),
                Some(true),
                Some(FilesystemIsolationMode::WorkspaceOnly),
                None,
            ),
            Path::new("/workspace"),
        );

        if let Some(launcher) =
            build_linux_sandbox_command("printf hi", Path::new("/workspace"), &status)
        {
            assert_eq!(launcher.program, "unshare");
            assert!(launcher.args.iter().any(|arg| arg == "--mount"));
            assert!(launcher.args.iter().any(|arg| arg == "--net") == status.network_active);
        }
    }
}
