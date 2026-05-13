//! LSP-style JSON-RPC transport framing.
//!
//! Provides utilities for reading and writing messages with `Content-Length`
//! headers, matching the framing used by the Model Context Protocol (MCP) and
//! Language Server Protocol (LSP).

use std::io;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Reads a single LSP-framed JSON-RPC payload from `reader`.
///
/// Returns `Ok(None)` on clean EOF before any header bytes have been read.
/// Returns `Err` if the stream closes mid-header or if the `Content-Length`
/// header is missing or invalid.
pub async fn read_msg<R>(reader: &mut R) -> io::Result<Option<Vec<u8>>>
where
    R: AsyncBufRead + Unpin,
{
    let mut content_length: Option<usize> = None;
    let mut first_header = true;
    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            if first_header {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "JSON-RPC stream closed while reading headers",
            ));
        }
        first_header = false;
        if line == "\r\n" || line == "\n" {
            break;
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if let Some((name, value)) = header.split_once(':') {
            if name.trim().eq_ignore_ascii_case("Content-Length") {
                let parsed = value
                    .trim()
                    .parse::<usize>()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                content_length = Some(parsed);
            }
        }
    }

    let content_length = content_length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header")
    })?;
    let mut payload = vec![0_u8; content_length];
    reader.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

/// Writes a single LSP-framed JSON-RPC payload to `writer`.
///
/// Encodes the payload with a `Content-Length` header followed by two
/// CRLFs, matching LSP and MCP conventions.
pub async fn write_msg<W>(writer: &mut W, payload: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let header = format!("Content-Length: {}\r\n\r\n", payload.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(payload).await?;
    writer.flush().await
}
