//! Compaction and its token preflight use the same authorized model mount.
mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use engine_core::{AuthMode, EngineApiClient, ModelAccess};
use kernel::core::dispatch::{HookContext, HookOutcome, NativeInterceptHook};
use kernel::kernel::Kernel;
use runtime::{ApiClient, ConversationMessage, KernelFsBackend};

struct ModelGate {
    writes: Arc<AtomicUsize>,
    deny: Arc<AtomicBool>,
}

impl NativeInterceptHook for ModelGate {
    fn name(&self) -> &'static str {
        "compaction-model-gate"
    }

    fn mutating_path_suffixes(&self) -> &'static [&'static str] {
        &[".prompt"]
    }

    fn on_pre(&self, ctx: &HookContext) -> Result<HookOutcome, String> {
        if let HookContext::Write(w) = ctx {
            if w.path.starts_with("/model/") && w.path.ends_with(".prompt") {
                assert_eq!(w.identity.user_id, "owner");
                assert_eq!(w.identity.agent_id, "compactor");
                if self.deny.load(Ordering::SeqCst) {
                    return Err("model policy refused this request".into());
                }
                self.writes.fetch_add(1, Ordering::SeqCst);
            }
        }
        Ok(HookOutcome::Pass)
    }
}

#[test]
fn compaction_and_token_count_cross_the_mount_and_respect_refusal() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("SUDO_CODE_CONFIG_HOME", home.path());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let service = rt
        .block_on(mock_anthropic_service::MockAnthropicService::spawn())
        .unwrap();
    let kernel = Arc::new(Kernel::new());
    common::mount_agent_world(&kernel);
    let _storage = common::mount_model(&kernel, "anthropic", &service.base_url(), "mount-key");
    let writes = Arc::new(AtomicUsize::new(0));
    let deny = Arc::new(AtomicBool::new(false));
    let hook = kernel
        .enlist_hook_only_service("compaction-model-gate")
        .unwrap();
    kernel.register_service_hook(
        &hook,
        Box::new(ModelGate {
            writes: writes.clone(),
            deny: deny.clone(),
        }),
    );
    let model = "claude-sonnet-4-6";
    let mut client = model_client(home.path(), model, kernel.clone());
    assert!(
        client.model_catalog().is_none(),
        "no direct HTTP discovery in co-host"
    );
    rt.block_on(async {
        let prompt = "Summarize. PARITY_SCENARIO:llm_compaction_roundtrip";
        let summary = client
            .send_compaction(
                model,
                prompt,
                vec![ConversationMessage::user_text(prompt)],
                128,
            )
            .await
            .unwrap();
        assert!(summary.contains("Primary Request and Intent"), "{summary}");

        // Enough history to invoke the provider's exact token preflight while
        // remaining below its context limit. Both requests must cross Nexus.
        let window = runtime::model_capabilities::context_window_or_default(model) as usize;
        client
            .send_compaction(
                model,
                prompt,
                vec![ConversationMessage::user_text(format!(
                    "{prompt} {}",
                    "x".repeat(window * 4 * 85 / 100)
                ))],
                128,
            )
            .await
            .unwrap();
        let captured = service.captured_requests().await;
        assert!(captured.iter().any(|r| r.path.ends_with("/count_tokens")));
        assert_eq!(
            captured
                .iter()
                .filter(|r| r.path.ends_with("/messages"))
                .count(),
            2
        );
        assert_eq!(writes.load(Ordering::SeqCst), captured.len());

        deny.store(true, Ordering::SeqCst);
        let error = client
            .send_compaction(
                model,
                prompt,
                vec![ConversationMessage::user_text("Denied work")],
                128,
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("model policy refused"),
            "{error}"
        );
        assert_eq!(
            service.captured_requests().await.len(),
            captured.len(),
            "a refused model write must never fall back to HTTP"
        );
    });
}

fn model_client(home: &std::path::Path, model: &str, kernel: Arc<Kernel>) -> EngineApiClient {
    std::fs::write(
        home.join("sudocode.json"),
        serde_json::json!({
            "auth_modes": {"api-key": {"anthropic": {"baseUrl": "nexus:///model"}}},
            "models": {model: {"alias": model, "name": "test", "input": ["text"],
                "providers": {"api-key": {"provider": "anthropic", "model": model}}}}
        })
        .to_string(),
    )
    .unwrap();
    let config = runtime::ConfigLoader::new(home, home)
        .load_sudocode_config()
        .unwrap();
    EngineApiClient::new(
        "model-compaction",
        &config,
        model,
        AuthMode::ApiKey,
        tools::GlobalToolRegistry::builtin(),
        false,
        None,
        &ModelAccess {
            fs: Arc::new(KernelFsBackend::for_agent(
                kernel,
                "owner",
                "root",
                "compactor",
                "/",
            )),
            require_mount: true,
        },
    )
    .unwrap()
}
