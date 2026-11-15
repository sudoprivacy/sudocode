//! scode `peerInput` version 3: the offer, its acknowledgement tokens and its
//! windows (`docs/design/codex-mailbox-wake.md`, "Offers and
//! acknowledgements" and "scode `peerInput` version 3").
//!
//! Everything here is a pure function of its inputs: the caller reads unread
//! frames and commits registers, and this module turns those frames into the
//! exact text a model reads, the tokens that acknowledge them, and the
//! structured result the client keeps. The same frames, binding and
//! `maxBytes` always produce the same text, windows and tokens, so a frame
//! offered again shows the tokens it showed before.
//!
//! A frame's content is its records' rendered blocks joined by newlines (its
//! *body*). The header around it carries presentation facts that differ
//! between offers, such as its position in this offer, so tokens and windows
//! are taken over the body alone.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::hash::BuildHasher;

use agent_client_protocol::{JsonRpcRequest, JsonRpcResponse};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Version label every token encoding starts with.
pub const ACK_ENCODING_VERSION: &str = "scode-ack-v1";

/// Token width: the first 80 bits of the HMAC, base32 without padding.
const TOKEN_BYTES: usize = 10;

/// The instruction every result ends with, after the unread count.
pub const FOOTER_INSTRUCTION: &str = "Acknowledge a message with its ack token only after reading and \
handling it in full: call scode_inbox again with ack set to every ack token shown. If a message shows a \
truncation or omission notice, or its text seems shorter than its stated size, read it alone with \
scode_inbox({read: frameId}) instead of acknowledging it.";

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// `_sudocode/session/peer_offer` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_sudocode/session/peer_offer", response = PeerOfferResponse)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PeerOfferRequest {
    pub session_id: String,
    pub thread_id: String,
    pub max_bytes: u64,
    #[serde(default)]
    pub ack: Vec<String>,
    #[serde(default)]
    pub read: Option<ReadRequest>,
    #[serde(default)]
    pub legacy: Vec<LegacyEntry>,
}

/// One frame read alone, optionally one of its windows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadRequest {
    pub frame_id: String,
    #[serde(default)]
    pub window: u64,
    #[serde(default)]
    pub after: Option<String>,
}

/// An open version 2 entry the cutover owner passes on each call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LegacyEntry {
    pub event_id: String,
    /// The entry's `ScodePeerMessage` projection, as the version 2 inbox kept it.
    pub message: serde_json::Value,
    #[serde(default)]
    pub possibly_delivered: bool,
}

impl PeerOfferRequest {
    /// Check the field combinations the wire contract fixes: a `read` names a
    /// frame; window 0 takes no `after`; a later window requires the `next`
    /// token of the window before it.
    pub fn validate(&self) -> Result<(), PeerOfferError> {
        let invalid = |what: &str| Err(PeerOfferError::InvalidParams(what.to_owned()));
        if self.session_id.is_empty() || self.thread_id.is_empty() {
            return invalid("sessionId and threadId are required");
        }
        if self.max_bytes == 0 {
            return invalid("maxBytes must be positive");
        }
        if self.ack.iter().any(String::is_empty) {
            return invalid("ack tokens must be non-empty");
        }
        if self.legacy.iter().any(|entry| entry.event_id.is_empty()) {
            return invalid("legacy entries need an eventId");
        }
        if let Some(read) = &self.read {
            if read.frame_id.is_empty() {
                return invalid("read.frameId is required");
            }
            match (read.window, read.after.as_deref()) {
                (0, None) => {}
                (0, Some(_)) => return invalid("window 0 takes no after token"),
                (_, None | Some("")) => {
                    return invalid("a later window requires the preceding window's next token")
                }
                (_, Some(_)) => {}
            }
        }
        Ok(())
    }
}

/// `_sudocode/session/peer_offer` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonRpcResponse)]
#[serde(rename_all = "camelCase")]
pub struct PeerOfferResponse {
    pub text: String,
    pub commits: Vec<CommitReport>,
    pub frames: Vec<FrameMeta>,
    pub unread: u64,
}

/// What `ack` committed, per conversation or per legacy entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CommitReport {
    Conversation(ConversationCommit),
    Legacy(LegacyCommit),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationCommit {
    pub conversation: String,
    pub from: u64,
    pub to: u64,
    pub status: CommitStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<CommitReason>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyCommit {
    pub legacy_event_id: String,
    pub status: CommitStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<CommitReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommitStatus {
    Committed,
    Already,
    Refused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitReason {
    LeaseLost,
    PositionMoved,
    TokenInvalid,
}

/// One offered frame, as the client records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameMeta {
    pub frame_id: String,
    pub conversation: String,
    pub delivery_ids: Vec<String>,
    pub bytes: u64,
    /// `null` for a frame offered whole.
    pub window: Option<WindowMeta>,
    /// This result carries the frame's `ack` token.
    pub complete: bool,
    pub redelivery: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowMeta {
    pub index: u64,
    pub count: u64,
}

/// A refusal of the whole call, as a JSON-RPC error with `data.reason`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerOfferError {
    InvalidParams(String),
    BindingMismatch,
    UnknownFrame,
    WindowTokenInvalid,
    WindowOutOfRange,
    MaxBytesTooSmall { minimum: u64 },
    MailboxUnavailable(String),
}

impl PeerOfferError {
    /// JSON-RPC error code.
    #[must_use]
    pub fn code(&self) -> i64 {
        match self {
            Self::MailboxUnavailable(_) => -32603,
            _ => -32602,
        }
    }

    /// The `data.reason` value.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::InvalidParams(_) => "invalid_params",
            Self::BindingMismatch => "binding_mismatch",
            Self::UnknownFrame => "unknown_frame",
            Self::WindowTokenInvalid => "window_token_invalid",
            Self::WindowOutOfRange => "window_out_of_range",
            Self::MaxBytesTooSmall { .. } => "max_bytes_too_small",
            Self::MailboxUnavailable(_) => "mailbox_unavailable",
        }
    }

    /// The JSON-RPC error `data` object.
    #[must_use]
    pub fn data(&self) -> serde_json::Value {
        match self {
            Self::MaxBytesTooSmall { minimum } => {
                serde_json::json!({ "reason": self.reason(), "minimum": minimum })
            }
            _ => serde_json::json!({ "reason": self.reason() }),
        }
    }
}

