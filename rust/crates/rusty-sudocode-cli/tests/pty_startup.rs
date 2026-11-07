//! The real CLI exposes startup phases only when explicitly enabled.
mod common;

#[cfg(windows)]
#[test]
fn native_startup_answers_conpty_before_its_device_attributes_timeout() {
    let env = common::TestEnv::new("startup-conpty-handshake");
    let started = std::time::Instant::now();
    let mut child = env.spawn_with_env(&["version"], &[]);
    child.expect("Sudo Code").unwrap();
    let elapsed = started.elapsed();
    assert_eq!(child.expect_eof().unwrap(), 0);
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "ConPTY device attributes handshake delayed native startup: {elapsed:?}"
    );
    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 0);
    }
}

#[test]
fn startup_information_fits_terminal_cells_without_losing_long_unicode_paths() {
    let env = common::TestEnv::new(
        "startup x y-workspace-abcdefghijklmnopqrstuvwxyz-abcdefghijklmnopqrstuvwxyz-中文",
    );
    let mut has_literal_path_space = false;
    for columns in [80, 40, 20] {
        let mut child =
            env.spawn_with_env_at_size(&["--permission-mode", "read-only"], &[], 100, columns);
        common::expect_input_line_cleared(&child, env.timeout(), "startup prompt ready");
        child.render(|screen| {
            let raw = screen.raw();
            assert_eq!(raw.size(), (100, columns));
            let rows: Vec<_> = raw.rows(0, columns).collect();
            has_literal_path_space |= rows.iter().any(|row| row.contains("x y"));
            let top = rows.iter().position(|row| row.starts_with('╭')).unwrap();
            let bottom = rows.iter().position(|row| row.starts_with('╰')).unwrap();
            assert!(
                bottom > top,
                "startup frame missing at {columns} columns: {rows:?}"
            );
            assert_eq!(
                raw.cell(u16::try_from(top).unwrap(), columns - 1)
                    .unwrap()
                    .contents(),
                "╮",
                "top border overflows {columns} columns: {rows:?}",
            );
            assert_eq!(
                raw.cell(u16::try_from(bottom).unwrap(), columns - 1)
                    .unwrap()
                    .contents(),
                "╯",
                "bottom border overflows {columns} columns: {rows:?}",
            );
            let mut information = String::new();
            for row in top + 1..bottom {
                let row = u16::try_from(row).unwrap();
                assert_eq!(raw.cell(row, 0).unwrap().contents(), "│");
                assert_eq!(
                    raw.cell(row, columns - 1).unwrap().contents(),
                    "│",
                    "information border misaligned at {columns} columns: {rows:?}",
                );
                for column in 1..columns - 1 {
                    information.push_str(raw.cell(row, column).unwrap().contents());
                }
            }
            let information: String = information.split_whitespace().collect();
            let directory = env.workspace_root().canonicalize().unwrap();
            // Windows canonicalization adds a verbatim prefix; getcwd does not.
            let directory = directory.to_str().unwrap().trim_start_matches(r"\\?\");
            // Apply the same visual whitespace projection to both operands.
            let directory: String = directory.split_whitespace().collect();
            assert!(
                information.contains(directory.as_str()),
                "full Unicode directory was lost at {columns} columns: {information:?}",
            );
            if columns == 20 {
                assert!(rows.iter().any(|row| row.trim() == "SudoCode"));
            }
        });
        child.send("/exit\r").unwrap();
        assert_eq!(child.expect_eof().unwrap(), 0);
    }
    assert!(
        has_literal_path_space,
        "filesystem spaces must remain visible"
    );
    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 0);
    }
}

#[test]
fn version_reports_opt_in_startup_phases_without_loading_an_agent() {
    let env = common::TestEnv::new("startup-timing");
    let mut child = env.spawn_with_env(&["version"], &[("SCODE_TRACE_STARTUP", "1")]);
    child.expect("startup phase=console elapsed_us=").unwrap();
    child.expect("startup phase=clap elapsed_us=").unwrap();
    child
        .expect("startup phase=permission_config elapsed_us=")
        .unwrap();
    child
        .expect("startup phase=legacy_config elapsed_us=")
        .unwrap();
    child
        .expect("startup phase=version_output elapsed_us=")
        .unwrap();
    assert_eq!(child.expect_eof().unwrap(), 0);
    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 0);
    }
}
