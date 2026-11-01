//! Real files and independent response lifetimes exercise the production codec.
#[path = "../../rust/crates/runtime/src/response_archive/gzip.rs"]
mod gzip;

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Barrier};
use std::thread;

struct Sink {
    file: File,
    drops: Arc<AtomicUsize>,
}

impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.file.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

struct Quota {
    file: File,
    limit: usize,
    written: usize,
}

impl Write for Quota {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.written + bytes.len() > self.limit {
            return Err(io::Error::other("session_response_budget_exhausted"));
        }
        self.file.write_all(bytes)?;
        self.written += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Job {
    index: usize,
    barrier: Option<Arc<Barrier>>,
    done: mpsc::Sender<()>,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().expect("old or new codec");
    assert!(matches!(mode.as_str(), "old" | "new"));
    let root = PathBuf::from(args.next().expect("private evidence directory")).join(&mode);
    fs::create_dir(&root).unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let mut workers = Vec::new();
    for _ in 0..6 {
        let (send, recv) = mpsc::channel::<Job>();
        let root = root.clone();
        let mode = mode.clone();
        let drops = Arc::clone(&drops);
        workers.push((
            send,
            thread::spawn(move || {
                while let Ok(job) = recv.recv() {
                    let index = job.index;
                    let size = match index % 6 {
                        0 => 0,
                        1 => 1024 * 1024,
                        _ => 600 + index,
                    };
                    let body = (0..size)
                        .map(|n| ((n * 17 + index * 71) % 256) as u8)
                        .collect::<Vec<_>>();
                    let path = root.join(format!("{index}.gz"));
                    let sink = Sink {
                        file: File::create(&path).unwrap(),
                        drops: Arc::clone(&drops),
                    };
                    let mut encoder: Box<dyn Write> = if mode == "old" {
                        Box::new(flate2::write::GzEncoder::new(
                            sink,
                            flate2::Compression::fast(),
                        ))
                    } else {
                        Box::new(gzip::Encoder::new(sink).unwrap())
                    };
                    if let Some(barrier) = job.barrier {
                        barrier.wait();
                    }
                    for part in body.chunks(613) {
                        encoder.write_all(part).unwrap();
                        encoder.flush().unwrap();
                    }
                    drop(encoder);
                    let mut decoded = Vec::new();
                    flate2::read::GzDecoder::new(File::open(path).unwrap())
                        .read_to_end(&mut decoded)
                        .unwrap();
                    assert_eq!(
                        decoded, body,
                        "CRC, payload or earlier-response state changed"
                    );
                    job.done.send(()).unwrap();
                }
            }),
        ));
    }
    for index in 0..36 {
        let (done, wait) = mpsc::channel();
        workers[index % 6]
            .0
            .send(Job {
                index,
                barrier: None,
                done,
            })
            .unwrap();
        wait.recv().unwrap();
        assert_eq!(
            drops.load(Ordering::SeqCst),
            index + 1,
            "an idle compressor retained its sink/lease"
        );
    }
    if let Ok(status) = fs::read_to_string("/proc/self/status") {
        fs::write(root.join("memory.txt"), status).unwrap();
    }
    let barrier = Arc::new(Barrier::new(7));
    let (done, wait) = mpsc::channel();
    for (worker, (send, _)) in workers.iter().enumerate() {
        send.send(Job {
            index: 36 + worker,
            barrier: Some(Arc::clone(&barrier)),
            done: done.clone(),
        })
        .unwrap();
    }
    barrier.wait();
    for _ in 0..6 {
        wait.recv().unwrap();
    }
    assert_eq!(drops.load(Ordering::SeqCst), 42);
    for (send, join) in workers {
        drop(send);
        join.join().unwrap();
    }
    for limit in [0, 5, 10, 15, 20, 30, 40, 50, 70, 1000] {
        let sink = Quota {
            file: File::create(root.join(format!("quota-{limit}.gz"))).unwrap(),
            limit,
            written: 0,
        };
        let payload = b"SSE prefix preserves bytes and budget errors";
        let (written, finished) = if mode == "old" {
            let mut encoder = flate2::write::GzEncoder::new(sink, flate2::Compression::fast());
            let written = encoder
                .write_all(payload)
                .and_then(|()| encoder.flush())
                .is_ok();
            (written, encoder.finish().is_ok())
        } else {
            let mut encoder = gzip::Encoder::new(sink).unwrap();
            let written = encoder
                .write_all(payload)
                .and_then(|()| encoder.flush())
                .is_ok();
            (written, encoder.finish().is_ok())
        };
        fs::write(
            root.join(format!("quota-{limit}.txt")),
            format!("{written} {finished}"),
        )
        .unwrap();
    }
    println!("PASS {mode}: 42 independent files, six concurrent captures, released sinks and quota boundaries");
}
