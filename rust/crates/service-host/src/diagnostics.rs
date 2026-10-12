use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

struct Log {
    path: PathBuf,
    file: Option<File>,
    size: u64,
}

impl Log {
    fn open(path: PathBuf) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path,
            file: Some(file),
            size,
        })
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.size + bytes.len() as u64 > MAX_LOG_BYTES {
            self.file.take();
            let previous = self.path.with_file_name("runtime.previous.log");
            if previous.exists() {
                fs::remove_file(&previous)?;
            }
            fs::rename(&self.path, previous)?;
            self.file = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?,
            );
            self.size = 0;
        }
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("diagnostic log is closed"))?
            .write_all(bytes)?;
        self.size += bytes.len() as u64;
        Ok(())
    }
}

fn pump(input: impl Read + Send + 'static, log: Arc<Mutex<Log>>) -> JoinHandle<io::Result<()>> {
    std::thread::spawn(move || {
        let mut input = input;
        let mut bytes = [0_u8; 8192];
        let mut failure = None;
        loop {
            let count = input.read(&mut bytes)?;
            if count == 0 {
                return failure.map_or(Ok(()), Err);
            }
            if failure.is_none() {
                if let Err(error) = log
                    .lock()
                    .map_err(|_| io::Error::other("diagnostic log lock poisoned"))?
                    .append(&bytes[..count])
                {
                    // Continue draining the child if diagnostics become unavailable.
                    failure = Some(error);
                }
            }
        }
    })
}

/// Drain both child pipes into one bounded pair of diagnostic files.
pub(crate) fn attach(
    child: &mut Child,
    root: &Path,
) -> io::Result<Vec<JoinHandle<io::Result<()>>>> {
    let log = Arc::new(Mutex::new(Log::open(root.join("run/runtime.log"))?));
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("runtime stdout is not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("runtime stderr is not piped"))?;
    Ok(vec![pump(stdout, log.clone()), pump(stderr, log)])
}