impl std::fmt::Display for PeerOfferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidParams(why) => write!(f, "invalid peer_offer parameters: {why}"),
            Self::BindingMismatch => {
                f.write_str("sessionId or threadId is not this session's binding")
            }
            Self::UnknownFrame => {
                f.write_str("read.frameId is not an unread frame of this session")
            }
            Self::WindowTokenInvalid => {
                f.write_str("read.after does not verify for the preceding window")
            }
            Self::WindowOutOfRange => f.write_str("read.window is past the frame's window count"),
            Self::MaxBytesTooSmall { minimum } => {
                write!(
                    f,
                    "maxBytes cannot hold one window; the smallest that can is {minimum}"
                )
            }
            Self::MailboxUnavailable(why) => write!(f, "mailbox unavailable: {why}"),
        }
    }
}

impl std::error::Error for PeerOfferError {}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

/// The Agent's private acknowledgement key. It stays inside scode.
#[derive(Clone)]
pub struct AckKey([u8; 32]);

impl AckKey {
    #[must_use]
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl std::fmt::Debug for AckKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AckKey(..)")
    }
}

/// Who a token speaks for: the Agent, its ACP session and the client thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckBinding {
    pub actor: String,
    pub session: String,
    pub thread: String,
}

/// What a token proves when it comes back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// The model's acknowledgement of a whole frame.
    Ack,
    /// Permission to read the next window of a frame.
    Next,
    /// The model's acknowledgement of a version 2 legacy entry.
    LegacyAck,
}

impl TokenKind {
    fn label(self) -> &'static str {
        match self {
            Self::Ack => "ack",
            Self::Next => "next",
            Self::LegacyAck => "legacy-ack",
        }
    }
}

/// The frame fields a token binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenFrame<'a> {
    pub conversation: &'a str,
    pub start: u64,
    pub end: u64,
    pub sha256: &'a [u8; 32],
}

/// The window fields a `next` token binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenWindow<'a> {
    pub index: u64,
    pub count: u64,
    pub start: u64,
    pub end: u64,
    pub sha256: &'a [u8; 32],
}

fn put_str(out: &mut Vec<u8>, value: &str) {
    let len = u32::try_from(value.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(value.as_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_slice());
    out
}

impl AckKey {
    /// A frame or window token. The encoding is versioned and length-prefixed:
    /// `"scode-ack-v1"`, kind, actor, ACP session, thread, conversation, frame
    /// start, frame end, the frame body's SHA-256, then for a window its index,
    /// count, start, end and SHA-256. Strings carry a u32 big-endian length,
    /// integers are u64 big-endian and digests are their 32 raw bytes.
    #[must_use]
    pub fn token(
        &self,
        kind: TokenKind,
        binding: &AckBinding,
        frame: TokenFrame<'_>,
        window: Option<TokenWindow<'_>>,
    ) -> String {
        let mut encoding = Vec::new();
        put_str(&mut encoding, ACK_ENCODING_VERSION);
        put_str(&mut encoding, kind.label());
        put_str(&mut encoding, &binding.actor);
        put_str(&mut encoding, &binding.session);
        put_str(&mut encoding, &binding.thread);
        put_str(&mut encoding, frame.conversation);
        put_u64(&mut encoding, frame.start);
        put_u64(&mut encoding, frame.end);
        encoding.extend_from_slice(frame.sha256);
        if let Some(window) = window {
            put_u64(&mut encoding, window.index);
            put_u64(&mut encoding, window.count);
            put_u64(&mut encoding, window.start);
            put_u64(&mut encoding, window.end);
            encoding.extend_from_slice(window.sha256);
        }
        self.sign(&encoding)
    }

    /// A legacy entry's acknowledgement token: kind `legacy-ack` over the
    /// binding, the event ID and the SHA-256 of the message's JSON bytes.
    #[must_use]
    pub fn legacy_token(
        &self,
        binding: &AckBinding,
        event_id: &str,
        message_sha256: &[u8; 32],
    ) -> String {
        let mut encoding = Vec::new();
        put_str(&mut encoding, ACK_ENCODING_VERSION);
        put_str(&mut encoding, TokenKind::LegacyAck.label());
        put_str(&mut encoding, &binding.actor);
        put_str(&mut encoding, &binding.session);
        put_str(&mut encoding, &binding.thread);
        put_str(&mut encoding, event_id);
        encoding.extend_from_slice(message_sha256);
        self.sign(&encoding)
    }

    fn sign(&self, encoding: &[u8]) -> String {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.0)
            .expect("HMAC accepts a key of any length");
        mac.update(encoding);
        let tag = mac.finalize().into_bytes();
        data_encoding::BASE32_NOPAD
            .encode(&tag[..TOKEN_BYTES])
            .to_ascii_lowercase()
    }
}

