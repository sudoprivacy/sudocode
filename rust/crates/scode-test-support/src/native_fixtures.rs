//! Native fault controls for owned PTY fixtures, independent of launch wrappers.

use std::path::{Path, PathBuf};

/// Verify the reported runtime belongs to this PTY before signalling it.
/// Windows shells retain a separate PID when they launch a native executable.
pub fn verify_owned_runtime_process(
    terminal: &pty_expect::PtySession,
    runtime_pid: u32,
    executable: &Path,
    fixture_root: &Path,
) -> Result<(), String> {
    let transport_pid = terminal.process_id().ok_or("PTY has no process")?;
    // Both paths use portable Rust APIs. Compile and lint both on every host;
    // only the native Windows host executes the PowerShell control process.
    if cfg!(windows) {
        let reported = runtime_pid.to_string();
        let transport = transport_pid.to_string();
        windows_control(
            fixture_root,
            &[
                "-Operation".as_ref(),
                "VerifyProcess".as_ref(),
                "-Target".as_ref(),
                reported.as_ref(),
                "-Value".as_ref(),
                transport.as_ref(),
                "-Executable".as_ref(),
                executable.as_os_str(),
            ],
        )?;
    } else if transport_pid != runtime_pid {
        return Err(format!(
            "runtime {runtime_pid} is not owned PTY {transport_pid}"
        ));
    }
    Ok(())
}

/// Make an index unreadable while its native processes and locks remain live.
/// Unix moves its directory; Windows denies enumeration without touching the
/// already-open ownership handles. Dropping the guard restores native ACL policy.
pub struct DirectoryReadFault {
    root: PathBuf,
    is_active: bool,
    // Restore the reader's original token after restoring the directory ACL.
    _read_bypass: Option<ReadBypassGuard>,
    #[cfg(not(windows))]
    hidden: PathBuf,
    #[cfg(windows)]
    fixture_root: PathBuf,
    #[cfg(windows)]
    original_acl: serde_json::Value,
}

impl DirectoryReadFault {
    /// Refuse ineffective faults and retain restoration through assertion unwinds.
    #[must_use]
    pub fn new(
        root: &Path,
        fixture_root: &Path,
        terminal: &pty_expect::PtySession,
        runtime_pid: u32,
        executable: &Path,
    ) -> Self {
        #[cfg(not(windows))]
        let _ = fixture_root;
        assert!(
            std::fs::read_dir(root).is_ok(),
            "fixture index must start readable"
        );
        let mut fault = Self {
            root: root.to_path_buf(),
            is_active: false,
            _read_bypass: if cfg!(windows) {
                Some(ReadBypassGuard::new(
                    fixture_root,
                    terminal,
                    runtime_pid,
                    executable,
                ))
            } else {
                None
            },
            #[cfg(not(windows))]
            hidden: root.with_extension("held-aside"),
            #[cfg(windows)]
            fixture_root: fixture_root.to_path_buf(),
            #[cfg(windows)]
            original_acl: native_acl_snapshot(
                &windows_control(
                    fixture_root,
                    &[
                        "-Operation".as_ref(),
                        "ReadDirectoryAcl".as_ref(),
                        "-Target".as_ref(),
                        root.as_os_str(),
                    ],
                )
                .expect("read owned directory ACL before fault"),
            )
            .expect("snapshot owned directory ACL before fault"),
        };
        #[cfg(not(windows))]
        {
            assert!(!fault.hidden.exists(), "fault destination must be fresh");
            std::fs::rename(root, &fault.hidden).expect("hide owned directory");
            fault.is_active = true;
        }
        #[cfg(windows)]
        {
            // Retain the restoration guard even if applying the ACL fails.
            fault.is_active = true;
            let denied = native_acl_snapshot(
                &windows_control(
                    fixture_root,
                    &[
                        "-Operation".as_ref(),
                        "DenyDirectoryRead".as_ref(),
                        "-Target".as_ref(),
                        root.as_os_str(),
                    ],
                )
                .expect("deny enumeration of owned directory"),
            )
            .expect("read denied native ACL policy");
            assert_ne!(
                denied["policy"], fault.original_acl["policy"],
                "native deny must change policy"
            );
        }
        fault.assert_unreadable();
        fault
    }

    /// Verify the native fault still holds across an external process query.
    pub fn assert_unreadable(&self) {
        let error = std::fs::read_dir(&self.root)
            .expect_err("native fault must actually block enumeration");
        eprintln!(
            "owned directory fault: root={}, pid={}, error={error:?}",
            self.root.display(),
            std::process::id()
        );
        #[cfg(windows)]
        {
            let actual = windows_control(
                &self.fixture_root,
                &[
                    "-Operation".as_ref(),
                    "ReadDirectoryAcl".as_ref(),
                    "-Target".as_ref(),
                    self.root.as_os_str(),
                ],
            )
            .expect("audit native ACL while the directory fault is held");
            eprintln!("owned directory fault ACL: {actual}");
        }
    }

