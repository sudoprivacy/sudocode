//! Text from bytes another process wrote.
//!
//! Every child sudocode starts — a shell running a command, git, a hook, a
//! plugin command, an MCP server's stderr — writes its stdout and stderr as
//! bytes, in whatever encoding that program uses. This crate is the one place
//! those bytes become text, so the model and the user read what the program
//! actually said.
//!
//! Each line is tried as UTF-8 first, which is what Unix programs, git and
//! PowerShell 7 write. On Windows a line that is not UTF-8 is decoded in the
//! console's code page, which is what console programs such as Windows
//! PowerShell 5.1 or `cmd` write: CP936 on a Chinese system, CP437 on an
//! English one. Decoding line by line keeps both right when one stream mixes
//! them, as when PowerShell runs `git`. Only bytes valid in neither are
//! replaced, and then the result says so.
//!
//! Not for bytes defined as UTF-8 by a protocol or written by sudocode itself
//! (HTTP bodies, JSON-RPC frames, files the model asked to read).

/// Text decoded from a child process's output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecodedText {
    pub text: String,
    /// Some bytes were valid neither as UTF-8 nor in the console's code page,
    /// and were replaced with U+FFFD.
    pub lossy: bool,
}

/// Decode bytes a child process wrote to stdout or stderr.
#[must_use]
pub fn decode_process_output(bytes: &[u8]) -> DecodedText {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return DecodedText {
            text: text.to_owned(),
            lossy: false,
        };
    }
    let mut decoded = DecodedText {
        text: String::with_capacity(bytes.len()),
        lossy: false,
    };
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if let Ok(text) = std::str::from_utf8(line) {
            decoded.text.push_str(text);
        } else if let Some(text) = stdin_peek::decode_console_bytes(line) {
            decoded.text.push_str(&text);
        } else {
            decoded.text.push_str(&String::from_utf8_lossy(line));
            decoded.lossy = true;
        }
    }
    decoded
}

/// Decode the end of a longer output, cut at a byte offset, as
/// [`decode_process_output`] does. The cut can fall inside a UTF-8 character;
/// when skipping the rest of that character is what makes the first line
/// UTF-8, it is skipped, so the line reads as written instead of starting with
/// a replacement character or being taken for code-page text.
#[must_use]
pub fn decode_process_output_tail(bytes: &[u8]) -> DecodedText {
    let first_line = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |newline| newline + 1);
    let skip = (0..=first_line.min(3))
        .find(|&skip| std::str::from_utf8(&bytes[skip..first_line]).is_ok())
        .unwrap_or(0);
    decode_process_output(&bytes[skip..])
}