// ---------------------------------------------------------------------------
// Frames and their presentation
// ---------------------------------------------------------------------------

/// One unread mailbox frame, as plain data: its conversation, its register
/// range and its records' rendered blocks (`MailboxEnvelope::to_prompt`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferFrame {
    pub conversation: String,
    pub start: u64,
    pub end: u64,
    pub delivery_ids: Vec<String>,
    pub blocks: Vec<String>,
    pub redelivery: bool,
}

impl OfferFrame {
    /// `<conversation>:<start>-<end>`.
    #[must_use]
    pub fn frame_id(&self) -> String {
        format!("{}:{}-{}", self.conversation, self.start, self.end)
    }
}

/// A frame or a legacy entry, reduced to what the presentation needs.
struct Unit<'a> {
    frame_id: String,
    conversation: &'a str,
    start: u64,
    end: u64,
    body: String,
    body_sha: [u8; 32],
    records: usize,
    delivery_ids: Vec<String>,
    redelivery: bool,
    /// `Some(event id)` for a legacy entry.
    legacy: Option<&'a str>,
    possibly_delivered: bool,
}

const LEGACY_CONVERSATION: &str = "legacy";

impl<'a> Unit<'a> {
    fn frame(frame: &'a OfferFrame) -> Self {
        let body = frame.blocks.join("\n");
        Self {
            frame_id: frame.frame_id(),
            conversation: &frame.conversation,
            start: frame.start,
            end: frame.end,
            body_sha: sha256(body.as_bytes()),
            body,
            records: frame.blocks.len(),
            delivery_ids: frame.delivery_ids.clone(),
            redelivery: frame.redelivery,
            legacy: None,
            possibly_delivered: false,
        }
    }

    fn legacy(entry: &'a LegacyEntry) -> Self {
        let body = match entry.message.get("text") {
            Some(serde_json::Value::String(text)) => text.clone(),
            _ => entry.message.to_string(),
        };
        let delivery_ids = entry
            .message
            .get("deliveryId")
            .and_then(serde_json::Value::as_str)
            .map_or_else(Vec::new, |id| vec![id.to_owned()]);
        Self {
            frame_id: format!("{LEGACY_CONVERSATION}:{}", entry.event_id),
            conversation: LEGACY_CONVERSATION,
            start: 0,
            end: u64::try_from(body.len()).unwrap_or(u64::MAX),
            body_sha: legacy_message_sha256(&entry.message),
            body,
            records: 1,
            delivery_ids,
            redelivery: false,
            legacy: Some(&entry.event_id),
            possibly_delivered: entry.possibly_delivered,
        }
    }

    fn token_frame(&self) -> TokenFrame<'_> {
        TokenFrame {
            conversation: if self.legacy.is_some() {
                &self.frame_id
            } else {
                self.conversation
            },
            start: self.start,
            end: self.end,
            sha256: &self.body_sha,
        }
    }

    fn ack_token(&self, key: &AckKey, binding: &AckBinding) -> String {
        match self.legacy {
            Some(event_id) => key.legacy_token(binding, event_id, &self.body_sha),
            None => key.token(TokenKind::Ack, binding, self.token_frame(), None),
        }
    }

    fn next_token(
        &self,
        key: &AckKey,
        binding: &AckBinding,
        window: &Window,
        count: u64,
    ) -> String {
        let sha = sha256(&self.body.as_bytes()[window.start..window.end]);
        key.token(
            TokenKind::Next,
            binding,
            self.token_frame(),
            Some(TokenWindow {
                index: window.index,
                count,
                start: u64::try_from(window.start).unwrap_or(u64::MAX),
                end: u64::try_from(window.end).unwrap_or(u64::MAX),
                sha256: &sha,
            }),
        )
    }

    fn bytes(&self) -> u64 {
        u64::try_from(self.body.len()).unwrap_or(u64::MAX)
    }

    fn header(&self, position: (u64, u64), window: Option<(u64, u64)>) -> String {
        self.header_flagged(position, window, self.redelivery, self.possibly_delivered)
    }

    fn header_flagged(
        &self,
        position: (u64, u64),
        window: Option<(u64, u64)>,
        redelivery: bool,
        possibly_delivered: bool,
    ) -> String {
        let mut flags = String::new();
        if redelivery {
            flags.push_str(" · redelivery");
        }
        if possibly_delivered {
            flags.push_str(" · possiblyDelivered");
        }
        if let Some((index, count)) = window {
            let _ = write!(flags, " · window {}/{count}", index + 1);
        }
        let kind = if self.legacy.is_some() {
            "legacy message"
        } else {
            "message"
        };
        format!(
            "⟦{kind} {}/{} · frame {} · {} bytes · {} record(s){flags}⟧\n",
            position.0,
            position.1,
            self.frame_id,
            self.bytes(),
            self.records
        )
    }

    fn whole(&self, key: &AckKey, binding: &AckBinding, position: (u64, u64)) -> String {
        format!(
            "{}{}\n⟦ack {}⟧\n",
            self.header(position, None),
            self.body,
            self.ack_token(key, binding)
        )
    }

    /// The longest a header plus window frame lines can be, whatever the
    /// position, window index, count and flags. Windows are cut against this
    /// bound, so they depend on the frame and `maxBytes` only: a frame read
    /// again as a redelivery shows the windows and tokens it showed first.
    fn window_overhead_max(&self) -> u64 {
        let header = self.header_flagged(
            (u64::MAX, u64::MAX),
            Some((u64::MAX - 1, u64::MAX)),
            true,
            true,
        );
        let token = "x".repeat(token_len());
        let lines = format!(
            "\n⟦next {token}⟧\n{}",
            continuation(&self.frame_id, u64::MAX, &token)
        );
        len_u64(&header) + len_u64(&lines) + len_u64(&footer(u64::MAX))
    }

    /// Windows of the body for `max_bytes`, cut on character boundaries.
    fn windows(&self, max_bytes: u64) -> Result<Vec<Window>, PeerOfferError> {
        let overhead = self.window_overhead_max();
        let widest = self.body.chars().map(char::len_utf8).max().unwrap_or(1);
        let minimum = overhead + u64::try_from(widest).unwrap_or(u64::MAX);
        if max_bytes < minimum {
            return Err(PeerOfferError::MaxBytesTooSmall { minimum });
        }
        let room = usize::try_from(max_bytes - overhead).unwrap_or(usize::MAX);
        let body = self.body.as_bytes();
        let mut windows = Vec::new();
        let mut start: usize = 0;
        loop {
            let mut end = start.saturating_add(room).min(body.len());
            while !self.body.is_char_boundary(end) {
                end -= 1;
            }
            windows.push(Window {
                index: u64::try_from(windows.len()).unwrap_or(u64::MAX),
                start,
                end,
            });
            if end >= body.len() {
                return Ok(windows);
            }
            start = end;
        }
    }

    fn window_text(
        &self,
        key: &AckKey,
        binding: &AckBinding,
        position: (u64, u64),
        window: &Window,
        count: u64,
    ) -> String {
        let slice = &self.body[window.start..window.end];
        let last = window.index + 1 == count;
        let header = self.header(position, Some((window.index, count)));
        if last {
            format!("{header}{slice}\n⟦ack {}⟧\n", self.ack_token(key, binding))
        } else {
            let next = self.next_token(key, binding, window, count);
            format!(
                "{header}{slice}\n⟦next {next}⟧\n{}",
                continuation(&self.frame_id, window.index + 1, &next)
            )
        }
    }

    fn meta(&self, window: Option<WindowMeta>, complete: bool) -> FrameMeta {
        FrameMeta {
            frame_id: self.frame_id.clone(),
            conversation: self.conversation.to_owned(),
            delivery_ids: self.delivery_ids.clone(),
            bytes: self.bytes(),
            window,
            complete,
            redelivery: self.redelivery,
        }
    }
}

