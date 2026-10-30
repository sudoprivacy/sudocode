//! Preserve reqwest semantics while recording body bytes before adapter parsing.
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use bytes::Bytes;
use telemetry::{ResponseCaptureEnd, ResponseCaptureMetadata, ResponseRecorder, SessionTracer};

const MAX_PENDING_BYTES: usize = 1024 * 1024;
const FLUSH_INTERVAL: Duration = Duration::from_millis(20);

#[inline]
fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Default)]
struct CaptureDrain {
    active: Mutex<usize>,
    completed: Condvar,
}

type DrainRegistry = Mutex<HashMap<String, Weak<CaptureDrain>>>;

#[inline]
fn drains() -> &'static DrainRegistry {
    static DRAINS: OnceLock<DrainRegistry> = OnceLock::new();
    DRAINS.get_or_init(DrainRegistry::default)
}

struct CaptureLease(Arc<CaptureDrain>);

impl CaptureLease {
    #[inline]
    fn new(session_id: &str) -> Self {
        let mut registry = lock(drains());
        registry.retain(|_, value| value.strong_count() > 0);
        let drain = registry
            .get(session_id)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let drain = Arc::new(CaptureDrain::default());
                registry.insert(session_id.to_owned(), Arc::downgrade(&drain));
                drain
            });
        *lock(&drain.active) += 1;
        Self(drain)
    }
}

impl Drop for CaptureLease {
    fn drop(&mut self) {
        *lock(&self.0.active) -= 1;
        self.0.completed.notify_all();
    }
}

/// Drain this session's logs at host shutdown, including earlier model clients.
/// Streaming and rendering never call this wait.
pub fn flush_response_captures(session_id: &str, timeout: Duration) -> bool {
    let Some(drain) = lock(drains()).get(session_id).and_then(Weak::upgrade) else {
        return true;
    };
    let deadline = Instant::now() + timeout;
    let mut active = lock(&drain.active);
    while *active != 0 {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        (active, _) = drain
            .completed
            .wait_timeout(active, remaining)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    true
}

#[derive(Default)]
struct CaptureState {
    pending: Vec<u8>,
    observed: u64,
    end: Option<&'static str>,
    error: Option<&'static str>,
    disabled: bool,
    ready_at: Option<Instant>,
}

#[derive(Default)]
struct CaptureQueue {
    state: Mutex<CaptureState>,
}

impl CaptureQueue {
    #[inline]
    fn lock(&self) -> MutexGuard<'_, CaptureState> {
        lock(&self.state)
    }
}

type CaptureWork = Option<Box<PendingCapture>>;

struct PendingCapture {
    queue: Arc<CaptureQueue>,
    tracer: SessionTracer,
    metadata: Option<ResponseCaptureMetadata>,
    recorder: Option<Box<dyn ResponseRecorder>>,
    _lease: CaptureLease,
}

impl PendingCapture {
    fn flush(&mut self, deadline: &mut Option<Instant>) -> bool {
        if let Some(metadata) = self.metadata.take() {
            self.recorder = self.tracer.begin_response_capture(metadata);
        }
        let Some(recorder) = self.recorder.as_mut() else {
            let mut state = self.queue.lock();
            state.disabled = true;
            state.pending = Vec::new();
            return false;
        };
        let (batch, end) = {
            let mut state = self.queue.lock();
            if state.end.is_none() {
                if state.pending.is_empty() {
                    return true;
                }
                if state.pending.len() < 64 * 1024 {
                    if let Some(ready_at) = state.ready_at.filter(|at| *at > Instant::now()) {
                        *deadline = Some(deadline.map_or(ready_at, |at| at.min(ready_at)));
                        return true;
                    }
                }
            }
            let end = state
                .end
                .map(|reason| (reason, state.observed, state.error));
            state.ready_at = None;
            (std::mem::take(&mut state.pending), end)
        };
        // The HTTP reader never waits for compression, fencing or filesystem I/O.
        if !batch.is_empty() {
            recorder.record(&batch);
        }
        if let Some((reason, observed_bytes, capture_error)) = end {
            recorder.finish(ResponseCaptureEnd {
                reason,
                observed_bytes,
                capture_error,
            });
            return false;
        }
        true
    }
}

fn run_capture_worker(incoming: &Receiver<CaptureWork>) {
    let mut active = Vec::<PendingCapture>::new();
    let mut deadline: Option<Instant> = None;
    loop {
        let next = match deadline {
            Some(at) => incoming.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => incoming.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match next {
            Ok(Some(capture)) => active.push(*capture),
            Ok(None) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        active.extend(incoming.try_iter().flatten().map(|capture| *capture));
        deadline = None;
        active.retain_mut(|capture| capture.flush(&mut deadline));
    }
}

/// One lazily started writer multiplexes response queues without occupying the
/// engine's blocking pool while an HTTP response is still open.
fn capture_worker() -> Option<&'static Sender<CaptureWork>> {
    static WORKER: OnceLock<Option<Sender<CaptureWork>>> = OnceLock::new();
    WORKER
        .get_or_init(|| {
            let (sender, incoming) = mpsc::channel();
            std::thread::Builder::new()
                .name("response-writer".into())
                .spawn(move || run_capture_worker(&incoming))
                .ok()
                .map(|_| sender)
        })
        .as_ref()
}

struct BackgroundCapture {
    queue: Arc<CaptureQueue>,
    sender: Option<&'static Sender<CaptureWork>>,
}

impl std::fmt::Debug for BackgroundCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundCapture").finish_non_exhaustive()
    }
}

