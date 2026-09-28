//! Print mode must never read approval answers, even from a real terminal.
mod common;

#[test]
fn print_denies_approval_without_reading_the_terminal() {
    let env = common::TestEnv::new("headless-permission");
    if env.is_live() {
        return;
    }
    let prompt = env.prompt("", "bash_permission_prompt_denied");
    let mut child = env.spawn(&[
        "-p",
        &prompt,
        "--permission-mode",
        "workspace-write",
        "--output-format=json",
    ]);
    child.set_default_timeout(std::time::Duration::from_secs(15));
    // Sending no keystrokes is the assertion: the old prompter would hang here.
    child
        .expect("interactive approval unavailable in print mode")
        .unwrap();
    assert_eq!(child.expect_eof().unwrap(), 0);
}
