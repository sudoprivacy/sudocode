//! The one reader of clipboard images, shared by both REPLs.
//!
//! A pasted image reaches the model only as an `@"path"` reference, which
//! `runtime::image_input::prompt_blocks` attaches exactly like a typed one. An
//! image file copied in a file manager is referenced where it is; raw image
//! data is stored in the shared `runtime::ImageRegistry` cache first.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The clipboard then holds only the image in this file, or no image when the
/// file is not one, and the OS clipboard is never read. Scripts and tests paste
/// a known image this way without touching the user's clipboard.
pub(crate) const CLIPBOARD_IMAGE_ENV: &str = "SUDOCODE_CLIPBOARD_IMAGE";

/// Shown when an explicit image paste finds no image.
pub(crate) const NO_IMAGE_NOTICE: &str = "No image on the clipboard";

/// A held or repeated paste key attaches the image once.
const REPEAT_WINDOW: Duration = Duration::from_millis(250);

/// The clipboard image as a path an `@` reference can name: `Ok(None)` when
/// the clipboard holds no image, `Err` when it cannot be read.
pub(crate) fn read_image() -> Result<Option<PathBuf>, String> {
    if let Some(source) = std::env::var_os(CLIPBOARD_IMAGE_ENV) {
        return Ok(image_file(PathBuf::from(source)));
    }
    let mut clipboard = arboard::Clipboard::new().map_err(|error| error.to_string())?;
    // A copied file wins over image data, as in Codex: a file manager puts
    // the file, not its pixels, on the clipboard.
    let copied = clipboard.get().file_list().unwrap_or_default();
    if let Some(path) = copied.into_iter().find_map(image_file) {
        return Ok(Some(path));
    }
    let image = match clipboard.get_image() {
        Ok(image) => image,
        Err(arboard::Error::ContentNotAvailable) => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let width = u32::try_from(image.width).map_err(|error| error.to_string())?;
    let height = u32::try_from(image.height).map_err(|error| error.to_string())?;
    let registry = runtime::ImageRegistry::default_cache().map_err(|error| error.to_string())?;
    registry
        .register_rgba(width, height, &image.bytes)
        .map(|registered| Some(registered.path))
        .map_err(|error| error.to_string())
}

/// The image behind a bracketed paste that carried no text. Terminals send an
/// empty paste (e.g. Cmd+V on macOS) when the clipboard holds only an image;
/// clipboard text always wins, so an ordinary text paste is never replaced.
pub(crate) fn read_image_for_paste() -> Result<Option<PathBuf>, String> {
    if std::env::var_os(CLIPBOARD_IMAGE_ENV).is_none() {
        let mut clipboard = arboard::Clipboard::new().map_err(|error| error.to_string())?;
        if clipboard.get_text().is_ok_and(|text| !text.is_empty()) {
            return Ok(None);
        }
    }
    read_image()
}

/// The draft a paste key left behind, and when: what [`is_held_repeat`]
/// compares the next paste key against.
static LAST_PASTE_KEY: Mutex<Option<(Instant, String)>> = Mutex::new(None);

/// Whether a paste key is the auto-repeat of a held one: it arrives within
/// [`REPEAT_WINDOW`] of the previous paste key and finds the draft exactly as
/// that key left it. Any edit in between makes it a new paste. A repeat
/// extends the window, so a held key acts once.
pub(crate) fn is_held_repeat(draft: &str) -> bool {
    let mut last = LAST_PASTE_KEY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match last.as_mut() {
        Some((at, left)) if at.elapsed() < REPEAT_WINDOW && left == draft => {
            *at = Instant::now();
            true
        }
        _ => false,
    }
}

/// Record the draft a paste key left behind, for [`is_held_repeat`].
pub(crate) fn note_paste_key(draft: &str) {
    *LAST_PASTE_KEY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((Instant::now(), draft.to_owned()));
}

/// What to insert before an image at the cursor, given the text left of it:
/// a space after a word, because `prompt_blocks` recognizes a reference only
/// at the start or after whitespace.
pub(crate) fn separator_before(before: &str) -> &'static str {
    if before.chars().last().is_some_and(|c| !c.is_whitespace()) {
        " "
    } else {
        ""
    }
}

/// `@"path"`, or `@'path'` for a path containing a double quote.
pub(crate) fn reference(path: &Path) -> String {
    let path = path.display().to_string();
    if path.contains('"') {
        format!("@'{path}'")
    } else {
        format!("@\"{path}\"")
    }
}

fn image_file(path: PathBuf) -> Option<PathBuf> {
    runtime::image_input::is_image_path(&path.to_string_lossy()).then_some(path)
}