impl BackgroundCapture {
    #[inline]
    fn new(tracer: SessionTracer, metadata: ResponseCaptureMetadata) -> Self {
        let queue = Arc::new(CaptureQueue::default());
        let capture = Box::new(PendingCapture {
            queue: Arc::clone(&queue),
            _lease: CaptureLease::new(tracer.session_id()),
            tracer,
            metadata: Some(metadata),
            recorder: None,
        });
        let sender = capture_worker();
        let unavailable = match sender {
            Some(sender) => sender.send(Some(capture)).err().and_then(|error| error.0),
            None => Some(capture),
        };
        if let Some(capture) = unavailable {
            capture.tracer.record(
                "response_capture_unavailable",
                serde_json::Map::from_iter([(
                    "reason".into(),
                    serde_json::Value::String("response_writer_unavailable".into()),
                )]),
            );
            queue.lock().disabled = true;
        }
        Self { queue, sender }
    }

    #[inline]
    fn record(&mut self, bytes: &[u8]) {
        let mut state = self.queue.lock();
        if state.disabled || state.end.is_some() {
            return;
        }
        state.observed = state.observed.saturating_add(bytes.len() as u64);
        if state.error.is_some() {
            return;
        }
        let was_empty = state.pending.is_empty();
        if was_empty {
            state.ready_at = Some(Instant::now() + FLUSH_INTERVAL);
        }
        let length = bytes.len().min(MAX_PENDING_BYTES - state.pending.len());
        let required = state.pending.len() + length;
        if required > state.pending.capacity() {
            let capacity = required.next_power_of_two().clamp(8192, MAX_PENDING_BYTES);
            let additional = capacity - state.pending.len();
            state.pending.reserve_exact(additional);
        }
        state.pending.extend_from_slice(&bytes[..length]);
        if length < bytes.len() {
            // Preserve a contiguous prefix and explicitly report missing bytes.
            state.error = Some("response_capture_queue_saturated");
        }
        drop(state);
        if was_empty {
            self.notify();
        }
    }

    #[inline]
    fn finish(&mut self, reason: &'static str) {
        let first = {
            let mut state = self.queue.lock();
            let first = state.end.is_none();
            state.end.get_or_insert(reason);
            first
        };
        if first {
            self.notify();
        }
    }

    fn notify(&self) {
        if let Some(sender) = self.sender {
            let _ = sender.send(None);
        }
    }
}

impl Drop for BackgroundCapture {
    fn drop(&mut self) {
        self.finish("body_reader_dropped");
    }
}

#[derive(Debug)]
pub struct RecordedResponse {
    inner: reqwest::Response,
    recorder: Option<BackgroundCapture>,
}

impl RecordedResponse {
    pub(crate) fn new(
        inner: reqwest::Response,
        capture: Option<(SessionTracer, ResponseCaptureMetadata)>,
    ) -> Self {
        Self {
            inner,
            recorder: capture.map(|(tracer, metadata)| BackgroundCapture::new(tracer, metadata)),
        }
    }

    pub async fn chunk(&mut self) -> reqwest::Result<Option<Bytes>> {
        let result = self.inner.chunk().await;
        if let Some(recorder) = &mut self.recorder {
            match &result {
                Ok(Some(bytes)) => recorder.record(bytes),
                Ok(None) => recorder.finish("http_eof"),
                Err(_) => recorder.finish("transport_error"),
            }
        }
        result
    }

    pub async fn bytes(self) -> reqwest::Result<Bytes> {
        let Self {
            inner,
            mut recorder,
        } = self;
        let result = inner.bytes().await;
        if let Some(recorder) = &mut recorder {
            match &result {
                Ok(bytes) => {
                    recorder.record(bytes);
                    recorder.finish("http_eof");
                }
                Err(_) => recorder.finish("transport_error"),
            }
        }
        result
    }

    pub async fn text(self) -> reqwest::Result<String> {
        if self.recorder.is_none() {
            return self.inner.text().await;
        }
        // Record before decoding, then preserve reqwest's charset/BOM handling.
        let headers = self.inner.headers().clone();
        let mut decoded = http::Response::new(self.bytes().await?);
        *decoded.headers_mut() = headers;
        reqwest::Response::from(decoded).text().await
    }
}

impl Deref for RecordedResponse {
    type Target = reqwest::Response;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
