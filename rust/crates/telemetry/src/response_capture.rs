//! Storage-neutral recording of model response bodies, separate from telemetry.

use std::fmt::Debug;
use std::io;

use serde::Serialize;

/// An allowlist of correlation metadata; never includes authentication headers.
#[derive(Debug, Clone, Serialize)]
pub struct ResponseCaptureMetadata {
    pub request_id: String,
    pub model: Option<String>,
    pub endpoint: String,
    pub status: u16,
    pub content_type: String,
    pub provider_request_id: Option<String>,
    pub gateway_request_id: Option<String>,
    pub started_at_ms: u64,
}

pub trait ResponseRecorder: Debug + Send + Sync {
    fn record(&mut self, bytes: &[u8]);
    fn finish(&mut self, end: ResponseCaptureEnd<'_>);
}

pub struct ResponseCaptureEnd<'a> {
    pub reason: &'a str,
    pub observed_bytes: u64,
    pub capture_error: Option<&'a str>,
}

/// The session owns durable placement; HTTP readers supply unmodified bytes.
pub trait ResponseArchive: Send + Sync {
    fn begin(&self, metadata: ResponseCaptureMetadata) -> io::Result<Box<dyn ResponseRecorder>>;
}
