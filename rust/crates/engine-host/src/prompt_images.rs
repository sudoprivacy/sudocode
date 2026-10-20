//! Model-aware image preparation shared by every frontend.
use crate::config::{extract_sudorouter_credentials, load_sudocode_config_for_cwd};
use crate::vlm_describe;
use crate::{SessionEngine, SessionLifecycle};
use runtime::ContentBlock;
use std::path::Path;

/// Prepare `images` for the session's active model and push them as `User`
/// messages before the turn runs. Native for vision-capable models; VLM-routed
/// (or preflight-downsampled) otherwise, so a text-only model still gets useful
/// content. Ports `AcpSdkDelegate::push_images`.
pub(crate) fn prepare_and_push_images(
    engine: &SessionEngine,
    images: &[(String, String)],
    cwd: &Path,
) -> Result<(), String> {
    let catalog = engine.model_catalog();
    let _scope = catalog
        .as_ref()
        .map(runtime::model_discovery::ModelCatalog::enter);
    let active_model = engine.current_model();
    let vision_capable = runtime::model_capabilities::vision_capable(&active_model);
    let sudocode_config = load_sudocode_config_for_cwd(cwd);
    let sudorouter_creds = extract_sudorouter_credentials(&sudocode_config);
    // Diagnostics on stderr (stdout is the JSON-RPC wire): the image-handling
    // e2e asserts on these lines to prove the VLM route actually fired.
    eprintln!("[push_images] entered — {} images", images.len());
    eprintln!("[push_images] active_model={active_model:?} vision_capable={vision_capable}");
    eprintln!(
        "[push_images] sudorouter_creds_present={}",
        sudorouter_creds.is_some()
    );

    let mut blocks: Vec<ContentBlock> = Vec::with_capacity(images.len());
    for (index, (data, mime_type)) in images.iter().enumerate() {
        let block = if vision_capable {
            match runtime::image_registry::preflight_base64(data, mime_type) {
                Ok((final_data, final_mime)) => ContentBlock::Image {
                    data: final_data,
                    mime_type: final_mime,
                },
                Err(err) if runtime::image_registry::is_image_too_large(&err) => {
                    vlm_describe_block_or_placeholder(
                        data,
                        mime_type,
                        index,
                        sudorouter_creds.as_ref(),
                    )
                }
                Err(_) => ContentBlock::Image {
                    data: data.clone(),
                    mime_type: mime_type.clone(),
                },
            }
        } else {
            vlm_describe_block_or_placeholder(data, mime_type, index, sudorouter_creds.as_ref())
        };
        blocks.push(block);
    }
    engine.push_user_blocks(blocks)
}

/// Route an image through a VLM (via sudorouter) on a dedicated OS thread with
/// its own current-thread runtime (decoupled from the ACP task pool — see the
/// v2 runtime-nesting fix), returning a `ContentBlock::Text` description or a
/// graceful placeholder on any failure. Ports the same-named CLI helper.
fn vlm_describe_block_or_placeholder(
    image_b64: &str,
    mime_type: &str,
    index: usize,
    sudorouter_creds: Option<&(String, String)>,
) -> ContentBlock {
    let human_idx = index + 1;
    let Some((base_url, api_key)) = sudorouter_creds else {
        eprintln!(
            "[push_images] image #{human_idx} — no sudorouter creds, falling back to placeholder"
        );
        return ContentBlock::Text {
            text: format!(
                "[Image #{human_idx} could not be sent (sudorouter not configured) — please configure proxy.sudorouter or use a vision-capable model.]"
            ),
        };
    };
    eprintln!(
        "[push_images] image #{human_idx} — VLM-route start, {} b64 bytes",
        image_b64.len()
    );

    let base_url = base_url.clone();
    let api_key = api_key.clone();
    let image_b64 = image_b64.to_string();
    let mime_type = mime_type.to_string();

    let spawn_result = std::thread::Builder::new()
        .name(format!("vlm-describe-{human_idx}"))
        .spawn(move || -> Result<String, String> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("failed to build VLM runtime: {e}"))?;
            rt.block_on(vlm_describe::describe_image_via_vlm(
                &base_url,
                &api_key,
                vlm_describe::DEFAULT_VISION_MODEL,
                &image_b64,
                &mime_type,
            ))
            .map_err(|e| e.to_string())
        });

    let result: Result<String, String> = match spawn_result {
        Ok(join) => join
            .join()
            .unwrap_or_else(|_| Err("VLM worker thread panicked".to_string())),
        Err(e) => Err(format!("failed to spawn VLM worker thread: {e}")),
    };

    match result {
        Ok(description) => {
            eprintln!(
                "[push_images] image #{human_idx} — VLM done, {} desc chars",
                description.len()
            );
            ContentBlock::Text {
                text: format!("[Image #{human_idx}: {description}]"),
            }
        }
        Err(e) => {
            eprintln!("[push_images] image #{human_idx} — VLM describe failed: {e}");
            ContentBlock::Text {
                text: format!(
                    "[Image #{human_idx} could not be described automatically ({e}) — please retype your question with the image's key contents in text.]"
                ),
            }
        }
    }
}
