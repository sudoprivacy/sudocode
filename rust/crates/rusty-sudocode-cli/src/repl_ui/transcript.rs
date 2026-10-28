//! Disposable disk-backed presentation. Sessions remain the durable record.
//! Records have backward links, so opening/scrolling history needs no in-memory
//! index. Only nearby wrapped records and the current visible frame are cached.

use std::{
    collections::VecDeque,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    sync::Arc,
};

use crossterm::style::ContentStyle;

use crate::render::{
    prompt_style,
    styled_text::{AnsiDecoder, StyledText},
    text_layout,
};

const NONE: u64 = u64::MAX;
const HEADER_BYTES: u64 = 32;
const MAX_RECORD_BYTES: usize = 32 * 1024;
const CACHE_BYTES: usize = 512 * 1024;
const CACHE_ENTRIES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Anchor {
    record: u64,
    byte: usize,
}

type FrameKey = (usize, usize, u64, Option<Anchor>);

struct Record {
    id: u64,
    previous: u64,
    next: u64,
    prompt: u64,
    is_prompt: bool,
    rows: Vec<(usize, Arc<StyledText>)>,
    bytes: usize,
}

#[derive(Clone)]
pub(super) struct Frame {
    pub header: Arc<StyledText>,
    pub body: Arc<StyledText>,
}

pub(super) struct Transcript {
    file: File,
    write_buffer: Vec<u8>,
    end: u64,
    last: u64,
    prompt: u64,
    pending: StyledText,
    pending_is_prompt: bool,
    parser: vte::Parser,
    decoder: AnsiDecoder,
    cache: VecDeque<Arc<Record>>,
    cache_bytes: usize,
    width: usize,
    reading: Option<Anchor>,
    first_visible: Option<Anchor>,
    revision: u64,
    frame: Option<(FrameKey, Frame)>,
    caption: Option<(u64, usize, Arc<StyledText>)>,
}

