//! Newline-delimited JSON (NDJSON) transport framing for MCP stdio.
//!
//! Per the MCP specification (2025-03-26), stdio-based MCP servers and
//! clients exchange JSON-RPC messages as newline-delimited JSON: each
//! message is a single UTF-8 line terminated by `\n`, and messages must
//! not contain embedded newlines. `serde_json::to_vec` emits compact
//! output with no embedded newlines by default, so callers that hand a
//! serde_json-encoded payload to `write_msg` need not escape anything
//! themselves.

use std::io;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Reads a single NDJSON-framed JSON-RPC payload from `reader`.
///
/// Returns `Ok(None)` on clean EOF at a message boundary. Blank lines
/// (some implementations emit `\n` as a separator between messages) are
/// skipped silently. If the stream closes mid-line, returns
/// `UnexpectedEof`.
pub async fn read_msg<R>(reader: &mut R) -> io::Result<Option<Vec<u8>>>
where
    R: AsyncBufRead + Unpin,
{
    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            return Ok(None);
        }
        // `read_line` returns 0 only on clean EOF; if it returns > 0
        // without a trailing newline, the stream was truncated mid-line.
        if !line.ends_with('\n') {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "MCP stdio stream closed mid-line while reading message",
            ));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            continue;
        }
        return Ok(Some(trimmed.as_bytes().to_vec()));
    }
}

/// Writes a single NDJSON-framed JSON-RPC payload to `writer`.
///
/// Appends `\n` and flushes so the peer sees a complete message
/// immediately. Callers must ensure `payload` contains no embedded
/// newlines (compact `serde_json` output satisfies this by default).
pub async fn write_msg<W>(writer: &mut W, payload: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(payload).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}