struct Window {
    index: u64,
    start: usize,
    end: usize,
}

fn token_len() -> usize {
    data_encoding::BASE32_NOPAD.encode_len(TOKEN_BYTES)
}

fn len_u64(text: &str) -> u64 {
    u64::try_from(text.len()).unwrap_or(u64::MAX)
}

fn continuation(frame_id: &str, next_window: u64, token: &str) -> String {
    format!(
        "⟦continue with scode_inbox({{read: \"{frame_id}\", window: {next_window}, after: \"{token}\"}})⟧\n"
    )
}

/// The text every result ends with: the unread count and the instruction.
#[must_use]
pub fn footer(unread: u64) -> String {
    format!("⟦unread {unread}⟧\n{FOOTER_INSTRUCTION}\n")
}

/// The line naming `ack` tokens that matched no unread frame or legacy entry
/// of this call: issued for something already acknowledged, or not issued
/// here at all. They acknowledged nothing.
#[must_use]
pub fn unmatched_notice(unmatched: &[String]) -> String {
    format!(
        "⟦ack not recognised: {}; nothing was acknowledged for them⟧\n",
        unmatched.join(", ")
    )
}

/// Name `unmatched` tokens in `presentation`'s text, just before its footer,
/// when the line still fits `max_bytes`. Windows are cut without this line,
/// so a read's windows and tokens stay the same whether or not it carries
/// a notice.
#[must_use]
pub fn with_unmatched_notice(
    mut presentation: Presentation,
    unmatched: &[String],
    unread: u64,
    max_bytes: u64,
) -> Presentation {
    if unmatched.is_empty() {
        return presentation;
    }
    let notice = unmatched_notice(unmatched);
    let footer = footer(unread);
    let fits = len_u64(&presentation.text) + len_u64(&notice) <= max_bytes;
    if fits && presentation.text.ends_with(&footer) {
        let at = presentation.text.len() - footer.len();
        presentation.text.insert_str(at, &notice);
    }
    presentation
}

/// SHA-256 of a legacy entry's message JSON bytes (`serde_json::to_vec`).
#[must_use]
pub fn legacy_message_sha256(message: &serde_json::Value) -> [u8; 32] {
    sha256(&serde_json::to_vec(message).unwrap_or_default())
}

/// An offer's or a read's model text and the frames it presented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presentation {
    pub text: String,
    pub frames: Vec<FrameMeta>,
}

