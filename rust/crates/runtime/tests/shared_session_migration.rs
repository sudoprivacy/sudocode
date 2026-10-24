//! Coordinate two real stream writers across the legacy migration boundary.
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use kernel::kernel::Kernel;
use runtime::fs_backend::{FsDirEntry, FsMetadata};
use runtime::{FsBackend, KernelFsBackend, Session};

struct PausedRead {
    inner: Arc<dyn FsBackend>,
    pause: AtomicBool,
    captured: mpsc::SyncSender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl FsBackend for PausedRead {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        let bytes = self.inner.read(path)?;
        if self.pause.swap(false, Ordering::AcqRel) {
            self.captured.send(()).unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
        }
        Ok(bytes)
    }
    fn write(&self, path: &str, data: &[u8]) -> io::Result<()> {
        self.inner.write(path, data)
    }
    fn append(&self, path: &str, data: &[u8]) -> io::Result<()> {
        self.inner.append(path, data)
    }
    fn delete(&self, path: &str) -> io::Result<()> {
        self.inner.delete(path)
    }
    fn stat(&self, path: &str) -> io::Result<FsMetadata> {
        self.inner.stat(path)
    }
    fn readdir(&self, path: &str) -> io::Result<Vec<FsDirEntry>> {
        self.inner.readdir(path)
    }
    fn create_dir_all(&self, path: &str) -> io::Result<()> {
        self.inner.create_dir_all(path)
    }
    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.inner.rename(from, to)
    }
    fn canonicalize(&self, path: &str) -> io::Result<String> {
        self.inner.canonicalize(path)
    }
    fn symlink_metadata(&self, path: &str) -> io::Result<FsMetadata> {
        self.inner.symlink_metadata(path)
    }
    fn create_append_log(&self, path: &str, retention: u64) -> io::Result<()> {
        self.inner.create_append_log(path, retention)
    }
    fn is_append_stream(&self, path: &str) -> io::Result<bool> {
        self.inner.is_append_stream(path)
    }
}

#[test]
fn delayed_stream_migration_preserves_a_newer_continuation() {
    let kernel = Arc::new(Kernel::new());
    kernel.vfs_router_arc().add_mount(
        "/",
        "root",
        Some(Arc::new(runtime::test_support::MemObjectStore::default())),
        false,
    );
    let path = std::path::Path::new("/workspace/transcript.jsonl");
    kernel
        .create_stream(path.to_str().unwrap(), 1 << 20)
        .unwrap();
    let fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent_descriptor(
        kernel,
        &kernel::core::agents::registry::AgentDescriptor {
            name: "agent-a".into(),
            owner_id: "test-owner".into(),
            zone_id: "root".into(),
            ..Default::default()
        },
        "/workspace",
    ));
    let mut legacy = Session::new();
    legacy.push_user_text("legacy prefix").unwrap();
    let mut snapshot: serde_json::Value =
        serde_json::from_str(&legacy.to_json().unwrap().render()).unwrap();
    for message in snapshot["messages"].as_array_mut().unwrap() {
        message.as_object_mut().unwrap().remove("history");
    }
    fs.append(path.to_str().unwrap(), format!("{snapshot}\n").as_bytes())
        .unwrap();
    let mut first = Session::load_from_path_with(fs.as_ref(), path)
        .unwrap()
        .with_fs_backend(Arc::clone(&fs));
    let second = Session::load_from_path_with(fs.as_ref(), path).unwrap();
    let (captured_tx, captured_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::channel();
    let paused = Arc::new(PausedRead {
        inner: Arc::clone(&fs),
        pause: AtomicBool::new(true),
        captured: captured_tx,
        release: Mutex::new(release_rx),
    });
    let delayed = std::thread::spawn(move || {
        let mut second = second.with_fs_backend(paused);
        second.activate_persistence().unwrap();
        second.save_to_path(path).unwrap();
    });
    captured_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    first.activate_persistence().unwrap();
    first.push_user_text("newer continuation").unwrap();
    release_tx.send(()).unwrap();
    delayed.join().unwrap();
    let resumed = Session::load_from_path_with(fs.as_ref(), path).unwrap();
    assert_eq!(resumed.messages().len(), 2);
    assert_eq!(resumed.messages(), first.messages());
}
