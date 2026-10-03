//! A file tool writes to the backend the HOST chose, not to local disk.
//!
//! `GlobalToolRegistry` dispatched built-in tools with a literal
//! `&StdFsBackend`, so every write went to the host filesystem regardless of
//! who was asking. That literal is precisely what a co-hosted agent cannot
//! live with: the reason to co-host is that a write reaches the kernel, where
//! the hooks, the audit trail and the permission checks are.
//!
//! The assertion that matters is the second one. Recording that the spy saw
//! the write only proves the spy was reachable; asserting that nothing
//! appeared on disk is what proves the literal is gone.

use std::io;
use std::sync::{Arc, Mutex};

use runtime::{FsBackend, FsDirEntry, FsMetadata};
use serde_json::json;
use tools::GlobalToolRegistry;

/// Records writes and refuses everything else.
///
/// Unimplemented methods panic rather than return plausible defaults: a file
/// tool that starts reading or renaming is doing something this test does not
/// describe, and a silent default would hide it.
#[derive(Default)]
struct SpyBackend {
    writes: Mutex<Vec<(String, String)>>,
}

impl FsBackend for SpyBackend {
    fn write(&self, path: &str, data: &[u8]) -> io::Result<()> {
        self.writes
            .lock()
            .unwrap()
            .push((path.to_string(), String::from_utf8_lossy(data).into_owned()));
        Ok(())
    }

    // `write_file` stats the target to report whether it created or replaced.
    // Reporting "missing" keeps this a create, which is the simplest shape.
    fn stat(&self, _: &str) -> io::Result<FsMetadata> {
        Err(io::Error::new(io::ErrorKind::NotFound, "spy: no such file"))
    }

    fn exists(&self, _: &str) -> io::Result<bool> {
        Ok(false)
    }

    fn create_dir_all(&self, _: &str) -> io::Result<()> {
        Ok(())
    }

    fn normalize_allow_missing(&self, path: &str) -> io::Result<String> {
        Ok(path.to_string())
    }

    fn normalize(&self, path: &str) -> io::Result<String> {
        Ok(path.to_string())
    }

    fn canonicalize(&self, path: &str) -> io::Result<String> {
        Ok(path.to_string())
    }

    // Read before write: `write_file` looks at the target to report whether
    // it created or replaced. "Missing" keeps this a create, matching `stat`.
    fn read(&self, _: &str) -> io::Result<Vec<u8>> {
        Err(io::Error::new(io::ErrorKind::NotFound, "spy: no such file"))
    }

    fn append(&self, _: &str, _: &[u8]) -> io::Result<()> {
        panic!("spy: write_file must not append")
    }

    fn delete(&self, _: &str) -> io::Result<()> {
        panic!("spy: write_file must not delete")
    }

    fn readdir(&self, _: &str) -> io::Result<Vec<FsDirEntry>> {
        panic!("spy: write_file must not readdir")
    }

    fn rename(&self, _: &str, _: &str) -> io::Result<()> {
        panic!("spy: write_file must not rename")
    }

    fn symlink_metadata(&self, _: &str) -> io::Result<FsMetadata> {
        panic!("spy: write_file must not symlink_metadata")
    }
}

#[test]
fn write_file_goes_to_the_injected_backend_and_not_to_disk() {
    let tmp = std::env::temp_dir().join(format!(
        "scode-fs-injection-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let target = tmp.join("written.txt");

    let spy = Arc::new(SpyBackend::default());
    let registry = GlobalToolRegistry::builtin().with_fs(spy.clone());

    registry
        .execute(
            "write_file",
            &json!({ "path": target.to_string_lossy(), "content": "from the host's backend" }),
        )
        .expect("write_file should succeed against the injected backend");

    let writes = spy.writes.lock().unwrap();
    assert_eq!(
        writes.len(),
        1,
        "expected exactly one write, got {writes:?}"
    );
    assert_eq!(writes[0].1, "from the host's backend");

    assert!(
        !target.exists(),
        "the write reached the real filesystem at {}; the tool is still \
         dispatching with a literal StdFsBackend rather than the host's",
        target.display()
    );
}
