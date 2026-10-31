# Session response recordings

Saved sessions record incoming model response bodies by default. Recording runs
in the shared HTTP reader before provider parsing. It preserves the body bytes
received by scode, including SSE events, signed thinking, usage, errors inside
an accepted stream, and an incomplete tail. It does not reconstruct bytes that
an upstream gateway removed, or save HTTP transfer chunk timing.

## Files and correlation

For a current session at `<session>/transcript.jsonl`, recordings live in
`<session>/responses/`. Legacy flat transcripts use
`<sessions>/responses/<session-id>/`. Both use the session's filesystem backend,
including hosted sessions. An independent attachment writer shares the session
deletion protocol, while transcript writers keep their existing concurrency.
Deleting a managed session also removes its response
attachments.

Each accepted streaming request produces an immutable `.sse.gz` body and a
small JSON metadata file. A gateway which returns JSON to a streaming request
is recorded exactly as received; the metadata carries its actual content type.
The JSON files form the recording index. They contain the local request ID,
model when available, endpoint without query parameters, HTTP status, provider
and gateway IDs, byte counts and end reason. Authorization headers are excluded.
Recordings stay in session storage and are never included in model messages.
They are separate from the conversation transcript: compaction neither reads
nor summarizes them, and normal session display does not render them. Compacting
or resuming a session preserves existing recording files unchanged.

## Streaming and resource use

The HTTP reader copies bytes into a bounded background queue. The existing
blocking pool performs archive setup, gzip compression and filesystem writes,
batching small chunks over 20 ms. Between batches, a response retains its
recorder without parking a blocking thread while the network is idle. A queue
lock makes the handoff atomic: new bytes or completion either reach the running
worker or schedule the next worker, with at most one worker using that recorder.
The queue holds at most 1 MiB; a worker batch holds at most another
1 MiB, in addition to the compressor's state. Existing gzip bodies are not
decompressed during session loading, rendering, compaction or model requests.
Storage-budget scanning and log writes use the attachment fence and do not hold
the transcript writer.

If storage cannot keep up and the queue fills, capture retains a contiguous
prefix, continues counting observed bytes, and marks the final metadata with
`response_capture_queue_saturated`. It does not wait for queue capacity or grow
the queue. Deletion acquires the attachment fence before marking the session
deleted, so an actively written recording can make cleanup report writer-busy.
Shared disk or remote-storage load still has a cost; these bounds are not a
claim of zero overhead on every backend.

Client destruction waits up to two seconds for pending session logs. A timeout
produces a local `response_capture_shutdown_timeout` trace; it does not cancel
filesystem work already running in the host's blocking worker pool.
During normal streaming, final metadata can briefly lag rendered output. An
abrupt process kill can lose queued bytes as well as leave an unfinished gzip.

The real PTY case `raw_stream_capture_keeps_streaming_and_resume_responsive`
prints startup, first-text, turn and exit timings with logging disabled,
enabled, and enabled with 1,000 existing logs. These are local timing samples;
they do not establish a hardware-independent latency budget or a live provider
cache hit.

To inspect a body:

```python
import gzip
from pathlib import Path

recording = next(Path("<session>/responses").glob("*.sse.gz"))
print(gzip.decompress(recording.read_bytes()).decode("utf-8", errors="replace"))
```

`gzip_complete` says the compressed file was finalized. `saved_all_observed_bytes`
says storage retained every byte read by the client. Neither alone proves that
the upstream finished its protocol. Check the raw terminal events and
`end_reason`: `http_eof`, `transport_error`, or `body_reader_dropped`. A provider
adapter may normally stop consuming HTTP after its protocol terminal event;
`body_reader_dropped` does not by itself mean a failed model response. An abrupt
process kill can leave the initial `recording` metadata and a partial gzip file.

## Limits and disabling

The defaults retain at most 32 MiB of raw bytes per response and allow 256 MiB
of response evidence per session. Session accounting includes existing files
and a conservative 16 KiB metadata reservation per new recording. When full,
existing recordings are preserved and new capture is skipped. Byte limits and
storage failures affect evidence, not model output; skipped recording is noted
as `response_capture_unavailable` in the local request trace.

Override these values in any settings scope:

```json
{
  "responseRecording": {
    "enabled": true,
    "maxResponseBytes": 33554432,
    "maxSessionBytes": 268435456
  }
}
```

Set `responseRecording.enabled` to `false` to stop creating recordings for that
scope. This does not remove earlier evidence. Raw response content has the same
sensitivity as the conversation; keep personal recordings out of bug reports
and CI artifacts unless their contents have been reviewed.
