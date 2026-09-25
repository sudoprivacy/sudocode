//! The complete list of ways the two hosts differ.
//!
//! Everything below the host is shared: one engine, one tool set, one prompt
//! assembly, one `FsBackend` trait, one session store. What is left over is what
//! a host INJECTS — and this file is that list, asserted for both hosts side by
//! side.
//!
//! Adding a sixth difference means editing this test, which is the point. A
//! divergence nobody had to declare is how two hosts drift apart while every
//! suite stays green: each gap found in the storage sweep lived in exactly such
//! a place — a `None` the CLI takes and the co-host does not, or a value only
//! one host supplies.
//!
//! Two differences are asserted elsewhere because they need a running agent:
//! the cron declaration (`cohost_mock_llm::a_cohost_is_not_offered_crons_…`) and
//! the reply framing. One has no assertion anywhere because it has no source:
//! `reasoning_effort` is a CLI flag, and nexus-vfs's session request carries no
//! field for it.

use std::sync::Arc;

use engine_host::HostContext;
use kernel::kernel::Kernel;
use runtime::{FsBackend, KernelFsBackend, ManagedRoot};

const COHOST_AGENT: &str = "parity-agent";
const COHOST_WORKSPACE: &str = "/proc/7/workspace";

/// One temp root per test, under a config home that is not the developer's —
/// both hosts read real configuration at boot. Set once for the binary: the
/// variable is process-wide and tests run in parallel.
fn sandbox(name: &str) -> tempfile::TempDir {
    static CONFIG_HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let home = CONFIG_HOME.get_or_init(|| {
        let home = tempfile::Builder::new()
            .prefix("host-parity-config-")
            .tempdir()
            .expect("config home");
        std::env::set_var("SUDO_CODE_CONFIG_HOME", home.path());
        home
    });
    debug_assert!(home.path().exists());
    tempfile::Builder::new()
        .prefix(&format!("host-parity-{name}-"))
        .tempdir()
        .expect("temp dir")
}

/// The co-host's context, built through the production constructor.
fn cohost() -> HostContext {
    let kernel = Arc::new(Kernel::new());
    let fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent(
        Arc::clone(&kernel),
        "test-owner",
        "root",
        COHOST_AGENT,
        COHOST_WORKSPACE.to_string(),
    ));
    let mailbox = Arc::new(runtime::mailbox::Mailbox::daemon_absolute(
        Arc::clone(&fs),
        COHOST_AGENT.to_string(),
    ));
    HostContext::for_cohost_agent(fs, COHOST_AGENT, mailbox).expect("build the co-host context")
}

#[test]
fn the_two_hosts_differ_in_exactly_these_ways() {
    let dir = sandbox("differences");
    let workspace = dir.path().join("project");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let cli = HostContext::for_cli_session(&workspace).expect("build the CLI context");
    let cohost = cohost();

    // 1. PATH SPELLING. The CLI answers in host paths because the model hands
    //    them to `bash`, which runs on the host and cannot open a VFS path. The
    //    co-host answers in the VFS spelling its kernel serves.
    assert_eq!(
        cli.fs.working_root().expect("cli root"),
        workspace.to_string_lossy(),
        "a CLI session addresses its workspace in host spelling"
    );
    assert_eq!(
        cohost.fs.working_root().expect("cohost root"),
        COHOST_WORKSPACE,
        "a co-hosted agent addresses the VFS subtree its descriptor gave it"
    );

    // 2. CONFIG ROOT. The CLI's is its workspace; the daemon's is its own
    //    directory, because it must read its configuration before it can serve
    //    the VFS that configuration describes.
    assert_eq!(cli.config_root, workspace);
    assert_eq!(
        cohost.config_root,
        std::env::current_dir().expect("cwd"),
        "the co-host reads configuration from the daemon's own directory"
    );

    // 3. AGENT NAME. Derived from the directory for a CLI session; taken from
    //    the descriptor for a co-hosted agent, which is the SSOT for who it is.
    assert_eq!(cli.resolved_agent_name(), cli.agent_name.clone().unwrap());
    assert_eq!(cohost.resolved_agent_name(), COHOST_AGENT);

    // 4. MAILBOX. Resolved later for the CLI; supplied for the co-host, whose
    //    mailbox IS its identity — a second one resolved downstream would give
    //    `send` a different address than the agent receives on.
    assert!(cli.mailbox.is_none(), "a CLI session resolves its own");
    assert!(cohost.mailbox.is_some(), "a co-hosted agent is given one");

    // 5. SHELL ROOT. One directory for the CLI: what its tools address and where
    //    its shell runs are the same place. Not so for the co-host — no shell can
    //    `cd` into a VFS path — so it gets its own host directory, and the model
    //    is told (`cohost_shell_prompt_section`).
    assert_eq!(
        cli.shell_root, workspace,
        "a CLI session's shell root IS its workspace"
    );
    assert_ne!(
        cohost.shell_root.to_string_lossy(),
        COHOST_WORKSPACE,
        "a co-hosted agent's shell cannot run in its VFS workspace"
    );
    assert!(
        cohost.shell_root.is_dir(),
        "and the directory it runs in exists: {}",
        cohost.shell_root.display()
    );
    assert!(
        cohost.shell_root.to_string_lossy().contains(COHOST_AGENT),
        "keyed by the agent, so two agents on one daemon do not share a shell \
         directory or a git repository: {}",
        cohost.shell_root.display()
    );

    // 6. MANAGED ROOTS. The CLI keeps every concern where its own tooling looks;
    //    the co-host roots each inside its own subtree of the VFS.
    for concern in [
        ManagedRoot::Sessions,
        ManagedRoot::SubAgents,
        ManagedRoot::Memory,
        ManagedRoot::Todos,
    ] {
        assert_eq!(
            cli.fs.managed_root(concern),
            None,
            "a CLI session keeps {concern:?} in its own layout"
        );
        let root = cohost
            .fs
            .managed_root(concern)
            .unwrap_or_else(|| panic!("a co-hosted agent roots {concern:?} in the VFS"));
        assert!(
            root.starts_with("/sessions") || root.starts_with(&format!("/agents/{COHOST_AGENT}")),
            "{concern:?} should be flat-global or under the agent; got {root}"
        );
    }
}

/// And the property that makes the list above the WHOLE list: shared code takes
/// the host's value rather than asking the process.
///
/// A relative tool path is the smallest case of it. Both hosts run the same
/// resolution; what differs is only the root each supplied. When that stopped
/// being true — the co-host had no workspace scope, so `current_workspace_root()`
/// returned the daemon's directory — `bash`, the git context and the
/// stale-branch check all silently answered about the daemon instead of the
/// agent.
#[test]
fn a_relative_path_resolves_against_the_root_its_host_supplied() {
    let dir = sandbox("relative");
    let workspace = dir.path().join("project");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let cli = HostContext::for_cli_session(&workspace).expect("build the CLI context");
    let cohost = cohost();

    let cli_path = cli
        .fs
        .normalize_allow_missing("notes.txt")
        .expect("cli normalize");
    assert!(
        cli_path.starts_with(&*workspace.to_string_lossy()),
        "the CLI resolves against its workspace; got {cli_path}"
    );

    let cohost_path = cohost
        .fs
        .normalize_allow_missing("notes.txt")
        .expect("cohost normalize");
    assert_eq!(
        cohost_path,
        format!("{COHOST_WORKSPACE}/notes.txt"),
        "the co-host resolves against the subtree its descriptor gave it"
    );
}
