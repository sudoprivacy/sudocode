//! Image files and tool attachments share the same validation and model routing.
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
    pub async fn from_dispatch(tool: &str, output: String, model: &str) -> Result<Self, ToolError> {
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
        let attachment = prepare_image(&image.data, &image.mime_type, model)
            .await
            .map_err(ToolError::new)?;
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

/// Validate before either native delivery or a VLM side call. Fail visibly;
/// a failed image decode must never be presented as a successful image read.
pub async fn prepare_image(data: &str, mime: &str, model: &str) -> Result<ContentBlock, String> {
    let (data, mime_type) =
        crate::image_registry::preflight_base64(data, mime).map_err(|e| e.to_string())?;
    if crate::model_capabilities::vision_capable(model) {
        return Ok(ContentBlock::Image { data, mime_type });
    }
    let config = crate::current_workspace_root().ok().and_then(|cwd| {
        crate::ConfigLoader::default_for(cwd)
            .load_sudocode_config()
            .ok()
    });
    let credentials = config
        .as_ref()
        .and_then(|c| c.auth_modes.get("proxy"))
        .and_then(|p| p.get("sudorouter"))
        .and_then(|p| {
            p.api_key
                .as_ref()
                .map(|key| (p.base_url.as_str(), key.as_str()))
        });
    let Some((url, key)) = credentials else {
        return Err("image requires a vision-capable model or configured proxy.sudorouter for visual description".into());
    };
    let description = crate::vlm_describe::describe_image_via_vlm(
        url,
        key,
        crate::vlm_describe::DEFAULT_VISION_MODEL,
        &data,
        &mime_type,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(ContentBlock::Text {
        text: format!(
            "[Image description from a vision model; not direct image input: {description}]"
        ),
    })
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
