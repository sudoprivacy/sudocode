//! Whole-record appends and rotation shared by all file telemetry sinks.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Rotation {
    pub max_bytes: u64,
    pub retained_files: usize,
}

#[derive(Debug)]
pub(crate) struct JsonlFile {
    path: PathBuf,
    lock_file: Mutex<File>,
    rotation: Option<Rotation>,
}

impl JsonlFile {
    pub fn new(path: &Path, rotation: Option<Rotation>) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // This inode remains stable when the data file is renamed. Do not
        // remove it on drop: another process can already be waiting on it.
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(suffixed_path(path, ".lock"))?;
        let writer = Self {
            path: path.to_path_buf(),
            lock_file: Mutex::new(lock_file),
            rotation,
        };
        writer.with_lock(|| writer.open_current().map(|_| ()))?;
        Ok(writer)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append_json(&self, value: &impl serde::Serialize) -> io::Result<()> {
        // Serialize before taking the process lock. JSON and its delimiter
        // must travel together, including through a partial write_all.
        let mut record = serde_json::to_vec(value).map_err(io::Error::other)?;
        record.push(b'\n');
        self.with_lock(|| {
            if let Some(rotation) = self.rotation {
                if std::fs::metadata(&self.path)
                    .is_ok_and(|metadata| metadata.len() >= rotation.max_bytes)
                {
                    if let Err(error) = self.rotate(rotation.retained_files) {
                        eprintln!("[scode telemetry] Failed to rotate log: {error}");
                    }
                }
            }
            // A cached data handle could now point at another process's
            // rotated file. Open the active path while holding the same lock
            // that protects rotation, then close it before unlocking.
            let mut file = self.open_current()?;
            file.write_all(&record)?;
            file.flush()
        })
    }

    fn open_current(&self) -> io::Result<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
    }

    fn with_lock<T>(&self, action: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        let file = self
            .lock_file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        file.lock()?;
        let _unlock = Unlock(&file);
        action()
    }

    fn rotate(&self, retained_files: usize) -> io::Result<()> {
        match std::fs::remove_file(suffixed_path(&self.path, &format!(".{retained_files}"))) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        for index in (1..retained_files).rev() {
            match std::fs::rename(
                suffixed_path(&self.path, &format!(".{index}")),
                suffixed_path(&self.path, &format!(".{}", index + 1)),
            ) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        std::fs::rename(&self.path, suffixed_path(&self.path, ".1"))
    }
}

struct Unlock<'a>(&'a File);

impl Drop for Unlock<'_> {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn suffixed_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}
