//! Independent ANSI controls through the same native PTY as the application.
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Mutex, OnceLock};

use pty_expect::PtySession;

/// Resolve an indexed fixture color through the terminal, without consulting
/// application output. System `ConPTY` can expand indices into RGB values.
pub fn expected_color(expected: &str) -> String {
    match expected
        .strip_prefix("Idx(")
        .and_then(|s| s.strip_suffix(')'))
        .and_then(|s| s.parse::<u8>().ok())
    {
        Some(index) if index >= 16 => indexed(index),
        _ => expected.to_owned(),
    }
}

fn indexed(index: u8) -> String {
    static COLORS: OnceLock<Mutex<BTreeMap<u8, String>>> = OnceLock::new();
    let mut colors = COLORS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .expect("terminal palette control cache");
    colors.entry(index).or_insert_with(|| probe(index)).clone()
}

fn probe(index: u8) -> String {
    // Measure the requested index instead of maintaining a second palette list
    // that can omit valid colors used by new terminal journeys.
    let indices = [index];
    let mut command = String::from("printf '\\033[0m\\033[2J\\033[H");
    for index in indices {
        for (channel, sgr) in [("FG", 38), ("BG", 48)] {
            write!(
                command,
                "\\033[{sgr};5;{index}m{channel}{index}\\033[0m\\r\\n"
            )
            .unwrap();
        }
    }
    command.push_str("PALETTE-DONE\\r\\n'; IFS= read -r release");
    let mut session = PtySession::spawn(&super::common::resolve_sh(), &["-c", &command]).unwrap();
    super::common::expect_screen(
        &session,
        |s| s.contains("PALETTE-DONE"),
        super::common::DEFAULT_TIMEOUT,
        "independent terminal palette control",
    );
    let colors: BTreeMap<_, _> = session.render(|screen| {
        let raw = screen.raw();
        let rows: Vec<_> = raw.rows(0, raw.size().1).collect();
        indices
            .into_iter()
            .map(|index| {
                let observed = ["FG", "BG"].map(|channel| {
                    let label = format!("{channel}{index}");
                    let row = rows.iter().position(|row| row.trim_end() == label).unwrap();
                    let cell = raw.cell(u16::try_from(row).unwrap(), 0).unwrap();
                    assert!(!cell.bold() && !cell.dim(), "{label}: intensity");
                    let (color, opposite) = if channel == "FG" {
                        (
                            format!("{:?}", cell.fgcolor()),
                            format!("{:?}", cell.bgcolor()),
                        )
                    } else {
                        (
                            format!("{:?}", cell.bgcolor()),
                            format!("{:?}", cell.fgcolor()),
                        )
                    };
                    assert_eq!(opposite, "Default", "{label}: opposite channel");
                    let rgb = if index >= 232 {
                        let value = 8 + (index - 232) * 10;
                        format!("Rgb({value}, {value}, {value})")
                    } else {
                        let cube = index - 16;
                        let levels = [0, 95, 135, 175, 215, 255];
                        format!(
                            "Rgb({}, {}, {})",
                            levels[usize::from(cube / 36)],
                            levels[usize::from(cube / 6 % 6)],
                            levels[usize::from(cube % 6)]
                        )
                    };
                    assert!(
                        color == format!("Idx({index})") || (cfg!(windows) && color == rgb),
                        "{label}: unexpected terminal conversion: {color}"
                    );
                    color
                });
                assert_eq!(
                    observed[0], observed[1],
                    "index {index}: foreground/background conversion"
                );
                let [foreground, _] = observed;
                (index, foreground)
            })
            .collect()
    });
    session.send("\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
    colors[&index].clone()
}
