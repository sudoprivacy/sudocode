//! Real terminal ownership across external editors and pagers.
mod common;

use std::path::Path;

fn helper_command(root: &Path, name: &str, script: &str) -> String {
    let path = root.join(format!("{name} helper.py"));
    std::fs::write(&path, script).unwrap();
    // Forward slashes are accepted by Windows Python and remain literal in
    // the quoted editor/pager argv syntax.
    let python = common::resolve_python().replace('\\', "/");
    let path = path.to_string_lossy().replace('\\', "/");
    format!("\"{python}\" -X utf8 \"{path}\"")
}

fn assert_prompt_recovers(child: &mut pty_expect::PtySession) {
    common::expect_input_line_cleared(child, common::DEFAULT_TIMEOUT, "restored input");
    child.send("/version\r").unwrap();
    child.expect("0.2.").unwrap();
    common::expect_input_line_cleared(child, common::DEFAULT_TIMEOUT, "version completed");
    child.send("/exit\r").unwrap();
    assert_eq!(child.expect_eof().unwrap(), 0);
}

#[test]
fn memory_editor_owns_input_and_stderr_then_restores_the_repl() {
    let env = common::TestEnv::new("memory-terminal-handoff");
    let command = helper_command(
        env.workspace_root(),
        "editor",
        r#"
import pathlib, sys
assert sys.stdin.isatty() and sys.stdout.isatty() and sys.stderr.isatty()
assert pathlib.Path(sys.argv[1]).resolve().is_relative_to(pathlib.Path.cwd().resolve())
if sys.platform != 'win32':
    import termios
    assert termios.tcgetattr(sys.stdin)[3] & termios.ICANON
print('EDITOR_READY', file=sys.stderr, flush=True)
text = input()
pathlib.Path(sys.argv[1]).write_bytes((text + '\n').encode())
print('EDITOR_DONE', flush=True)
"#,
    );
    let mut child = env.spawn_with_env(
        &[],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("VISUAL", &command),
            ("EDITOR", "must-not-be-used"),
        ],
    );
    common::expect_input_line_cleared(&child, env.timeout(), "initial prompt");
    for payload in ["first editor input", "second editor input"] {
        child.send("/memory\r").unwrap();
        child.expect("EDITOR_READY").unwrap();
        child.send(&format!("{payload}\r")).unwrap();
        child.expect("Opened memory file at").unwrap();
        common::expect_input_line_cleared(&child, env.timeout(), "editor returned");
        assert_eq!(
            std::fs::read_to_string(env.workspace_root().join("AGENTS.md")).unwrap(),
            format!("{payload}\n")
        );
    }
    assert_prompt_recovers(&mut child);
    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 0);
    }
}

#[test]
fn memory_editor_failures_restore_terminal_input() {
    for is_missing in [true, false] {
        let env = common::TestEnv::new("memory-editor-error");
        let command = if is_missing {
            "scode-editor-that-does-not-exist".to_string()
        } else {
            helper_command(
                env.workspace_root(),
                "failing editor",
                "import sys; sys.exit(17)",
            )
        };
        let mut child = env.spawn_with_env(
            &[],
            &[
                ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
                ("VISUAL", &command),
            ],
        );
        common::expect_input_line_cleared(&child, env.timeout(), "initial prompt");
        child.send("/memory\r").unwrap();
        child
            .expect(if is_missing {
                "Failed to launch editor"
            } else {
                "exited with"
            })
            .unwrap();
        assert_prompt_recovers(&mut child);
    }
}

#[test]
fn memory_picker_cancels_and_selects_without_a_second_input_reader() {
    let env = common::TestEnv::new("memory-file-picker");
    std::fs::write(env.workspace_root().join("AGENTS.md"), "original agents\n").unwrap();
    std::fs::create_dir_all(env.workspace_root().join(".nexus/sudocode")).unwrap();
    std::fs::write(
        env.workspace_root().join(".nexus/sudocode/AGENTS.md"),
        "original claude\n",
    )
    .unwrap();
    let command = helper_command(
        env.workspace_root(),
        "select editor",
        r#"
import pathlib, sys
assert pathlib.Path(sys.argv[1]).resolve().is_relative_to(pathlib.Path.cwd().resolve())
pathlib.Path(sys.argv[1]).write_bytes(b'selected memory\n')
"#,
    );
    let mut child = env.spawn_with_env(
        &[],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("VISUAL", &command),
        ],
    );
    common::expect_input_line_cleared(&child, env.timeout(), "initial prompt");
    child.send("/memory\r").unwrap();
    child.expect("Select memory file to edit").unwrap();
    child.send("\x1b").unwrap();
    common::expect_input_line_cleared(&child, env.timeout(), "cancelled memory picker");
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("AGENTS.md")).unwrap(),
        "original agents\n"
    );
    child.send("/memory\r").unwrap();
    child.expect("Select memory file to edit").unwrap();
    child.send(".nexus").unwrap();
    child.expect(r"1 match.*filter: .nexus").unwrap();
    child.send("\r").unwrap();
    child.expect("Opened memory file at").unwrap();
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join(".nexus/sudocode/AGENTS.md")).unwrap(),
        "selected memory\n"
    );
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("AGENTS.md")).unwrap(),
        "original agents\n"
    );
    assert_prompt_recovers(&mut child);
}

#[test]
fn status_pager_receives_eof_and_exclusive_terminal_input() {
    let env = common::TestEnv::new("status-pager-handoff");
    let command = helper_command(
        env.workspace_root(),
        "pager",
        r#"
import pathlib, sys
text = sys.stdin.read()
assert text.strip(), 'pager must receive the report and EOF'
pathlib.Path('pager-report.txt').write_text(text, encoding='utf-8')
if sys.platform == 'win32':
    import msvcrt
    print('PAGER_READY', file=sys.stderr, flush=True)
    key = msvcrt.getwch()
else:
    import termios, tty
    with open('/dev/tty', 'r') as terminal:
        attrs = termios.tcgetattr(terminal)
        assert attrs[3] & termios.ICANON
        try:
            tty.setraw(terminal)
            print('PAGER_READY', file=sys.stderr, flush=True)
            key = terminal.read(1)
        finally:
            termios.tcsetattr(terminal, termios.TCSANOW, attrs)
assert key == 'q', repr(key)
print('PAGER_DONE', flush=True)
"#,
    );
    let mut child = env.spawn_with_env(
        &[],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("PAGER", &command),
        ],
    );
    // Start with a small viewport so pagination is required without racing a
    // live resize against command input. Resize has its own acceptance suite.
    child.resize(12, 80).unwrap();
    common::expect_input_line_cleared(&child, env.timeout(), "initial prompt");
    child.send("/status").unwrap();
    common::expect_input_line(&child, "/status", env.timeout(), "pager command");
    child.send("\r").unwrap();
    child.expect("PAGER_READY").unwrap_or_else(|error| {
        panic!(
            "pager did not take terminal ownership: {error}\n{}",
            common::screen_tail(&child, 5000)
        );
    });
    child.send("q").unwrap();
    child.expect("PAGER_DONE").unwrap();
    assert!(
        std::fs::read_to_string(env.workspace_root().join("pager-report.txt"))
            .unwrap()
            .contains("Session")
    );
    assert_prompt_recovers(&mut child);
}