impl Transcript {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            file: tempfile::tempfile()?,
            write_buffer: Vec::new(),
            end: 0,
            last: NONE,
            prompt: NONE,
            pending: StyledText::default(),
            pending_is_prompt: false,
            parser: vte::Parser::new(),
            decoder: AnsiDecoder::new(),
            cache: VecDeque::new(),
            cache_bytes: 0,
            width: 0,
            reading: None,
            first_visible: None,
            revision: 0,
            frame: None,
            caption: None,
        })
    }

    pub fn is_reading(&self) -> bool {
        self.reading.is_some()
    }

    pub fn begin_prompt(&mut self) -> io::Result<()> {
        if !self.pending.text.is_empty() {
            self.flush_pending()?;
        }
        self.pending_is_prompt = true;
        Ok(())
    }

    pub fn append(&mut self, text: &str, terminated: bool) -> io::Result<()> {
        // Bound decoder working memory even when a tool sends a huge packet.
        let mut input = text;
        while !input.is_empty() {
            let mut end = input.len().min(4096);
            while !input.is_char_boundary(end) {
                end -= 1;
            }
            self.decoder.advance(&mut self.parser, &input[..end]);
            let decoded = std::mem::take(&mut self.decoder.output);
            for (style, part, link) in decoded.rich_spans(0..decoded.text.len()) {
                for fragment in part.split_inclusive('\n') {
                    let body = fragment
                        .trim_end_matches('\n')
                        .replace('\t', " ")
                        .replace('\r', "");
                    self.pending.push_link(style, &body, link);
                    if fragment.ends_with('\n') || self.pending.text.len() >= MAX_RECORD_BYTES {
                        self.flush_pending()?;
                    }
                }
            }
            input = &input[end..];
        }
        if terminated {
            self.flush_pending()?;
        }
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    fn flush_pending(&mut self) -> io::Result<()> {
        let id = self.end;
        if self.pending_is_prompt {
            self.prompt = id;
        }
        let mut payload = String::new();
        self.pending
            .write_ansi(0..self.pending.text.len(), &mut payload);
        for value in [
            self.last,
            payload.len() as u64,
            self.prompt,
            u64::from(self.pending_is_prompt),
        ] {
            self.write_buffer.extend_from_slice(&value.to_le_bytes());
        }
        self.write_buffer.extend_from_slice(payload.as_bytes());
        self.end += HEADER_BYTES + payload.len() as u64;
        self.last = id;
        self.pending = StyledText::default();
        self.pending_is_prompt = false;
        self.revision = self.revision.wrapping_add(1);
        if self.write_buffer.len() >= 64 * 1024 {
            self.flush_disk()?;
        }
        Ok(())
    }

    fn flush_disk(&mut self) -> io::Result<()> {
        if !self.write_buffer.is_empty() {
            self.file.seek(SeekFrom::End(0))?;
            self.file.write_all(&self.write_buffer)?;
            self.write_buffer.clear();
        }
        Ok(())
    }

    fn record(&mut self, id: u64) -> io::Result<Arc<Record>> {
        if let Some(index) = self.cache.iter().position(|entry| entry.id == id) {
            let entry = self.cache.remove(index).expect("located cache entry");
            self.cache.push_back(Arc::clone(&entry));
            return Ok(entry);
        }
        self.flush_disk()?;
        self.file.seek(SeekFrom::Start(id))?;
        let mut header = [0; HEADER_BYTES as usize];
        self.file.read_exact(&mut header)?;
        let field = |offset| {
            u64::from_le_bytes(header[offset..offset + 8].try_into().expect("fixed field"))
        };
        let length = usize::try_from(field(8)).map_err(io::Error::other)?;
        let mut payload = vec![0; length];
        self.file.read_exact(&mut payload)?;
        let text = StyledText::from_ansi(&String::from_utf8_lossy(&payload));
        let mut byte = 0;
        let rows: Vec<_> = text_layout::wrap_line(&text, 0..text.text.len(), self.width.max(1), 0)
            .into_iter()
            .map(|(row, _)| {
                let start = byte;
                byte += row.text.len();
                (start, Arc::new(row))
            })
            .collect();
        let bytes = payload.len()
            + rows
                .iter()
                .map(|(_, row)| row.retained_bytes())
                .sum::<usize>()
            + rows.capacity() * std::mem::size_of::<(usize, Arc<StyledText>)>();
        let entry = Arc::new(Record {
            id,
            previous: field(0),
            next: id + HEADER_BYTES + length as u64,
            prompt: field(16),
            is_prompt: field(24) != 0,
            rows,
            bytes,
        });
        while !self.cache.is_empty()
            && (self.cache.len() >= CACHE_ENTRIES || self.cache_bytes + bytes > CACHE_BYTES)
        {
            let old = self.cache.pop_front().expect("nonempty cache");
            self.cache_bytes -= old.bytes;
        }
        // A pathological narrow/large record can exceed the cache budget.
        // Paint it once without retaining it; other records still stay bounded.
        if bytes <= CACHE_BYTES {
            self.cache_bytes += bytes;
            self.cache.push_back(Arc::clone(&entry));
        }
        Ok(entry)
    }

    fn row_index(record: &Record, byte: usize) -> usize {
        record
            .rows
            .partition_point(|(start, _)| *start <= byte)
            .saturating_sub(1)
    }

    fn previous(&mut self, anchor: Anchor) -> io::Result<Option<Anchor>> {
        let entry = self.record(anchor.record)?;
        let index = Self::row_index(&entry, anchor.byte);
        if index > 0 {
            Ok(Some(Anchor {
                record: entry.id,
                byte: entry.rows[index - 1].0,
            }))
        } else if entry.previous != NONE {
            let previous = self.record(entry.previous)?;
            Ok(Some(Anchor {
                record: previous.id,
                byte: previous.rows.last().expect("one row").0,
            }))
        } else {
            Ok(None)
        }
    }

    fn next(&mut self, anchor: Anchor) -> io::Result<Option<Anchor>> {
        let entry = self.record(anchor.record)?;
        let index = Self::row_index(&entry, anchor.byte);
        if index + 1 < entry.rows.len() {
            Ok(Some(Anchor {
                record: entry.id,
                byte: entry.rows[index + 1].0,
            }))
        } else if entry.next < self.end {
            Ok(Some(Anchor {
                record: entry.next,
                byte: 0,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn scroll(&mut self, delta: i32) -> io::Result<()> {
        let Some(mut anchor) = self.reading.or(self.first_visible) else {
            return Ok(());
        };
        for _ in 0..delta.unsigned_abs() {
            let next = if delta < 0 {
                self.previous(anchor)?
            } else {
                self.next(anchor)?
            };
            let Some(next) = next else {
                if delta > 0 {
                    self.reading = None;
                    return Ok(());
                }
                break;
            };
            anchor = next;
        }
        self.reading = Some(anchor);
        Ok(())
    }

    pub fn latest(&mut self) {
        self.reading = None;
    }

    fn caption(&mut self, prompt: u64, width: usize) -> io::Result<Arc<StyledText>> {
        if let Some((previous, previous_width, caption)) = &self.caption {
            if *previous == prompt && *previous_width == width {
                return Ok(Arc::clone(caption));
            }
        }
        self.flush_disk()?;
        self.file.seek(SeekFrom::Start(prompt + 8))?;
        let mut size = [0; 8];
        self.file.read_exact(&mut size)?;
        let length = usize::try_from(u64::from_le_bytes(size).min(4096)).expect("bounded caption");
        self.file.seek(SeekFrom::Start(prompt + HEADER_BYTES))?;
        let mut payload = vec![0; length];
        self.file.read_exact(&mut payload)?;
        let plain = StyledText::from_ansi(&String::from_utf8_lossy(&payload));
        let prefix: String = plain.text.chars().take(512).collect();
        let caption = prefix.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut styled = StyledText::default();
        styled.push(
            prompt_style(false),
            &text_layout::truncate_to_width(&caption, width.saturating_sub(1)),
        );
        let caption = Arc::new(styled);
        self.caption = Some((prompt, width, Arc::clone(&caption)));
        Ok(caption)
    }

    pub fn render(&mut self, width: usize, height: usize) -> io::Result<Frame> {
        if self.width != width {
            self.width = width;
            self.cache.clear();
            self.cache_bytes = 0;
        }
        // Incoming output must not rebuild a held reading frame or selection.
        let revision = if self.reading.is_some() {
            0
        } else {
            self.revision
        };
        let key = (width, height, revision, self.reading);
        if let Some((previous, frame)) = &self.frame {
            if *previous == key {
                return Ok(frame.clone());
            }
        }
        let body_height = height.saturating_sub(1);
        let mut anchor = self.reading;
        if anchor.is_none() && self.last != NONE {
            let last = self.record(self.last)?;
            anchor = Some(Anchor {
                record: last.id,
                byte: last.rows.last().expect("one row").0,
            });
            for _ in 1..body_height {
                let previous = self.previous(anchor.expect("last exists"))?;
                if previous.is_none() {
                    break;
                }
                anchor = previous;
            }
        }
        self.first_visible = anchor;
        let mut body = StyledText::default();
        let mut header = Arc::new(StyledText::default());
        if let Some(first) = anchor {
            let entry = self.record(first.record)?;
            if entry.prompt != NONE && (!entry.is_prompt || first.byte != 0) && width >= 16 {
                header = self.caption(entry.prompt, width)?;
            }
        }
        for row_index in 0..body_height {
            let Some(current) = anchor else {
                break;
            };
            let entry = self.record(current.record)?;
            let row = &entry.rows[Self::row_index(&entry, current.byte)].1;
            if row_index > 0 {
                body.push(ContentStyle::default(), "\n");
            }
            for (style, part, link) in row.rich_spans(0..row.text.len()) {
                body.push_link(style, part, link);
            }
            anchor = self.next(current)?;
        }
        let frame = Frame {
            header,
            body: Arc::new(body),
        };
        self.frame = Some((key, frame.clone()));
        Ok(frame)
    }

    /// Restore an inspectable native record after leaving the experimental view.
    pub fn replay(&mut self, output: &mut impl Write) -> io::Result<()> {
        if !self.pending.text.is_empty() {
            self.flush_pending()?;
        }
        self.flush_disk()?;
        let mut id = 0;
        let mut buffer = [0; 8192];
        while id < self.end {
            self.file.seek(SeekFrom::Start(id + 8))?;
            let mut size = [0; 8];
            self.file.read_exact(&mut size)?;
            let length = u64::from_le_bytes(size);
            self.file.seek(SeekFrom::Start(id + HEADER_BYTES))?;
            let mut remaining = length;
            while remaining > 0 {
                let count =
                    usize::try_from(remaining.min(buffer.len() as u64)).expect("buffer size");
                self.file.read_exact(&mut buffer[..count])?;
                output.write_all(&buffer[..count])?;
                remaining -= count as u64;
            }
            output.write_all(b"\n")?;
            id += HEADER_BYTES + length;
        }
        output.flush()
    }
}
