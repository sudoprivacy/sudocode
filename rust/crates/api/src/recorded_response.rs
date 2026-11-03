//! Preserve reqwest semantics while recording body bytes before adapter parsing.
use std::collections::HashMap;
use std::ops::Deref;
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
    running: bool,
}

struct CaptureQueue {
    state: Mutex<CaptureState>,
    ready: Condvar,
    worker: Mutex<CaptureWorker>,
    runtime: tokio::runtime::Handle,
}

struct CaptureWorker {
    tracer: SessionTracer,
    metadata: Option<ResponseCaptureMetadata>,
    recorder: Option<Box<dyn ResponseRecorder>>,
    lease: Option<CaptureLease>,
}

impl CaptureQueue {
    #[inline]
    fn lock(&self) -> MutexGuard<'_, CaptureState> {
        lock(&self.state)
    }

    fn run(&self) {
        // Keep the recorder between batches, not a parked blocking worker.
        // A different archive can still finalize while this one acquires its fence.
        let mut worker = lock(&self.worker);
        if worker.lease.is_none() {
            return;
        }
        if let Some(metadata) = worker.metadata.take() {
            worker.recorder = worker.tracer.begin_response_capture(metadata);
        }
        let Some(recorder) = worker.recorder.as_mut() else {
            let mut state = self.lock();
            state.disabled = true;
            state.pending = Vec::new();
            worker.lease.take();
            return;
        };
        loop {
            let (batch, end) = {
                let mut state = self.lock();
                if state.pending.is_empty() && state.end.is_none() {
                    state.running = false;
                    return;
                }
                // Batch small network chunks; an end notification flushes immediately.
                if state.end.is_none()
                    && !state.pending.is_empty()
                    && state.pending.len() < 64 * 1024
                {
                    (state, _) = self
                        .ready
                        .wait_timeout(state, FLUSH_INTERVAL)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                let end = state
                    .end
                    .map(|reason| (reason, state.observed, state.error));
                (std::mem::take(&mut state.pending), end)
            };
            // No queue mutex is held during compression, fencing or filesystem I/O.
            if !batch.is_empty() {
                recorder.record(&batch);
            }
            if let Some((reason, observed_bytes, capture_error)) = end {
                recorder.finish(ResponseCaptureEnd {
                    reason,
                    observed_bytes,
                    capture_error,
                });
                worker.recorder.take();
                worker.lease.take();
                return;
            }
        }
    }

    fn schedule(self: &Arc<Self>) {
        let queue = Arc::clone(self);
        self.runtime.spawn_blocking(move || queue.run());
    }
}

struct BackgroundCapture {
    queue: Arc<CaptureQueue>,
}

impl std::fmt::Debug for BackgroundCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundCapture").finish_non_exhaustive()
    }
}

impl BackgroundCapture {
    #[inline]
    fn new(tracer: SessionTracer, metadata: ResponseCaptureMetadata) -> Self {
        let lease = CaptureLease::new(tracer.session_id());
        let queue = Arc::new(CaptureQueue {
            state: Mutex::new(CaptureState {
                running: true,
                ..CaptureState::default()
            }),
            ready: Condvar::new(),
            worker: Mutex::new(CaptureWorker {
                tracer,
                metadata: Some(metadata),
                recorder: None,
                lease: Some(lease),
            }),
            runtime: tokio::runtime::Handle::current(),
        });
        queue.schedule();
        Self { queue }
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
        let schedule = !std::mem::replace(&mut state.running, true);
        drop(state);
        if was_empty {
            self.queue.ready.notify_one();
        }
        if schedule {
            self.queue.schedule();
        }
    }

    #[inline]
    fn finish(&mut self, reason: &'static str) {
        let mut state = self.queue.lock();
        if state.disabled || state.end.is_some() {
            return;
        }
        state.end = Some(reason);
        let schedule = !std::mem::replace(&mut state.running, true);
        drop(state);
        self.queue.ready.notify_one();
        if schedule {
            self.queue.schedule();
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