/// The next offer: legacy entries first, then frames in order, whole while
/// the entire text (every header, token and the footer) stays within
/// `max_bytes`. When the first unit does not fit whole, it is offered as its
/// first window with a `next` token instead. When a later unit does not fit,
/// the offer ends before it, and the next offer starts with it, so every
/// window is cut against the full `max_bytes`.
pub fn build_offer(
    key: &AckKey,
    binding: &AckBinding,
    legacy: &[LegacyEntry],
    frames: &[OfferFrame],
    unread: u64,
    max_bytes: u64,
) -> Result<Presentation, PeerOfferError> {
    let footer = footer(unread);
    if len_u64(&footer) > max_bytes {
        return Err(PeerOfferError::MaxBytesTooSmall {
            minimum: len_u64(&footer),
        });
    }
    let units: Vec<Unit<'_>> = legacy
        .iter()
        .map(Unit::legacy)
        .chain(frames.iter().map(Unit::frame))
        .collect();
    // Budget against the widest possible position so the final numbering
    // can only shorten the text.
    let widest = (u64::MAX, u64::MAX);
    let mut used = len_u64(&footer);
    let mut whole = 0;
    for unit in &units {
        let needed = len_u64(&unit.whole(key, binding, widest));
        if used.saturating_add(needed) > max_bytes {
            break;
        }
        used += needed;
        whole += 1;
    }
    if whole == 0 {
        if let Some(first) = units.first() {
            let windows = first.windows(max_bytes)?;
            let count = u64::try_from(windows.len()).unwrap_or(u64::MAX);
            let text = first.window_text(key, binding, (1, 1), &windows[0], count) + &footer;
            let complete = count == 1;
            return Ok(Presentation {
                text,
                frames: unit_meta(first, Some(WindowMeta { index: 0, count }), complete),
            });
        }
    }
    let total = u64::try_from(whole).unwrap_or(u64::MAX);
    let mut text = String::new();
    let mut metas = Vec::new();
    for (position, unit) in units.iter().take(whole).enumerate() {
        let position = u64::try_from(position).unwrap_or(u64::MAX) + 1;
        text.push_str(&unit.whole(key, binding, (position, total)));
        metas.extend(unit_meta(unit, None, true));
    }
    text.push_str(&footer);
    Ok(Presentation {
        text,
        frames: metas,
    })
}

/// Legacy entries are reported through `commits`, not `frames`, which list
/// mailbox frames only.
fn unit_meta(unit: &Unit<'_>, window: Option<WindowMeta>, complete: bool) -> Vec<FrameMeta> {
    if unit.legacy.is_some() {
        Vec::new()
    } else {
        vec![unit.meta(window, complete)]
    }
}

