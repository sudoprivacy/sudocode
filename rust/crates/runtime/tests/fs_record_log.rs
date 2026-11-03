//! Record transactions across real processes, including process death mid-write.
use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use runtime::fs_backend::{FsBackend, RecordLog, RecordLogMode, StdFsBackend};
use serde_json::{json, Value};

#[path = "common/live_nexus.rs"]
mod live_nexus;
use live_nexus::backend as live_backend;

const WORKER_DIR: &str = "SCODE_RECORD_LOG_WORKER_DIR";
const WORKER_ID: &str = "SCODE_RECORD_LOG_WORKER_ID";

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("scode-records-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Worker(Child);

impl Worker {
    fn spawn(root: &Path, id: &str, test: &str) -> Self {
        Self(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test, "--nocapture"])
                .env(WORKER_DIR, root)
                .env(WORKER_ID, id)
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }

    fn finish(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success(), "record worker failed: {status}");
                return;
            }
            assert!(Instant::now() < deadline, "record worker did not finish");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn encode(value: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    bytes
}

fn wait_for_marker(worker: &mut Worker, path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        assert!(
            worker.0.try_wait().unwrap().is_none(),
            "record writer exited"
        );
        assert!(
            Instant::now() < deadline,
            "record writer did not become ready"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn native_snapshot_waits_for_another_process_to_finish_its_commit() {
    if let Ok(root) = std::env::var(WORKER_DIR) {
        let root = Path::new(&root);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(root.join("events.jsonl"))
            .unwrap();
        file.lock().unwrap();
        file.write_all(b"{\"committed\":").unwrap();
        std::fs::write(root.join("snapshot-ready"), b"ready").unwrap();
        while !root.join("snapshot-release").exists() {
            thread::sleep(Duration::from_millis(10));
        }
        file.write_all(b"\"after-lock\"}\n").unwrap();
        file.sync_data().unwrap();
        return;
    }
    let fixture = Fixture::new();
    let path = fixture.0.join("events.jsonl");
    let mut writer = Worker::spawn(
        &fixture.0,
        "snapshot",
        "native_snapshot_waits_for_another_process_to_finish_its_commit",
    );
    wait_for_marker(&mut writer, &fixture.0.join("snapshot-ready"));
    #[cfg(windows)]
    assert_eq!(
        StdFsBackend
            .read(&path.to_string_lossy())
            .unwrap_err()
            .raw_os_error(),
        Some(33)
    );
    let (started, running) = std::sync::mpsc::sync_channel(1);
    let (ready, result) = std::sync::mpsc::sync_channel(1);
    let reader = thread::spawn(move || {
        let fs: std::sync::Arc<dyn FsBackend> = std::sync::Arc::new(StdFsBackend);
        started.send(()).unwrap();
        ready
            .send(fs.read_snapshot(&path.to_string_lossy()))
            .unwrap();
    });
    running.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(
            result.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ),
        "snapshot bypassed the writer's commit lock"
    );
    std::fs::write(fixture.0.join("snapshot-release"), b"commit").unwrap();
    writer.finish();
    assert_eq!(
        result
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap(),
        b"{\"committed\":\"after-lock\"}\n"
    );
    reader.join().unwrap();
}

#[test]
fn native_legacy_writes_wait_for_shared_snapshot_readers() {
    if let Ok(root) = std::env::var(WORKER_DIR) {
        let root = Path::new(&root);
        let path = root.join("events.jsonl");
        std::fs::write(root.join("legacy-ready"), b"ready").unwrap();
        match std::env::var(WORKER_ID).unwrap().as_str() {
            "append" => StdFsBackend
                .append(&path.to_string_lossy(), b"{\"after\":true}\n")
                .unwrap(),
            "recover" => StdFsBackend
                .recover_append_log(&path.to_string_lossy())
                .unwrap(),
            operation => panic!("unknown legacy operation: {operation}"),
        }
        return;
    }

    for operation in ["append", "recover"] {
        let fixture = Fixture::new();
        let path = fixture.0.join("events.jsonl");
        let initial = if operation == "append" {
            b"{\"before\":true}\n".as_slice()
        } else {
            b"{\"before\":true}\n{\"partial\":".as_slice()
        };
        std::fs::write(&path, initial).unwrap();
        let mut snapshot = std::fs::File::open(&path).unwrap();
        snapshot.lock_shared().unwrap();
        let mut worker = Worker::spawn(
            &fixture.0,
            operation,
            "native_legacy_writes_wait_for_shared_snapshot_readers",
        );
        wait_for_marker(&mut worker, &fixture.0.join("legacy-ready"));
        let deadline = Instant::now() + Duration::from_millis(100);
        while Instant::now() < deadline {
            assert!(
                worker.0.try_wait().unwrap().is_none(),
                "legacy {operation} bypassed or failed on the snapshot lock"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let mut observed = Vec::new();
        std::io::Read::read_to_end(&mut snapshot, &mut observed).unwrap();
        assert_eq!(
            observed, initial,
            "snapshot bytes changed during {operation}"
        );
        snapshot.unlock().unwrap();
        worker.finish();
        assert_eq!(
            std::fs::read(path).unwrap(),
            if operation == "append" {
                b"{\"before\":true}\n{\"after\":true}\n".as_slice()
            } else {
                b"{\"before\":true}\n".as_slice()
            },
            "legacy {operation} must finish after the reader releases its lock"
        );
    }
}

#[test]
fn native_records_preserve_large_concurrent_process_writes() {
    if let Ok(root) = std::env::var(WORKER_DIR) {
        let id = std::env::var(WORKER_ID).unwrap();
        let log = StdFsBackend
            .record_log(
                &Path::new(&root).join("events.jsonl").to_string_lossy(),
                RecordLogMode::Existing,
            )
            .unwrap();
        for index in 0..8 {
            log.append(&encode(&json!({
                "worker": id, "index": index, "payload": "x\nλ".repeat(65_536),
            })))
            .unwrap();
        }
        return;
    }
    let fixture = Fixture::new();
    let path = fixture.0.join("events.jsonl");
    let log = StdFsBackend
        .record_log(&path.to_string_lossy(), RecordLogMode::CreateNew)
        .unwrap();
    assert_eq!(
        StdFsBackend
            .record_log(&path.to_string_lossy(), RecordLogMode::CreateNew)
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::AlreadyExists,
    );
    let mut workers: Vec<_> = (0..4)
        .map(|id| {
            Worker::spawn(
                &fixture.0,
                &id.to_string(),
                "native_records_preserve_large_concurrent_process_writes",
            )
        })
        .collect();
    for worker in &mut workers {
        worker.finish();
    }
    let mut cursor = 0;
    let mut seen = BTreeSet::new();
    loop {
        let (bytes, next) = log.read(cursor).unwrap();
        if bytes.is_empty() {
            assert_eq!(next, cursor);
            break;
        }
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let value: Value = serde_json::from_slice(line).unwrap();
            assert_eq!(value["payload"], "x\nλ".repeat(65_536));
            assert!(seen.insert((
                value["worker"].as_str().unwrap().to_owned(),
                value["index"].as_u64().unwrap(),
            )));
        }
        assert!(next > cursor);
        cursor = next;
    }
    assert_eq!(seen.len(), 32);
    assert_eq!(std::fs::metadata(path).unwrap().len(), cursor);
}

#[test]
fn native_record_recovery_preserves_commits_after_process_death() {
    if let Ok(root) = std::env::var(WORKER_DIR) {
        let path = Path::new(&root).join("events.jsonl");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        file.lock().unwrap();
        file.seek(SeekFrom::End(0)).unwrap();
        file.write_all(b"{\"uncommitted\":\"").unwrap();
        file.write_all(&vec![b'x'; 256 * 1024]).unwrap();
        file.sync_data().unwrap();
        std::fs::write(Path::new(&root).join("partial-ready"), b"ready").unwrap();
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }
    let fixture = Fixture::new();
    let path = fixture.0.join("events.jsonl");
    let log = StdFsBackend
        .record_log(&path.to_string_lossy(), RecordLogMode::CreateNew)
        .unwrap();
    let first = encode(&json!({"committed": "before-crash"}));
    let cursor = log.append(&first).unwrap();
    let mut worker = Worker::spawn(
        &fixture.0,
        "partial",
        "native_record_recovery_preserves_commits_after_process_death",
    );
    wait_for_marker(&mut worker, &fixture.0.join("partial-ready"));
    worker.0.kill().unwrap();
    worker.0.wait().unwrap();
    assert_eq!(log.read(cursor).unwrap(), (Vec::new(), cursor));
    let second = encode(&json!({"committed": "after-crash"}));
    let next = log.append(&second).unwrap();
    assert_eq!(log.read(cursor).unwrap(), (second.clone(), next));
    let mut expected = first;
    expected.extend(second);
    assert_eq!(std::fs::read(path).unwrap(), expected);
    assert!(log.append(b"{\"partial\":true}").is_err());
    assert!(log.append(b"{}\n{}\n").is_err());
    assert_eq!(log.read(next).unwrap(), (Vec::new(), next));
}

fn read_wal_records(log: &dyn RecordLog, count: usize) -> (Vec<Value>, u64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut records = Vec::new();
    let mut cursor = 0;
    while records.len() < count {
        let (bytes, next) = log.read(cursor).expect("read committed WAL frame");
        if bytes.is_empty() {
            assert_eq!(next, cursor);
            assert!(
                Instant::now() < deadline,
                "committed WAL history did not arrive"
            );
            thread::sleep(Duration::from_millis(25));
            continue;
        }
        assert!(next > cursor);
        records.push(serde_json::from_slice(&bytes).expect("one complete JSON record per frame"));
        cursor = next;
    }
    (records, cursor)
}

#[test]
#[ignore = "real mTLS daemon; e2e/nexus-a2a/run.sh supplies the fixture"]
fn live_record_log_commits_large_frames_from_independent_clients() {
    let fs = live_backend();
    let root = format!("/agents/live-probe/records-{}", uuid::Uuid::new_v4());
    fs.create_dir_all(&root).unwrap();
    let path = format!("{root}/events");
    let log = fs.record_log(&path, RecordLogMode::CreateNew).unwrap();
    let alias = format!("{root}-alias");
    fs.link(&alias, &root).unwrap();
    let alias_path = format!("{alias}/events");
    let alias_log = fs
        .record_log(&alias_path, RecordLogMode::Existing)
        .expect("a directory link opens the same committed record log");
    assert_eq!(
        fs.record_log(&alias_path, RecordLogMode::CreateNew)
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::AlreadyExists,
    );
    let second_path = format!("{alias}/second");
    let second = fs
        .record_log(&second_path, RecordLogMode::CreateNew)
        .unwrap();
    let marker = json!({"through_directory_link": true});
    second.append(&encode(&marker)).unwrap();
    let canonical = fs
        .record_log(&format!("{root}/second"), RecordLogMode::Existing)
        .unwrap();
    assert_eq!(read_wal_records(&*canonical, 1).0, vec![marker]);
    assert_eq!(
        fs.record_log(&path, RecordLogMode::CreateNew)
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::AlreadyExists,
    );
    let regular = format!("{root}/regular");
    fs.write(&regular, b"preserve regular file").unwrap();
    assert!(fs.record_log(&regular, RecordLogMode::Existing).is_err());
    assert!(fs.record_log(&regular, RecordLogMode::CreateNew).is_err());
    assert_eq!(fs.read(&regular).unwrap(), b"preserve regular file");
    let regular_alias = format!("{root}/regular-alias");
    fs.link(&regular_alias, &regular).unwrap();
    assert_eq!(
        fs.record_log(&regular_alias, RecordLogMode::Existing)
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::InvalidData,
    );
    assert_eq!(fs.read(&regular).unwrap(), b"preserve regular file");
    let writers: Vec<_> =
        (0..4)
            .map(|worker| {
                let path = path.clone();
                thread::spawn(move || {
                    let writer = live_backend()
                        .record_log(&path, RecordLogMode::Existing)
                        .unwrap();
                    let mut receipts = Vec::new();
                    for index in 0..8 {
                        receipts.push(
                        writer.append(&encode(&json!({
                            "worker": worker, "index": index, "payload": "λ\nx".repeat(32_768),
                        }))).unwrap(),
                    );
                    }
                    receipts
                })
            })
            .collect();
    let receipts: BTreeSet<_> = writers
        .into_iter()
        .flat_map(|writer| writer.join().unwrap())
        .collect();
    assert_eq!(
        receipts.len(),
        32,
        "each committed frame has its own cursor"
    );
    let (records, cursor) = read_wal_records(&*log, 32);
    assert_eq!(read_wal_records(&*alias_log, 32), (records.clone(), cursor));
    let mut seen = BTreeSet::new();
    for record in &records {
        assert_eq!(record["payload"], "λ\nx".repeat(32_768));
        assert!(seen.insert((
            record["worker"].as_u64().unwrap(),
            record["index"].as_u64().unwrap()
        )));
    }
    assert_eq!(seen.len(), 32);
    assert_eq!(Some(&cursor), receipts.last());
    assert_eq!(log.read(cursor).unwrap(), (Vec::new(), cursor));
    let receipt = std::env::var("NEXUS_RECORD_LOG_RECEIPT").expect("restart receipt path");
    std::fs::write(
        receipt,
        serde_json::to_vec(&json!({"path": path, "cursor": cursor})).unwrap(),
    )
    .unwrap();
}

#[test]
#[ignore = "real daemon restart; e2e/nexus-a2a/run.sh owns stop/start"]
fn live_record_log_recovers_after_daemon_restart() {
    let receipt = std::env::var("NEXUS_RECORD_LOG_RECEIPT").expect("restart receipt path");
    let before: Value = serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
    let log = live_backend()
        .record_log(before["path"].as_str().unwrap(), RecordLogMode::Existing)
        .unwrap();
    let (records, cursor) = read_wal_records(&*log, 32);
    let mut seen = BTreeSet::new();
    for record in records {
        assert_eq!(record["payload"], "λ\nx".repeat(32_768));
        assert!(seen.insert((
            record["worker"].as_u64().unwrap(),
            record["index"].as_u64().unwrap()
        )));
    }
    assert_eq!(seen.len(), 32, "restart must retain every committed frame");
    assert_eq!(cursor, before["cursor"].as_u64().unwrap());
    let marker = json!({"after_restart": uuid::Uuid::new_v4().to_string()});
    let after = log.append(&encode(&marker)).unwrap();
    assert!(
        after > cursor,
        "restart must preserve the store-owned cursor"
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (bytes, next) = log.read(cursor).unwrap();
        if bytes.is_empty() {
            assert_eq!(next, cursor);
            assert!(
                Instant::now() < deadline,
                "post-restart append did not become readable"
            );
            thread::sleep(Duration::from_millis(25));
            continue;
        }
        assert_eq!(next, after);
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), marker);
        break;
    }
    std::fs::write(
        Path::new(&receipt).with_extension("after-restart.json"),
        serde_json::to_vec(
            &json!({"result":"PASS", "cursor_before": cursor, "cursor_after":after}),
        )
        .unwrap(),
    )
    .unwrap();
}
