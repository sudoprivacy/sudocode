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
    #[cfg(windows)]
    {
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
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (executable, fixture_root);
        if transport_pid == runtime_pid {
            Ok(())
        } else {
            Err(format!(
                "runtime {runtime_pid} is not owned PTY {transport_pid}"
            ))
        }
    }
}

/// Make an index unreadable while its native processes and locks remain live.
/// Unix moves its directory; Windows denies enumeration without touching the
/// already-open ownership handles. Dropping the guard restores the exact state.
pub struct DirectoryReadFault {
    root: PathBuf,
    is_active: bool,
    #[cfg(not(windows))]
    hidden: PathBuf,
    #[cfg(windows)]
    fixture_root: PathBuf,
    #[cfg(windows)]
    original_acl: String,
}

impl DirectoryReadFault {
    /// Refuse ineffective faults and retain restoration through assertion unwinds.
    #[must_use]
    pub fn new(root: &Path, fixture_root: &Path) -> Self {
        #[cfg(not(windows))]
        let _ = fixture_root;
        assert!(
            std::fs::read_dir(root).is_ok(),
            "fixture index must start readable"
        );
        let mut fault = Self {
            root: root.to_path_buf(),
            is_active: false,
            #[cfg(not(windows))]
            hidden: root.with_extension("held-aside"),
            #[cfg(windows)]
            fixture_root: fixture_root.to_path_buf(),
            #[cfg(windows)]
            original_acl: windows_control(
                fixture_root,
                &[
                    "-Operation".as_ref(),
                    "ReadDirectoryAcl".as_ref(),
                    "-Target".as_ref(),
                    root.as_os_str(),
                ],
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
            windows_control(
                fixture_root,
                &[
                    "-Operation".as_ref(),
                    "DenyDirectoryRead".as_ref(),
                    "-Target".as_ref(),
                    root.as_os_str(),
                ],
            )
            .expect("deny enumeration of owned directory");
        }
        assert!(
            std::fs::read_dir(root).is_err(),
            "native fault must actually block enumeration"
        );
        fault
    }

    fn restore(&mut self) -> Result<(), String> {
        if !self.is_active {
            return Ok(());
        }
        #[cfg(not(windows))]
        std::fs::rename(&self.hidden, &self.root).map_err(|error| error.to_string())?;
        #[cfg(windows)]
        {
            let restored = windows_control(
                &self.fixture_root,
                &[
                    "-Operation".as_ref(),
                    "RestoreDirectoryRead".as_ref(),
                    "-Target".as_ref(),
                    self.root.as_os_str(),
                    "-Value".as_ref(),
                    self.original_acl.as_ref(),
                ],
            )?;
            if restored != self.original_acl {
                return Err("restored directory ACL differs from its snapshot".into());
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

#[cfg(windows)]
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