/// What a `read` names: a mailbox frame or a legacy entry.
pub enum ReadTarget<'a> {
    Frame(&'a OfferFrame),
    Legacy(&'a LegacyEntry),
}

/// One unit read alone. Window 0 of a unit that fits whole returns it whole.
/// Window `k > 0` requires `after` to be window `k - 1`'s `next` token.
pub fn read_window(
    key: &AckKey,
    binding: &AckBinding,
    target: &ReadTarget<'_>,
    window: u64,
    after: Option<&str>,
    unread: u64,
    max_bytes: u64,
) -> Result<Presentation, PeerOfferError> {
    let unit = match target {
        ReadTarget::Frame(frame) => Unit::frame(frame),
        ReadTarget::Legacy(entry) => Unit::legacy(entry),
    };
    let footer = footer(unread);
    if window == 0 {
        let whole = unit.whole(key, binding, (1, 1));
        if len_u64(&whole) + len_u64(&footer) <= max_bytes {
            return Ok(Presentation {
                text: whole + &footer,
                frames: unit_meta(&unit, None, true),
            });
        }
    }
    let windows = unit.windows(max_bytes)?;
    let count = u64::try_from(windows.len()).unwrap_or(u64::MAX);
    let index = usize::try_from(window).map_err(|_| PeerOfferError::WindowOutOfRange)?;
    let Some(current) = windows.get(index) else {
        return Err(PeerOfferError::WindowOutOfRange);
    };
    if index > 0 {
        let expected = unit.next_token(key, binding, &windows[index - 1], count);
        if after != Some(expected.as_str()) {
            return Err(PeerOfferError::WindowTokenInvalid);
        }
    }
    let complete = window + 1 == count;
    Ok(Presentation {
        text: unit.window_text(key, binding, (1, 1), current, count) + &footer,
        frames: unit_meta(
            &unit,
            Some(WindowMeta {
                index: window,
                count,
            }),
            complete,
        ),
    })
}

// ---------------------------------------------------------------------------
// Acknowledgements
// ---------------------------------------------------------------------------

/// A frame's acknowledgement token.
#[must_use]
pub fn frame_ack_token(key: &AckKey, binding: &AckBinding, frame: &OfferFrame) -> String {
    Unit::frame(frame).ack_token(key, binding)
}

/// A legacy entry's acknowledgement token.
#[must_use]
pub fn legacy_ack_token(key: &AckKey, binding: &AckBinding, entry: &LegacyEntry) -> String {
    Unit::legacy(entry).ack_token(key, binding)
}

/// The longest run of one conversation's unread `frames`, in order and from
/// the first, whose `ack` tokens are all in `acks`: the register range
/// `(register_start, end of the last acknowledged frame)` to commit. `None`
/// when the first frame is not acknowledged.
#[must_use]
pub fn acknowledged_run<S: BuildHasher>(
    key: &AckKey,
    binding: &AckBinding,
    register_start: u64,
    frames: &[OfferFrame],
    acks: &HashSet<String, S>,
) -> Option<(u64, u64)> {
    let mut end = None;
    for frame in frames {
        if !acks.contains(&frame_ack_token(key, binding, frame)) {
            break;
        }
        end = Some(frame.end);
    }
    end.map(|end| (register_start, end))
}

/// The event IDs of `legacy` entries whose `legacy-ack` tokens are in `acks`.
#[must_use]
pub fn acknowledged_legacy<S: BuildHasher>(
    key: &AckKey,
    binding: &AckBinding,
    legacy: &[LegacyEntry],
    acks: &HashSet<String, S>,
) -> Vec<String> {
    legacy
        .iter()
        .filter(|entry| acks.contains(&legacy_ack_token(key, binding, entry)))
        .map(|entry| entry.event_id.clone())
        .collect()
}

/// Tokens in `acks` that acknowledge none of the given unread frames or
/// legacy entries. A token for a frame committed earlier lands here too.
#[must_use]
pub fn unmatched_acks<S: BuildHasher>(
    key: &AckKey,
    binding: &AckBinding,
    frames: &[&OfferFrame],
    legacy: &[LegacyEntry],
    acks: &HashSet<String, S>,
) -> Vec<String> {
    let known: HashSet<String> = frames
        .iter()
        .map(|frame| frame_ack_token(key, binding, frame))
        .chain(
            legacy
                .iter()
                .map(|entry| legacy_ack_token(key, binding, entry)),
        )
        .collect();
    let mut unmatched: Vec<String> = acks
        .iter()
        .filter(|token| !known.contains(*token))
        .cloned()
        .collect();
    unmatched.sort();
    unmatched
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> AckKey {
        AckKey::new([7; 32])
    }

    #[test]
    fn a_redelivered_frame_keeps_its_windows_and_tokens() {
        let mut body = String::new();
        for i in 0..300 {
            let _ = writeln!(body, "line {i} 窗口 🙂");
        }
        let first = OfferFrame {
            conversation: "peer".into(),
            start: 0,
            end: 9000,
            delivery_ids: vec!["d".into()],
            blocks: vec![body],
            redelivery: false,
        };
        let again = OfferFrame {
            redelivery: true,
            ..first.clone()
        };
        let offered = build_offer(&key(), &binding(), &[], &[first], 1, 1500).unwrap();
        let next = offered
            .text
            .split("after: \"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap()
            .to_owned();
        let read = read_window(
            &key(),
            &binding(),
            &ReadTarget::Frame(&again),
            1,
            Some(&next),
            1,
            1500,
        );
        assert!(read.is_ok(), "{read:?}");
        assert_eq!(
            offered.frames[0].window.map(|window| window.count),
            read.unwrap().frames[0].window.map(|window| window.count)
        );
    }

    fn binding() -> AckBinding {
        AckBinding {
            actor: "claude-abc".into(),
            session: "session-1".into(),
            thread: "thread-1".into(),
        }
    }

    fn frame(conversation: &str, start: u64, end: u64, body: &str) -> OfferFrame {
        OfferFrame {
            conversation: conversation.into(),
            start,
            end,
            delivery_ids: vec![format!("d-{start}")],
            blocks: vec![format!(
                "<mailbox-message from=\"peer\">{body}</mailbox-message>"
            )],
            redelivery: false,
        }
    }

    fn tokens_in(text: &str, kind: &str) -> Vec<String> {
        let marker = format!("⟦{kind} ");
        text.match_indices(&marker)
            .map(|(at, _)| {
                text[at + marker.len()..]
                    .split('⟧')
                    .next()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn tokens_are_deterministic_and_bound_to_the_binding() {
        let f = frame("c1", 0, 40, "hello");
        let first = frame_ack_token(&key(), &binding(), &f);
        assert_eq!(first, frame_ack_token(&key(), &binding(), &f));
        assert_eq!(first.len(), 16);
        assert!(first
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        for other in [
            AckBinding {
                thread: "thread-2".into(),
                ..binding()
            },
            AckBinding {
                session: "session-2".into(),
                ..binding()
            },
            AckBinding {
                actor: "claude-xyz".into(),
                ..binding()
            },
        ] {
            assert_ne!(first, frame_ack_token(&key(), &other, &f));
        }
        assert_ne!(
            first,
            frame_ack_token(&AckKey::new([8; 32]), &binding(), &f)
        );
        assert_ne!(
            first,
            frame_ack_token(&key(), &binding(), &frame("c1", 0, 41, "hello"))
        );
    }

    #[test]
    fn a_whole_offer_carries_ack_tokens_and_stays_within_budget() {
        let frames = vec![frame("c1", 0, 10, "one"), frame("c1", 10, 20, "two")];
        let offer = build_offer(&key(), &binding(), &[], &frames, 2, 64 * 1024).unwrap();
        assert!(offer.text.len() <= 64 * 1024);
        assert_eq!(tokens_in(&offer.text, "ack").len(), 2);
        assert!(offer.text.contains("⟦message 1/2 · frame c1:0-10 · "));
        assert!(offer.text.ends_with(&footer(2)));
        assert!(offer
            .frames
            .iter()
            .all(|meta| meta.complete && meta.window.is_none()));
        let again = build_offer(&key(), &binding(), &[], &frames, 2, 64 * 1024).unwrap();
        assert_eq!(offer, again, "same inputs, same offer");
    }

    #[test]
    fn an_offer_stops_before_a_frame_that_no_longer_fits() {
        let frames = vec![
            frame("c1", 0, 10, "one"),
            frame("c1", 10, 20, &"x".repeat(5000)),
        ];
        let offer = build_offer(&key(), &binding(), &[], &frames, 2, 2000).unwrap();
        assert!(offer.text.len() <= 2000);
        assert_eq!(offer.frames.len(), 1);
        assert_eq!(offer.frames[0].frame_id, "c1:0-10");
    }

    #[test]
    fn an_oversized_frame_is_read_in_windows_ending_with_its_ack() {
        // Multi-byte characters everywhere so window edges fall mid-character.
        let body: String = "🙂中文ab".repeat(400);
        let f = frame("c1", 0, 99, &body);
        let max = 1200;
        let offer = build_offer(&key(), &binding(), &[], std::slice::from_ref(&f), 1, max).unwrap();
        assert!(offer.text.len() <= usize::try_from(max).unwrap());
        assert!(
            tokens_in(&offer.text, "ack").is_empty(),
            "window 0 carries no ack"
        );
        let meta = offer.frames[0].window.unwrap();
        assert_eq!(meta.index, 0);
        assert!(meta.count > 1);
        let mut after = tokens_in(&offer.text, "next").pop().unwrap();
        let mut seen = String::new();
        let first = Unit::frame(&f).windows(max).unwrap();
        seen.push_str(&f.blocks[0][first[0].start..first[0].end]);
        for k in 1..meta.count {
            let read = read_window(
                &key(),
                &binding(),
                &ReadTarget::Frame(&f),
                k,
                Some(&after),
                1,
                max,
            )
            .unwrap();
            assert!(read.text.len() <= usize::try_from(max).unwrap());
            let windows = Unit::frame(&f).windows(max).unwrap();
            let w = &windows[usize::try_from(k).unwrap()];
            seen.push_str(&f.blocks[0][w.start..w.end]);
            if k + 1 == meta.count {
                assert_eq!(
                    tokens_in(&read.text, "ack"),
                    vec![frame_ack_token(&key(), &binding(), &f)]
                );
                assert!(read.frames[0].complete);
            } else {
                after = tokens_in(&read.text, "next").pop().unwrap();
                assert!(!read.frames[0].complete);
            }
        }
        assert_eq!(
            seen, f.blocks[0],
            "windows cover the body exactly once, on char boundaries"
        );
        // Deterministic windows.
        let again = build_offer(&key(), &binding(), &[], std::slice::from_ref(&f), 1, max).unwrap();
        assert_eq!(offer.text, again.text);
    }

    #[test]
    fn window_reads_check_order_range_and_budget() {
        let f = frame("c1", 0, 99, &"y".repeat(5000));
        let max = 1000;
        let target = ReadTarget::Frame(&f);
        let count = u64::try_from(Unit::frame(&f).windows(max).unwrap().len()).unwrap();
        assert_eq!(
            read_window(
                &key(),
                &binding(),
                &target,
                1,
                Some("wrongwrongwrong1"),
                0,
                max
            ),
            Err(PeerOfferError::WindowTokenInvalid)
        );
        assert_eq!(
            read_window(&key(), &binding(), &target, count, Some("x"), 0, max),
            Err(PeerOfferError::WindowOutOfRange)
        );
        let Err(PeerOfferError::MaxBytesTooSmall { minimum }) =
            read_window(&key(), &binding(), &target, 0, None, 0, 50)
        else {
            panic!("a 50-byte budget cannot hold a window");
        };
        assert!(
            read_window(&key(), &binding(), &target, 0, None, 0, minimum).is_ok(),
            "the minimum is enough"
        );
        assert!(read_window(&key(), &binding(), &target, 0, None, 0, minimum - 1).is_err());
    }

    #[test]
    fn a_frame_that_fits_is_read_whole() {
        let f = frame("c1", 0, 10, "short");
        let read =
            read_window(&key(), &binding(), &ReadTarget::Frame(&f), 0, None, 3, 4096).unwrap();
        assert_eq!(
            tokens_in(&read.text, "ack"),
            vec![frame_ack_token(&key(), &binding(), &f)]
        );
        assert_eq!(read.frames[0].window, None);
    }

    #[test]
    fn acknowledged_runs_are_prefixes() {
        let frames = vec![
            frame("c1", 5, 10, "a"),
            frame("c1", 10, 20, "b"),
            frame("c1", 20, 30, "c"),
        ];
        let token = |i: usize| frame_ack_token(&key(), &binding(), &frames[i]);
        let acks: HashSet<String> = [token(0), token(2)].into_iter().collect();
        assert_eq!(
            acknowledged_run(&key(), &binding(), 0, &frames, &acks),
            Some((0, 10))
        );
        let all: HashSet<String> = (0..3).map(token).collect();
        assert_eq!(
            acknowledged_run(&key(), &binding(), 0, &frames, &all),
            Some((0, 30))
        );
        let later: HashSet<String> = [token(1)].into_iter().collect();
        assert_eq!(
            acknowledged_run(&key(), &binding(), 0, &frames, &later),
            None
        );
        let stray: HashSet<String> = [token(0), "zzzzzzzzzzzzzzzz".to_owned()]
            .into_iter()
            .collect();
        let refs: Vec<&OfferFrame> = frames.iter().collect();
        assert_eq!(
            unmatched_acks(&key(), &binding(), &refs, &[], &stray),
            vec!["zzzzzzzzzzzzzzzz"]
        );
    }

    #[test]
    fn legacy_entries_render_first_and_verify() {
        let entry = LegacyEntry {
            event_id: "0b8810f5-0000-4000-8000-000000000001".into(),
            message: serde_json::json!({"text": "<mailbox-message from=\"peer\">old</mailbox-message>", "deliveryId": "dd"}),
            possibly_delivered: true,
        };
        let frames = vec![frame("c1", 0, 10, "new")];
        let offer = build_offer(
            &key(),
            &binding(),
            std::slice::from_ref(&entry),
            &frames,
            1,
            8192,
        )
        .unwrap();
        let legacy_at = offer.text.find("legacy message 1/2").unwrap();
        let frame_at = offer.text.find("message 2/2 · frame c1:0-10").unwrap();
        assert!(legacy_at < frame_at);
        assert!(offer.text.contains("possiblyDelivered"));
        assert_eq!(offer.frames.len(), 1, "frames list mailbox frames only");
        let acks: HashSet<String> = tokens_in(&offer.text, "ack").into_iter().collect();
        assert_eq!(
            acknowledged_legacy(&key(), &binding(), std::slice::from_ref(&entry), &acks),
            vec![entry.event_id.clone()]
        );
        let changed = LegacyEntry {
            message: serde_json::json!({"text": "other"}),
            ..entry.clone()
        };
        assert_eq!(
            acknowledged_legacy(&key(), &binding(), &[changed], &acks),
            Vec::<String>::new()
        );
    }

    #[test]
    fn requests_validate_their_combinations() {
        let parse = |json: serde_json::Value| serde_json::from_value::<PeerOfferRequest>(json);
        let base = serde_json::json!({"sessionId": "s", "threadId": "t", "maxBytes": 65536});
        assert!(parse(base.clone()).unwrap().validate().is_ok());
        let mut extra = base.clone();
        extra["unknown"] = serde_json::json!(1);
        assert!(parse(extra).is_err(), "unknown fields are refused");
        let with_read = |read: serde_json::Value| {
            let mut value = base.clone();
            value["read"] = read;
            parse(value).unwrap().validate()
        };
        assert!(with_read(serde_json::json!({"frameId": "c:0-1"})).is_ok());
        assert!(
            with_read(serde_json::json!({"frameId": "c:0-1", "window": 2, "after": "tok"})).is_ok()
        );
        for bad in [
            serde_json::json!({"frameId": "c:0-1", "after": "tok"}),
            serde_json::json!({"frameId": "c:0-1", "window": 1}),
            serde_json::json!({"frameId": ""}),
        ] {
            assert!(matches!(
                with_read(bad),
                Err(PeerOfferError::InvalidParams(_))
            ));
        }
        let mut zero = base.clone();
        zero["maxBytes"] = serde_json::json!(0);
        assert!(parse(zero).unwrap().validate().is_err());
    }

    #[test]
    fn responses_and_errors_use_the_contract_names() {
        let response = PeerOfferResponse {
            text: "t".into(),
            commits: vec![
                CommitReport::Conversation(ConversationCommit {
                    conversation: "c1".into(),
                    from: 0,
                    to: 10,
                    status: CommitStatus::Committed,
                    reason: None,
                }),
                CommitReport::Conversation(ConversationCommit {
                    conversation: "c2".into(),
                    from: 4,
                    to: 9,
                    status: CommitStatus::Refused,
                    reason: Some(CommitReason::PositionMoved),
                }),
                CommitReport::Legacy(LegacyCommit {
                    legacy_event_id: "e1".into(),
                    status: CommitStatus::Already,
                    reason: None,
                }),
            ],
            frames: vec![FrameMeta {
                frame_id: "c1:0-10".into(),
                conversation: "c1".into(),
                delivery_ids: vec!["d".into()],
                bytes: 3,
                window: None,
                complete: true,
                redelivery: false,
            }],
            unread: 5,
        };
        assert_eq!(
            serde_json::to_value(&response).unwrap(),
            serde_json::json!({
                "text": "t",
                "commits": [
                    {"conversation": "c1", "from": 0, "to": 10, "status": "committed"},
                    {"conversation": "c2", "from": 4, "to": 9, "status": "refused", "reason": "position_moved"},
                    {"legacyEventId": "e1", "status": "already"}
                ],
                "frames": [{"frameId": "c1:0-10", "conversation": "c1", "deliveryIds": ["d"], "bytes": 3,
                            "window": null, "complete": true, "redelivery": false}],
                "unread": 5
            })
        );
        let windowed = FrameMeta {
            window: Some(WindowMeta { index: 1, count: 3 }),
            ..response.frames[0].clone()
        };
        assert_eq!(
            serde_json::to_value(&windowed).unwrap()["window"],
            serde_json::json!({"index": 1, "count": 3})
        );
        assert_eq!(
            (
                PeerOfferError::MaxBytesTooSmall { minimum: 700 }.code(),
                PeerOfferError::MaxBytesTooSmall { minimum: 700 }.data()
            ),
            (
                -32602,
                serde_json::json!({"reason": "max_bytes_too_small", "minimum": 700})
            )
        );
        assert_eq!(
            PeerOfferError::MailboxUnavailable("x".into()).code(),
            -32603
        );
        for (error, reason) in [
            (
                PeerOfferError::InvalidParams(String::new()),
                "invalid_params",
            ),
            (PeerOfferError::BindingMismatch, "binding_mismatch"),
            (PeerOfferError::UnknownFrame, "unknown_frame"),
            (PeerOfferError::WindowTokenInvalid, "window_token_invalid"),
            (PeerOfferError::WindowOutOfRange, "window_out_of_range"),
            (
                PeerOfferError::MailboxUnavailable(String::new()),
                "mailbox_unavailable",
            ),
        ] {
            assert_eq!(error.reason(), reason);
        }
    }
}