    fn restore(&mut self) -> Result<(), String> {
        if !self.is_active {
            return Ok(());
        }
        #[cfg(not(windows))]
        std::fs::rename(&self.hidden, &self.root).map_err(|error| error.to_string())?;
        #[cfg(windows)]
        {
            let descriptor = self.original_acl["descriptor"]
                .as_str()
                .ok_or("missing original native ACL descriptor")?;
            let restored = native_acl_snapshot(&windows_control(
                &self.fixture_root,
                &[
                    "-Operation".as_ref(),
                    "RestoreDirectoryRead".as_ref(),
                    "-Target".as_ref(),
                    self.root.as_os_str(),
                    "-Value".as_ref(),
                    descriptor.as_ref(),
                ],
            )?)?;
            if restored["policy"] != self.original_acl["policy"] {
                return Err(format!(
                    "restored native ACL policy differs: original={}; restored={restored}",
                    self.original_acl
                ));
            }
        }
        self.is_active = false;
        std::fs::read_dir(&self.root).map_err(|error| error.to_string())?;
        Ok(())
    }
}

impl Drop for DirectoryReadFault {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            if std::thread::panicking() {
                eprintln!("could not restore owned directory fault: {error}");
            } else {
                panic!("could not restore owned directory fault: {error}");
            }
        }
    }
}

struct ReadBypassGuard {
    root: PathBuf,
    arguments: Vec<std::ffi::OsString>,
    original: serde_json::Value,
}

impl ReadBypassGuard {
    fn new(
        root: &Path,
        terminal: &pty_expect::PtySession,
        runtime_pid: u32,
        executable: &Path,
    ) -> Self {
        verify_owned_runtime_process(terminal, runtime_pid, executable, root)
            .expect("read privilege belongs to the exact owned native PTY runtime");
        let arguments = vec![
            "-Operation".into(),
            "SetReadBypass".into(),
            "-Target".into(),
            runtime_pid.to_string().into(),
            "-Value".into(),
            terminal.process_id().unwrap().to_string().into(),
            "-Executable".into(),
            executable.as_os_str().to_owned(),
        ];
        let raw = windows_control(
            root,
            &arguments.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
        )
        .expect("disable only the owned directory reader's backup privilege");
        let original: serde_json::Value = serde_json::from_str(&raw)
            .expect("native read privilege control returns its original state");
        assert_eq!(original["pid"].as_u64(), Some(u64::from(runtime_pid)));
        assert!(original["creation_ticks"].is_string(), "{original}");
        assert!(original["present"].is_boolean(), "{original}");
        assert!(original["was_enabled"].is_boolean(), "{original}");
        assert_eq!(original["enabled"], false, "{original}");
        eprintln!("owned directory reader backup privilege: {original}");
        Self {
            root: root.to_path_buf(),
            arguments,
            original,
        }
    }

    fn restore(&self) -> Result<(), String> {
        let mut arguments: Vec<&std::ffi::OsStr> =
            self.arguments.iter().map(AsRef::as_ref).collect();
        let original = if self.original["was_enabled"] == true {
            "Enabled"
        } else {
            "Disabled"
        };
        let creation = self.original["creation_ticks"].as_str().unwrap();
        arguments.extend(
            ["-ReadBypass", original, "-ExpectedCreationTicks", creation].map(std::ffi::OsStr::new),
        );
        let raw = windows_control(&self.root, &arguments)?;
        let restored: serde_json::Value =
            serde_json::from_str(&raw).map_err(|error| error.to_string())?;
        if restored["enabled"] != self.original["was_enabled"]
            || restored["was_enabled"] != false
            || restored["present"] != self.original["present"]
            || restored["pid"] != self.original["pid"]
            || restored["creation_ticks"] != self.original["creation_ticks"]
        {
            return Err(format!(
                "native read privilege differs: original={}; restored={restored}",
                self.original
            ));
        }
        eprintln!("restored owned directory reader backup privilege: {restored}");
        Ok(())
    }
}

impl Drop for ReadBypassGuard {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            if std::thread::panicking() {
                eprintln!("could not restore owned reader token: {error}");
            } else {
                panic!("could not restore owned reader token: {error}");
            }
        }
    }
}

fn native_acl_snapshot(raw: &str) -> Result<serde_json::Value, String> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|error| error.to_string())?;
    let policy = &value["policy"];
    let is_null_dacl = policy.get("dacl_revision") == Some(&serde_json::Value::Null)
        && policy.get("aces") == Some(&serde_json::Value::Null);
    let is_present_dacl = policy["dacl_revision"].is_u64()
        && policy["aces"]
            .as_array()
            .is_some_and(|aces| aces.iter().all(serde_json::Value::is_string));
    if !value["descriptor"].is_string()
        || !policy.is_object()
        || !policy["owner"].is_string()
        || !policy["group"].is_string()
        || !policy["control_flags"].is_u64()
        || !(is_null_dacl || is_present_dacl)
    {
        return Err(format!("invalid native ACL snapshot: {value}"));
    }
    Ok(value)
}

fn windows_control(root: &Path, args: &[&std::ffi::OsStr]) -> Result<String, String> {
    use std::io::Write;
    let script = root.join("scode-native-runtime-controls.ps1");
    let source: &[u8] = include_bytes!("native_fixtures.ps1");
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&script)
    {
        Ok(mut file) => file.write_all(source).map_err(|error| error.to_string())?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::read(&script)
                .map_err(|error| error.to_string())?
                .as_slice()
                != source
            {
                return Err("native fixture script differs from compiled source".into());
            }
        }
        Err(error) => return Err(error.to_string()),
    }
    let powershell = PathBuf::from(std::env::var_os("SystemRoot").ok_or("missing SystemRoot")?)
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let output = std::process::Command::new(powershell)
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-File"])
        .arg(script)
        .args(args)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "native fixture control failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8(output.stdout)
        .map_err(|error| error.to_string())?
        .trim()
        .to_owned())
}
