mod common;

use common::managed_session::seed_managed_session;
use std::sync::Arc;

use backends::storage::local_connector::LocalConnectorBackend;
use engine_host::managed_agent::prepare_managed_agent;
use engine_host::SessionLifecycle;
use kernel::core::agents::registry::{AgentDescriptor, AgentKind, RepoMount};
use kernel::kernel::Kernel;
use runtime::{FsBackend, ManagedRoot, WorkspaceRootScope};

#[test]
fn hosted_sessions_share_native_files_and_skills_while_retaining_durable_agent_state() {
    let config = tempfile::tempdir().unwrap();
    std::env::set_var("SUDO_CODE_CONFIG_HOME", config.path());
    write_hosted_config(config.path());
    let storage = tempfile::tempdir().unwrap();
    let kernel = Arc::new(Kernel::new());
    kernel.vfs_router_arc().add_mount(
        "/",
        "root",
        Some(Arc::new(
            LocalConnectorBackend::new(storage.path(), true, false).unwrap(),
        )),
        false,
    );
    let project = storage.path().join("projects/first");
    let skill = project.join(".nexus/sudocode/skills/shared-workspace");
    std::fs::create_dir_all(skill.join("scripts")).unwrap();
    std::fs::write(skill.join("SKILL.md"), "---\nname: shared-workspace\ndescription: Check the shared project\n---\nRun sh scripts/run.sh from this skill directory.\n").unwrap();
    std::fs::write(skill.join("scripts/run.sh"), "printf skill-script-ok").unwrap();
    let first = descriptor("first", "/projects/first");
    let prepared =
        prepare_managed_agent(&kernel, &first, &seed_managed_session(&kernel, &first)).unwrap();
    let original_id = prepared.durable_session_id().to_string();
    let cwd = prepared.cwd();
    assert_eq!(cwd, project.canonicalize().unwrap());
    let engine = prepared
        .open(
            std::collections::BTreeMap::default(),
            runtime::SystemPromptOverrides::default(),
            runtime::memory::MemoryMode::default(),
        )
        .unwrap();
    let snapshot = engine.session_snapshot();
    assert_eq!(engine.workspace_root().unwrap(), cwd.to_string_lossy());
    assert_eq!(snapshot.workspace_root(), Some(cwd.as_path()));
    let (_skill_writer, _scope) = assert_shared_workspace_tools(&snapshot, &cwd, &skill);
    let fs = snapshot.fs_handle();
    let memory = fs.managed_root(ManagedRoot::Memory).unwrap();
    assert_eq!(memory, "/agents/same-agent/memory");
    assert_eq!(fs.managed_root(ManagedRoot::Sessions).unwrap(), "/sessions");
    fs.write(&format!("{memory}/MEMORY.md"), b"same-agent-memory")
        .unwrap();
    persist_inflight_workspace_effect(&engine);
    drop(engine);
    let second_descriptor = descriptor("second", "/projects/second");
    let second = prepare_managed_agent(
        &kernel,
        &second_descriptor,
        &seed_managed_session(&kernel, &second_descriptor),
    )
    .unwrap();
    assert_ne!(second.cwd(), cwd);
    let second = second
        .open(
            std::collections::BTreeMap::default(),
            runtime::SystemPromptOverrides::default(),
            runtime::memory::MemoryMode::default(),
        )
        .unwrap();
    let second_fs = second.session_snapshot().fs_handle();
    assert!(!second_fs.exists("from-shell.txt").unwrap());
    assert_eq!(
        second_fs.read(&format!("{memory}/MEMORY.md")).unwrap(),
        b"same-agent-memory"
    );
    let restored = prepare_managed_agent(
        &kernel,
        &first,
        &managed_agent::SpawnOptions {
            resume_session_id: Some(original_id.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(restored.durable_session_id(), original_id);
    assert_eq!(restored.cwd(), cwd);
    let restored = restored
        .open(
            std::collections::BTreeMap::default(),
            runtime::SystemPromptOverrides::default(),
            runtime::memory::MemoryMode::default(),
        )
        .unwrap();
    assert_recovered_effect(&restored);
}

fn descriptor(pid: &str, path: &str) -> AgentDescriptor {
    let mut desc = AgentDescriptor {
        pid: pid.into(),
        name: "same-agent".into(),
        owner_id: "owner".into(),
        zone_id: "root".into(),
        kind: AgentKind::Managed,
        repos: vec![RepoMount {
            alias: "workspace".into(),
            mount_path: path.into(),
        }],
        ..Default::default()
    };
    desc.labels.insert("model".into(), "test-model".into());
    desc
}

fn assert_shared_workspace_tools(
    snapshot: &runtime::Session,
    cwd: &std::path::Path,
    skill: &std::path::Path,
) -> (std::fs::File, WorkspaceRootScope) {
    let prompt = snapshot
        .prompt_snapshot()
        .unwrap()
        .prompt
        .dynamic_sections()
        .join("\n");
    assert!(
        prompt.contains("shared-workspace"),
        "project skill must be advertised: {prompt}"
    );
    assert!(!prompt.contains("Your files and your shell are in different places"));
    let fs = snapshot.fs_handle();
    fs.write("from-file-tool.txt", b"file-tool-bytes").unwrap();
    fs.write(
        ".nexus/sudocode/skills/shared-workspace/scripts/run.sh",
        b"#!/bin/sh\nprintf skill-script-ok\n",
    )
    .unwrap();
    // A concurrent spawn can inherit a writable descriptor until its exec.
    // Model that window; the skill's interpreter must still read these bytes.
    let skill_writer = std::fs::OpenOptions::new()
        .write(true)
        .open(skill.join("scripts/run.sh"))
        .unwrap();
    let execute_skill = "sh .nexus/sudocode/skills/shared-workspace/scripts/run.sh";
    let scope = WorkspaceRootScope::enter(cwd);
    let result = runtime::execute_bash(serde_json::from_value(serde_json::json!({
        "command": format!("cat from-file-tool.txt; printf shell-bytes > from-shell.txt; {execute_skill}"),
        "dangerouslyDisableSandbox": true,
        "timeout": 10000,
    })).unwrap()).unwrap();
    assert_eq!(result.exit_code, Some(0), "{}", result.stderr);
    assert!(result.stdout.contains("file-tool-bytes"));
    assert!(result.stdout.contains("skill-script-ok"));
    assert_eq!(fs.read("from-shell.txt").unwrap(), b"shell-bytes");
    // Use the actual canonicalized cohost mount and the absolute spellings a
    // native command can return. All aliases must reach the same VFS object.
    let native_file = cwd.join("from-shell.txt");
    let native_paths = vec![native_file.to_string_lossy().into_owned()];
    #[cfg(windows)]
    let native_paths = {
        let ordinary = native_paths[0]
            .strip_prefix(r"\\?\")
            .expect("canonicalized Windows cohost workspace")
            .to_string();
        vec![
            native_paths[0].clone(),
            ordinary.clone(),
            ordinary.replace('\\', "/"),
        ]
    };
    for path in native_paths {
        assert_eq!(fs.read(&path).unwrap(), b"shell-bytes", "{path}");
        assert_eq!(fs.normalize(&path).unwrap(), native_file.to_string_lossy());
    }
    (skill_writer, scope)
}

fn assert_recovered_effect(restored: &engine_host::SessionEngine) {
    restored.with_session_mut(&mut |session| {
        assert_eq!(session.repair_orphan_tool_uses().unwrap(), 0);
        assert_eq!(session.repair_orphan_tool_uses().unwrap(), 0);
    });
    let recovered = restored.session_snapshot();
    assert_eq!(
        recovered.fs_handle().read("side-effect.txt").unwrap(),
        b"effect"
    );
    assert!(recovered.messages().iter().flat_map(|message| &message.blocks).any(|block| matches!(block,
        runtime::ContentBlock::ToolResult { tool_use_id, is_error: true, output, .. }
            if tool_use_id == "in-flight-effect" && output.contains("Side effects may already have occurred")
    )));
    assert_eq!(
        restored
            .session_snapshot()
            .fs_handle()
            .read("from-shell.txt")
            .unwrap(),
        b"shell-bytes"
    );
}

fn write_hosted_config(config: &std::path::Path) {
    std::fs::write(
        config.join("sudocode.json"),
        serde_json::json!({
            "auth_modes": {"api-key": {"anthropic": {"baseUrl": "nexus:///model"}}},
            "models": {"test-model": {"alias": "test-model", "name": "test", "input": ["text"],
                "providers": {"api-key": {"provider": "anthropic", "model": "claude-sonnet-4-6"}}}},
        })
        .to_string(),
    )
    .unwrap();
}

fn persist_inflight_workspace_effect(engine: &engine_host::SessionEngine) {
    engine.with_session_mut(&mut |session| {
        session
            .push_message(runtime::ConversationMessage::assistant(vec![
                runtime::ContentBlock::ToolUse {
                    id: "in-flight-effect".into(),
                    name: "bash".into(),
                    input: r#"{"command":"printf effect >> side-effect.txt"}"#.into(),
                    thought_signature: None,
                },
            ]))
            .unwrap();
    });
    let effect = runtime::execute_bash(
        serde_json::from_value(serde_json::json!({
            "command": "printf effect >> side-effect.txt",
            "dangerouslyDisableSandbox": true,
            "timeout": 10000,
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(effect.exit_code, Some(0));
    engine.persist().unwrap();
}
