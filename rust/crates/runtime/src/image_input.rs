//! Read image files and construct native image attachments.
use std::io;
use std::path::Path;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::{ContentBlock, ConversationMessage, FsBackend, ToolError};

const MAX_SOURCE_BYTES: u64 = 20 * 1024 * 1024;

/// Internal Read result. The tool adapter removes the encoded bytes from text
/// before hooks, output offloading, rendering, or provider serialization.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageReadResult {
    #[serde(rename = "type")]
    pub kind: String,
    pub path: String,
    pub data: String,
    pub mime_type: String,
}

#[must_use]
pub fn is_image_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp"
            )
        })
}

pub fn read_image(fs: &dyn FsBackend, path: &str) -> io::Result<ImageReadResult> {
    let path = fs.normalize(path)?;
    if fs.stat(&path)?.len > MAX_SOURCE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "image exceeds the 20 MiB source limit",
        ));
    }
    let bytes = fs.read(&path)?;
    if bytes.len() as u64 > MAX_SOURCE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "image exceeds the 20 MiB source limit",
        ));
    }
    let format = image::guess_format(&bytes).map_err(io::Error::other)?;
    let mime = match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported image format",
            ))
        }
    };
    let (bytes, mime_type) = crate::image_registry::maybe_downsample_raw(&bytes, mime)?;
    Ok(ImageReadResult {
        kind: "image".into(),
        path,
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
        mime_type,
    })
}

/// Structured runtime output; attachments never pass through text truncation.
#[derive(Clone, Debug)]
pub struct ToolOutput {
    pub text: String,
    pub attachments: Vec<ContentBlock>,
}

impl ToolOutput {
    #[must_use]
    pub fn text(text: String) -> Self {
        Self {
            text,
            attachments: Vec::new(),
        }
    }

    /// Adapt the legacy string dispatcher at the trusted Read boundary only.
    /// A shell/plugin returning image-looking JSON cannot inject attachments.
    pub fn from_dispatch(tool: &str, output: String, model: &str) -> Result<Self, ToolError> {
        if !matches!(tool, "Read" | "read_file") {
            return Ok(Self::text(output));
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&output) else {
            return Ok(Self::text(output));
        };
        if value.get("type").and_then(serde_json::Value::as_str) != Some("image") {
            return Ok(Self::text(output));
        }
        let image: ImageReadResult =
            serde_json::from_value(value).map_err(|e| ToolError::new(e.to_string()))?;
        ensure_vision_capable(model).map_err(ToolError::new)?;
        // read_image already validated and preflighted this trusted Read result.
        let attachment = ContentBlock::Image {
            data: image.data,
            mime_type: image.mime_type.clone(),
        };
        Ok(Self {
            text: format!(
                "Image read from {} ({}); attached below.",
                image.path, image.mime_type
            ),
            attachments: vec![attachment],
        })
    }

    #[must_use]
    pub fn into_message(self, id: String, name: String, is_error: bool) -> ConversationMessage {
        let mut message = ConversationMessage::tool_result(id, name, self.text, is_error);
        if !is_error {
            message.blocks.extend(self.attachments);
        }
        message
    }
}

/// Reject models explicitly marked text-only. Unknown capabilities keep the
/// existing optimistic policy and are ultimately validated by the provider.
fn ensure_vision_capable(model: &str) -> Result<(), String> {
    if !crate::model_capabilities::vision_capable(model) {
        return Err(format!(
            "Model '{model}' does not support image input. Switch to a vision-capable model to inspect this image."
        ));
    }
    Ok(())
}

/// Validate incoming prompt attachments before native delivery.
pub fn prepare_image(data: &str, mime: &str, model: &str) -> Result<ContentBlock, String> {
    ensure_vision_capable(model)?;
    let (data, mime_type) =
        crate::image_registry::preflight_base64(data, mime).map_err(|e| e.to_string())?;
    Ok(ContentBlock::Image { data, mime_type })
}

/// Resolve explicit CLI image references, including quoted paths containing
/// spaces. Ordinary @mentions remain text. Called only for user input.
pub fn prompt_blocks(text: &str, fs: &dyn FsBackend) -> io::Result<Vec<ContentBlock>> {
    let mut blocks = vec![ContentBlock::Text {
        text: text.to_owned(),
    }];
    let pattern = regex::Regex::new(r#"(?:^|\s)@(?:"([^"]+)"|'([^']+)'|([^\s]+))"#)
        .expect("constant image reference regex");
    let mut seen = std::collections::BTreeSet::new();
    for capture in pattern.captures_iter(text) {
        let path = capture
            .get(1)
            .or_else(|| capture.get(2))
            .or_else(|| capture.get(3))
            .expect("path capture")
            .as_str();
        if !is_image_path(path) || !seen.insert(path.to_owned()) {
            continue;
        }
        let image = read_image(fs, path)?;
        blocks.push(ContentBlock::Image {
            data: image.data,
            mime_type: image.mime_type,
        });
    }
    Ok(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StdFsBackend;

    fn write_png(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "scode-image-input-{}-{name}.png",
            std::process::id()
        ));
        image::RgbImage::from_pixel(4, 4, image::Rgb([255, 0, 0]))
            .save_with_format(&path, image::ImageFormat::Png)
            .expect("write test png");
        path
    }

    #[test]
    fn read_file_on_png_attaches_the_image_after_the_tool_result() {
        let path = write_png("attach");
        let read = read_image(&StdFsBackend, path.to_str().unwrap()).expect("read png");
        assert_eq!(read.mime_type, "image/png");

        let dispatched = serde_json::to_string(&read).unwrap();
        let output = ToolOutput::from_dispatch("read_file", dispatched, "proxy/gemini-3.5-flash")
            .expect("vision model accepts the image");
        assert!(output.text.contains("attached below"));
        assert_eq!(output.attachments.len(), 1);

        let message = output.into_message("call_1".into(), "read_file".into(), false);
        assert!(matches!(message.blocks[0], ContentBlock::ToolResult { .. }));
        assert!(matches!(
            &message.blocks[1],
            ContentBlock::Image { mime_type, .. } if mime_type == "image/png"
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn only_read_can_attach_images() {
        let path = write_png("bash");
        let read = read_image(&StdFsBackend, path.to_str().unwrap()).expect("read png");
        let dispatched = serde_json::to_string(&read).unwrap();
        let output = ToolOutput::from_dispatch("bash", dispatched.clone(), "any").unwrap();
        assert!(output.attachments.is_empty());
        assert_eq!(output.text, dispatched);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn text_read_output_passes_through_unchanged() {
        let text = r#"{"type":"text","file":{"content":"hello"}}"#.to_string();
        let output = ToolOutput::from_dispatch("read_file", text.clone(), "any").unwrap();
        assert!(output.attachments.is_empty());
        assert_eq!(output.text, text);
    }

    #[test]
    fn image_paths_are_recognised_by_extension() {
        assert!(is_image_path("_sql_approval_images/story4276_0_spec.png"));
        assert!(is_image_path("a/b/story4184_0_spec.JPG"));
        assert!(!is_image_path("scripts/fetch_story.py"));
    }
}
